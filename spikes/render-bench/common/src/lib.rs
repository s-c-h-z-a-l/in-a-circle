//! Shared Wayland scaffolding and the measurement protocol for the render spikes.
//!
//! Every backend implements [`Renderer`]; [`run`] drives the identical
//! show → animate → hide → idle sequence and prints `M ...` metric lines.

pub mod metrics;
pub mod wheel;

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler},
};
use wayland_client::{
    Connection, EventQueue, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_surface::WlSurface},
};

pub use smithay_client_toolkit::shm;

/// What a renderer gets to talk to the compositor.
pub struct Ctx<'a> {
    pub conn: &'a Connection,
    pub shm: &'a Shm,
}

/// Per-frame scene description.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub hover: u32,
}

pub trait Renderer: Sized + 'static {
    /// Process-wide setup (GPU instance/device/pipeline). Called once at
    /// startup, or at every show in `--lazy` mode.
    fn init(cx: &Ctx) -> Self;
    /// Bind per-surface resources (swapchain, EGL surface, shm pool).
    fn attach(&mut self, cx: &Ctx, surface: &WlSurface, width: u32, height: u32);
    /// Draw one frame. Must attach a buffer and commit `surface`.
    fn draw(&mut self, surface: &WlSurface, frame: &Frame);
    /// Release per-surface resources. Called before the surface is destroyed.
    fn detach(&mut self);
}

#[derive(Debug, Clone)]
pub struct Opts {
    pub full: bool,
    pub lazy: bool,
    pub cycles: u32,
    pub frames: u32,
    pub idle_secs: u64,
}

impl Opts {
    pub fn from_args() -> Self {
        let mut o = Opts {
            full: false,
            lazy: false,
            cycles: 5,
            frames: 240,
            idle_secs: 10,
        };
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            let mut num = || args.next().and_then(|v| v.parse().ok()).expect(&a);
            match a.as_str() {
                "--full" => o.full = true,
                "--lazy" => o.lazy = true,
                "--cycles" => o.cycles = num() as u32,
                "--frames" => o.frames = num() as u32,
                "--idle" => o.idle_secs = num(),
                _ => panic!("unknown arg {a}"),
            }
        }
        o
    }
}

#[derive(Default)]
struct Cycle {
    t_show: Option<Instant>,
    configured: Option<Instant>,
    attached: Option<Instant>,
    first_submit: Option<Instant>,
    first_done: Option<Instant>,
    pointer_enter: Option<(Instant, f64, f64)>,
    frame: u32,
    draw_ns: u128,
    anim_start: Option<(Instant, u64)>,
    done: bool,
}

pub struct App<R: Renderer> {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    conn: Connection,
    pointer: Option<wl_pointer::WlPointer>,

    opts: Opts,
    renderer: Option<R>,
    layer: Option<LayerSurface>,
    size: (u32, u32),
    cycle: Cycle,
}

