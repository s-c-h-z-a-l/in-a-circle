//! The vp path on a hand-written wire protocol (libc only): full-screen layer
//! surface with a 1x1 buffer scaled by wp_viewporter -> cursor + focus ->
//! selection offers (MIME lists) -> wheel in a subsurface -> wp_presentation.
//! Same timestamps as `latency/surf vp`, plus context detection.

mod wire;

use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use wire::{Arg::*, DISPLAY, Iface, Wl};

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

fn memfd(size: usize, fill: Option<u32>) -> OwnedFd {
    // SAFETY: static C string; checked.
    let fd = unsafe { libc::memfd_create(c"wire".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: valid fd.
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), size as i64) }, 0);
    if let Some(c) = fill {
        // SAFETY: mapping exactly `size` bytes of our memfd, unmapped below.
        unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            );
            assert_ne!(p, libc::MAP_FAILED);
            std::slice::from_raw_parts_mut(p as *mut u32, size / 4).fill(c);
            libc::munmap(p, size);
        }
    }
    fd
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

#[derive(Default)]
struct Offer {
    id: u32,
    mimes: Vec<String>,
}

#[derive(Default)]
struct St {
    globals: Vec<(u32, String, u32)>,
    sync_done: bool,
    configured: Option<(u32, u32, u32)>, // serial, w, h
    pointer: Option<(u64, f64, f64)>,
    kbd: u64,
    offers: Vec<Offer>,
    clip: Option<Option<Offer>>,
    prim: Option<Option<Offer>>,
    present: u64,
    done: bool,
}

fn dispatch(wl: &mut Wl, st: &mut St) {
    while let Some(mut e) = wl.event() {
        use Iface::*;
        match (e.iface, e.op) {
            (Some(Display), 0) => {
                let (obj, code) = (e.u32(), e.u32());
                panic!("protocol error obj={obj} code={code}: {}", e.string());
            }
            (Some(Registry), 0) => {
                let (name, iface, ver) = (e.u32(), e.string(), e.u32());
                st.globals.push((name, iface, ver));
            }
            (Some(Callback), 0) => st.sync_done = true,
            (Some(LayerSurface), 0) => {
                let (s, w, h) = (e.u32(), e.u32(), e.u32());
                st.configured = Some((s, w, h));
            }
            (Some(Pointer), 0) if st.pointer.is_none() => {
                let (_serial, _surf) = (e.u32(), e.u32());
                let (x, y) = (e.fixed(), e.fixed());
                st.pointer = Some((mono(), x, y));
            }
            (Some(Keyboard), 0) => drop(wl.take_fd()), // keymap fd: not needed
            (Some(Keyboard), 1) if st.kbd == 0 => st.kbd = mono(),
            (Some(DataDevice), 0) | (Some(PrimaryDevice), 0) => {
                let id = e.u32();
                let kind = if e.iface == Some(DataDevice) {
                    DataOffer
                } else {
                    PrimaryOffer
                };
                wl.register_server_id(id, kind);
                st.offers.push(Offer {
                    id,
                    mimes: Vec::new(),
                });
            }
            (Some(DataOffer), 0) | (Some(PrimaryOffer), 0) => {
                let m = e.string();
                if let Some(o) = st.offers.iter_mut().find(|o| o.id == e.obj) {
                    o.mimes.push(m);
                }
            }
            (Some(DataDevice), 5) | (Some(PrimaryDevice), 1) => {
                let id = e.u32();
                let offer = st
                    .offers
                    .iter()
                    .position(|o| o.id == id)
                    .map(|i| st.offers.swap_remove(i));
                if e.iface == Some(DataDevice) {
                    st.clip = Some(offer)
                } else {
                    st.prim = Some(offer)
                }
            }
            (Some(PresentationFeedback), 1) => {
                let (hi, lo, ns) = (e.u32(), e.u32(), e.u32());
                st.present = (((hi as u64) << 32) | lo as u64) * 1_000_000_000 + ns as u64;
                st.done = true;
            }
            (Some(PresentationFeedback), 2) => st.done = true,
            _ => {}
        }
    }
}

