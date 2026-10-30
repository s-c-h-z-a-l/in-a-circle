//! The wheel's Wayland side: registry, layer surface, viewporter, the
//! wheel subsurface with an empty input region, shm buffers, seat input and
//! drag and drop. One `poll` loop on the Wayland fd (and the drop pipe while
//! one is read) drives the `Machine` and carries out its effects.
//!
//! Layout (the spike's, proven on niri): a full-screen overlay layer surface
//! with a 1x1 transparent buffer stretched by `wp_viewporter` takes every
//! pointer and drag event; the wheel sits in a subsurface whose input region
//! is empty. Buffer 0 is painted while the compositor configures us; only
//! the subsurface position waits for the pointer. Buffer 1 is painted only
//! for a wheel that is on the screen (`prepaint`, after the first commit
//! left): the pointer or the drag decides the first wheel, and a second
//! buffer painted before that would be for a wheel nobody sees. Its pages
//! are faulted in while the wheel waits after that commit
//! (`populate_second`), so the swap to the next wheel pays only the paint.
//!
//! Globals are bound at the version our requests need, or treated as
//! absent: a `wl_data_device_manager` older than v3 (no `set_actions`)
//! means no drop target, and the wheel says `Nothing to do`; the others are
//! required. What the compositor makes us keep is capped (globals, live
//! data offers, MIME types per offer); past a cap it is broken. Every data
//! offer is destroyed once we are done with it, except the dropped one
//! until its data is read.
//!
//! Two clocks, never mixed. Startup (connect, registry, configure, the
//! first position, the first commit of the wheel) has `STARTUP`; the
//! user's wait (`STAGE`, re-armed per stage) starts only when the wheel is
//! on the screen. A startup that does not finish ends in a named
//! [`StartupError`], not in the stage's 30 s.
//!
//! Every error ends the wheel: nothing here retries. The close is ordered
//! (`teardown`): the layer surface (the keyboard grab) and the surfaces are
//! destroyed, the compositor's answer to a `sync` is awaited for at most
//! `TEARDOWN`, and only then is the connection dropped; the caller then
//! frees the lock, and only after that tells the desktop.

use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use ui::damage::Screen;
use ui::wheel::{BOX, Rect};
use wire::{Arg::*, Conn, DISPLAY, Event, Iface, op};

use crate::measure::Meter;
use crate::uri::MAX_BYTES;
use crate::wheel::{
    Effect, Input, KEY_ESC, ListError, Machine, PathList, STAGE, URI_LIST, list_refused,
};

const FRAME: usize = (BOX * BOX * 4) as usize;
/// Both wheel buffers, then the 1x1 background pixel.
const POOL: usize = 2 * FRAME + 4;
/// The one-line switch for prefaulting the mapping: the spike measured
/// `MAP_POPULATE` no cheaper than the faults it saves
/// (docs/benchmarks/wire-protocol.md); to be re-measured on the real path.
const MAP_FLAGS: libc::c_int = libc::MAP_SHARED; // | libc::MAP_POPULATE
/// Stride of the fallback first touch of buffer 1 (`Shm::populate`): the
/// smallest Linux page size, so every page gets a store; on larger pages
/// some stores land in the same page, which costs a few ns, not a fault.
const TOUCH_STRIDE: usize = 4096;
/// Longest wait for the drag source to write and close the URI list.
const READ_DROP: Duration = Duration::from_millis(500);
/// One read from the drop pipe.
const READ_CHUNK: usize = 4096;
/// The surface gives up this much before the `alarm` would kill us, so a
/// timeout normally ends in an orderly close.
const SOFT_MARGIN: Duration = Duration::from_millis(500);
/// `ui_startup_deadline`: from the connection to the wheel's first commit.
/// The fake-compositor measurement (docs/benchmarks/wheel-startup-deadlines.md)
/// puts a normal startup near 5 ms there; 2 s is 400 times that and still a wait a user takes for a slow session, so it is a policy ceiling,
/// not a measured presentation bound. Past it the wheel closes in order and
/// says why (`StartupError`); it does not wait for the user's stage.
pub const STARTUP: Duration = Duration::from_millis(2000);
/// `ui_teardown_deadline`: the most the close waits for the compositor to
/// confirm that the layer surface is gone. The close itself is a handful of
/// requests (about 0.2 ms on the fake); the ceiling only bounds a
/// compositor that has stopped answering, and past it the connection is
/// dropped anyway, which removes the surfaces.
pub const TEARDOWN: Duration = Duration::from_millis(1000);
/// The `alarm` armed for the close (`run`): `TEARDOWN` plus a second of
/// scheduling slack, whole seconds only. The close's own wait ends within
/// `TEARDOWN`, so this fires only where that wait cannot be reached; it
/// replaces whatever the stage or the startup had armed.
pub const TEARDOWN_BACKSTOP: Duration = Duration::from_secs(2);
/// The `alarm` that backs the startup (and the lock before it) up, until
/// the wheel's first commit; then `STAGE`'s takes over (`begin_user_stage`).
/// The soft path ends a startup in at most `STARTUP` + `TEARDOWN` = 3 s;
/// 5 s leaves 2 s of scheduling slack (whole seconds only), so the alarm
/// fires only where no deadline reaches, and a stuck startup cannot hold
/// the full-screen grab for the user's 30 s.
pub const STARTUP_BACKSTOP: Duration = Duration::from_secs(5);

const DND_COPY: u32 = 1;
const LAYER_OVERLAY: u32 = 3;
const ANCHOR_ALL: u32 = 15;
const KEYBOARD_EXCLUSIVE: u32 = 1;
const ARGB8888: u32 = 0;
const PRESSED: u32 = 1;
const SEAT_POINTER: u32 = 1;
const SEAT_KEYBOARD: u32 = 2;

/// The files and preset chosen, or `None` if the wheel closed without one.
pub type Chosen = Option<(PathList, String)>;

/// Why the wheel did not open (or broke on the way). A cancel, a close by
/// the compositor and a second instance are not errors.
#[derive(Debug)]
pub enum StartupError {
    /// No position arrived and the seat has no pointer: proven.
    PointerUnavailable,
    /// The seat had no keyboard by the startup deadline, so Esc could not
    /// close the overlay.
    KeyboardUnavailable,
    /// The compositor never configured the overlay in time.
    ConfigureTimeout,
    /// The Wayland connection or protocol failed, or the start did not
    /// finish in time for a cause that is not proven (`TimedOut`).
    Protocol(io::Error),
    /// Setup before the surface failed: the alarm, the lock, the shm pool.
    Initialization(io::Error),
    /// The wheel had been on the screen (the first commit was done) when the
    /// connection or the protocol failed. It did open, so this is not a
    /// startup failure and is only logged, never announced as one.
    AfterOpen(io::Error),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartupError::PointerUnavailable => f.write_str("the pointer position was unavailable"),
            StartupError::KeyboardUnavailable => f.write_str("the keyboard was unavailable"),
            StartupError::ConfigureTimeout => {
                f.write_str("the compositor did not configure the overlay in time")
            }
            StartupError::Protocol(e) => write!(f, "{e}"),
            StartupError::Initialization(e) => write!(f, "initialization failed: {e}"),
            StartupError::AfterOpen(e) => write!(f, "the wheel closed on an error: {e}"),
        }
    }
}

impl From<io::Error> for StartupError {
    fn from(e: io::Error) -> StartupError {
        StartupError::Protocol(e)
    }
}

impl StartupError {
    /// Whether the compositor is known not to answer: the close then does
    /// not wait for its confirmation.
    fn compositor_silent(&self) -> bool {
        matches!(
            self,
            StartupError::ConfigureTimeout | StartupError::Protocol(_) | StartupError::AfterOpen(_)
        )
    }
}

/// Shows the wheel and runs `m` until it closes or chooses.
///
/// With a `meter` (the measurement switch) the surface feeds it and hands it
/// back, on every path.
pub fn run(m: &mut Machine, meter: &mut Option<Meter>) -> Result<Chosen, StartupError> {
    let mut s = Surface::connect()?;
    s.meter = meter.take();
    let r = s.setup(m).and_then(|()| s.event_loop(m));
    // A failure once the wheel was on the screen is not a failure to open:
    // the first commit decides, not the loop pass that starts the user's
    // stage.
    let r = classify(r, s.shown.is_some());
    *meter = s.meter.take();
    // Release the grabs and the surfaces, then drop the connection; the
    // caller frees the lock after this returns. The close may start in the
    // last second of the user's stage (a choice, or the deadline), whose
    // `alarm` would then kill the wait for the compositor and with it the
    // choice: the close gets its own, so it always finishes first.
    // SAFETY: alarm(2) has no memory effects; see main.
    unsafe { libc::alarm(TEARDOWN_BACKSTOP.as_secs() as u32) };
    let ack = !matches!(&r, Err(e) if e.compositor_silent());
    s.teardown(m, ack);
    r
}

/// A protocol failure after the wheel's first commit (`opened`) is
/// [`StartupError::AfterOpen`]; anything else is left as it is.
fn classify(r: Result<Chosen, StartupError>, opened: bool) -> Result<Chosen, StartupError> {
    match r {
        Err(StartupError::Protocol(e)) if opened => Err(StartupError::AfterOpen(e)),
        r => r,
    }
}

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// The shm pool mapping; unmapped on drop.
struct Shm {
    ptr: *mut u8,
    fd: OwnedFd,
}

impl Shm {
    fn new() -> io::Result<Shm> {
        // SAFETY: static C string; result checked.
        let fd = unsafe { libc::memfd_create(c"inacircle".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: valid fd. Fresh pages read as zero (transparent).
        if unsafe { libc::ftruncate(fd.as_raw_fd(), POOL as i64) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: mapping our own memfd of exactly POOL bytes; checked.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                POOL,
                libc::PROT_READ | libc::PROT_WRITE,
                MAP_FLAGS,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Shm { ptr: p.cast(), fd })
    }

    /// Wheel buffer `i` (0 or 1).
    fn canvas(&mut self, i: usize) -> &mut [u8] {
        assert!(i < 2);
        // SAFETY: i < 2, so the range lies inside the POOL-byte mapping,
        // which lives as long as `self`; `&mut self` makes it unique on our
        // side. Callers only write a buffer the compositor does not hold
        // (not attached, or released).
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(i * FRAME), FRAME) }
    }

    /// Faults wheel buffer `i` in (allocates its pages) without changing
    /// its bytes: the first paint of a fresh buffer otherwise pays one
    /// minor fault per page (225 for the wheel) inside a measured wait.
    /// The caller guarantees the compositor does not hold it.
    fn populate(&mut self, i: usize) {
        assert!(i < 2);
        // SAFETY: the range is buffer i inside our own POOL-byte mapping
        // (i < 2); MADV_POPULATE_WRITE only faults pages in, it neither
        // changes their contents nor the mapping. A kernel without it
        // (before 5.14) says EINVAL and the touch below does the same work. Any
        // other failure (ENOMEM) takes the same path: the stores fault the
        // pages exactly as painting would, so it is no worse than no touch.
        let r = unsafe {
            libc::madvise(
                self.ptr.add(i * FRAME).cast(),
                FRAME,
                libc::MADV_POPULATE_WRITE,
            )
        };
        if r != 0 {
            // An unpainted buffer is all zero (a fresh memfd), so storing
            // a zero per page keeps its bytes.
            for b in self.canvas(i).iter_mut().step_by(TOUCH_STRIDE) {
                *b = 0;
            }
        }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, unmapped once.
        unsafe { libc::munmap(self.ptr.cast(), POOL) };
    }
}

/// A list being read: the pipe, the offer it comes from, what came so far,
/// the read's deadline.
struct Reading {
    fd: OwnedFd,
    offer: u32,
    data: Vec<u8>,
    until: Instant,
}

/// The drop's read, with the action negotiated at the drop (for `finish`).
struct DropRead {
    read: Reading,
    action: u32,
}

/// The early read, with the machine's tag of the `enter` that started it.
struct EarlyRead {
    read: Reading,
    seq: u32,
}

impl Reading {
    fn new(fd: OwnedFd, offer: u32, until: Instant) -> Reading {
        Reading {
            fd,
            offer,
            // Grows with the data (doubling): a normal drag of a few files
            // costs one chunk, not the 1 MiB limit.
            data: Vec::with_capacity(READ_CHUNK),
            until,
        }
    }

    /// Reads what the (non-blocking) pipe has: the data at EOF, an error
    /// past `MAX_BYTES`, on a read error or once `now` reaches the
    /// deadline with the pipe still open; `None` to wait for more.
    fn pump(&mut self, now: Instant) -> Option<Result<Vec<u8>, ListError>> {
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // SAFETY: buf is valid for buf.len() bytes.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            match n {
                0 => return Some(Ok(std::mem::take(&mut self.data))),
                n if n > 0 => {
                    if self.data.capacity() - self.data.len() < n as usize {
                        // Doubling, but never past the limit plus one chunk:
                        // `len <= MAX_BYTES` before this read and `n <=
                        // READ_CHUNK`, so that always fits, and the
                        // overflow chunk cannot double a 1 MiB buffer to 2.
                        let cap = (2 * self.data.capacity()).min(MAX_BYTES + READ_CHUNK);
                        self.data.reserve_exact(cap - self.data.len());
                    }
                    self.data.extend_from_slice(&buf[..n as usize]);
                    if self.data.len() > MAX_BYTES {
                        return Some(Err(ListError::TooBig));
                    }
                }
                _ => {
                    let e = io::Error::last_os_error();
                    match e.kind() {
                        io::ErrorKind::Interrupted => {}
                        io::ErrorKind::WouldBlock if now < self.until => return None,
                        io::ErrorKind::WouldBlock => return Some(Err("URI list timed out".into())),
                        _ => return Some(Err(e.to_string().into())),
                    }
                }
            }
        }
    }
}

