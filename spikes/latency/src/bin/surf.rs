//! Surface strategy probe. Every mode ends with "wheel visible at the cursor,
//! with keyboard focus"; timestamps from main to the wheel's presentation.
//!
//!   box  — 480x480 layer surface, wheel in the first commit (no cursor pos)
//!   full — full-screen layer, full-size shm buffer: map transparent, wait for
//!          pointer+keyboard enter, then draw the wheel at the cursor
//!   vp   — full-screen layer with a 1x1 buffer scaled by wp_viewporter, wheel
//!          in a 480x480 subsurface placed at the cursor
//!
//! Usage: surf <box|full|vp> [--hold <secs>]   (hold: keep mapped, then exit)

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_pointer::{self, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
        wl_shm,
        wl_shm::WlShm,
        wl_shm_pool::WlShmPool,
        wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::WpPresentation,
    wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

const BOX: i32 = 480;

fn mono() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[derive(Default)]
struct State {
    size: (i32, i32),
    configured: bool,
    pointer: Option<(u64, f64, f64)>,
    kbd: u64,
    present: u64,
    done: bool,
}

/// shm buffer of w x h; `fill` = premultiplied ARGB to paint, or None to leave
/// the fresh (zero) pages untouched.
fn buffer(shm: &WlShm, qh: &QueueHandle<State>, w: i32, h: i32, fill: Option<u32>) -> WlBuffer {
    let size = (w * h * 4) as usize;
    // SAFETY: static C string; checked.
    let fd = unsafe { libc::memfd_create(c"surf".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: valid fd.
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), size as i64) }, 0);
    if let Some(c) = fill {
        // SAFETY: mapping exactly `size` bytes of our memfd.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED);
        // SAFETY: p maps `size` bytes; unmapped right after.
        unsafe {
            std::slice::from_raw_parts_mut(p as *mut u32, size / 4).fill(c);
            libc::munmap(p, size);
        }
    }
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let b = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    b
}

fn hwm_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