fn wait(wl: &mut Wl, st: &mut St, until: impl Fn(&St) -> bool) {
    wl.flush().unwrap();
    dispatch(wl, st);
    while !until(st) {
        wl.read(-1).unwrap();
        dispatch(wl, st);
    }
}

fn main() {
    let t_main = mono();
    let mut wl = Wl::connect().expect("connect");
    let mut st = St::default();

    let registry = wl.new_id(Iface::Registry);
    wl.req(DISPLAY, 1, &[U(registry)]);
    let cb = wl.new_id(Iface::Callback);
    wl.req(DISPLAY, 0, &[U(cb)]);
    wait(&mut wl, &mut st, |s| s.sync_done);

    let bind = |wl: &mut Wl, name: &str, iface: Iface, max: u32| -> u32 {
        let (gname, _, ver) = st
            .globals
            .iter()
            .find(|g| g.1 == name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .clone();
        let id = wl.new_id(iface);
        wl.req(registry, 0, &[U(gname), S(name), U(ver.min(max)), U(id)]);
        id
    };
    let compositor = bind(&mut wl, "wl_compositor", Iface::Compositor, 4);
    let shm = bind(&mut wl, "wl_shm", Iface::Shm, 1);
    let seat = bind(&mut wl, "wl_seat", Iface::Seat, 7);
    let subc = bind(&mut wl, "wl_subcompositor", Iface::Subcompositor, 1);
    let ddm = bind(
        &mut wl,
        "wl_data_device_manager",
        Iface::DataDeviceManager,
        3,
    );
    let viewporter = bind(&mut wl, "wp_viewporter", Iface::Viewporter, 1);
    let presentation = bind(&mut wl, "wp_presentation", Iface::Presentation, 1);
    let psm = bind(
        &mut wl,
        "zwp_primary_selection_device_manager_v1",
        Iface::PrimaryManager,
        1,
    );
    let layer_shell = bind(&mut wl, "zwlr_layer_shell_v1", Iface::LayerShell, 4);
    let t_registry = mono();

    let pointer = wl.new_id(Iface::Pointer);
    wl.req(seat, 0, &[U(pointer)]);
    let keyboard = wl.new_id(Iface::Keyboard);
    wl.req(seat, 1, &[U(keyboard)]);
    let dd = wl.new_id(Iface::DataDevice);
    wl.req(ddm, 1, &[U(dd), U(seat)]);
    let pd = wl.new_id(Iface::PrimaryDevice);
    wl.req(psm, 1, &[U(pd), U(seat)]);

    // Full-screen overlay, exclusive keyboard focus.
    let surface = wl.new_id(Iface::Surface);
    wl.req(compositor, 0, &[U(surface)]);
    let layer = wl.new_id(Iface::LayerSurface);
    wl.req(
        layer_shell,
        0,
        &[
            U(layer),
            U(surface),
            U(0),
            U(3 /* overlay */),
            S("inacircle-wire"),
        ],
    );
    wl.req(layer, 1, &[U(15 /* all anchors */)]);
    wl.req(layer, 2, &[I(-1)]);
    wl.req(layer, 4, &[U(1 /* exclusive */)]);
    wl.req(surface, 6, &[]);
    wait(&mut wl, &mut st, |s| s.configured.is_some());
    let (serial, sw, sh) = st.configured.unwrap();
    let (sw, sh) = (sw as i32, sh as i32);
    wl.req(layer, 6, &[U(serial)]);
    let t_configure = mono();

    // 1x1 transparent buffer scaled to the whole output.
    let fd1 = memfd(4, None);
    let pool1 = wl.new_id(Iface::ShmPool);
    wl.req(shm, 0, &[U(pool1), Fd(fd1.as_fd()), I(4)]);
    let bg = wl.new_id(Iface::Buffer);
    wl.req(
        pool1,
        0,
        &[U(bg), I(0), I(1), I(1), I(4), U(0 /* argb8888 */)],
    );
    let viewport = wl.new_id(Iface::Viewport);
    wl.req(viewporter, 1, &[U(viewport), U(surface)]);
    wl.req(viewport, 2, &[I(sw), I(sh)]);
    wl.req(surface, 1, &[U(bg), I(0), I(0)]);
    wl.req(surface, 9, &[I(0), I(0), I(i32::MAX), I(i32::MAX)]);
    wl.req(surface, 6, &[]);
    wait(&mut wl, &mut st, |s| s.pointer.is_some() && s.kbd != 0);
    let (_, px, py) = st.pointer.unwrap();
    let (x, y) = (
        (px as i32 - BOX / 2).clamp(0, sw - BOX),
        (py as i32 - BOX / 2).clamp(0, sh - BOX),
    );

    // Wheel subsurface at the cursor (synced: applied by the parent commit).
    let ws = wl.new_id(Iface::Surface);
    wl.req(compositor, 0, &[U(ws)]);
    let sub = wl.new_id(Iface::Subsurface);
    wl.req(subc, 1, &[U(sub), U(ws), U(surface)]);
    wl.req(sub, 1, &[I(x), I(y)]);
    let size = (BOX * BOX * 4) as usize;
    let fd2 = memfd(size, Some(0xC020_2028));
    let pool2 = wl.new_id(Iface::ShmPool);
    wl.req(shm, 0, &[U(pool2), Fd(fd2.as_fd()), I(size as i32)]);
    let wb = wl.new_id(Iface::Buffer);
    wl.req(pool2, 0, &[U(wb), I(0), I(BOX), I(BOX), I(BOX * 4), U(0)]);
    wl.req(ws, 1, &[U(wb), I(0), I(0)]);
    wl.req(ws, 9, &[I(0), I(0), I(BOX), I(BOX)]);
    wl.req(ws, 6, &[]);
    let fb = wl.new_id(Iface::PresentationFeedback);
    wl.req(presentation, 1, &[U(surface), U(fb)]);
    wl.req(surface, 6, &[]);
    wl.flush().unwrap();
    let t_wheel_commit = mono();

    // Context: selection events follow keyboard enter.
    wait(&mut wl, &mut st, |s| s.clip.is_some() && s.prim.is_some());
    let t_offers = mono();
    wait(&mut wl, &mut st, |s| s.done);

    // Lazy peek (off the critical path): first 4 KB of the clipboard text.
    let t_peek = mono();
    let mut peek = 0usize;
    if let Some(Some(o)) = &st.clip
        && let Some(m) = o.mimes.iter().find(|m| m.starts_with("text/plain"))
    {
        let mut fds = [0; 2];
        // SAFETY: valid 2-element array.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: fresh descriptors we own.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        wl.req(o.id, 1, &[S(m), Fd(w.as_fd())]);
        wl.flush().unwrap();
        drop(w);
        let mut buf = [0u8; 4096];
        peek = std::fs::File::from(r).read(&mut buf).unwrap_or(0);
    }
    let t_end = mono();

    // SAFETY: getrusage writes into the zeroed struct.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    let cpu_us = tv(ru.ru_utime) + tv(ru.ru_stime);
    let us = |b: u64| {
        if b == 0 {
            -1
        } else {
            (b as i64 - t_main as i64) / 1000
        }
    };
    let mimes = |o: &Option<Option<Offer>>| {
        o.as_ref()
            .and_then(|o| o.as_ref())
            .map_or("none".into(), |o| o.mimes.join(","))
    };
    println!(
        "M wire size={sw}x{sh} registry_us={} configure_us={} pointer_us={} kbd_us={} wheel_commit_us={} offers_us={} present_us={} peek_bytes={peek} peek_us={} cpu_us={cpu_us} minflt={} hwm_kb={} clip=[{}] prim=[{}]",
        us(t_registry),
        us(t_configure),
        st.pointer.map_or(-1, |p| us(p.0)),
        us(st.kbd),
        us(t_wheel_commit),
        us(t_offers),
        us(st.present),
        (t_end - t_peek) / 1000,
        ru.ru_minflt,
        hwm_kb(),
        mimes(&st.clip),
        mimes(&st.prim),
    );
}