/// A pipe for a drop's data: the read end non-blocking (ours), the write
/// end blocking (the source writes as it likes), both close-on-exec.
fn drop_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: a valid 2-element array; checked.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors we own.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: fcntl on a valid fd we own.
    unsafe {
        let fl = libc::fcntl(r.as_raw_fd(), libc::F_GETFL);
        if fl < 0 || libc::fcntl(r.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((r, w))
}

/// What one buffer holds: the wheel generation and hover it was painted
/// with.
type Painted = Option<(u32, Option<u8>)>;

#[derive(Default)]
struct Ids {
    registry: u32,
    compositor: u32,
    shm: u32,
    seat: u32,
    subcompositor: u32,
    ddm: u32,
    viewporter: u32,
    layer_shell: u32,
    pointer: u32,
    keyboard: u32,
    data_device: u32,
    surface: u32,
    layer: u32,
    viewport: u32,
    bg: u32,
    wheel: u32,
    sub: u32,
    bufs: [u32; 2],
}

struct Surface {
    conn: Conn,
    id: Ids,
    shm: Option<Shm>,
    /// Soft deadline of the current clock: `STARTUP` until the wheel's
    /// first commit, then the user's stage.
    deadline: Instant,
    /// The wheel has been committed: the deadline is the user's.
    user_stage: bool,
    /// The seat's capabilities, once announced.
    caps: Option<u32>,
    /// The version the seat was bound at (`wl_pointer.release` and
    /// `wl_keyboard.release` need 3).
    seat_version: u32,
    /// The teardown's `sync` callback, and its `done`.
    ack_cb: u32,
    acked: bool,
    globals: Globals,
    synced: bool,
    /// Size from the last configure; the first one maps the surface.
    size: Option<(u32, u32)>,
    offers: Offers,
    /// The drag over us: offer id (0: none offered) and enter serial.
    dnd: Option<(u32, u32)>,
    /// The action last negotiated for the drag's offer.
    dnd_action: u32,
    /// `dnd_action` at the drop, for the dropped offer's `finish`.
    drop_action: u32,
    reading: Option<DropRead>,
    /// The early read of the current drag's list, before the drop.
    early: Option<EarlyRead>,
    /// The machine's clock: inputs are timed from here.
    started: Instant,
    busy: [bool; 2],
    painted: [Painted; 2],
    /// What a buffer was repainted with since its last commit, ahead of
    /// time (`prepaint`); its next commit damages this too.
    prepainted: [Rect; 2],
    /// Buffer 1's pages were faulted in by `populate_second`.
    populated: bool,
    /// What the screen shows; `None` before the wheel's first commit.
    shown: Painted,
    screen: Screen,
    /// A render waits for a free buffer.
    pending: bool,
    inputs: Vec<Input>,
    /// The measurement switch's meter; `None` unless it is on.
    meter: Option<Meter>,
    /// Process CPU when the latest list or drop input reached `feed`.
    input_cpu: i64,
    /// `CLOCK_MONOTONIC` at the same arrival, in µs (-1 if unreadable).
    input_mono: i64,
    /// Minor page faults at the same arrival (-1 if unreadable).
    input_flt: i64,
}

impl Surface {
    fn connect() -> Result<Surface, StartupError> {
        let conn = Conn::connect().map_err(|e| {
            StartupError::Protocol(io::Error::new(
                e.kind(),
                format!("cannot reach the compositor: {e}"),
            ))
        })?;
        Ok(Surface::new(conn))
    }

    /// The startup clock ran out: the cause, as far as it is proven.
    fn startup_timeout(&self) -> StartupError {
        if self.size.is_none() {
            StartupError::ConfigureTimeout
        } else if self.caps.is_some_and(|c| c & SEAT_KEYBOARD == 0) {
            // The wheel is not shown without a keyboard (see `render`), so
            // a seat that never offered one is the proven cause.
            StartupError::KeyboardUnavailable
        } else if self.caps.is_some_and(|c| c & SEAT_POINTER == 0) {
            StartupError::PointerUnavailable
        } else {
            // Configured, a pointer (or no word from the seat), and still no
            // position: nothing proves why, so nothing is blamed.
            StartupError::Protocol(io::Error::new(
                io::ErrorKind::TimedOut,
                "startup incomplete: no cursor position arrived after the configure",
            ))
        }
    }

    /// An error from the connection while starting: a timeout is the
    /// startup clock's, anything else is the connection's.
    fn start_error(&self, e: io::Error) -> StartupError {
        if e.kind() == io::ErrorKind::TimedOut && self.shown.is_none() {
            self.startup_timeout()
        } else {
            StartupError::Protocol(e)
        }
    }

    /// `flush` with the startup clock's classification.
    fn flush_start(&mut self) -> Result<(), StartupError> {
        self.flush().map_err(|e| self.start_error(e))
    }

    /// The ordered close. First the layer surface (the keyboard grab, then
    /// the overlay) and the surfaces are destroyed; then, with `ack`, the
    /// compositor's answer to a `sync` is awaited, so the grab is known to
    /// be released before the lock is; all within `TEARDOWN`. Whatever goes
    /// wrong here is ignored: dropping the connection removes the surfaces
    /// anyway.
    fn teardown(&mut self, m: &Machine, ack: bool) {
        let until = Instant::now() + TEARDOWN;
        // No render waits for a buffer that will not be released.
        self.pending = false;
        let (id, c) = (&self.id, &mut self.conn);
        for (obj, op) in [
            (id.layer, op::zwlr_layer_surface_v1::DESTROY),
            (id.sub, op::wl_subsurface::DESTROY),
            (id.wheel, op::wl_surface::DESTROY),
            (id.surface, op::wl_surface::DESTROY),
        ] {
            if obj != 0 {
                c.req(obj, op, &[]);
            }
        }
        if ack {
            self.ack_cb = c.new_id(Iface::Callback);
            c.req(DISPLAY, op::wl_display::SYNC, &[U(self.ack_cb)]);
        }
        if self.conn.flush(until).is_err() {
            return;
        }
        while ack && !self.acked {
            if !matches!(self.conn.read(until), Ok(true)) || self.dispatch(m).is_err() {
                return;
            }
        }
    }

    fn new(conn: Conn) -> Surface {
        Surface {
            conn,
            id: Ids::default(),
            shm: None,
            deadline: Instant::now() + STARTUP,
            user_stage: false,
            caps: None,
            seat_version: 1,
            ack_cb: 0,
            acked: false,
            globals: Globals::default(),
            synced: false,
            size: None,
            offers: Offers::default(),
            dnd: None,
            dnd_action: 0,
            drop_action: 0,
            reading: None,
            early: None,
            started: Instant::now(),
            busy: [false; 2],
            painted: [None; 2],
            prepainted: [Rect::EMPTY; 2],
            populated: false,
            shown: None,
            screen: Screen::new(),
            pending: false,
            inputs: Vec::new(),
            meter: None,
            input_cpu: 0,
            input_mono: -1,
            input_flt: -1,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.flush(self.deadline)?;
        // The first flush after the first commit sent it; the check is a
        // field read, the clocks only run once.
        if let Some(meter) = &mut self.meter
            && meter.awaiting_flush()
        {
            meter.flushed(crate::cpu_us(libc::RUSAGE_SELF), Instant::now());
        }
        Ok(())
    }

    /// Handles every event read so far.
    fn dispatch(&mut self, m: &Machine) -> io::Result<()> {
        while let Some(e) = self.conn.event()? {
            self.handle(e, m)?;
        }
        Ok(())
    }

    /// Reads and dispatches until `done`, within the current deadline.
    fn wait(&mut self, m: &Machine, done: fn(&Surface) -> bool) -> Result<(), StartupError> {
        self.flush_start()?;
        self.dispatch(m)?;
        while !done(self) {
            match self.conn.read(self.deadline) {
                Ok(true) => {}
                Ok(false) => return Err(self.startup_timeout()),
                Err(e) => return Err(self.start_error(e)),
            }
            self.dispatch(m)?;
            // What the events asked for (the seat's devices) goes out now,
            // not after the wait: Esc must reach us while it lasts.
            self.flush_start()?;
        }
        Ok(())
    }

    /// Binds global `name` at version `min..=max`; `None` if it is absent
    /// or older than `min` (the requests we send need `min`).
    fn bind(&mut self, name: &str, iface: Iface, min: u32, max: u32) -> Option<u32> {
        let (gname, ver) = self.globals.find(name, min, max)?;
        let id = self.conn.new_id(iface);
        let reg = self.id.registry;
        self.conn.req(
            reg,
            op::wl_registry::BIND,
            &[U(gname), S(name), U(ver), U(id)],
        );
        Some(id)
    }

    fn need(&mut self, name: &str, iface: Iface, min: u32, max: u32) -> io::Result<u32> {
        self.bind(name, iface, min, max)
            .ok_or_else(|| other(format!("the compositor lacks {name} v{min}")))
    }

    fn setup(&mut self, m: &mut Machine) -> Result<(), StartupError> {
        let c = &mut self.conn;
        self.id.registry = c.new_id(Iface::Registry);
        c.req(
            DISPLAY,
            op::wl_display::GET_REGISTRY,
            &[U(self.id.registry)],
        );
        let cb = c.new_id(Iface::Callback);
        c.req(DISPLAY, op::wl_display::SYNC, &[U(cb)]);
        self.wait(m, |s| s.synced)?;

        // Minimum versions: what our requests need. wl_surface.damage_buffer
        // is v4; everything else we send on these is v1.
        self.id.compositor = self.need("wl_compositor", Iface::Compositor, 4, 4)?;
        self.id.shm = self.need("wl_shm", Iface::Shm, 1, 1)?;
        self.id.seat = self.need("wl_seat", Iface::Seat, 1, 7)?;
        self.seat_version = self.globals.find("wl_seat", 1, 7).map_or(1, |g| g.1);
        self.id.subcompositor = self.need("wl_subcompositor", Iface::Subcompositor, 1, 1)?;
        self.id.viewporter = self.need("wp_viewporter", Iface::Viewporter, 1, 1)?;
        self.id.layer_shell = self.need("zwlr_layer_shell_v1", Iface::LayerShell, 1, 4)?;
        // wl_data_offer.set_actions/finish/action are v3. Older or absent:
        // no drop target, and the wheel says `Nothing to do`.
        self.id.ddm = self
            .bind("wl_data_device_manager", Iface::DataDeviceManager, 3, 3)
            .unwrap_or(0);
        self.globals = Globals::default();
        let (id, c) = (&mut self.id, &mut self.conn);
        if id.ddm != 0 {
            id.data_device = c.new_id(Iface::DataDevice);
            c.req(
                id.ddm,
                op::wl_data_device_manager::GET_DATA_DEVICE,
                &[U(id.data_device), U(id.seat)],
            );
        }

        // The full-screen overlay; it is the only input target.
        id.surface = c.new_id(Iface::Surface);
        c.req(
            id.compositor,
            op::wl_compositor::CREATE_SURFACE,
            &[U(id.surface)],
        );
        id.layer = c.new_id(Iface::LayerSurface);
        c.req(
            id.layer_shell,
            op::zwlr_layer_shell_v1::GET_LAYER_SURFACE,
            &[
                U(id.layer),
                U(id.surface),
                Null, // output: the compositor's choice (the focused one)
                U(LAYER_OVERLAY),
                S("inacircle"),
            ],
        );
        c.req(
            id.layer,
            op::zwlr_layer_surface_v1::SET_ANCHOR,
            &[U(ANCHOR_ALL)],
        );
        // Ignore other surfaces' exclusive zones: cover the whole output.
        c.req(
            id.layer,
            op::zwlr_layer_surface_v1::SET_EXCLUSIVE_ZONE,
            &[I(-1)],
        );
        c.req(
            id.layer,
            op::zwlr_layer_surface_v1::SET_KEYBOARD_INTERACTIVITY,
            &[U(KEYBOARD_EXCLUSIVE)],
        );
        c.req(id.surface, op::wl_surface::COMMIT, &[]);
        self.flush_start()?;

        // While the compositor configures: the wheel surface and buffer 0.
        self.build_wheel_surface()?;
        let generation = m.generation();
        let shm = self.shm.as_mut().expect("built above");
        m.wheel().paint_full(shm.canvas(0), None);
        self.painted[0] = Some((generation, None));
        if let Some(meter) = &mut self.meter {
            meter.paint('S', 0, generation, self.busy[0]);
        }

        // The configure is acked and the overlay mapped in `handle`; its
        // inputs wait in the queue for the event loop.
        // Esc ends the wait too: a user who cancelled before the compositor
        // configured the overlay is not told it failed to open; the event
        // loop feeds the Esc and closes in order.
        self.wait(m, |s| {
            s.size.is_some() || s.inputs.contains(&Input::Key(KEY_ESC))
        })?;
        self.flush_start()?;
        Ok(())
    }

    fn build_wheel_surface(&mut self) -> Result<(), StartupError> {
        let shm = Shm::new().map_err(StartupError::Initialization)?;
        let (id, c) = (&mut self.id, &mut self.conn);
        id.wheel = c.new_id(Iface::Surface);
        c.req(
            id.compositor,
            op::wl_compositor::CREATE_SURFACE,
            &[U(id.wheel)],
        );
        // An empty input region: every pointer and drag event goes to the
        // parent, so the wheel never splits a drag in two.
        let region = c.new_id(Iface::Region);
        c.req(
            id.compositor,
            op::wl_compositor::CREATE_REGION,
            &[U(region)],
        );
        c.req(id.wheel, op::wl_surface::SET_INPUT_REGION, &[U(region)]);
        c.req(region, op::wl_region::DESTROY, &[]);
        id.sub = c.new_id(Iface::Subsurface);
        c.req(
            id.subcompositor,
            op::wl_subcompositor::GET_SUBSURFACE,
            &[U(id.sub), U(id.wheel), U(id.surface)],
        );
        let pool = c.new_id(Iface::ShmPool);
        c.req(
            id.shm,
            op::wl_shm::CREATE_POOL,
            &[U(pool), Fd(shm.fd.as_fd()), I(POOL as i32)],
        );
        let b = BOX as i32;
        for i in 0..2 {
            id.bufs[i] = c.new_id(Iface::Buffer);
            c.req(
                pool,
                op::wl_shm_pool::CREATE_BUFFER,
                &[
                    U(id.bufs[i]),
                    I((i * FRAME) as i32),
                    I(b),
                    I(b),
                    I(b * 4),
                    U(ARGB8888),
                ],
            );
        }
        id.bg = c.new_id(Iface::Buffer);
        c.req(
            pool,
            op::wl_shm_pool::CREATE_BUFFER,
            &[U(id.bg), I(2 * FRAME as i32), I(1), I(1), I(4), U(ARGB8888)],
        );
        // The buffers keep the pool's memory alive.
        c.req(pool, op::wl_shm_pool::DESTROY, &[]);
        id.viewport = c.new_id(Iface::Viewport);
        c.req(
            id.viewporter,
            op::wp_viewporter::GET_VIEWPORT,
            &[U(id.viewport), U(id.surface)],
        );
        self.shm = Some(shm);
        Ok(())
    }

    /// One event; machine inputs are queued for `feed`.
    fn handle(&mut self, mut e: Event, m: &Machine) -> io::Result<()> {
        let input = match (e.iface, e.op) {
            (Iface::Display, op::wl_display::ERROR) => {
                let (obj, code) = (e.u32()?, e.u32()?);
                let msg = e.string()?;
                return Err(other(format!(
                    "protocol error on object {obj}, code {code}: {msg}"
                )));
            }
            (Iface::Registry, op::wl_registry::GLOBAL) => {
                let (name, iface, ver) = (e.u32()?, e.string()?, e.u32()?);
                if !self.synced {
                    self.globals.add(name, iface, ver)?;
                }
                None
            }
            (Iface::Callback, op::wl_callback::DONE) => {
                if e.obj == self.ack_cb && self.ack_cb != 0 {
                    self.acked = true;
                } else {
                    self.synced = true;
                }
                None
            }
            (Iface::Seat, op::wl_seat::CAPABILITIES) => {
                let caps = e.u32()?;
                let (id, c) = (&mut self.id, &mut self.conn);
                // A capability that went away and came back is a new
                // device: the old object is inert, so it is released (where
                // the seat's version has the request) and asked for again.
                // Its late events are ignored (`handle` checks the object).
                if caps & SEAT_POINTER == 0 && id.pointer != 0 {
                    if self.seat_version >= 3 {
                        c.req(id.pointer, op::wl_pointer::RELEASE, &[]);
                    }
                    id.pointer = 0;
                }
                if caps & SEAT_KEYBOARD == 0 && id.keyboard != 0 {
                    if self.seat_version >= 3 {
                        c.req(id.keyboard, op::wl_keyboard::RELEASE, &[]);
                    }
                    id.keyboard = 0;
                }
                if caps & SEAT_POINTER != 0 && id.pointer == 0 {
                    id.pointer = c.new_id(Iface::Pointer);
                    c.req(id.seat, op::wl_seat::GET_POINTER, &[U(id.pointer)]);
                }
                if caps & SEAT_KEYBOARD != 0 && id.keyboard == 0 {
                    id.keyboard = c.new_id(Iface::Keyboard);
                    c.req(id.seat, op::wl_seat::GET_KEYBOARD, &[U(id.keyboard)]);
                }
                self.caps = Some(caps);
                // Without a keyboard Esc cannot close the overlay, so the
                // wheel is not shown (`render`) until the seat has one; the
                // capability may come after the first event, so the verdict
                // waits for the startup deadline (`startup_timeout`). The
                // pointer is judged there too: a drag brings its own
                // position. What decides is what is on the screen, not
                // which clock runs: this very event may have shown the
                // wheel (`render`) and the next one take the keyboard away
                // before the loop starts the user's stage.
                if caps & SEAT_KEYBOARD != 0 {
                    if self.shown.is_none() {
                        self.render(m)?;
                    }
                    None
                } else if self.shown.is_some() {
                    // Lost once the wheel is up: Esc is gone, so close in
                    // order now instead of holding the grab until the stage
                    // ends. A close, not an error.
                    Some(Input::Closed)
                } else {
                    None
                }
            }
            (Iface::LayerSurface, op::zwlr_layer_surface_v1::CONFIGURE) => {
                let (serial, w, h) = (e.u32()?, e.u32()?, e.u32()?);
                self.configure(serial, w, h)?;
                Some(Input::Configured(w, h))
            }
            (Iface::LayerSurface, op::zwlr_layer_surface_v1::CLOSED) => Some(Input::Closed),
            (Iface::Buffer, op::wl_buffer::RELEASE) => {
                if let Some(i) = self.id.bufs.iter().position(|&b| b == e.obj) {
                    self.busy[i] = false;
                    if self.pending {
                        self.render(m)?;
                    }
                }
                None
            }

            // Pointer: without a drag (a drag owns the pointer).
            (Iface::Pointer, op::wl_pointer::ENTER) if e.obj == self.id.pointer => {
                let (_serial, _surface) = (e.u32()?, e.u32()?);
                Some(Input::PointerAt(e.fixed()?, e.fixed()?))
            }
            (Iface::Pointer, op::wl_pointer::MOTION) if e.obj == self.id.pointer => {
                let _time = e.u32()?;
                Some(Input::PointerAt(e.fixed()?, e.fixed()?))
            }
            (Iface::Pointer, op::wl_pointer::BUTTON) if e.obj == self.id.pointer => {
                let (_serial, _time, button, state) = (e.u32()?, e.u32()?, e.u32()?, e.u32()?);
                (state == PRESSED).then_some(Input::Click(button))
            }

            (Iface::Keyboard, op::wl_keyboard::KEYMAP) => {
                // Not needed, but taken so the fd is closed now.
                drop(self.conn.take_fd());
                None
            }
            (Iface::Keyboard, op::wl_keyboard::KEY) if e.obj == self.id.keyboard => {
                let (_serial, _time, key, state) = (e.u32()?, e.u32()?, e.u32()?, e.u32()?);
                (state == PRESSED).then_some(Input::Key(key))
            }

            (Iface::DataDevice, op::wl_data_device::DATA_OFFER) => {
                let id = e.u32()?;
                self.conn.register_server_id(id, Iface::DataOffer);
                self.offers.add(id)?;
                None
            }
            (Iface::DataOffer, op::wl_data_offer::OFFER) => {
                let mime = e.string()?;
                self.offers.mime(e.obj, mime)?;
                None
            }
            (Iface::DataOffer, op::wl_data_offer::ACTION) => {
                let action = e.u32()?;
                if self.dnd.is_some_and(|d| d.0 == e.obj) {
                    self.dnd_action = action;
                }
                None
            }
            (Iface::DataDevice, op::wl_data_device::ENTER) => {
                let (serial, _surface) = (e.u32()?, e.u32()?);
                let (x, y) = (e.fixed()?, e.fixed()?);
                let offer = e.u32()?;
                if let Some((old, _)) = self.dnd
                    && old != offer
                {
                    self.retire(old);
                }
                // A new drag forgets the old one's list: close its pipe.
                self.early = None;
                self.dnd = Some((offer, serial));
                self.dnd_action = 0;
                let mimes = self.offers.mimes(offer);
                Some(Input::DragEnter { x, y, mimes })
            }
            (Iface::DataDevice, op::wl_data_device::MOTION) => {
                let _time = e.u32()?;
                Some(Input::DragMotion(e.fixed()?, e.fixed()?))
            }
            (Iface::DataDevice, op::wl_data_device::LEAVE) => {
                self.early = None;
                // After a drop the offer stays until its data is read.
                if let Some((offer, _)) = self.dnd
                    && self.offers.kept() != Some(offer)
                {
                    self.retire(offer);
                    self.dnd = None;
                }
                Some(Input::DragLeave)
            }
            (Iface::DataDevice, op::wl_data_device::DROP) => {
                // One drop is read; a later one (refused, but the compositor
                // may still send it) must not replace the keep.
                if let Some((offer, _)) = self.dnd
                    && self.offers.kept().is_none()
                {
                    self.offers.keep(offer);
                    self.drop_action = self.dnd_action;
                }
                Some(Input::Drop)
            }
            (Iface::DataDevice, op::wl_data_device::SELECTION) => {
                // Not ours to keep: the clipboard is a later context source.
                let id = e.u32()?;
                if self.dnd.is_none_or(|d| d.0 != id) {
                    self.retire(id);
                }
                None
            }
            // Formats, names, scales, axis and frame events, modifiers,
            // offer actions: nothing to do.
            _ => None,
        };
        if let Some(i) = input {
            self.inputs.push(i);
        }
        Ok(())
    }

    /// Acks a configure; the first one maps the overlay with its 1x1
    /// transparent buffer stretched to the output.
    fn configure(&mut self, serial: u32, w: u32, h: u32) -> io::Result<()> {
        if w == 0 || h == 0 || w > i32::MAX as u32 || h > i32::MAX as u32 {
            return Err(other(format!("unusable overlay size {w}x{h}")));
        }
        let (id, c) = (&mut self.id, &mut self.conn);
        c.req(
            id.layer,
            op::zwlr_layer_surface_v1::ACK_CONFIGURE,
            &[U(serial)],
        );
        if self.size != Some((w, h)) {
            c.req(
                id.viewport,
                op::wp_viewport::SET_DESTINATION,
                &[I(w as i32), I(h as i32)],
            );
        }
        if self.size.is_none() {
            c.req(id.surface, op::wl_surface::ATTACH, &[U(id.bg), I(0), I(0)]);
            c.req(
                id.surface,
                op::wl_surface::DAMAGE_BUFFER,
                &[I(0), I(0), I(1), I(1)],
            );
        }
        c.req(id.surface, op::wl_surface::COMMIT, &[]);
        self.size = Some((w, h));
        Ok(())
    }

    /// Hands the queued inputs to the machine and carries out its effects.
    /// `Some` once the machine closed or chose.
    fn feed(&mut self, m: &mut Machine) -> io::Result<Option<Chosen>> {
        let inputs = std::mem::take(&mut self.inputs);
        if let Some(meter) = &mut self.meter {
            for _ in &inputs {
                meter.event();
            }
        }
        for i in inputs {
            let list = matches!(i, Input::DragList(..));
            // The measured waits start at the input's arrival, before the
            // machine builds a menu from it.
            if self.meter.is_some() && (list || matches!(i, Input::Drop)) {
                self.input_cpu = crate::cpu_us(libc::RUSAGE_SELF);
                self.input_mono = crate::monotonic_us().unwrap_or(-1);
                self.input_flt = crate::measure::minflt();
            }
            let was_ready = m.early_ready();
            let fxs = m.on_at(self.started.elapsed(), i);
            // Only a list the machine took (right tag, right moment) counts.
            if list
                && !was_ready
                && m.early_ready()
                && let Some(meter) = &mut self.meter
            {
                meter.listed(self.input_cpu, self.input_mono, self.input_flt);
            }
            for fx in fxs {
                if let Some(end) = self.apply(fx, m)? {
                    return Ok(Some(end));
                }
            }
        }
        Ok(None)
    }

    fn apply(&mut self, fx: Effect, m: &Machine) -> io::Result<Option<Chosen>> {
        match fx {
            Effect::Draw | Effect::Hover(_) => self.render(m)?,
            Effect::Accept(yes) => self.accept(yes),
            Effect::ReadEarly(seq) => self.start_early(seq)?,
            Effect::EndDrop => self.end_drop(),
            Effect::ReadDrop => self.start_read()?,
            Effect::Arm(d) => {
                // The user's wait runs only once the wheel is on the
                // screen: the startup keeps its own, shorter clock (and
                // its 5 s `alarm`) until the first commit, whatever a drag
                // did to the machine meanwhile. `begin_user_stage` arms
                // the stage then.
                if self.user_stage {
                    self.deadline = Instant::now() + d.saturating_sub(SOFT_MARGIN);
                    // SAFETY: alarm(2) has no memory effects; see main.
                    unsafe { libc::alarm(d.as_secs().max(1) as u32) };
                }
            }
            Effect::Close => return Ok(Some(None)),
            Effect::Run(files, preset) => {
                if let Some(meter) = &mut self.meter {
                    meter.chose(
                        crate::cpu_us(libc::RUSAGE_SELF),
                        crate::monotonic_us().unwrap_or(-1),
                    );
                }
                return Ok(Some(Some((files, preset))));
            }
        }
        Ok(None)
    }

    fn accept(&mut self, yes: bool) {
        let Some((offer, serial)) = self.dnd else {
            return;
        };
        if offer == 0 {
            return;
        }
        let c = &mut self.conn;
        if yes {
            // Copy only: with move, the source deletes the original.
            c.req(offer, op::wl_data_offer::ACCEPT, &[U(serial), S(URI_LIST)]);
            c.req(
                offer,
                op::wl_data_offer::SET_ACTIONS,
                &[U(DND_COPY), U(DND_COPY)],
            );
        } else {
            // Both: niri still sends `drop` after `accept(null)` alone.
            c.req(offer, op::wl_data_offer::ACCEPT, &[U(serial), Null]);
            c.req(offer, op::wl_data_offer::SET_ACTIONS, &[U(0), U(0)]);
        }
    }

    /// Destroys offer `offer` if still alive, unless it is the dropped one
    /// (its data is still to be read).
    fn retire(&mut self, offer: u32) {
        if self.offers.retire(offer) {
            self.conn.req(offer, op::wl_data_offer::DESTROY, &[]);
        }
    }

    /// Receives the dropped offer's URI list: the offer kept at the drop,
    /// not the current drag (an enter may have come in between).
    fn start_read(&mut self) -> io::Result<()> {
        // The drop's own read replaces an early one still in flight.
        self.early = None;
        let Some(offer) = self.offers.to_read() else {
            if let Some(offer) = self.offers.release() {
                self.conn.req(offer, op::wl_data_offer::DESTROY, &[]);
            }
            self.inputs.push(Input::DropData(Err("no offer".into())));
            return Ok(());
        };
        let (r, w) = drop_pipe()?;
        self.conn.req(
            offer,
            op::wl_data_offer::RECEIVE,
            &[S(URI_LIST), Fd(w.as_fd())],
        );
        self.flush()?;
        // Our copy of the write end must go, or the read never sees EOF.
        drop(w);
        self.reading = Some(DropRead {
            read: Reading::new(r, offer, Instant::now() + READ_DROP),
            action: self.drop_action,
        });
        Ok(())
    }

    /// The drop was taken without a second read: ends the dropped offer
    /// like the tail of `pump_read`.
    fn end_drop(&mut self) {
        let Some(offer) = self.offers.to_read() else {
            return;
        };
        let copy = self.drop_action == DND_COPY;
        self.end_offer(offer, copy);
        if let Some(meter) = &mut self.meter {
            meter.dropped(self.input_cpu, self.input_mono);
        }
    }

    /// The tail of every ended drop: `finish` only after a negotiated copy
    /// (anything else is a protocol error; niri drops even without it),
    /// then the keep ends, so a reused id is destroyed later, and the offer
    /// is destroyed.
    fn end_offer(&mut self, offer: u32, copy: bool) {
        if copy {
            self.conn.req(offer, op::wl_data_offer::FINISH, &[]);
        }
        if self.offers.release() == Some(offer) {
            self.conn.req(offer, op::wl_data_offer::DESTROY, &[]);
        }
    }

    /// Receives the current drag's URI list now, before the drop, tagged
    /// `seq`. The offer is neither finished nor destroyed: it lives on to
    /// the drop. No offer, an offer without the list or a pipe that cannot
    /// be made fails the read at once (the machine falls back to `Drop
    /// here`); only a connection error ends the wheel.
    fn start_early(&mut self, seq: u32) -> io::Result<()> {
        self.early = None;
        let Some(offer) = self.dnd.map(|d| d.0).filter(|&o| o != 0) else {
            self.inputs
                .push(Input::DragList(seq, Err("no offer".into())));
            return Ok(());
        };
        // Two enters handled in one go leave the first one's read for the
        // second one's offer, which may not carry a list.
        if !self.offers.mimes(offer).iter().any(|m| m == URI_LIST) {
            self.inputs
                .push(Input::DragList(seq, Err("no uri-list".into())));
            return Ok(());
        }
        let (r, w) = match drop_pipe() {
            Ok(p) => p,
            Err(e) => {
                self.inputs
                    .push(Input::DragList(seq, Err(e.to_string().into())));
                return Ok(());
            }
        };
        self.conn.req(
            offer,
            op::wl_data_offer::RECEIVE,
            &[S(URI_LIST), Fd(w.as_fd())],
        );
        self.flush()?;
        // Our copy of the write end must go, or the read never sees EOF.
        drop(w);
        self.early = Some(EarlyRead {
            read: Reading::new(r, offer, Instant::now() + READ_DROP),
            seq,
        });
        Ok(())
    }

    /// Reads what the early pipe has; queues `DragList` at EOF, past the
    /// size limit, on error or past its deadline.
    fn pump_early(&mut self) {
        let Some(rd) = &mut self.early else {
            return;
        };
        if let Some(r) = rd.read.pump(Instant::now()) {
            let seq = rd.seq;
            self.early = None;
            self.inputs.push(Input::DragList(seq, r));
        }
    }

    /// Reads what the pipe has; queues `DropData` at EOF, past the size
    /// limit, on error or past the read's deadline.
    fn pump_read(&mut self) {
        let Some(rd) = &mut self.reading else {
            return;
        };
        if let Some(r) = rd.read.pump(Instant::now()) {
            let (offer, action) = (rd.read.offer, rd.action);
            self.reading = None;
            // A list the wheel refuses (either limit) is a cancel, not a copy.
            let taken = r.is_ok() && !list_refused(&r);
            self.end_offer(offer, taken && action == DND_COPY);
            if let Some(meter) = &mut self.meter {
                meter.dropped(
                    crate::cpu_us(libc::RUSAGE_SELF),
                    crate::monotonic_us().unwrap_or(-1),
                );
            }
            self.inputs.push(Input::DropData(r));
        }
    }

    /// Brings the screen to the machine's state with a free buffer; waits
    /// for a release if both are held.
    fn render(&mut self, m: &Machine) -> io::Result<()> {
        let Some((x, y)) = m.origin() else {
            return Ok(());
        };
        let first = self.shown.is_none();
        // The first commit waits for a keyboard when the seat has announced
        // its capabilities without one: the keyboard capability may still
        // come, and `startup_timeout` names it if it does not.
        if first && self.caps.is_some_and(|c| c & SEAT_KEYBOARD == 0) {
            return Ok(());
        }
        let want = Some((m.generation(), m.hover()));
        if self.shown == want {
            self.pending = false;
            return Ok(());
        }
        // The first frame is buffer 0: `Screen::new` assumes it.
        let Some(i) = (if first {
            Some(0)
        } else {
            (0..2).find(|&i| !self.busy[i])
        })
        .filter(|&i| !self.busy[i]) else {
            self.pending = true;
            return Ok(());
        };
        self.pending = false;
        let hover_repaint = !first && self.shown.is_some_and(|(g, _)| g == m.generation());
        let started = (hover_repaint && self.meter.is_some()).then(Instant::now);
        let was_busy = self.busy[i];
        let shm = self.shm.as_mut().expect("built in setup");
        let canvas = shm.canvas(i);
        let (phase, painted) = match self.painted[i] {
            Some((g, old)) if g == m.generation() => {
                ('H', m.wheel().paint_hover(canvas, old, m.hover()))
            }
            _ => ('F', m.wheel().paint_full(canvas, m.hover())),
        };
        let painted = painted.union(std::mem::replace(&mut self.prepainted[i], Rect::EMPTY));
        self.painted[i] = want;
        let full = Rect {
            x: 0,
            y: 0,
            w: BOX,
            h: BOX,
        };
        let damage = if first {
            full
        } else {
            self.screen.commit(i, painted)
        };
        let (id, c) = (&self.id, &mut self.conn);
        if first {
            c.req(id.sub, op::wl_subsurface::SET_POSITION, &[I(x), I(y)]);
        }
        c.req(
            id.wheel,
            op::wl_surface::ATTACH,
            &[U(id.bufs[i]), I(0), I(0)],
        );
        if !damage.is_empty() {
            c.req(
                id.wheel,
                op::wl_surface::DAMAGE_BUFFER,
                &[
                    I(damage.x as i32),
                    I(damage.y as i32),
                    I(damage.w as i32),
                    I(damage.h as i32),
                ],
            );
        }
        c.req(id.wheel, op::wl_surface::COMMIT, &[]);
        // A synced subsurface: the parent's commit applies it.
        c.req(id.surface, op::wl_surface::COMMIT, &[]);
        self.busy[i] = true;
        let swapped = !first && self.shown.is_some_and(|(g, _)| g != m.generation());
        self.shown = want;
        if let Some(meter) = &mut self.meter {
            if let Some(t) = started {
                meter.hover(t.elapsed());
            }
            if first {
                meter.committed(crate::cpu_us(libc::RUSAGE_SELF), Instant::now());
            }
            if swapped {
                meter.swapped(crate::cpu_us(libc::RUSAGE_SELF));
            }
            // After the timings are taken: the trace costs a push, and
            // reading smaps costs tens of µs.
            meter.paint(phase, i, m.generation(), was_busy);
            if started.is_none() {
                let c0 = crate::cpu_us(libc::RUSAGE_SELF);
                meter.pss(crate::measure::pss_kb());
                meter.overhead(crate::cpu_us(libc::RUSAGE_SELF) - c0);
            }
        }
        Ok(())
    }

    /// Before waiting: a free buffer that still holds an older wheel (the
    /// stage-2 swap) is painted in full now, so the next hover on it is a
    /// cheap `paint_hover` and not a full paint inside the event.
    ///
    /// Not while a list is being read (the early read, or the drop's): the
    /// machine replaces the wheel when it arrives, and a buffer made ready
    /// for the wheel about to go would be painted again at once. A list
    /// that fails changes nothing for the wheel, and the next pass paints.
    fn prepaint(&mut self, m: &Machine) {
        if self.shown.is_none() || self.early.is_some() || self.reading.is_some() {
            return;
        }
        for i in to_prepaint(&self.painted, &self.busy, m.generation()) {
            {
                let shm = self.shm.as_mut().expect("built in setup");
                let r = m.wheel().paint_full(shm.canvas(i), m.hover());
                self.painted[i] = Some((m.generation(), m.hover()));
                self.prepainted[i] = self.prepainted[i].union(r);
                if let Some(meter) = &mut self.meter {
                    meter.paint('P', i, m.generation(), self.busy[i]);
                }
            }
        }
    }

    /// Whether buffer 1 is still fresh (never painted, pages not faulted
    /// in) while the wheel is on the screen, so the wait before the next
    /// input can take its first touch (`populate_second`).
    fn second_is_fresh(&self) -> bool {
        self.shown.is_some() && !self.populated && self.painted[1].is_none() && !self.busy[1]
    }

    /// Buffer 1's first touch, done while the wheel waits: the next wheel
    /// (level 1 from the early list, stage 2 from the drop, or a hover)
    /// goes into it, and its paint then pays no page faults. Not before
    /// the first commit (it would add to main-to-commit), never on a buffer
    /// the compositor holds or one already painted.
    fn populate_second(&mut self) {
        if !self.second_is_fresh() {
            return;
        }
        self.shm.as_mut().expect("built in setup").populate(1);
        self.populated = true;
    }

    fn event_loop(&mut self, m: &mut Machine) -> Result<Chosen, StartupError> {
        loop {
            self.dispatch(m)?;
            self.pump_read();
            self.pump_early();
            if let Some(end) = self.feed(m)? {
                return Ok(end);
            }
            // Inputs queued by an effect (a failed read) go round again.
            if !self.inputs.is_empty() {
                continue;
            }
            self.flush_start()?;
            self.prepaint(m);
            // The wheel's first commit ends the startup; the user's stage
            // (soft deadline and the `alarm` backstop) starts from here.
            if !self.user_stage && self.shown.is_some() {
                self.begin_user_stage();
            }
            let now = Instant::now();
            if now >= self.deadline {
                if !self.user_stage {
                    return Err(self.startup_timeout());
                }
                self.inputs.push(Input::Deadline);
                continue;
            }
            // The nearest read deadline: one wakeup for either read.
            let read_until = [
                self.reading.as_ref().map(|r| r.read.until),
                self.early.as_ref().map(|r| r.read.until),
            ]
            .into_iter()
            .flatten()
            .min();
            let until = read_until.map_or(self.deadline, |u| u.min(self.deadline));
            let mut pfds = [
                pollfd(self.conn.as_raw_fd()),
                pollfd(self.reading.as_ref().map_or(-1, |r| r.read.fd.as_raw_fd())),
                pollfd(self.early.as_ref().map_or(-1, |r| r.read.fd.as_raw_fd())),
            ];
            // Only in a real wait: an input already there (the early list,
            // Esc) is handled first, and a later pass touches the buffer
            // if it is still fresh.
            if self.second_is_fresh() {
                // SAFETY: three valid pollfds (a negative fd is ignored).
                if unsafe { libc::poll(pfds.as_mut_ptr(), 3, 0) } == 0 {
                    self.populate_second();
                }
            }
            let now = Instant::now();
            let left = until.saturating_duration_since(now);
            let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
            // SAFETY: three valid pollfds (a negative fd is ignored).
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), 3, ms) };
            if r >= 0
                && let Some(meter) = &mut self.meter
                && idle_wakeup(
                    pfds.iter().any(|p| p.revents != 0),
                    Instant::now(),
                    self.deadline,
                    read_until,
                )
            {
                meter.idle_wakeup();
            }
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            if pfds[0].revents != 0 {
                // Readable (or hung up, which the read reports).
                self.conn
                    .read(until.max(Instant::now() + Duration::from_millis(1)))?;
            }
        }
    }

    /// The wheel is on the screen: from here the user's stage runs.
    fn begin_user_stage(&mut self) {
        self.user_stage = true;
        self.deadline = user_deadline(Instant::now());
        // SAFETY: alarm(2) has no memory effects; see main.
        unsafe { libc::alarm(STAGE.as_secs() as u32) };
    }
}