fn main() {
    let t_main = mono();
    let args: Vec<String> = std::env::args().collect();
    let mode = args[1].as_str();
    let hold: u64 = args
        .iter()
        .position(|a| a == "--hold")
        .map_or(0, |i| args[i + 1].parse().unwrap());

    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut eq) = registry_queue_init::<State>(&conn).unwrap();
    let qh = eq.handle();
    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
    let seat: WlSeat = globals.bind(&qh, 1..=9, ()).unwrap();
    let presentation: WpPresentation = globals.bind(&qh, 1..=1, ()).unwrap();
    let _ptr = seat.get_pointer(&qh, ());
    let _kbd = seat.get_keyboard(&qh, ());

    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Overlay,
        "inacircle-surf".into(),
        &qh,
        (),
    );
    if mode == "box" {
        layer.set_size(BOX as u32, BOX as u32);
    } else {
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(-1);
    }
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    surface.commit();

    let mut st = State::default();
    while !st.configured {
        eq.blocking_dispatch(&mut st).unwrap();
    }
    let (sw, sh) = if mode == "box" { (BOX, BOX) } else { st.size };
    let t_configure = mono();

    let wheel_color = 0xC020_2028;
    let t_wheel_commit;
    let mut _keep: Vec<Box<dyn std::any::Any>> = Vec::new();
    match mode {
        "box" => {
            let b = buffer(&shm, &qh, BOX, BOX, Some(wheel_color));
            surface.attach(Some(&b), 0, 0);
            surface.damage_buffer(0, 0, BOX, BOX);
            presentation.feedback(&surface, &qh, ());
            surface.commit();
            t_wheel_commit = mono();
            _keep.push(Box::new(b));
        }
        "full" | "vp" => {
            // Phase 1: transparent full-screen surface, to get cursor + focus.
            let vp = mode == "vp";
            let bg = if vp {
                buffer(&shm, &qh, 1, 1, None)
            } else {
                buffer(&shm, &qh, sw, sh, None)
            };
            let mut viewport = None;
            if vp {
                let vpr: WpViewporter = globals.bind(&qh, 1..=1, ()).unwrap();
                let v = vpr.get_viewport(&surface, &qh, ());
                v.set_destination(sw, sh);
                viewport = Some((vpr, v));
            }
            surface.attach(Some(&bg), 0, 0);
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            surface.commit();
            while st.pointer.is_none() || st.kbd == 0 {
                eq.blocking_dispatch(&mut st).unwrap();
            }
            let (_, px, py) = st.pointer.unwrap();
            let (x, y) = (
                (px as i32 - BOX / 2).clamp(0, sw - BOX),
                (py as i32 - BOX / 2).clamp(0, sh - BOX),
            );

            // Phase 2: wheel at the cursor.
            if vp {
                let sub_c: WlSubcompositor = globals.bind(&qh, 1..=1, ()).unwrap();
                let ws = compositor.create_surface(&qh, ());
                let sub = sub_c.get_subsurface(&ws, &surface, &qh, ());
                sub.set_position(x, y);
                let wb = buffer(&shm, &qh, BOX, BOX, Some(wheel_color));
                ws.attach(Some(&wb), 0, 0);
                ws.damage_buffer(0, 0, BOX, BOX);
                ws.commit();
                presentation.feedback(&surface, &qh, ());
                surface.commit(); // applies the synced subsurface state
                _keep.push(Box::new((sub_c, ws, sub, wb)));
            } else {
                // Full-size buffer: fresh one with only the wheel rect painted.
                let size = (sw * sh * 4) as usize;
                // SAFETY: static C string; checked.
                let fd = unsafe { libc::memfd_create(c"surf2".as_ptr(), libc::MFD_CLOEXEC) };
                // SAFETY: fresh fd we own.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                // SAFETY: valid fd.
                assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), size as i64) }, 0);
                // SAFETY: mapping exactly `size` bytes of our memfd.
                let p = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        fd.as_raw_fd(),
                        0,
                    )
                };
                // SAFETY: p maps `size` bytes.
                let px = unsafe { std::slice::from_raw_parts_mut(p as *mut u32, size / 4) };
                for row in y..y + BOX {
                    let s = (row * sw + x) as usize;
                    px[s..s + BOX as usize].fill(wheel_color);
                }
                let pool = shm.create_pool(fd.as_fd(), size as i32, &qh, ());
                let b = pool.create_buffer(0, sw, sh, sw * 4, wl_shm::Format::Argb8888, &qh, ());
                surface.attach(Some(&b), 0, 0);
                surface.damage_buffer(x, y, BOX, BOX);
                presentation.feedback(&surface, &qh, ());
                surface.commit();
                _keep.push(Box::new((pool, b, fd)));
            }
            t_wheel_commit = mono();
            _keep.push(Box::new((bg, viewport)));
        }
        "daemon" => {
            daemon(
                &conn,
                &mut eq,
                &qh,
                &globals,
                &compositor,
                &shm,
                &surface,
                &layer,
                &presentation,
                sw,
                sh,
            );
            return;
        }
        _ => panic!("mode"),
    }
    conn.flush().unwrap();
    while !st.done {
        eq.blocking_dispatch(&mut st).unwrap();
    }

    let us = |a: u64, b: u64| {
        if a == 0 || b == 0 {
            -1
        } else {
            (b as i64 - a as i64) / 1000
        }
    };
    println!(
        "M surf mode={mode} size={sw}x{sh} configure_us={} pointer_us={} kbd_us={} wheel_commit_us={} present_us={} hwm_kb={} pos={:?}",
        us(t_main, t_configure),
        st.pointer.map_or(-1, |p| us(t_main, p.0)),
        us(t_main, st.kbd),
        us(t_main, t_wheel_commit),
        us(t_main, st.present),
        hwm_kb(),
        st.pointer.map(|p| (p.1 as i32, p.2 as i32)),
    );

    if hold > 0 {
        println!("M hold_begin");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(hold);
        while std::time::Instant::now() < end {
            conn.flush().unwrap();
            if let Some(g) = eq.prepare_read() {
                let mut pfd = libc::pollfd {
                    fd: g.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                if unsafe { libc::poll(&mut pfd, 1, 100) } > 0 {
                    g.read().ok();
                }
            }
            eq.dispatch_pending(&mut st).unwrap();
        }
    }
}