pub fn run<R: Renderer>(name: &str) {
    let t_main = Instant::now();
    let opts = Opts::from_args();
    println!(
        "M meta backend={name} full={} lazy={} cycles={} frames={}",
        opts.full, opts.lazy, opts.cycles, opts.frames
    );

    let conn = Connection::connect_to_env().expect("wayland connect");
    let (globals, mut eq) = registry_queue_init::<App<R>>(&conn).expect("registry");
    let qh = eq.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm");

    let mut app = App::<R> {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        conn: conn.clone(),
        pointer: None,
        opts: opts.clone(),
        renderer: None,
        layer: None,
        size: (0, 0),
        cycle: Cycle::default(),
    };
    eq.roundtrip(&mut app).expect("roundtrip");
    let t_wl = t_main.elapsed();
    metrics::report_mem("wayland_only");

    if !opts.lazy {
        let t = Instant::now();
        app.renderer = Some(R::init(&app.ctx()));
        println!(
            "M init wl_ms={:.2} init_ms={:.2}",
            t_wl.as_secs_f64() * 1e3,
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    metrics::report_mem("after_init");

    for i in 0..opts.cycles {
        app.show(&qh);
        while !app.cycle.done {
            eq.blocking_dispatch(&mut app).expect("dispatch");
        }
        app.report_cycle(i);
        if i == 0 {
            println!(
                "M cold first_frame_ms_since_main={:.2} first_frame_realtime_ns={}",
                (app.cycle.first_done.unwrap() - t_main).as_secs_f64() * 1e3,
                metrics::realtime_ns() - app.cycle.first_done.unwrap().elapsed().as_nanos()
            );
        }
        metrics::report_mem("shown");
        app.hide();
        eq.roundtrip(&mut app).expect("roundtrip");
        // Let drivers settle between cycles.
        idle(&mut eq, &mut app, Duration::from_millis(500));
        metrics::report_mem("hidden");
    }

    // Idle phase: the daemon's steady state, 99.9% of its life.
    let (c0, u0) = (metrics::ctx_switches(), metrics::cpu_us());
    println!("M idle_begin pid={}", std::process::id());
    idle(&mut eq, &mut app, Duration::from_secs(opts.idle_secs));
    let (c1, u1) = (metrics::ctx_switches(), metrics::cpu_us());
    println!(
        "M idle secs={} ctx_switches={} cpu_us={}",
        opts.idle_secs,
        c1 - c0,
        u1 - u0
    );
    metrics::report_mem("idle_end");
}

/// Dispatch events for `dur`, sleeping in poll() between them.
fn idle<R: Renderer>(eq: &mut EventQueue<App<R>>, app: &mut App<R>, dur: Duration) {
    let end = Instant::now() + dur;
    loop {
        let now = Instant::now();
        if now >= end {
            break;
        }
        eq.flush().ok();
        let Some(guard) = eq.prepare_read() else {
            eq.dispatch_pending(app).ok();
            continue;
        };
        let mut pfd = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (end - now).as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd, count 1.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n > 0 {
            guard.read().ok();
        } else {
            drop(guard);
        }
        eq.dispatch_pending(app).ok();
    }
}

fn ms(a: Instant, b: Option<Instant>) -> f64 {
    b.map_or(f64::NAN, |b| (b - a).as_secs_f64() * 1e3)
}

impl<R: Renderer> App<R> {
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            conn: &self.conn,
            shm: &self.shm,
        }
    }

    fn show(&mut self, qh: &QueueHandle<Self>) {
        self.cycle = Cycle {
            t_show: Some(Instant::now()),
            ..Default::default()
        };
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("inacircle-bench"),
            None,
        );
        if self.opts.full {
            layer.set_anchor(Anchor::all());
            layer.set_exclusive_zone(-1);
            layer.set_size(0, 0);
        } else {
            layer.set_size(wheel::BOX, wheel::BOX);
        }
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.layer = Some(layer);
    }

    fn hide(&mut self) {
        if let Some(r) = self.renderer.as_mut() {
            r.detach();
        }
        if self.opts.lazy {
            self.renderer = None;
        }
        self.layer = None;
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let Some(layer) = self.layer.as_ref() else {
            return;
        };
        let surface = layer.wl_surface().clone();
        surface.frame(qh, FrameCallbackData(surface.clone()));
        let frame = Frame {
            width: self.size.0,
            height: self.size.1,
            hover: wheel::hover_for(self.cycle.frame),
        };
        let t = Instant::now();
        self.renderer.as_mut().unwrap().draw(&surface, &frame);
        self.cycle.draw_ns += t.elapsed().as_nanos();
        if self.cycle.frame == 0 {
            self.cycle.first_submit = Some(Instant::now());
        }
    }

    fn report_cycle(&self, i: u32) {
        let c = &self.cycle;
        let t0 = c.t_show.unwrap();
        let (anim_t, anim_cpu) = c.anim_start.unwrap();
        let anim_frames = self.opts.frames.saturating_sub(1).max(1) as f64;
        let wall = anim_t.elapsed().as_secs_f64();
        let enter = c.pointer_enter.map_or("none".to_string(), |(t, x, y)| {
            format!("{:.2}ms@{x:.0},{y:.0}", (t - t0).as_secs_f64() * 1e3)
        });
        println!(
            "M cycle i={i} size={}x{} configure_ms={:.2} attach_ms={:.2} first_submit_ms={:.2} \
             first_frame_ms={:.2} draw_avg_us={:.1} anim_cpu_us_per_frame={:.1} anim_fps={:.1} \
             pointer_enter={enter}",
            self.size.0,
            self.size.1,
            ms(t0, c.configured),
            ms(t0, c.attached),
            ms(t0, c.first_submit),
            ms(t0, c.first_done),
            c.draw_ns as f64 / 1e3 / self.opts.frames as f64,
            (metrics::cpu_us() - anim_cpu) as f64 / anim_frames,
            anim_frames / wall,
        );
    }
}

impl<R: Renderer> CompositorHandler for App<R> {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, surface: &WlSurface, _: u32) {
        if self
            .layer
            .as_ref()
            .is_none_or(|l| l.wl_surface() != surface)
            || self.cycle.done
        {
            return;
        }
        let now = Instant::now();
        if self.cycle.frame == 0 {
            self.cycle.first_done = Some(now);
            self.cycle.anim_start = Some((now, metrics::cpu_us()));
        }
        self.cycle.frame += 1;
        if self.cycle.frame >= self.opts.frames {
            self.cycle.done = true;
            return;
        }
        self.draw(qh);
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl<R: Renderer> LayerShellHandler for App<R> {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.cycle.done = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        cfg: LayerSurfaceConfigure,
        _: u32,
    ) {
        if self.cycle.configured.is_some() {
            return;
        }
        self.cycle.configured = Some(Instant::now());
        let w = if cfg.new_size.0 == 0 {
            wheel::BOX
        } else {
            cfg.new_size.0
        };
        let h = if cfg.new_size.1 == 0 {
            wheel::BOX
        } else {
            cfg.new_size.1
        };
        self.size = (w, h);
        if self.renderer.is_none() {
            let t = Instant::now();
            self.renderer = Some(R::init(&self.ctx()));
            println!("M lazy_init init_ms={:.2}", t.elapsed().as_secs_f64() * 1e3);
        }
        let surface = layer.wl_surface().clone();
        let cx = Ctx {
            conn: &self.conn,
            shm: &self.shm,
        };
        self.renderer.as_mut().unwrap().attach(&cx, &surface, w, h);
        self.cycle.attached = Some(Instant::now());
        self.draw(qh);
    }
}

impl<R: Renderer> PointerHandler for App<R> {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for e in events {
            if self
                .layer
                .as_ref()
                .is_none_or(|l| l.wl_surface() != &e.surface)
            {
                continue;
            }
            if let PointerEventKind::Enter { .. } = e.kind {
                if self.cycle.pointer_enter.is_none() {
                    self.cycle.pointer_enter = Some((Instant::now(), e.position.0, e.position.1));
                }
            }
        }
    }
}

impl<R: Renderer> SeatHandler for App<R> {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl<R: Renderer> OutputHandler for App<R> {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl<R: Renderer> ShmHandler for App<R> {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl<R: Renderer> ProvidesRegistryState for App<R> {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(@<R: Renderer> App<R>);
smithay_client_toolkit::delegate_dispatch2!(@<R: Renderer> App<R>);