/// The user's stage ends this long after `from`: `STAGE` less the margin
/// that lets the surface close in order before the `alarm`.
fn user_deadline(from: Instant) -> Instant {
    from + STAGE.saturating_sub(SOFT_MARGIN)
}

/// The free buffers that still hold an older wheel than `generation`.
fn to_prepaint(painted: &[Painted; 2], busy: &[bool; 2], generation: u32) -> Vec<usize> {
    (0..2)
        .filter(|&i| !busy[i] && !matches!(painted[i], Some((g, _)) if g == generation))
        .collect()
}

/// Whether a `poll` return is an idle wakeup for the meter: nothing
/// `ready`, and `now` before both our deadlines (the stage's, and the drop
/// read's if one is in progress), which are wakeups we asked for.
fn idle_wakeup(ready: bool, now: Instant, deadline: Instant, read_until: Option<Instant>) -> bool {
    !ready && now < deadline && read_until.is_none_or(|u| now < u)
}

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Most globals kept from the registry; past it the compositor is broken.
const MAX_GLOBALS: usize = 256;
/// Most MIME types kept per data offer.
const MAX_MIMES: usize = 64;
/// Most data offers alive at once (we destroy every one we are done with).
const MAX_OFFERS: usize = 16;

/// The registry's globals until the first sync: name, interface, version.
#[derive(Default)]
struct Globals(Vec<(u32, String, u32)>);

