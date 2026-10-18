//! Startup latency probe: timestamps every phase from process start to the
//! moment the first frame is actually presented (wp_presentation), using
//! wayland-client directly (no SCTK).
//!
//! Usage: probe --t0 <CLOCK_MONOTONIC ns> --out <file> [--noop]

use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor,
        wl_registry::WlRegistry, wl_shm, wl_shm::WlShm, wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::{self, WpPresentation},
    wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

const W: i32 = 480;
const H: i32 = 480;

fn mono() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[derive(Default)]
struct State {
    configure: u64,
    configured: bool,
    frame: u64,
    present: u64,
    present_flags: u32,
    refresh_ns: u32,
    clk_id: u32,
    done: bool,
}

fn main() {
    let t_main = mono();
    let mut t0 = 0u64;
    let mut out = String::new();
    let mut noop = false;
    let mut persist = 0u32;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--t0" => t0 = args.next().unwrap().parse().unwrap(),
            "--out" => out = args.next().unwrap(),
            "--noop" => noop = true,
            "--persist" => persist = args.next().unwrap().parse().unwrap(),
            _ => panic!("unknown arg {a}"),
        }
    }
    let write = |line: String| {
        let mut f = OpenOptions::new().create(true).append(true).open(&out).unwrap();
        writeln!(f, "{line}").unwrap();
    };
    if noop {
        write(format!("t0={t0} main={t_main}"));
        return;
    }

    let conn = Connection::connect_to_env().unwrap();
    let t_connect = mono();
    let (globals, mut eq) = registry_queue_init::<State>(&conn).unwrap();
    let qh = eq.handle();
    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
    let presentation: WpPresentation = globals.bind(&qh, 1..=1, ()).unwrap();
    let t_registry = mono();

    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Overlay,
        "inacircle-probe".into(),
        &qh,
        (),
    );
    layer.set_size(W as u32, H as u32);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    surface.commit();

    // Prepare the buffer while the configure is in flight.
    let size = (W * H * 4) as usize;
    // SAFETY: memfd_create with a static C string; result checked.
    let fd = unsafe { libc::memfd_create(c"inacircle".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: fd is a fresh owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fd valid; size fits.
    unsafe {
        assert_eq!(libc::ftruncate(fd.as_fd().as_raw_fd_compat(), size as i64), 0);
    }
    // SAFETY: mapping a memfd we own, of exactly `size` bytes.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_fd().as_raw_fd_compat(),
            0,
        )
    };
    assert_ne!(ptr, libc::MAP_FAILED);
    // SAFETY: ptr maps `size` bytes, exclusively ours until attached.
    let px = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, size / 4) };
    px.fill(0x8020_2020); // premultiplied translucent grey
    let pool = shm.create_pool(fd.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(0, W, H, W * 4, wl_shm::Format::Argb8888, &qh, ());
    let t_buffer = mono();

    let mut st = State::default();
    while !st.configured {
        eq.blocking_dispatch(&mut st).unwrap();
    }

    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, W, H);
    surface.frame(&qh, ());
    presentation.feedback(&surface, &qh, ());
    surface.commit();
    conn.flush().unwrap();
    let t_commit = mono();

    while !st.done {
        eq.blocking_dispatch(&mut st).unwrap();
    }

    // Persist mode: surface stays mapped; measure commit -> present for
    // content updates (what a daemon with a mapped surface would see).
    let mut upd = Vec::new();
    for i in 0..persist {
        std::thread::sleep(std::time::Duration::from_millis(120));
        px[(i as usize * 997) % px.len()] ^= 0x0000_00ff;
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, 1, 1);
        presentation.feedback(&surface, &qh, ());
        st.done = false;
        surface.commit();
        conn.flush().unwrap();
        let tc = mono();
        while !st.done {
            eq.blocking_dispatch(&mut st).unwrap();
        }
        if st.present > tc {
            upd.push(format!("{}", st.present - tc));
        }
    }
    if persist > 0 {
        write(format!("upd {}", upd.join(" ")));
    }

    write(format!(
        "t0={t0} main={t_main} connect={t_connect} registry={t_registry} buffer={t_buffer} \
         configure={} commit={t_commit} frame={} present={} clk={} flags={} refresh={}",
        st.configure, st.frame, st.present, st.clk_id, st.present_flags, st.refresh_ns
    ));
}

trait RawFdCompat {
    fn as_raw_fd_compat(&self) -> i32;
}
impl RawFdCompat for std::os::fd::BorrowedFd<'_> {
    fn as_raw_fd_compat(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.as_raw_fd()
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as wayland_client::Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(st: &mut Self, ls: &ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = ev {
            ls.ack_configure(serial);
            if !st.configured {
                st.configure = mono();
                st.configured = true;
            }
        }
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn event(st: &mut Self, _: &WlCallback, _: <WlCallback as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        st.frame = mono();
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn event(st: &mut Self, _: &WpPresentation, ev: wp_presentation::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wp_presentation::Event::ClockId { clk_id } = ev {
            st.clk_id = clk_id;
        }
    }
}

impl Dispatch<WpPresentationFeedback, ()> for State {
    fn event(st: &mut Self, _: &WpPresentationFeedback, ev: wp_presentation_feedback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wp_presentation_feedback::Event::Presented { tv_sec_hi, tv_sec_lo, tv_nsec, refresh, flags, .. } => {
                let sec = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
                st.present = sec * 1_000_000_000 + tv_nsec as u64;
                st.refresh_ns = refresh;
                st.present_flags = flags.into();
                st.done = true;
            }
            wp_presentation_feedback::Event::Discarded => {
                st.present = 0;
                st.done = true;
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore ZwlrLayerShellV1);
