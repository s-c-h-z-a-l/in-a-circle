//! Integration probe: drag files into the wheel, on the real surface layout.
//!
//! Own wire protocol + vp surface (1x1 buffer scaled by wp_viewporter) +
//! wheel subsurface with an empty input region + cpu-sdf wheel + drag and
//! drop. The wheel is painted while waiting for the cursor position, which
//! comes from wl_pointer.enter (no drag) or wl_data_device.enter (drag).
//! Hover follows motion; a drop on a slice accepts copy only, anywhere else
//! (hub, outside the ring) the offer is rejected, which cancels the drag.
//!
//! Usage: drag [exclusive|ondemand|none]   (keyboard interactivity)
//! Run it from a compositor keybind with repeat=false. Log, appended:
//! $XDG_RUNTIME_DIR/inacircle-drag.log

#[path = "../wheel.rs"]
mod wheel;
#[path = "../wire.rs"]
mod wire;

use std::fmt::Write as _;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use wheel::{BOX, NONE, Wheel};
use wire::{Arg::*, DISPLAY, Iface, Wl};

/// The event loop gives up after this; SIGALRM kills us a bit later if the
/// loop itself is stuck. A hung full-screen overlay locks the user out.
const DEADLINE_MS: u64 = 15_000;
const ALARM_S: u32 = 20;
/// Longest we wait for the drag source to write the payload.
const PAYLOAD_MS: u64 = 500;
const DND_COPY: u32 = 1;
const KEY_ESC: u32 = 1;
const URI_LIST: &str = "text/uri-list";

fn mono_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn runtime_path(name: &str) -> String {
    format!(
        "{}/{name}",
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())
    )
}

struct St {
    t0: u64,
    log: String,
    globals: Vec<(u32, String, u32)>,
    sync_done: bool,
    configured: Option<(u32, u32, u32)>,
    surface: u32,
    /// Top-left of the wheel in surface coordinates, once the cursor is known.
    wheel_at: Option<(i32, i32)>,
    hover: u32,
    /// Server-created offers and their MIME types.
    offers: Vec<(u32, Vec<String>)>,
    /// Current drag: offer id, enter serial, whether we accept right now.
    dnd: Option<(u32, u32, bool)>,
    dropped: bool,
    released: Vec<u32>,
    motions: u32,
    quit: Option<String>,
}

impl St {
    fn note(&mut self, msg: impl AsRef<str>) {
        let ms = (mono_ns() - self.t0) as f64 / 1e6;
        let _ = writeln!(self.log, "{ms:9.3} ms  {}", msg.as_ref());
    }

    /// First cursor position wins: it decides where the wheel goes.
    fn place(&mut self, x: f64, y: f64, src: &str) {
        if self.wheel_at.is_some() {
            return;
        }
        let (_, sw, sh) = self.configured.unwrap();
        let b = BOX as i32;
        let at = (
            (x as i32 - b / 2).clamp(0, (sw as i32 - b).max(0)),
            (y as i32 - b / 2).clamp(0, (sh as i32 - b).max(0)),
        );
        self.wheel_at = Some(at);
        self.note(format!("cursor from {src} @{x:.0},{y:.0}"));
    }

    fn hover_at(&mut self, x: f64, y: f64) {
        if let Some((wx, wy)) = self.wheel_at {
            self.hover = wheel::segment_at(x - wx as f64, y - wy as f64);
        }
    }
}