impl Globals {
    fn add(&mut self, name: u32, iface: String, version: u32) -> io::Result<()> {
        if self.0.len() >= MAX_GLOBALS {
            return Err(other(format!("over {MAX_GLOBALS} Wayland globals")));
        }
        self.0.push((name, iface, version));
        Ok(())
    }

    /// The global `iface` if it has at least version `min`: its name and
    /// the version to bind (at most `max`).
    fn find(&self, iface: &str, min: u32, max: u32) -> Option<(u32, u32)> {
        self.0
            .iter()
            .find(|g| g.1 == iface && g.2 >= min)
            .map(|g| (g.0, g.2.min(max)))
    }
}

/// Live server-created data offers and their MIME types, and the one kept
/// alive whatever happens (the dropped offer, until its data is read).
#[derive(Default)]
struct Offers(Vec<(u32, Vec<String>)>, Option<u32>);

impl Offers {
    fn add(&mut self, id: u32) -> io::Result<()> {
        if self.0.len() >= MAX_OFFERS {
            return Err(other(format!("over {MAX_OFFERS} live data offers")));
        }
        self.0.push((id, Vec::new()));
        Ok(())
    }

    /// A MIME type for offer `id`; ignored for an offer not alive.
    fn mime(&mut self, id: u32, mime: String) -> io::Result<()> {
        let Some((_, mimes)) = self.0.iter_mut().find(|o| o.0 == id) else {
            return Ok(());
        };
        if mimes.len() >= MAX_MIMES {
            return Err(other(format!(
                "over {MAX_MIMES} MIME types in a data offer"
            )));
        }
        mimes.push(mime);
        Ok(())
    }