/// Daemon variant: a vp surface stays mapped, click-through and unfocused.
/// Each trigger enables input + exclusive focus, waits for pointer/keyboard
/// enter, shows the wheel subsurface at the cursor; then reverts.
#[allow(clippy::too_many_arguments)]
fn daemon(
    conn: &Connection,
    eq: &mut wayland_client::EventQueue<State>,
    qh: &QueueHandle<State>,
    globals: &wayland_client::globals::GlobalList,
    compositor: &WlCompositor,
    shm: &WlShm,
    surface: &WlSurface,
    layer: &ZwlrLayerSurfaceV1,
    presentation: &WpPresentation,
    sw: i32,
    sh: i32,
) {
    let mut st = State::default();
    let vpr: WpViewporter = globals.bind(qh, 1..=1, ()).unwrap();
    let v = vpr.get_viewport(surface, qh, ());
    v.set_destination(sw, sh);
    let bg = buffer(shm, qh, 1, 1, None);
    let empty = compositor.create_region(qh, ());
    surface.set_input_region(Some(&empty));
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    surface.attach(Some(&bg), 0, 0);
    surface.commit();

    let sub_c: WlSubcompositor = globals.bind(qh, 1..=1, ()).unwrap();
    let ws = compositor.create_surface(qh, ());
    let sub = sub_c.get_subsurface(&ws, surface, qh, ());
    ws.set_input_region(Some(&empty));
    let wb = buffer(shm, qh, BOX, BOX, Some(0xC020_2028));
    conn.flush().unwrap();
    eq.roundtrip(&mut st).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    eq.dispatch_pending(&mut st).unwrap();

    for i in 0..15 {
        st.pointer = None;
        st.kbd = 0;
        st.done = false;
        let t0 = mono();
        // Trigger: whole surface accepts input again, take focus.
        surface.set_input_region(None);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        surface.commit();
        conn.flush().unwrap();
        let deadline = t0 + 300_000_000;
        while (st.pointer.is_none() || st.kbd == 0) && mono() < deadline {
            if let Some(g) = eq.prepare_read() {
                let mut pfd = libc::pollfd {
                    fd: g.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                if unsafe { libc::poll(&mut pfd, 1, 10) } > 0 {
                    g.read().ok();
                }
            }
            eq.dispatch_pending(&mut st).unwrap();
        }
        let got_ptr = st.pointer.map(|p| p.0);
        let (x, y) = st
            .pointer
            .map_or((sw / 2 - BOX / 2, sh / 2 - BOX / 2), |(_, px, py)| {
                (
                    (px as i32 - BOX / 2).clamp(0, sw - BOX),
                    (py as i32 - BOX / 2).clamp(0, sh - BOX),
                )
            });
        sub.set_position(x, y);
        ws.attach(Some(&wb), 0, 0);
        ws.damage_buffer(0, 0, BOX, BOX);
        ws.commit();
        presentation.feedback(surface, qh, ());
        surface.commit();
        conn.flush().unwrap();
        while !st.done {
            eq.blocking_dispatch(&mut st).unwrap();
        }
        let us = |b: u64| {
            if b == 0 {
                -1
            } else {
                (b as i64 - t0 as i64) / 1000
            }
        };
        println!(
            "M daemon i={i} pointer_us={} kbd_us={} present_us={}",
            got_ptr.map_or(-1, us),
            us(st.kbd),
            us(st.present)
        );
        // Hide: remove wheel, click-through, release focus.
        std::thread::sleep(std::time::Duration::from_millis(150));
        ws.attach(None, 0, 0);
        ws.commit();
        surface.set_input_region(Some(&empty));
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();
        conn.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(250));
        eq.dispatch_pending(&mut st).unwrap();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        st: &mut Self,
        ls: &ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = ev
        {
            ls.ack_configure(serial);
            st.size = (width as i32, height as i32);
            st.configured = true;
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        st: &mut Self,
        _: &WlPointer,
        ev: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Enter {
            surface_x,
            surface_y,
            ..
        } = ev
            && st.pointer.is_none()
        {
            st.pointer = Some((mono(), surface_x, surface_y));
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        st: &mut Self,
        _: &WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Enter { .. } = ev
            && st.kbd == 0
        {
            st.kbd = mono();
        }
    }
}

impl Dispatch<WpPresentationFeedback, ()> for State {
    fn event(
        st: &mut Self,
        _: &WpPresentationFeedback,
        ev: wp_presentation_feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                ..
            } => {
                st.present = (((tv_sec_hi as u64) << 32) | tv_sec_lo as u64) * 1_000_000_000
                    + tv_nsec as u64;
                st.done = true;
            }
            wp_presentation_feedback::Event::Discarded => st.done = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore wayland_client::protocol::wl_region::WlRegion);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore WlSubcompositor);
delegate_noop!(State: ignore WlSubsurface);
delegate_noop!(State: ignore WpPresentation);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore ZwlrLayerShellV1);