fn dispatch(wl: &mut Wl, st: &mut St) {
    while let Some(mut e) = wl.event() {
        use Iface::*;
        match (e.iface, e.op) {
            (Some(Display), 0) => {
                let (obj, code) = (e.u32(), e.u32());
                let msg = e.string();
                st.note(format!("PROTOCOL ERROR obj={obj} code={code}: {msg}"));
                st.quit = Some("protocol error".into());
            }
            (Some(Registry), 0) => {
                let (name, iface, ver) = (e.u32(), e.string(), e.u32());
                st.globals.push((name, iface, ver));
            }
            (Some(Callback), 0) => st.sync_done = true,
            (Some(LayerSurface), 0) => st.configured = Some((e.u32(), e.u32(), e.u32())),
            (Some(LayerSurface), 1) => st.quit = Some("layer surface closed".into()),
            (Some(Buffer), 0) => st.released.push(e.obj),

            // Pointer: only without a drag (the drag owns the pointer).
            (Some(Pointer), 0) => {
                let (_serial, surf) = (e.u32(), e.u32());
                let (x, y) = (e.fixed(), e.fixed());
                st.note(format!("pointer enter surface={}", tag(st, surf)));
                st.place(x, y, "wl_pointer.enter");
                st.hover_at(x, y);
            }
            (Some(Pointer), 1) => st.note("pointer leave"),
            (Some(Pointer), 2) => {
                let (_time, x, y) = (e.u32(), e.fixed(), e.fixed());
                st.hover_at(x, y);
            }
            (Some(Pointer), 3) => {
                let (_serial, _time, button, state) = (e.u32(), e.u32(), e.u32(), e.u32());
                if state == 1 {
                    st.note(format!(
                        "button {button:#x} pressed, hover={}",
                        seg(st.hover)
                    ));
                    st.quit = Some(format!("click on {}", seg(st.hover)));
                }
            }

            (Some(Keyboard), 0) => {
                let _format = e.u32();
                drop(wl.take_fd()); // keymap: not needed, but the fd must be taken
            }
            (Some(Keyboard), 1) => {
                let (_serial, surf) = (e.u32(), e.u32());
                st.note(format!("keyboard enter surface={}", tag(st, surf)));
            }
            (Some(Keyboard), 2) => st.note("keyboard leave"),
            (Some(Keyboard), 3) => {
                let (_serial, _time, key, state) = (e.u32(), e.u32(), e.u32(), e.u32());
                st.note(format!("key {key} state {state}"));
                if key == KEY_ESC && state == 1 {
                    st.quit = Some("esc".into());
                }
            }

            (Some(DataDevice), 0) => {
                let id = e.u32();
                wl.register_server_id(id, DataOffer);
                st.offers.push((id, Vec::new()));
            }
            (Some(DataOffer), 0) => {
                let m = e.string();
                if let Some(o) = st.offers.iter_mut().find(|o| o.0 == e.obj) {
                    o.1.push(m);
                }
            }
            (Some(DataOffer), 1) => {
                let a = e.u32();
                if st.dnd.is_some_and(|d| d.0 == e.obj) {
                    st.note(format!("source actions {a:#b}"));
                }
            }
            (Some(DataOffer), 2) => {
                let a = e.u32();
                st.note(format!("negotiated action {a:#b}"));
            }
            (Some(DataDevice), 1) => {
                let (serial, surf) = (e.u32(), e.u32());
                let (x, y) = (e.fixed(), e.fixed());
                let id = e.u32();
                let mimes = st
                    .offers
                    .iter()
                    .find(|o| o.0 == id)
                    .map_or(String::new(), |o| o.1.join(" | "));
                st.note(format!(
                    "DND ENTER surface={} offer={id:#x} mimes: {mimes}",
                    tag(st, surf)
                ));
                st.place(x, y, "wl_data_device.enter");
                st.hover_at(x, y);
                if id != 0 {
                    // Copy only: with move, the source deletes the original.
                    wl.req(id, 4, &[U(DND_COPY), U(DND_COPY)]);
                    st.dnd = Some((id, serial, false));
                    update_accept(wl, st);
                }
            }
            (Some(DataDevice), 2) => {
                st.note(format!("DND LEAVE, hover={}", seg(st.hover)));
                // The compositor sends leave right after drop; the offer stays
                // valid until we finish it.
                if !st.dropped {
                    st.dnd = None;
                    st.quit = Some("drag left or was cancelled".into());
                }
            }
            (Some(DataDevice), 3) => {
                let (_time, x, y) = (e.u32(), e.fixed(), e.fixed());
                st.motions += 1;
                st.hover_at(x, y);
                update_accept(wl, st);
            }
            (Some(DataDevice), 4) => {
                st.note(format!("DND DROP on {}", seg(st.hover)));
                st.dropped = true;
            }
            _ => {}
        }
    }
}

/// Accept the URI list only while over a slice; elsewhere reject, so a drop
/// there cancels the drag.
fn update_accept(wl: &mut Wl, st: &mut St) {
    let Some((id, serial, accepted)) = st.dnd else {
        return;
    };
    let has_uris = st
        .offers
        .iter()
        .any(|o| o.0 == id && o.1.iter().any(|m| m == URI_LIST));
    let want = has_uris && st.hover != NONE;
    if want != accepted {
        if want {
            wl.req(id, 0, &[U(serial), S(URI_LIST)]);
        } else {
            wl.req(id, 0, &[U(serial), Null]);
        }
        st.dnd = Some((id, serial, want));
    }
}