    fn mimes(&self, id: u32) -> Vec<String> {
        self.0
            .iter()
            .find(|o| o.0 == id)
            .map_or_else(Vec::new, |o| o.1.clone())
    }

    /// Keeps offer `id` through `retire` until `release`.
    fn keep(&mut self, id: u32) {
        self.1 = Some(id);
    }

    fn kept(&self) -> Option<u32> {
        self.1
    }

    /// The offer whose data to read: the one kept at the drop, if the drag
    /// had one.
    fn to_read(&self) -> Option<u32> {
        self.1.filter(|&id| id != 0)
    }

    /// Ends the keep and retires the kept offer: its id if it must now be
    /// destroyed. A later offer with the same (reused) id is not kept.
    fn release(&mut self) -> Option<u32> {
        let id = self.1.take()?;
        self.retire(id).then_some(id)
    }

    /// Forgets offer `id` unless it is kept; true if it was alive and must
    /// now be destroyed (so it is destroyed exactly once).
    fn retire(&mut self, id: u32) -> bool {
        if self.1 == Some(id) {
            return false;
        }
        let before = self.0.len();
        self.0.retain(|o| o.0 != id);
        self.0.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wheel::{GroupMenu, Menu, NOTHING};

    /// Regression (the user's niri log): after the stage-2 swap the first
    /// hover did a full paint inside the event (~150 µs). The free buffer
    /// that still holds the old wheel is now painted before the wait.
    #[test]
    fn a_free_buffer_with_an_older_wheel_is_prepainted() {
        // Stage 2 (generation 2) is on buffer 0; buffer 1 still holds stage 1.
        let painted = [Some((2, None)), Some((1, None))];
        assert_eq!(to_prepaint(&painted, &[true, false], 2), [1]);
        // Held by the compositor: not touched until released.
        assert!(to_prepaint(&painted, &[true, true], 2).is_empty());
        // Both current: nothing to do, so no repeated work per loop.
        let current = [Some((2, None)), Some((2, Some(3)))];
        assert!(to_prepaint(&current, &[false, false], 2).is_empty());
        // Never painted counts as older.
        assert_eq!(
            to_prepaint(&[Some((2, None)), None], &[true, false], 2),
            [1]
        );
    }

    fn reading(r: OwnedFd) -> Reading {
        Reading::new(r, 7, Instant::now() + READ_DROP)
    }

    /// Whatever the data, stage 2 has nothing: only the read's result
    /// matters here.
    fn empty_groups(_: Vec<u8>) -> GroupMenu {
        GroupMenu {
            paths: PathList::default(),
            slices: Default::default(),
            hub: NOTHING.into(),
            tally: Default::default(),
            catalog: None,
        }
    }

    fn empty_targets(_: &GroupMenu, _: &str) -> Menu {
        Menu {
            paths: PathList::default(),
            slices: Default::default(),
            hub: NOTHING.into(),
        }
    }

    /// A machine waiting for the dropped data.
    fn dropped() -> Machine {
        let mut m = Machine::with_menus(empty_groups, empty_targets);
        m.on(Input::Configured(1920, 1080));
        m.on(Input::DragEnter {
            x: 960.0,
            y: 540.0,
            mimes: vec![URI_LIST.into()],
        });
        assert_eq!(m.on(Input::Drop), [Effect::Arm(STAGE), Effect::ReadDrop]);
        m
    }

    #[test]
    fn stalled_source_times_out() {
        // A source that writes a little and never closes its end.
        let (r, w) = drop_pipe().unwrap();
        let mut rd = reading(r);
        // SAFETY: writing 5 bytes from a static buffer to a valid fd.
        assert_eq!(
            unsafe { libc::write(w.as_raw_fd(), b"file:".as_ptr().cast(), 5) },
            5
        );
        let start = Instant::now();
        assert_eq!(rd.pump(start), None, "still within the deadline");
        assert_eq!(rd.pump(start + READ_DROP / 2), None);
        let r = rd.pump(start + READ_DROP).expect("past the deadline");
        assert!(
            r.as_ref()
                .is_err_and(|e| e.to_string().contains("timed out")),
            "{r:?}"
        );
        assert_eq!(dropped().on(Input::DropData(r)), [Effect::Close]);
        drop(w);
    }

    #[test]
    fn oversized_drop_is_refused() {
        // Past the pipe's buffer: a writer thread, the reader pumping.
        let pump_all = |len: usize| {
            let (r, w) = drop_pipe().unwrap();
            let mut rd = reading(r);
            let writer = std::thread::spawn(move || {
                let mut f = std::fs::File::from(w);
                // The reader stops at the limit and closes: EPIPE is fine.
                let _ = io::Write::write_all(&mut f, &vec![b'a'; len]);
            });
            let give_up = Instant::now() + Duration::from_secs(5);
            let r = loop {
                if let Some(r) = rd.pump(Instant::now()) {
                    break r;
                }
                assert!(Instant::now() < give_up, "no result");
                std::thread::sleep(Duration::from_millis(1));
            };
            drop(rd);
            writer.join().unwrap();
            r
        };
        let r = pump_all(MAX_BYTES).expect("the limit itself is fine");
        assert_eq!(r.len(), MAX_BYTES);
        let r = pump_all(MAX_BYTES + 1);
        assert_eq!(r.as_ref().err(), Some(&ListError::TooBig));
        // Said on the hub, not a silent close.
        assert_eq!(dropped().on(Input::DropData(r)), [Effect::Draw]);
    }

    #[test]
    fn read_capacity_never_passes_the_limit_plus_one_chunk() {
        let (r, w) = drop_pipe().unwrap();
        let mut rd = reading(r);
        let writer = std::thread::spawn(move || {
            let mut f = std::fs::File::from(w);
            let _ = io::Write::write_all(&mut f, &vec![b'a'; MAX_BYTES + 1]);
        });
        let give_up = Instant::now() + Duration::from_secs(5);
        let res = loop {
            if let Some(res) = rd.pump(Instant::now()) {
                break res;
            }
            assert!(Instant::now() < give_up, "no result");
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(res, Err(ListError::TooBig));
        assert!(
            rd.data.capacity() <= MAX_BYTES + READ_CHUNK,
            "{}",
            rd.data.capacity()
        );
        drop(rd);
        writer.join().unwrap();
    }

    /// A `poll` that returns at one of our own deadlines (the stage's, or
    /// the drop read's) is not an idle wakeup; one with nothing ready
    /// before them is.
    #[test]
    fn startup_and_user_clocks_are_separate() {
        let now = Instant::now();
        // The user's stage is its own 30 s less the soft margin, counted
        // from the wheel's commit, however long the startup took.
        assert_eq!(user_deadline(now) - now, STAGE - SOFT_MARGIN);
        assert!(STARTUP < STAGE - SOFT_MARGIN);
        // A new surface runs on the startup clock.
        let (s, _server) = fake_compositor();
        assert!(!s.user_stage);
        assert!(s.deadline <= Instant::now() + STARTUP);
    }

    /// The wheel is on the screen from its first commit, not from the next
    /// pass of the loop that starts the user's stage: an error between the
    /// two (the flush right after the commit) is an error after the open,
    /// and a timeout there is the connection's, not the startup clock's.
    #[test]
    fn after_open_is_keyed_on_the_first_commit() {
        let (mut s, _server) = fake_compositor();
        let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "slow");
        assert!(matches!(
            s.start_error(timed_out()),
            StartupError::ConfigureTimeout
        ));
        // Committed, the loop has not yet begun the user's stage.
        s.shown = Some((0, None));
        assert!(!s.user_stage);
        assert!(matches!(
            s.start_error(timed_out()),
            StartupError::Protocol(_)
        ));
        let broke = |opened| classify(Err(StartupError::Protocol(timed_out())), opened);
        assert!(matches!(broke(true), Err(StartupError::AfterOpen(_))));
        assert!(matches!(broke(false), Err(StartupError::Protocol(_))));
        // Neither a choice nor another kind of failure is rewritten.
        assert!(matches!(classify(Ok(None), true), Ok(None)));
        let init = Err(StartupError::Initialization(timed_out()));
        assert!(matches!(
            classify(init, true),
            Err(StartupError::Initialization(_))
        ));
    }

    /// The startup's hard backstop leaves the soft path (the startup clock
    /// and the close) room to finish in order, and is far below the user's
    /// stage.
    #[test]
    fn startup_backstop_covers_the_soft_path_and_not_the_stage() {
        assert!(STARTUP + TEARDOWN < STARTUP_BACKSTOP);
        // The close started at any instant of the user's stage ends before
        // its own alarm, which is the only one then pending.
        assert!(TEARDOWN < TEARDOWN_BACKSTOP);
        assert_eq!(TEARDOWN_BACKSTOP.subsec_nanos(), 0, "alarm counts seconds");
        assert!(STARTUP_BACKSTOP < STAGE);
        assert_eq!(STARTUP_BACKSTOP.subsec_nanos(), 0, "alarm counts seconds");
    }

    /// A wheel with one lit slice; the machine states below swap it.
    fn render_surface() -> (Surface, OwnedFd) {
        let (mut s, server) = fake_compositor();
        s.shm = Some(Shm::new().unwrap());
        s.id.surface = s.conn.new_id(Iface::Surface);
        s.id.wheel = s.conn.new_id(Iface::Surface);
        s.id.sub = s.conn.new_id(Iface::Subsurface);
        for i in 0..2 {
            s.id.bufs[i] = s.conn.new_id(Iface::Buffer);
        }
        (s, server)
    }

    /// Every buffer that claims the current wheel holds exactly a full paint of it:
    /// the damage-only paths (hover, prepaint, buffer reuse) leave the same
    /// pixels a repaint from scratch would.
    fn assert_buffers_are_full_paints(s: &mut Surface, m: &Machine, what: &str) {
        for i in 0..2 {
            let Some((g, hover)) = s.painted[i] else {
                continue;
            };
            // A buffer of an older wheel is not shown again before its
            // next full paint (`render`, `prepaint`): nothing to compare.
            if g != m.generation() {
                continue;
            }
            let mut want = vec![0xAAu8; s.shm.as_mut().unwrap().canvas(i).len()];
            m.wheel().paint_full(&mut want, hover);
            assert!(
                s.shm.as_mut().unwrap().canvas(i) == &want[..],
                "{what}: buffer {i} differs from a full paint of {hover:?}"
            );
        }
    }

    /// Two lit categories (north and south).
    fn two_lit(_: Vec<u8>) -> GroupMenu {
        let mut gm = empty_groups(Vec::new());
        for (k, name) in [(0, "North"), (4, "South")] {
            gm.slices[k] = Some(crate::wheel::GroupChoice {
                label: name.into(),
                group: name.into(),
                lit: true,
            });
        }
        gm.hub = "src".into();
        gm
    }

    /// Startup with the second buffer unpainted (the first commit alone),
    /// the prepaint that makes it ready, a stage swap, the prepaint of the
    /// released buffer, and hover on both buffers in turn: after each step
    /// every claimed state is pixel-equal to a full repaint.
    #[test]
    fn render_pixels_equal_a_full_repaint_through_swaps_and_hover() {
        let (mut s, server) = render_surface();
        let mut m = Machine::with_menus(two_lit, empty_targets);
        // What `setup` paints for the first frame: buffer 0 only.
        let first = m.generation();
        m.wheel()
            .paint_full(s.shm.as_mut().unwrap().canvas(0), None);
        s.painted[0] = Some((first, None));
        m.on(Input::Configured(1920, 1080));
        m.on(Input::DragEnter {
            x: 960.0,
            y: 540.0,
            mimes: vec![URI_LIST.into()],
        });
        s.render(&m).unwrap();
        assert!(
            s.painted[1].is_none(),
            "the second buffer is not painted yet"
        );
        assert_buffers_are_full_paints(&mut s, &m, "first commit");
        // Not shown yet until released: buffer 1 is free but unpainted.
        s.prepaint(&m);
        assert_eq!(s.painted[1], Some((m.generation(), m.hover())));
        assert_buffers_are_full_paints(&mut s, &m, "prepaint");
        // The list arrives: another wheel, both buffers stale.
        m.on(Input::DragList(1, Ok(b"a".to_vec())));
        s.render(&m).unwrap();
        assert_buffers_are_full_paints(&mut s, &m, "swap");
        // The old frame is released and repainted ahead of time.
        let release = msg(s.id.bufs[0], op::wl_buffer::RELEASE, &[]);
        deliver_to(&mut s, &server, &release, &m);
        s.prepaint(&m);
        assert_buffers_are_full_paints(&mut s, &m, "prepaint after swap");
        // Hover north, south, off, north again: both buffers take turns.
        for (k, (x, y)) in [
            (960.0, 390.0),
            (960.0, 690.0),
            (960.0, 540.0),
            (960.0, 390.0),
        ]
        .into_iter()
        .enumerate()
        {
            m.on(Input::DragMotion(x, y));
            s.render(&m).unwrap();
            assert_buffers_are_full_paints(&mut s, &m, &format!("hover {k}"));
            for i in 0..2 {
                let release = msg(s.id.bufs[i], op::wl_buffer::RELEASE, &[]);
                deliver_to(&mut s, &server, &release, &m);
            }
            s.prepaint(&m);
            assert_buffers_are_full_paints(&mut s, &m, &format!("released {k}"));
        }
        assert_eq!(s.shown, Some((m.generation(), m.hover())));
    }

    /// The compositor holds both buffers (no release yet): a hover, a
    /// stage swap and the prepaint write into neither, and send nothing.
    /// Checked on the bytes of the buffers themselves, at the point of
    /// writing, not on a count taken after the choice of buffer. After one
    /// release only the released buffer changes.
    #[test]
    fn render_never_writes_into_a_buffer_the_compositor_holds() {
        let (mut s, server) = render_surface();
        let mut m = Machine::with_menus(two_lit, empty_targets);
        m.wheel()
            .paint_full(s.shm.as_mut().unwrap().canvas(0), None);
        s.painted[0] = Some((m.generation(), None));
        m.on(Input::Configured(1920, 1080));
        m.on(Input::DragEnter {
            x: 960.0,
            y: 540.0,
            mimes: vec![URI_LIST.into()],
        });
        s.render(&m).unwrap(); // buffer 0 attached
        m.on(Input::DragList(1, Ok(b"a".to_vec())));
        s.render(&m).unwrap(); // the swap: buffer 1 attached
        assert!(s.busy[0] && s.busy[1], "{:?} {:?}", s.busy, s.shown);
        let _ = wheel_commits(&mut s, &server);
        let held = [
            s.shm.as_mut().unwrap().canvas(0).to_vec(),
            s.shm.as_mut().unwrap().canvas(1).to_vec(),
        ];
        // Changes of every kind while nothing is released.
        for (x, y) in [(960.0, 690.0), (960.0, 540.0), (960.0, 390.0)] {
            m.on(Input::DragMotion(x, y));
            s.render(&m).unwrap();
            s.prepaint(&m);
        }
        for (i, before) in held.iter().enumerate() {
            assert!(
                s.shm.as_mut().unwrap().canvas(i) == &before[..],
                "buffer {i} written while the compositor holds it"
            );
        }
        assert!(
            wheel_commits(&mut s, &server).is_empty(),
            "attached a held buffer"
        );
        // Release buffer 0 only: it takes the frame, buffer 1 stays as is.
        let release = msg(s.id.bufs[0], op::wl_buffer::RELEASE, &[]);
        deliver_to(&mut s, &server, &release, &m);
        s.render(&m).unwrap();
        let commits = wheel_commits(&mut s, &server);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, s.id.bufs[0]);
        assert!(s.shm.as_mut().unwrap().canvas(0) != &held[0][..]);
        assert!(s.shm.as_mut().unwrap().canvas(1) == &held[1][..]);
    }

    /// Pages of wheel buffer `i` the process has in memory (`mincore`).
    fn resident_pages(s: &mut Surface, i: usize) -> usize {
        let page = 4096;
        let canvas = s.shm.as_mut().unwrap().canvas(i);
        let mut v = vec![0u8; canvas.len().div_ceil(page)];
        // SAFETY: the canvas is page aligned (the mapping's start plus a
        // multiple of 4096) and `v` has one byte per page of its length.
        let r = unsafe { libc::mincore(canvas.as_mut_ptr().cast(), canvas.len(), v.as_mut_ptr()) };
        assert_eq!(r, 0, "{}", io::Error::last_os_error());
        v.iter().filter(|&&b| b & 1 != 0).count()
    }

    /// Buffer 1 is faulted in while the wheel waits after its first commit
    /// (here: for the early list), before the stage-1 swap paints into it;
    /// not before that commit, and its bytes do not change.
    #[test]
    fn the_second_buffer_is_populated_in_the_wait_before_the_swap() {
        // With shmem THP "always" painting buffer 0 can fault one huge page
        // that also covers buffer 1, so "fresh" is not observable.
        if std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/shmem_enabled")
            .is_ok_and(|t| t.contains("[always]"))
        {
            eprintln!("skipped: shmem THP is [always]");
            return;
        }
        let (mut s, _server) = render_surface();
        let mut m = Machine::with_menus(two_lit, empty_targets);
        m.wheel()
            .paint_full(s.shm.as_mut().unwrap().canvas(0), None);
        s.painted[0] = Some((m.generation(), None));
        m.on(Input::Configured(1920, 1080));
        m.on(Input::DragEnter {
            x: 960.0,
            y: 540.0,
            mimes: vec![URI_LIST.into()],
        });
        // Before the first commit: nothing to do (main-to-commit pays it).
        s.populate_second();
        assert_eq!(resident_pages(&mut s, 1), 0);
        s.render(&m).unwrap(); // `Drop here` on buffer 0
        assert_eq!(resident_pages(&mut s, 1), 0, "fresh before the wait");
        s.populate_second();
        let pages = FRAME.div_ceil(4096);
        assert_eq!(resident_pages(&mut s, 1), pages);
        assert!(s.shm.as_mut().unwrap().canvas(1).iter().all(|&b| b == 0));
        assert!(!s.second_is_fresh());
        // The swap then paints into it.
        m.on(Input::DragList(1, Ok(b"a".to_vec())));
        s.render(&m).unwrap();
        assert_eq!(s.painted[1], Some((m.generation(), m.hover())));
    }

    /// The first touch never writes into a buffer the compositor holds or
    /// one that was already painted.
    #[test]
    fn the_second_buffer_is_not_populated_when_held_or_painted() {
        let (mut s, _server) = render_surface();
        s.shown = Some((1, None));
        s.busy[1] = true;
        assert!(!s.second_is_fresh());
        s.populate_second();
        assert_eq!(resident_pages(&mut s, 1), 0);
        s.busy[1] = false;
        s.painted[1] = Some((1, None));
        assert!(!s.second_is_fresh());
        s.populate_second();
        assert_eq!(resident_pages(&mut s, 1), 0);
    }