fn seg(h: u32) -> String {
    if h == NONE {
        "none".into()
    } else {
        format!("slice {h}")
    }
}

fn tag(st: &St, surf: u32) -> &'static str {
    if surf == st.surface {
        "parent"
    } else {
        "OTHER (input region leak?)"
    }
}

/// Read the drop payload with a hard deadline; the source app may hang.
fn read_payload(r: OwnedFd) -> Result<Vec<u8>, String> {
    // SAFETY: fcntl on a valid fd we own.
    unsafe {
        let fl = libc::fcntl(r.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(r.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let end = mono_ns() + PAYLOAD_MS * 1_000_000;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let now = mono_ns();
        if now >= end {
            return Err(format!("timeout after {} bytes", out.len()));
        }
        let mut pfd = libc::pollfd {
            fd: r.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut pfd, 1, ((end - now) / 1_000_000) as i32 + 1) };
        // SAFETY: buf is valid for buf.len() bytes.
        let n = unsafe { libc::read(r.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
        match n {
            0 => return Ok(out),
            n if n > 0 => {
                out.extend_from_slice(&buf[..n as usize]);
                if out.len() > 1 << 20 {
                    return Err("payload over 1 MB".into());
                }
            }
            _ => {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(err.to_string());
                }
            }
        }
    }
}

fn main() {
    let t0 = mono_ns();
    // SAFETY: alarm(2) has no memory effects; SIGALRM's default action
    // terminates the process, which is the point.
    unsafe { libc::alarm(ALARM_S) };
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "exclusive".into());
    let kbd_mode = match mode.as_str() {
        "none" => 0,
        "exclusive" => 1,
        "ondemand" => 2,
        m => panic!("unknown keyboard mode {m}"),
    };
    let mut st = St {
        t0,
        log: String::new(),
        globals: Vec::new(),
        sync_done: false,
        configured: None,
        surface: 0,
        wheel_at: None,
        hover: NONE,
        offers: Vec::new(),
        dnd: None,
        dropped: false,
        released: Vec::new(),
        motions: 0,
        quit: None,
    };
    st.note(format!("--- drag probe, keyboard={mode}"));

    // Single instance: a held hotkey may spawn us many times.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(runtime_path("inacircle-drag.lock"))
        .expect("lock file");
    // SAFETY: flock on a valid fd we keep open for the process lifetime.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return;
    }

    let mut wl = Wl::connect().expect("connect");
    run(&mut wl, &mut st, kbd_mode);

    // SAFETY: getrusage writes into the zeroed struct.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    let hwm = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(|l| l.split_whitespace().nth(1).unwrap_or("?").to_string())
        })
        .unwrap_or_default();
    let reason = st.quit.take().unwrap_or_default();
    let motions = st.motions;
    st.note(format!(
        "exit: {reason}; dnd motions={motions} cpu_us={} minflt={} hwm_kb={hwm}",
        tv(ru.ru_utime) + tv(ru.ru_stime),
        ru.ru_minflt
    ));
    print!("{}", st.log);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(runtime_path("inacircle-drag.log"))
    {
        let _ = f.write_all(st.log.as_bytes());
    }
}