    /// Pixels covered by `rects` (`[x, y, w, h]`), overlaps counted once.
    fn union_area(rects: &[[i32; 4]]) -> i64 {
        let b = BOX as i32;
        let mut n = 0;
        for y in 0..b {
            for x in 0..b {
                if rects
                    .iter()
                    .any(|r| (r[0]..r[0] + r[2]).contains(&x) && (r[1]..r[1] + r[3]).contains(&y))
                {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn union_area_counts_overlaps_once() {
        assert_eq!(union_area(&[[0, 0, 10, 10]]), 100);
        assert_eq!(union_area(&[[0, 0, 10, 10], [5, 5, 10, 10]]), 175);
        assert_eq!(union_area(&[[0, 0, 10, 10], [0, 0, 10, 10]]), 100);
        assert_eq!(union_area(&[]), 0);
    }

    /// The damage sent with each commit covers every pixel that differs
    /// between the frame on screen before and after it (the attached
    /// buffer's pixels at the commit), with buffers alternating, and stays
    /// well under the whole box for a hover. Damage computed against the
    /// buffer being drawn, not the one on screen, leaves the pixels the
    /// other buffer still shows undamaged.
    #[test]
    fn render_damage_covers_what_changed_on_screen() {
        let (mut s, server) = render_surface();
        let mut m = Machine::with_menus(two_lit, empty_targets);
        m.wheel()
            .paint_full(s.shm.as_mut().unwrap().canvas(0), None);
        s.painted[0] = Some((m.generation(), None));
        m.on(Input::Configured(1920, 1080));
        m.on(Input::DragEnter {
            x: 960.0,
            y: 540.0,
            mimes: vec![URI_LIST.into()],
        });
        // `tight`: the damage must also stay near the change. Not asked
        // of the frames whose buffer was prepainted in full (first hovers
        // after the swap): their damage is the whole box by design, so
        // there is no over-damage to see there (a bound of "the box" is
        // all the full-box damage could break).
        let mut on_screen: Vec<u8> = Vec::new();
        let step = |s: &mut Surface, m: &Machine, what: &str, tight: bool, screen: &mut Vec<u8>| {
            s.render(m).unwrap();
            let commits = wheel_commits(s, &server);
            assert_eq!(commits.len(), 1, "{what}: one commit");
            let (buf, rects) = &commits[0];
            let i = s.id.bufs.iter().position(|b| b == buf).unwrap();
            let now = s.shm.as_mut().unwrap().canvas(i).to_vec();
            let b = BOX as usize;
            let mut diff = (usize::MAX, usize::MAX, 0, 0);
            for p in 0..b * b {
                let (x, y) = (p % b, p / b);
                let changed =
                    screen.is_empty() || screen[4 * p..4 * p + 4] != now[4 * p..4 * p + 4];
                if !changed {
                    continue;
                }
                diff = (diff.0.min(x), diff.1.min(y), diff.2.max(x), diff.3.max(y));
                assert!(
                    rects.iter().any(|r| {
                        (r[0]..r[0] + r[2]).contains(&(x as i32))
                            && (r[1]..r[1] + r[3]).contains(&(y as i32))
                    }),
                    "{what}: pixel ({x},{y}) changed on screen but is not damaged: {rects:?}"
                );
            }
            if tight && diff.2 >= diff.0 {
                let area = union_area(rects);
                let bbox = ((diff.2 - diff.0 + 1) * (diff.3 - diff.1 + 1)) as i64;
                assert!(
                    area <= 4 * bbox + 64 * 64,
                    "{what}: damage {area} px for a change within {bbox} px"
                );
            }
            *screen = now;
            i
        };
        let first_shown = step(&mut s, &m, "first", false, &mut on_screen);
        // The compositor releases every buffer but the one on screen, so
        // the buffers alternate.
        let release_others = |s: &mut Surface, m: &Machine, shown: usize| {
            for i in (0..2).filter(|&i| i != shown) {
                let release = msg(s.id.bufs[i], op::wl_buffer::RELEASE, &[]);
                deliver_to(s, &server, &release, m);
            }
        };
        release_others(&mut s, &m, first_shown);
        s.prepaint(&m);
        m.on(Input::DragList(1, Ok(b"a".to_vec())));
        let shown = step(&mut s, &m, "swap", false, &mut on_screen);
        release_others(&mut s, &m, shown);
        s.prepaint(&m);
        // Buffers alternate through several hovers, then back to none.
        let path = [
            (960.0, 390.0),
            (960.0, 690.0),
            (960.0, 540.0),
            (960.0, 390.0),
            (960.0, 690.0),
            (960.0, 390.0),
        ];
        for (k, (x, y)) in path.into_iter().enumerate() {
            m.on(Input::DragMotion(x, y));
            let shown = step(&mut s, &m, &format!("hover {k}"), k >= 2, &mut on_screen);
            // No prepaint: each hover paints over what that buffer showed
            // two frames ago, so only the damage rectangles can tell what
            // differs from the frame on screen.
            release_others(&mut s, &m, shown);
        }
    }

    #[test]
    fn idle_wakeups_exclude_our_deadlines() {
        let now = Instant::now();
        let (later, earlier) = (now + Duration::from_secs(1), now - Duration::from_millis(1));
        assert!(idle_wakeup(false, now, later, None));
        assert!(idle_wakeup(false, now, later, Some(later)));
        assert!(!idle_wakeup(true, now, later, None), "input");
        assert!(
            !idle_wakeup(false, now, earlier, None),
            "the stage deadline"
        );
        assert!(
            !idle_wakeup(false, now, later, Some(earlier)),
            "the read deadline"
        );
        assert!(
            !idle_wakeup(false, now, later, Some(now)),
            "the read deadline, exactly"
        );
    }

    // A fake compositor: the server end of a socketpair under a Surface.

    const DEVICE_OFFER_A: u32 = 0xff00_0001;
    const DEVICE_OFFER_B: u32 = 0xff00_0002;

    /// A Surface with a data device, and the compositor's end of its socket.
    fn fake_compositor() -> (Surface, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: valid 2-element out array; checked.
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(r, 0);
        // SAFETY: fresh descriptors we own.
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut s = Surface::new(Conn::from_fd(a));
        s.id.data_device = s.conn.new_id(Iface::DataDevice);
        (s, b)
    }

    /// One wire message: header, then the argument words.
    fn msg(obj: u32, op: u16, args: &[u32]) -> Vec<u8> {
        let size = (8 + 4 * args.len()) as u32;
        [obj, (size << 16) | u32::from(op)]
            .iter()
            .chain(args)
            .flat_map(|w| w.to_ne_bytes())
            .collect()
    }

    /// The events of a drag over us: a new offer with copy negotiated,
    /// entered at serial `serial`.
    fn drag_enters(s: &Surface, offer: u32, serial: u32) -> Vec<u8> {
        drag_enters_with(s, offer, serial, DND_COPY)
    }

    /// `drag_enters`, with `action` negotiated instead of copy.
    fn drag_enters_with(s: &Surface, offer: u32, serial: u32, action: u32) -> Vec<u8> {
        drag_enters_as(s, offer, serial, action, URI_LIST)
    }

    /// `drag_enters_with`, the offer advertising `mime`.
    fn drag_enters_as(s: &Surface, offer: u32, serial: u32, action: u32, mime: &str) -> Vec<u8> {
        let dd = s.id.data_device;
        let at = 100 * 256; // wl_fixed 100.0
        let mut m = msg(dd, op::wl_data_device::DATA_OFFER, &[offer]);
        // The OFFER event: a string (length with NUL, bytes, NUL, padding).
        let mut str_words: Vec<u32> = vec![mime.len() as u32 + 1];
        let mut bytes = mime.as_bytes().to_vec();
        bytes.push(0);
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
        str_words.extend(
            bytes
                .chunks(4)
                .map(|c| u32::from_ne_bytes(c.try_into().unwrap())),
        );
        m.extend(msg(offer, op::wl_data_offer::OFFER, &str_words));
        m.extend(msg(
            dd,
            op::wl_data_device::ENTER,
            &[serial, 1, at, at, offer],
        ));
        m.extend(msg(offer, op::wl_data_offer::ACTION, &[action]));
        m
    }

    /// Hands `events` to the surface and dispatches them.
    fn deliver(s: &mut Surface, server: &OwnedFd, events: &[u8]) {
        deliver_to(s, server, events, &Machine::new());
    }

    /// `deliver`, dispatched against machine `m`.
    fn deliver_to(s: &mut Surface, server: &OwnedFd, events: &[u8], m: &Machine) {
        // SAFETY: writing a valid buffer to a valid fd.
        let n = unsafe { libc::write(server.as_raw_fd(), events.as_ptr().cast(), events.len()) };
        assert_eq!(n as usize, events.len());
        assert!(
            s.conn
                .read(Instant::now() + Duration::from_secs(1))
                .unwrap()
        );
        s.dispatch(m).unwrap();
    }

    /// The requests the surface sent so far, as (object, opcode), and the
    /// fds that came with them.
    fn requests(s: &mut Surface, server: &OwnedFd) -> (Vec<(u32, u16)>, Vec<OwnedFd>) {
        let (bytes, fds) = drain(s, server);
        let word = |i: usize| u32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap());
        let mut reqs = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let (obj, head) = (word(i), word(i + 4));
            reqs.push((obj, (head & 0xffff) as u16));
            i += (head >> 16) as usize;
        }
        (reqs, fds)
    }

    /// Flushes the surface and takes every byte (and fd) the compositor's
    /// end has received so far.
    fn drain(s: &mut Surface, server: &OwnedFd) -> (Vec<u8>, Vec<OwnedFd>) {
        s.flush().unwrap();
        let (mut bytes, mut fds) = (Vec::new(), Vec::new());
        loop {
            let mut buf = [0u8; 4096];
            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            let mut cbuf = [0u64; 16];
            // SAFETY: a zeroed msghdr is valid; its pointers outlive recvmsg.
            let mut mh: libc::msghdr = unsafe { std::mem::zeroed() };
            mh.msg_iov = &mut iov;
            mh.msg_iovlen = 1;
            mh.msg_control = cbuf.as_mut_ptr().cast();
            mh.msg_controllen = std::mem::size_of_val(&cbuf);
            // SAFETY: mh describes valid buffers.
            let n = unsafe {
                libc::recvmsg(
                    server.as_raw_fd(),
                    &mut mh,
                    libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
                )
            };
            if n <= 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n as usize]);
            // SAFETY: walking the cmsgs the kernel wrote; their fds are ours.
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&mh);
                while !c.is_null() {
                    let count = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / 4;
                    let data = libc::CMSG_DATA(c) as *const RawFd;
                    for i in 0..count {
                        fds.push(OwnedFd::from_raw_fd(*data.add(i)));
                    }
                    c = libc::CMSG_NXTHDR(&mh, c);
                }
            }
        }
        (bytes, fds)
    }

    /// The wheel surface's commits so far: the buffer each attached and the
    /// `damage_buffer` rectangles (x, y, w, h) sent before it.
    fn wheel_commits(s: &mut Surface, server: &OwnedFd) -> Vec<(u32, Vec<[i32; 4]>)> {
        let (bytes, _) = drain(s, server);
        let word = |i: usize| u32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap());
        let (mut out, mut buf, mut rects) = (Vec::new(), 0, Vec::new());
        let mut i = 0;
        while i < bytes.len() {
            let (obj, head) = (word(i), word(i + 4));
            if obj == s.id.wheel {
                match (head & 0xffff) as u16 {
                    op::wl_surface::ATTACH => buf = word(i + 8),
                    op::wl_surface::DAMAGE_BUFFER => {
                        rects.push([0, 4, 8, 12].map(|o| word(i + 8 + o) as i32));
                    }
                    op::wl_surface::COMMIT => out.push((buf, std::mem::take(&mut rects))),
                    _ => {}
                }
            }
            i += (head >> 16) as usize;
        }
        out
    }

    /// The source writes the list and closes its end; the surface reads it.
    fn source_writes(s: &mut Surface, w: OwnedFd) {
        let list = b"file:///x\r\n";
        // SAFETY: writing a valid buffer to a valid fd.
        let n = unsafe { libc::write(w.as_raw_fd(), list.as_ptr().cast(), list.len()) };
        assert_eq!(n as usize, list.len());
        drop(w);
        s.pump_read();
        assert!(s.reading.is_none(), "read to EOF");
    }

    /// Regression: the read took the current drag's offer and `finish`
    /// required it, with its action, so an enter handled between the drop
    /// and the read made us read the new drag and never finish the drop.
    #[test]
    fn the_dropped_offer_is_read_and_finished_after_a_new_enter() {
        let (mut s, server) = fake_compositor();
        let mut ev = drag_enters(&s, DEVICE_OFFER_A, 1);
        ev.extend(msg(s.id.data_device, op::wl_data_device::DROP, &[]));
        // Drag B negotiates no copy: `finish` must use the action of the
        // drop (copy), not the current drag's.
        ev.extend(drag_enters_with(&s, DEVICE_OFFER_B, 2, 0));
        deliver(&mut s, &server, &ev);
        s.start_read().unwrap();
        let (reqs, mut fds) = requests(&mut s, &server);
        assert_eq!(reqs, [(DEVICE_OFFER_A, op::wl_data_offer::RECEIVE)]);
        assert_eq!(fds.len(), 1, "the pipe's write end");
        source_writes(&mut s, fds.remove(0));
        let (reqs, _) = requests(&mut s, &server);
        assert_eq!(
            reqs,
            [
                (DEVICE_OFFER_A, op::wl_data_offer::FINISH),
                (DEVICE_OFFER_A, op::wl_data_offer::DESTROY),
            ],
            "nothing to the new drag's offer"
        );
        assert_eq!(
            s.inputs.last(),
            Some(&Input::DropData(Ok(b"file:///x\r\n".to_vec())))
        );
    }

    /// A list the wheel refuses is a cancel toward the source, whichever
    /// limit it passed: no `finish`, only the offer's `destroy`. A
    /// positive control reads a list within both limits: it is finished.
    #[test]
    fn a_refused_drop_list_is_not_finished_either_way() {
        let uris = |n: usize| "file:///x\r\n".repeat(n).into_bytes();
        let too_big = vec![b'a'; MAX_BYTES + 1];
        for (list, finished) in [
            (uris(1), true),
            (uris(crate::uri::MAX_URIS), true),
            (uris(crate::uri::MAX_URIS + 1), false),
            (too_big, false),
        ] {
            let (mut s, server) = fake_compositor();
            let mut ev = drag_enters(&s, DEVICE_OFFER_A, 1);
            ev.extend(msg(s.id.data_device, op::wl_data_device::DROP, &[]));
            deliver(&mut s, &server, &ev);
            s.start_read().unwrap();
            let (_, mut fds) = requests(&mut s, &server);
            let w = fds.remove(0);
            let writer = std::thread::spawn(move || {
                let mut f = std::fs::File::from(w);
                // The reader may stop at the limit and close: EPIPE is fine.
                let _ = io::Write::write_all(&mut f, &list);
            });
            let give_up = Instant::now() + Duration::from_secs(5);
            while s.reading.is_some() {
                s.pump_read();
                assert!(Instant::now() < give_up, "no result");
                std::thread::sleep(Duration::from_millis(1));
            }
            writer.join().unwrap();
            let mut want = vec![(DEVICE_OFFER_A, op::wl_data_offer::DESTROY)];
            if finished {
                want.insert(0, (DEVICE_OFFER_A, op::wl_data_offer::FINISH));
            }
            assert_eq!(requests(&mut s, &server).0, want);
        }
    }

    /// A second drop while the first is read must not replace the keep:
    /// the first offer is the one read, finished and destroyed.
    #[test]
    fn a_drop_during_the_read_keeps_the_first_offer() {
        let (mut s, server) = fake_compositor();
        let dd = s.id.data_device;
        let mut ev = drag_enters(&s, DEVICE_OFFER_A, 1);
        ev.extend(msg(dd, op::wl_data_device::DROP, &[]));
        deliver(&mut s, &server, &ev);
        s.start_read().unwrap();
        let (_, mut fds) = requests(&mut s, &server);
        let mut ev = drag_enters(&s, DEVICE_OFFER_B, 2);
        ev.extend(msg(dd, op::wl_data_device::DROP, &[]));
        deliver(&mut s, &server, &ev);
        assert_eq!(s.offers.kept(), Some(DEVICE_OFFER_A));
        source_writes(&mut s, fds.remove(0));
        let (reqs, _) = requests(&mut s, &server);
        assert_eq!(
            reqs,
            [
                (DEVICE_OFFER_A, op::wl_data_offer::FINISH),
                (DEVICE_OFFER_A, op::wl_data_offer::DESTROY),
            ]
        );
        assert_eq!(s.offers.kept(), None);
    }

    /// The early read's source writes the list and closes its end.
    fn early_source_writes(s: &mut Surface, w: OwnedFd) {
        let list = b"file:///x\r\n";
        // SAFETY: writing a valid buffer to a valid fd.
        let n = unsafe { libc::write(w.as_raw_fd(), list.as_ptr().cast(), list.len()) };
        assert_eq!(n as usize, list.len());
        drop(w);
        s.pump_early();
        assert!(s.early.is_none(), "read to EOF");
    }

    /// Whether the reader of the pipe whose write end is `w` is gone.
    fn reader_gone(w: &OwnedFd) -> bool {
        let mut p = libc::pollfd {
            fd: w.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut p, 1, 0) };
        p.revents & libc::POLLERR != 0
    }

    /// The tail of every ended drop: `finish` only after a negotiated copy
    /// (anything else is a protocol error), then the keep ends and the offer
    /// is destroyed, once.
    #[test]
    fn ending_a_dropped_offer_finishes_only_after_copy() {
        for (action, expect) in [
            (
                DND_COPY,
                vec![
                    (DEVICE_OFFER_A, op::wl_data_offer::FINISH),
                    (DEVICE_OFFER_A, op::wl_data_offer::DESTROY),
                ],
            ),
            (0, vec![(DEVICE_OFFER_A, op::wl_data_offer::DESTROY)]),
        ] {
            let (mut s, server) = fake_compositor();
            let mut ev = drag_enters_with(&s, DEVICE_OFFER_A, 1, action);
            ev.extend(msg(s.id.data_device, op::wl_data_device::DROP, &[]));
            deliver(&mut s, &server, &ev);
            s.end_drop();
            let (reqs, _) = requests(&mut s, &server);
            assert_eq!(reqs, expect, "action {action}");
            assert_eq!(s.offers.kept(), None);
            // Ending twice destroys nothing again.
            s.end_drop();
            assert_eq!(requests(&mut s, &server).0, []);
        }
    }

    /// The early read receives the drag's list on a pipe, tags the result
    /// with the machine's tag, and leaves the offer alone: no `finish`, no
    /// `destroy`.
    #[test]
    fn the_early_read_delivers_the_tagged_list_and_keeps_the_offer() {
        let (mut s, server) = fake_compositor();
        let ev = drag_enters(&s, DEVICE_OFFER_A, 1);
        deliver(&mut s, &server, &ev);
        s.apply(Effect::ReadEarly(7), &Machine::new()).unwrap();
        let (reqs, mut fds) = requests(&mut s, &server);
        assert_eq!(reqs, [(DEVICE_OFFER_A, op::wl_data_offer::RECEIVE)]);
        assert_eq!(fds.len(), 1, "the pipe's write end");
        early_source_writes(&mut s, fds.remove(0));
        assert_eq!(
            s.inputs.last(),
            Some(&Input::DragList(7, Ok(b"file:///x\r\n".to_vec())))
        );
        assert_eq!(requests(&mut s, &server).0, []);
    }

    /// Level 1 that burns measurable CPU, like a real classification.
    fn slow_groups(b: Vec<u8>) -> GroupMenu {
        let t = crate::cpu_us(libc::RUSAGE_SELF);
        while crate::cpu_us(libc::RUSAGE_SELF) - t < 5000 {
            std::hint::spin_loop();
        }
        empty_groups(b)
    }

    /// The choice's CPU and CLOCK_MONOTONIC readings live in the meter,
    /// which `run` hands back after the surface is gone: the report built
    /// from it still carries both, under this process's id, bracketed by
    /// the clocks around the choice (the run's entry marker uses the same
    /// clocks, so the two logs correlate).
    #[test]
    fn choice_and_entry_markers_survive_surface_teardown() {
        let (mut s, _server) = fake_compositor();
        s.meter = Some(Meter::new(0, 0, Instant::now()));
        let (cpu0, mono0) = (
            crate::cpu_us(libc::RUSAGE_SELF),
            crate::monotonic_us().unwrap(),
        );
        s.apply(
            Effect::Run(PathList::default(), "p".into()),
            &Machine::new(),
        )
        .unwrap();
        let (cpu1, mono1) = (
            crate::cpu_us(libc::RUSAGE_SELF),
            crate::monotonic_us().unwrap(),
        );
        // What `run` does on its way out, then the surface is dropped.
        let meter = s.meter.take().unwrap();
        drop(s);
        let r = meter.report(0, -1);
        let get = |k: &str| -> i64 {
            r.split_whitespace()
                .find_map(|w| w.strip_prefix(k)?.strip_prefix('='))
                .unwrap_or_else(|| panic!("no {k} in {r}"))
                .parse()
                .unwrap()
        };
        assert!((cpu0..=cpu1).contains(&get("choice_abs_cpu_us")), "{r}");
        assert!((mono0..=mono1).contains(&get("choice_abs_wall_us")), "{r}");
        assert_eq!(get("pid"), i64::from(std::process::id()));
    }

    /// `list_to_level1_cpu_us` starts at a list the machine took: a list
    /// with a stale tag, or one after a failure, is not counted.
    #[test]
    fn the_meter_counts_only_a_list_the_machine_accepted() {
        let (mut s, _server) = fake_compositor();
        s.meter = Some(Meter::new(0, 0, Instant::now()));
        let mut m = Machine::with_menus(empty_groups, empty_targets);
        let enter = Input::DragEnter {
            x: 10.0,
            y: 10.0,
            mimes: vec![URI_LIST.into()],
        };
        m.on(enter);
        // The enter tagged its read 1; a list tagged 9 is another drag's.
        s.inputs
            .push(Input::DragList(9, Ok(b"file:///x\r\n".to_vec())));
        s.feed(&mut m).unwrap();
        assert!(!s.meter.as_ref().unwrap().has_list(), "stale tag");
        // The start is the list's arrival: before the menu is built (5 ms
        // of CPU here), not after.
        let mut m = Machine::with_menus(slow_groups, empty_targets);
        m.on(Input::DragEnter {
            x: 10.0,
            y: 10.0,
            mimes: vec![URI_LIST.into()],
        });
        let before = crate::cpu_us(libc::RUSAGE_SELF);
        s.inputs
            .push(Input::DragList(1, Ok(b"file:///x\r\n".to_vec())));
        s.feed(&mut m).unwrap();
        let at = s.meter.as_ref().unwrap().list_cpu().expect("taken list");
        assert!(at >= before && at < before + 2000, "{at} vs {before}");
        assert!(
            crate::cpu_us(libc::RUSAGE_SELF) - at >= 5000,
            "span covers the build"
        );
    }

    /// No offer to read (the null offer): the read fails at once, so the
    /// machine falls back to `Drop here`.
    #[test]
    fn an_early_read_without_an_offer_fails_at_once() {
        let (mut s, _server) = fake_compositor();
        s.apply(Effect::ReadEarly(3), &Machine::new()).unwrap();
        assert!(s.early.is_none());
        assert!(matches!(s.inputs.last(), Some(Input::DragList(3, Err(_)))));
    }

    /// A source that never writes: the early read ends in an error at its
    /// deadline, tagged, and the offer is untouched.
    #[test]
    fn a_silent_early_source_times_out() {
        let (mut s, server) = fake_compositor();
        let ev = drag_enters(&s, DEVICE_OFFER_A, 1);
        deliver(&mut s, &server, &ev);
        s.apply(Effect::ReadEarly(2), &Machine::new()).unwrap();
        let (_, fds) = requests(&mut s, &server);
        s.pump_early();
        assert!(s.early.is_some(), "still waiting");
        s.early.as_mut().unwrap().read.until = Instant::now() - Duration::from_millis(1);
        s.pump_early();
        assert!(s.early.is_none());
        assert!(matches!(s.inputs.last(), Some(Input::DragList(2, Err(_)))));
        drop(fds);
    }

    /// Regression guard: a new enter (another offer, or the same one again)
    /// closes the early pipe in flight, so its list cannot reach the new
    /// drag; the same for a leave.
    #[test]
    fn a_new_enter_or_a_leave_closes_the_early_pipe() {
        let (mut s, server) = fake_compositor();
        let ev = drag_enters(&s, DEVICE_OFFER_A, 1);
        deliver(&mut s, &server, &ev);
        s.apply(Effect::ReadEarly(1), &Machine::new()).unwrap();
        let (_, mut fds) = requests(&mut s, &server);
        let w = fds.remove(0);
        assert!(!reader_gone(&w));
        let ev = drag_enters(&s, DEVICE_OFFER_B, 2);
        deliver(&mut s, &server, &ev);
        assert!(s.early.is_none());
        assert!(reader_gone(&w), "the pipe was closed");

        s.apply(Effect::ReadEarly(2), &Machine::new()).unwrap();
        let (_, mut fds) = requests(&mut s, &server);
        let w = fds.remove(0);
        let leave = msg(s.id.data_device, op::wl_data_device::LEAVE, &[]);
        deliver(&mut s, &server, &leave);
        assert!(reader_gone(&w));
    }

    /// A drop while the early read is still in flight takes the fallback's
    /// read: the early pipe is closed, and the one drop read follows.
    #[test]
    fn a_drop_closes_the_early_read_before_the_fallback_read() {
        let (mut s, server) = fake_compositor();
        let mut ev = drag_enters(&s, DEVICE_OFFER_A, 1);
        deliver(&mut s, &server, &ev);
        s.apply(Effect::ReadEarly(1), &Machine::new()).unwrap();
        let (_, mut fds) = requests(&mut s, &server);
        let early_w = fds.remove(0);
        ev = msg(s.id.data_device, op::wl_data_device::DROP, &[]);
        deliver(&mut s, &server, &ev);
        s.start_read().unwrap();
        assert!(s.early.is_none());
        assert!(reader_gone(&early_w));
        let (reqs, fds) = requests(&mut s, &server);
        assert_eq!(reqs, [(DEVICE_OFFER_A, op::wl_data_offer::RECEIVE)]);
        assert_eq!(fds.len(), 1);
    }

    /// The early path's drop: no second receive, `finish` (copy) and
    /// `destroy` of the same offer.
    #[test]
    fn the_early_path_ends_the_drop_without_a_second_read() {
        let (mut s, server) = fake_compositor();
        let ev = drag_enters(&s, DEVICE_OFFER_A, 1);
        deliver(&mut s, &server, &ev);
        s.apply(Effect::ReadEarly(1), &Machine::new()).unwrap();
        let (_, mut fds) = requests(&mut s, &server);
        early_source_writes(&mut s, fds.remove(0));
        let ev = msg(s.id.data_device, op::wl_data_device::DROP, &[]);
        deliver(&mut s, &server, &ev);
        s.apply(Effect::EndDrop, &Machine::new()).unwrap();
        assert_eq!(
            requests(&mut s, &server).0,
            [
                (DEVICE_OFFER_A, op::wl_data_offer::FINISH),
                (DEVICE_OFFER_A, op::wl_data_offer::DESTROY),
            ]
        );
    }

    /// An early read only asks for what the offer advertised: a source
    /// that never offered `text/uri-list` is not sent a `receive` for it
    /// (it matters when two enters are handled before the first one's
    /// read is applied).
    #[test]
    fn an_early_read_needs_the_offer_to_advertise_the_list() {
        let (mut s, server) = fake_compositor();
        let ev = drag_enters_as(&s, DEVICE_OFFER_A, 1, DND_COPY, "text/plain");
        deliver(&mut s, &server, &ev);
        s.apply(Effect::ReadEarly(4), &Machine::new()).unwrap();
        assert!(s.early.is_none());
        assert_eq!(requests(&mut s, &server).0, []);
        assert_eq!(
            s.inputs.last(),
            Some(&Input::DragList(4, Err("no uri-list".into())))
        );
    }

    /// Both buffers held by the compositor: a repaint sends nothing and
    /// waits; the release brings exactly one attach and commit of the
    /// current wheel. (A real compositor releases a buffer only after
    /// the next one is on screen, so this is the everyday path.)
    #[test]
    fn a_repaint_with_both_buffers_busy_waits_for_the_release() {
        let (mut s, server) = fake_compositor();
        s.shm = Some(Shm::new().unwrap());
        s.id.surface = s.conn.new_id(Iface::Surface);
        s.id.wheel = s.conn.new_id(Iface::Surface);
        s.id.sub = s.conn.new_id(Iface::Subsurface);
        for i in 0..2 {
            s.id.bufs[i] = s.conn.new_id(Iface::Buffer);
        }
        let mut m = Machine::with_menus(empty_groups, empty_targets);
        m.on(Input::Configured(1920, 1080));
        m.on(Input::PointerAt(960.0, 540.0));
        s.render(&m).unwrap();
        // Another wheel: the second buffer.
        m.on(Input::DragEnter {
            x: 960.0,
            y: 540.0,
            mimes: vec![URI_LIST.into()],
        });
        s.render(&m).unwrap();
        assert!(s.busy[0] && s.busy[1], "{:?} {:?}", s.busy, s.shown);
        requests(&mut s, &server);
        // A third wheel while neither is released.
        m.on(Input::DragList(1, Ok(b"a".to_vec())));
        s.render(&m).unwrap();
        assert!(s.pending);
        assert_eq!(requests(&mut s, &server).0, [], "nothing sent while busy");

        let release = msg(s.id.bufs[0], op::wl_buffer::RELEASE, &[]);
        deliver_to(&mut s, &server, &release, &m);
        assert!(!s.pending);
        let (reqs, _) = requests(&mut s, &server);
        let (wheel, surface) = (s.id.wheel, s.id.surface);
        let count = |r: (u32, u16)| reqs.iter().filter(|&&x| x == r).count();
        assert_eq!(count((wheel, op::wl_surface::ATTACH)), 1, "{reqs:?}");
        assert_eq!(count((wheel, op::wl_surface::COMMIT)), 1, "{reqs:?}");
        assert_eq!(count((surface, op::wl_surface::COMMIT)), 1, "{reqs:?}");
        assert_eq!(s.shown, Some((m.generation(), m.hover())));
    }

    /// The kept offer is the one to read, never the null offer.
    #[test]
    fn the_dropped_offer_is_read_after_a_new_enter() {
        let mut o = Offers::default();
        o.add(7).unwrap();
        o.keep(7);
        // A new drag enters before the read starts; its enter retires the
        // old offer, which the keep protects.
        o.add(8).unwrap();
        assert!(!o.retire(7));
        assert_eq!(o.to_read(), Some(7));
        // Nothing dropped (or a drag without an offer): nothing to read.
        assert_eq!(Offers::default().to_read(), None);
        let mut o = Offers::default();
        o.keep(0);
        assert_eq!(o.to_read(), None);
    }

    #[test]
    fn globals_are_capped_and_versions_checked() {
        let mut g = Globals::default();
        g.add(1, "wl_compositor".into(), 3).unwrap();
        g.add(2, "wl_data_device_manager".into(), 3).unwrap();
        g.add(3, "wl_seat".into(), 9).unwrap();
        // Too old is as good as absent.
        assert_eq!(g.find("wl_compositor", 4, 4), None);
        assert_eq!(g.find("wl_compositor", 1, 4), Some((1, 3)));
        assert_eq!(g.find("wl_data_device_manager", 3, 3), Some((2, 3)));
        // Bound at most at the version we speak.
        assert_eq!(g.find("wl_seat", 1, 7), Some((3, 7)));
        assert_eq!(g.find("wp_viewporter", 1, 1), None);
        for i in 3..MAX_GLOBALS as u32 {
            g.add(10 + i, "x".into(), 1).unwrap();
        }
        assert!(g.add(9999, "y".into(), 1).is_err(), "past the cap");
        assert!(g.0.len() <= MAX_GLOBALS);
    }

    #[test]
    fn offers_are_capped() {
        let mut o = Offers::default();
        for id in 0..MAX_OFFERS as u32 {
            o.add(0xff00_0000 + id).unwrap();
        }
        assert!(o.add(0xff00_1000).is_err(), "too many live offers");
        let id = 0xff00_0000;
        for k in 0..MAX_MIMES {
            o.mime(id, format!("type/{k}")).unwrap();
        }
        assert!(
            o.mime(id, "one/more".into()).is_err(),
            "too many MIME types"
        );
        assert_eq!(o.mimes(id).len(), MAX_MIMES);
        // A retired offer frees its slot; MIME types for it are ignored.
        assert!(o.retire(id));
        o.mime(id, "late/type".into()).unwrap();
        assert!(o.mimes(id).is_empty());
        o.add(0xff00_1000).unwrap();
    }

    #[test]
    fn offers_are_retired_once_and_never_the_one_kept() {
        let mut o = Offers::default();
        o.add(7).unwrap();
        o.add(8).unwrap();
        o.mime(7, URI_LIST.into()).unwrap();
        assert_eq!(o.mimes(7), [URI_LIST]);
        // The dropped offer stays until it is released.
        o.keep(7);
        assert!(!o.retire(7));
        assert_eq!(o.mimes(7), [URI_LIST]);
        assert!(o.retire(8));
        assert!(!o.retire(8), "destroyed once");
        assert!(!o.retire(0), "no offer");
        assert_eq!(o.release(), Some(7), "released: destroy it now");
        assert!(!o.retire(7), "and only once");
    }

    /// Regression: the keep outlived the read, so a later offer that
    /// reused the dropped one's id (libwayland reuses ids) was never
    /// destroyed.
    #[test]
    fn a_reused_id_is_retired_after_release() {
        let mut o = Offers::default();
        o.add(0xff00_0001).unwrap();
        o.keep(0xff00_0001);
        assert_eq!(o.release(), Some(0xff00_0001));
        assert_eq!(o.kept(), None);
        // The server reuses the id for the next drag.
        o.add(0xff00_0001).unwrap();
        assert!(o.retire(0xff00_0001), "the new offer is destroyed");
        // Releasing with nothing kept, or a kept offer already gone, is a
        // no-op.
        assert_eq!(o.release(), None);
        o.keep(9);
        assert_eq!(o.release(), None);
    }

    /// Which startup failures close without waiting for the compositor's
    /// confirmation. Kills the two constant replacements.
    #[test]
    fn only_a_silent_compositor_skips_the_close_confirmation() {
        let e = || io::Error::other("x");
        assert!(StartupError::ConfigureTimeout.compositor_silent());
        assert!(StartupError::Protocol(e()).compositor_silent());
        assert!(StartupError::AfterOpen(e()).compositor_silent());
        assert!(!StartupError::PointerUnavailable.compositor_silent());
        assert!(!StartupError::KeyboardUnavailable.compositor_silent());
        assert!(!StartupError::Initialization(e()).compositor_silent());
    }
}