fn run(wl: &mut Wl, st: &mut St, kbd_mode: u32) {
    let deadline = st.t0 + DEADLINE_MS * 1_000_000;
    // Wait for `until`, bounded by the global deadline.
    let wait = |wl: &mut Wl, st: &mut St, until: &dyn Fn(&St) -> bool| -> bool {
        wl.flush().expect("flush");
        dispatch(wl, st);
        while !until(st) && st.quit.is_none() {
            let now = mono_ns();
            if now >= deadline {
                st.quit = Some("deadline".into());
                return false;
            }
            wl.read(((deadline - now) / 1_000_000) as i32 + 1)
                .expect("read");
            dispatch(wl, st);
        }
        st.quit.is_none()
    };

    let registry = wl.new_id(Iface::Registry);
    wl.req(DISPLAY, 1, &[U(registry)]);
    let cb = wl.new_id(Iface::Callback);
    wl.req(DISPLAY, 0, &[U(cb)]);
    if !wait(wl, st, &|s| s.sync_done) {
        return;
    }
    let globals = std::mem::take(&mut st.globals);
    let mut bind = |name: &str, iface: Iface, max: u32| -> u32 {
        let (gname, _, ver) = globals
            .iter()
            .find(|g| g.1 == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        let id = wl.new_id(iface);
        wl.req(
            registry,
            0,
            &[U(*gname), S(name), U((*ver).min(max)), U(id)],
        );
        id
    };
    let compositor = bind("wl_compositor", Iface::Compositor, 4);
    let shm = bind("wl_shm", Iface::Shm, 1);
    let seat = bind("wl_seat", Iface::Seat, 7);
    let subc = bind("wl_subcompositor", Iface::Subcompositor, 1);
    let ddm = bind("wl_data_device_manager", Iface::DataDeviceManager, 3);
    let viewporter = bind("wp_viewporter", Iface::Viewporter, 1);
    let layer_shell = bind("zwlr_layer_shell_v1", Iface::LayerShell, 4);

    let pointer = wl.new_id(Iface::Pointer);
    wl.req(seat, 0, &[U(pointer)]);
    let keyboard = wl.new_id(Iface::Keyboard);
    wl.req(seat, 1, &[U(keyboard)]);
    let dd = wl.new_id(Iface::DataDevice);
    wl.req(ddm, 1, &[U(dd), U(seat)]);

    // Full-screen overlay; it is the only input target.
    let surface = wl.new_id(Iface::Surface);
    st.surface = surface;
    wl.req(compositor, 0, &[U(surface)]);
    let layer = wl.new_id(Iface::LayerSurface);
    wl.req(
        layer_shell,
        0,
        &[
            U(layer),
            U(surface),
            Null, // output: compositor's choice
            U(3), // overlay
            S("inacircle-drag"),
        ],
    );
    wl.req(layer, 1, &[U(15)]); // all anchors
    wl.req(layer, 2, &[I(-1)]); // exclusive zone: ignore others
    wl.req(layer, 4, &[U(kbd_mode)]);
    wl.req(surface, 6, &[]);
    if !wait(wl, st, &|s| s.configured.is_some()) {
        return;
    }
    let (serial, sw, sh) = st.configured.unwrap();
    wl.req(layer, 6, &[U(serial)]);

    // 1x1 transparent buffer scaled to the output.
    let fd1 = memfd(4);
    let pool1 = wl.new_id(Iface::ShmPool);
    wl.req(shm, 0, &[U(pool1), Fd(fd1.as_fd()), I(4)]);
    let bg = wl.new_id(Iface::Buffer);
    wl.req(pool1, 0, &[U(bg), I(0), I(1), I(1), I(4), U(0)]);
    let viewport = wl.new_id(Iface::Viewport);
    wl.req(viewporter, 1, &[U(viewport), U(surface)]);
    wl.req(viewport, 2, &[I(sw as i32), I(sh as i32)]);
    wl.req(surface, 1, &[U(bg), I(0), I(0)]);
    wl.req(surface, 9, &[I(0), I(0), I(i32::MAX), I(i32::MAX)]);
    wl.req(surface, 6, &[]);
    wl.flush().expect("flush");
    st.note(format!("mapped {sw}x{sh}"));

    // While the compositor works out the cursor: build the wheel surface and
    // paint it. Nothing here depends on the position.
    let ws = wl.new_id(Iface::Surface);
    wl.req(compositor, 0, &[U(ws)]);
    let region = wl.new_id(Iface::Region);
    wl.req(compositor, 1, &[U(region)]);
    wl.req(ws, 5, &[U(region)]); // empty input region: events go to the parent
    wl.req(region, 0, &[]);
    let sub = wl.new_id(Iface::Subsurface);
    wl.req(subc, 1, &[U(sub), U(ws), U(surface)]);

    let frame = (BOX * BOX * 4) as usize;
    let fd2 = memfd(frame * 2);
    // SAFETY: mapping our own memfd of exactly frame*2 bytes. It stays mapped
    // until exit; we only write a buffer the compositor has released.
    let base = unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            frame * 2,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd2.as_raw_fd(),
            0,
        );
        assert_ne!(p, libc::MAP_FAILED);
        p as *mut u8
    };
    // SAFETY: see above; i < 2, so the slice is inside the mapping.
    let canvas = |i: usize| unsafe { std::slice::from_raw_parts_mut(base.add(i * frame), frame) };
    let pool2 = wl.new_id(Iface::ShmPool);
    wl.req(shm, 0, &[U(pool2), Fd(fd2.as_fd()), I((frame * 2) as i32)]);
    let bufs = [wl.new_id(Iface::Buffer), wl.new_id(Iface::Buffer)];
    for (i, b) in bufs.iter().enumerate() {
        let off = (i * frame) as i32;
        let b_ = BOX as i32;
        wl.req(pool2, 0, &[U(*b), I(off), I(b_), I(b_), I(b_ * 4), U(0)]);
    }
    let t = mono_ns();
    let wheel = Wheel::new();
    let mut painted: [Option<u32>; 2] = [None, None];
    let mut busy = [false; 2];
    wheel.paint_full(canvas(0), NONE);
    painted[0] = Some(NONE);
    st.note(format!(
        "wheel painted while waiting ({} us)",
        (mono_ns() - t) / 1000
    ));

    // Event loop: place the wheel, follow hover, take the drop.
    let mut shown = NONE;
    let mut placed = false;
    while wait(wl, st, &|s| {
        s.dropped
            || !s.released.is_empty()
            || (s.wheel_at.is_some() && !placed)
            || (placed && s.hover != shown && busy.contains(&false))
    }) {
        for id in st.released.drain(..) {
            if let Some(i) = bufs.iter().position(|b| *b == id) {
                busy[i] = false;
            }
        }
        if !placed && let Some((x, y)) = st.wheel_at {
            wl.req(sub, 1, &[I(x), I(y)]);
            placed = true;
            shown = st.hover;
            if shown != NONE {
                wheel.paint_hover(canvas(0), NONE, shown);
                painted[0] = Some(shown);
            }
            wl.req(ws, 1, &[U(bufs[0]), I(0), I(0)]);
            wl.req(ws, 9, &[I(0), I(0), I(BOX as i32), I(BOX as i32)]);
            wl.req(ws, 6, &[]);
            wl.req(surface, 6, &[]); // synced subsurface: parent commit applies it
            busy[0] = true;
            st.note("wheel committed");
        }
        if placed
            && st.hover != shown
            && let Some(i) = (0..2).find(|&i| !busy[i])
        {
            let t = mono_ns();
            let new = st.hover;
            let damage = match painted[i] {
                None => vec![wheel.paint_full(canvas(i), new)],
                Some(old) => wheel.paint_hover(canvas(i), old, new),
            };
            for r in damage {
                wl.req(
                    ws,
                    9,
                    &[I(r.x as i32), I(r.y as i32), I(r.w as i32), I(r.h as i32)],
                );
            }
            wl.req(ws, 1, &[U(bufs[i]), I(0), I(0)]);
            wl.req(ws, 6, &[]);
            wl.req(surface, 6, &[]);
            painted[i] = Some(new);
            busy[i] = true;
            shown = new;
            st.note(format!(
                "hover -> {} ({} us)",
                seg(new),
                (mono_ns() - t) / 1000
            ));
        }
        if st.dropped {
            take_drop(wl, st);
            st.quit = Some(format!("dropped on {}", seg(st.hover)));
        }
    }
}

fn take_drop(wl: &mut Wl, st: &mut St) {
    let Some((id, _, accepted)) = st.dnd else {
        st.note("drop without an offer");
        return;
    };
    if !accepted {
        st.note("drop while not accepting: nothing to read");
        return;
    }
    let mut fds = [0; 2];
    // SAFETY: valid 2-element array.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: fresh descriptors we own.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    wl.req(id, 1, &[S(URI_LIST), Fd(w.as_fd())]);
    wl.flush().expect("flush");
    drop(w);
    let t = mono_ns();
    let payload = read_payload(r);
    let us = (mono_ns() - t) / 1000;
    wl.req(id, 3, &[]); // finish
    wl.req(id, 2, &[]); // destroy
    wl.flush().expect("flush");
    match payload {
        Ok(p) => {
            let text = String::from_utf8_lossy(&p);
            let uris: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
            st.note(format!(
                "payload {} bytes in {us} us, {} URIs: {}",
                p.len(),
                uris.len(),
                uris.join(" ")
            ));
        }
        Err(e) => st.note(format!("payload FAILED after {us} us: {e}")),
    }
}

fn memfd(size: usize) -> OwnedFd {
    // SAFETY: static C string; checked.
    let fd = unsafe { libc::memfd_create(c"inacircle".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: valid fd. Fresh pages read as zero (transparent) and are
    // never faulted in unless we paint them.
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), size as i64) }, 0);
    fd
}
