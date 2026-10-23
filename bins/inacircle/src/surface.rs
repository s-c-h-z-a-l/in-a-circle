//! The wheel's Wayland side: registry, layer surface, viewporter, the
//! wheel subsurface with an empty input region, shm buffers, seat input and
//! drag and drop. One `poll` loop on the Wayland fd (and the drop pipe while
//! one is read) drives the `Machine` and carries out its effects.
//!
//! Layout (the spike's, proven on niri): a full-screen overlay layer surface
//! with a 1x1 transparent buffer stretched by `wp_viewporter` takes every
//! pointer and drag event; the wheel sits in a subsurface whose input region
//! is empty. Buffer 0 is painted while the compositor configures us, buffer
//! 1 after the map while the pointer position is on its way; only the
//! subsurface position waits for it.
//!
//! Globals are bound at the version our requests need, or treated as
//! absent: a `wl_data_device_manager` older than v3 (no `set_actions`)
//! means no drop target, and the wheel says `Nothing to do`; the others are
//! required. What the compositor makes us keep is capped (globals, live
//! data offers, MIME types per offer); past a cap it is broken. Every data
//! offer is destroyed once we are done with it, except the dropped one
//! until its data is read.
//!
//! Every error ends the wheel: nothing here retries. Returning drops the
//! connection, which removes the surface; the caller then frees the lock.

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ui::damage::Screen;
use ui::wheel::{BOX, Rect};
use wire::{Arg::*, Conn, DISPLAY, Event, Iface, op};

use crate::measure::Meter;
use crate::uri::MAX_BYTES;
use crate::wheel::{Effect, Input, Machine, STAGE, URI_LIST};

const FRAME: usize = (BOX * BOX * 4) as usize;
/// Both wheel buffers, then the 1x1 background pixel.
const POOL: usize = 2 * FRAME + 4;
/// The one-line switch for prefaulting the mapping: the spike measured
/// `MAP_POPULATE` no cheaper than the faults it saves
/// (docs/benchmarks/wire-protocol.md); to be re-measured on the real path.
const MAP_FLAGS: libc::c_int = libc::MAP_SHARED; // | libc::MAP_POPULATE
/// Longest wait for the drag source to write and close the URI list.
const READ_DROP: Duration = Duration::from_millis(500);
/// One read from the drop pipe.
const READ_CHUNK: usize = 4096;
/// The surface gives up this much before the `alarm` would kill us, so a
/// timeout normally ends in an orderly close.
const SOFT_MARGIN: Duration = Duration::from_millis(500);

const DND_COPY: u32 = 1;
const LAYER_OVERLAY: u32 = 3;
const ANCHOR_ALL: u32 = 15;
const KEYBOARD_EXCLUSIVE: u32 = 1;
const ARGB8888: u32 = 0;
const PRESSED: u32 = 1;
const SEAT_POINTER: u32 = 1;
const SEAT_KEYBOARD: u32 = 2;

/// The files and preset chosen, or `None` if the wheel closed without one.
pub type Chosen = Option<(Vec<PathBuf>, String)>;

/// Shows the wheel and runs `m` until it closes or chooses.
///
/// With a `meter` (the measurement switch) the surface feeds it and hands it
/// back, on every path.
pub fn run(m: &mut Machine, meter: &mut Option<Meter>) -> io::Result<Chosen> {
    let mut s = Surface::connect()?;
    s.meter = meter.take();
    let r = s.setup(m).and_then(|()| s.event_loop(m));
    *meter = s.meter.take();
    r
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
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, unmapped once.
        unsafe { libc::munmap(self.ptr.cast(), POOL) };
    }
}

/// A drop being read: the pipe, the offer and the action negotiated for it
/// at the drop, what came so far, the read's deadline.
struct Reading {
    fd: OwnedFd,
    offer: u32,
    action: u32,
    data: Vec<u8>,
    until: Instant,
}

impl Reading {
    fn new(fd: OwnedFd, offer: u32, action: u32, until: Instant) -> Reading {
        Reading {
            fd,
            offer,
            action,
            // Room for the limit and one more read: never regrown (a
            // doubling would hold two copies). Pages are touched only as
            // data arrives.
            data: Vec::with_capacity(MAX_BYTES + READ_CHUNK),
            until,
        }
    }

    /// Reads what the (non-blocking) pipe has: the data at EOF, an error
    /// past `MAX_BYTES`, on a read error or once `now` reaches the
    /// deadline with the pipe still open; `None` to wait for more.
    fn pump(&mut self, now: Instant) -> Option<Result<Vec<u8>, String>> {
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // SAFETY: buf is valid for buf.len() bytes.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            match n {
                0 => return Some(Ok(std::mem::take(&mut self.data))),
                n if n > 0 => {
                    self.data.extend_from_slice(&buf[..n as usize]);
                    if self.data.len() > MAX_BYTES {
                        return Some(Err(format!("URI list over {MAX_BYTES} bytes")));
                    }
                }
                _ => {
                    let e = io::Error::last_os_error();
                    match e.kind() {
                        io::ErrorKind::Interrupted => {}
                        io::ErrorKind::WouldBlock if now < self.until => return None,
                        io::ErrorKind::WouldBlock => return Some(Err("URI list timed out".into())),
                        _ => return Some(Err(e.to_string())),
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
    /// Soft deadline of the current stage.
    deadline: Instant,
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
    reading: Option<Reading>,
    busy: [bool; 2],
    painted: [Painted; 2],
    /// What a buffer was repainted with since its last commit, ahead of
    /// time (`prepaint`); its next commit damages this too.
    prepainted: [Rect; 2],
    /// What the screen shows; `None` before the wheel's first commit.
    shown: Painted,
    screen: Screen,
    /// A render waits for a free buffer.
    pending: bool,
    inputs: Vec<Input>,
    /// The measurement switch's meter; `None` unless it is on.
    meter: Option<Meter>,
}

impl Surface {
    fn connect() -> io::Result<Surface> {
        let conn = Conn::connect()
            .map_err(|e| io::Error::new(e.kind(), format!("cannot reach the compositor: {e}")))?;
        Ok(Surface::new(conn))
    }

    fn new(conn: Conn) -> Surface {
        Surface {
            conn,
            id: Ids::default(),
            shm: None,
            deadline: Instant::now() + STAGE.saturating_sub(SOFT_MARGIN),
            globals: Globals::default(),
            synced: false,
            size: None,
            offers: Offers::default(),
            dnd: None,
            dnd_action: 0,
            drop_action: 0,
            reading: None,
            busy: [false; 2],
            painted: [None; 2],
            prepainted: [Rect::EMPTY; 2],
            shown: None,
            screen: Screen::new(),
            pending: false,
            inputs: Vec::new(),
            meter: None,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.flush(self.deadline)
    }

    /// Handles every event read so far.
    fn dispatch(&mut self, m: &Machine) -> io::Result<()> {
        while let Some(e) = self.conn.event()? {
            self.handle(e, m)?;
        }
        Ok(())
    }

    /// Reads and dispatches until `done`, within the stage's deadline.
    fn wait(&mut self, m: &Machine, done: fn(&Surface) -> bool) -> io::Result<()> {
        self.flush()?;
        self.dispatch(m)?;
        while !done(self) {
            if !self.conn.read(self.deadline)? {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "compositor did not answer",
                ));
            }
            self.dispatch(m)?;
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

    fn setup(&mut self, m: &mut Machine) -> io::Result<()> {
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
        self.flush()?;

        // While the compositor configures: the wheel surface and buffer 0.
        self.build_wheel_surface()?;
        let generation = m.generation();
        let shm = self.shm.as_mut().expect("built above");
        m.wheel().paint_full(shm.canvas(0), None);
        self.painted[0] = Some((generation, None));

        // The configure is acked and the overlay mapped in `handle`; its
        // inputs wait in the queue for the event loop.
        self.wait(m, |s| s.size.is_some())?;
        self.flush()?;
        // While the pointer position is on its way: buffer 1.
        if self.painted[1].is_none() && !self.busy[1] {
            let shm = self.shm.as_mut().expect("built above");
            m.wheel().paint_full(shm.canvas(1), m.hover());
            self.painted[1] = Some((m.generation(), m.hover()));
        }
        Ok(())
    }

    fn build_wheel_surface(&mut self) -> io::Result<()> {
        let shm = Shm::new()?;
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
                self.synced = true;
                None
            }
            (Iface::Seat, op::wl_seat::CAPABILITIES) => {
                let caps = e.u32()?;
                let (id, c) = (&mut self.id, &mut self.conn);
                if caps & SEAT_POINTER != 0 && id.pointer == 0 {
                    id.pointer = c.new_id(Iface::Pointer);
                    c.req(id.seat, op::wl_seat::GET_POINTER, &[U(id.pointer)]);
                }
                if caps & SEAT_KEYBOARD != 0 && id.keyboard == 0 {
                    id.keyboard = c.new_id(Iface::Keyboard);
                    c.req(id.seat, op::wl_seat::GET_KEYBOARD, &[U(id.keyboard)]);
                }
                None
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
            (Iface::Pointer, op::wl_pointer::ENTER) => {
                let (_serial, _surface) = (e.u32()?, e.u32()?);
                Some(Input::PointerAt(e.fixed()?, e.fixed()?))
            }
            (Iface::Pointer, op::wl_pointer::MOTION) => {
                let _time = e.u32()?;
                Some(Input::PointerAt(e.fixed()?, e.fixed()?))
            }
            (Iface::Pointer, op::wl_pointer::BUTTON) => {
                let (_serial, _time, button, state) = (e.u32()?, e.u32()?, e.u32()?, e.u32()?);
                (state == PRESSED).then_some(Input::Click(button))
            }

            (Iface::Keyboard, op::wl_keyboard::KEYMAP) => {
                // Not needed, but taken so the fd is closed now.
                drop(self.conn.take_fd());
                None
            }
            (Iface::Keyboard, op::wl_keyboard::KEY) => {
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
            for fx in m.on(i) {
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
            // TODO(task 3): receive the list before the drop. Until then no
            // `DragList` comes, so the machine takes its `Drop here` fallback.
            Effect::ReadEarly => {}
            Effect::ReadDrop => self.start_read()?,
            Effect::Arm(d) => {
                self.deadline = Instant::now() + d.saturating_sub(SOFT_MARGIN);
                // SAFETY: alarm(2) has no memory effects; see main.
                unsafe { libc::alarm(d.as_secs().max(1) as u32) };
            }
            Effect::Close => return Ok(Some(None)),
            Effect::Run(files, preset) => {
                if let Some(meter) = &mut self.meter {
                    meter.chose(crate::cpu_us(libc::RUSAGE_SELF));
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
        self.reading = Some(Reading::new(
            r,
            offer,
            self.drop_action,
            Instant::now() + READ_DROP,
        ));
        Ok(())
    }

    /// Reads what the pipe has; queues `DropData` at EOF, past the size
    /// limit, on error or past the read's deadline.
    fn pump_read(&mut self) {
        let Some(rd) = &mut self.reading else {
            return;
        };
        if let Some(r) = rd.pump(Instant::now()) {
            let (offer, action) = (rd.offer, rd.action);
            self.reading = None;
            // `finish` is a protocol error unless copy was negotiated
            // (niri drops even without it).
            if r.is_ok() && action == DND_COPY {
                self.conn.req(offer, op::wl_data_offer::FINISH, &[]);
            }
            // The keep ends with the read, so a reused id is destroyed later.
            if self.offers.release() == Some(offer) {
                self.conn.req(offer, op::wl_data_offer::DESTROY, &[]);
            }
            if let Some(meter) = &mut self.meter {
                meter.dropped(crate::cpu_us(libc::RUSAGE_SELF));
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
        let want = Some((m.generation(), m.hover()));
        let first = self.shown.is_none();
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
        let shm = self.shm.as_mut().expect("built in setup");
        let canvas = shm.canvas(i);
        let painted = match self.painted[i] {
            Some((g, old)) if g == m.generation() => m.wheel().paint_hover(canvas, old, m.hover()),
            _ => m.wheel().paint_full(canvas, m.hover()),
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
            // After the timings are taken: reading smaps costs tens of µs.
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
    fn prepaint(&mut self, m: &Machine) {
        if self.shown.is_none() {
            return;
        }
        for i in to_prepaint(&self.painted, &self.busy, m.generation()) {
            {
                let shm = self.shm.as_mut().expect("built in setup");
                let r = m.wheel().paint_full(shm.canvas(i), m.hover());
                self.painted[i] = Some((m.generation(), m.hover()));
                self.prepainted[i] = self.prepainted[i].union(r);
            }
        }
    }

    fn event_loop(&mut self, m: &mut Machine) -> io::Result<Chosen> {
        loop {
            self.dispatch(m)?;
            self.pump_read();
            if let Some(end) = self.feed(m)? {
                return Ok(end);
            }
            // Inputs queued by an effect (a failed read) go round again.
            if !self.inputs.is_empty() {
                continue;
            }
            self.flush()?;
            self.prepaint(m);
            let now = Instant::now();
            if now >= self.deadline {
                self.inputs.push(Input::Deadline);
                continue;
            }
            let until = self
                .reading
                .as_ref()
                .map_or(self.deadline, |r| r.until.min(self.deadline));
            let mut pfds = [
                pollfd(self.conn.as_raw_fd()),
                pollfd(self.reading.as_ref().map_or(-1, |r| r.fd.as_raw_fd())),
            ];
            let left = until.saturating_duration_since(now);
            let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
            // SAFETY: two valid pollfds (a negative fd is ignored).
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, ms) };
            if r >= 0
                && let Some(meter) = &mut self.meter
                && idle_wakeup(
                    pfds.iter().any(|p| p.revents != 0),
                    Instant::now(),
                    self.deadline,
                    self.reading.as_ref().map(|r| r.until),
                )
            {
                meter.idle_wakeup();
            }
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if pfds[0].revents != 0 {
                // Readable (or hung up, which the read reports).
                self.conn
                    .read(until.max(Instant::now() + Duration::from_millis(1)))?;
            }
        }
    }
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
        Reading::new(r, 7, DND_COPY, Instant::now() + READ_DROP)
    }

    /// Whatever the data, stage 2 has nothing: only the read's result
    /// matters here.
    fn empty_groups(_: &[u8]) -> GroupMenu {
        GroupMenu {
            paths: Vec::new(),
            slices: Default::default(),
            hub: NOTHING.into(),
            tally: Default::default(),
        }
    }

    fn empty_targets(_: &GroupMenu, _: &str) -> Menu {
        Menu {
            paths: Vec::new(),
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
        assert!(r.as_ref().is_err_and(|e| e.contains("timed out")), "{r:?}");
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
        assert!(r.as_ref().is_err_and(|e| e.contains("bytes")), "{r:?}");
        assert_eq!(dropped().on(Input::DropData(r)), [Effect::Close]);
    }

    /// A `poll` that returns at one of our own deadlines (the stage's, or
    /// the drop read's) is not an idle wakeup; one with nothing ready
    /// before them is.
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
        let dd = s.id.data_device;
        let at = 100 * 256; // wl_fixed 100.0
        let mut m = msg(dd, op::wl_data_device::DATA_OFFER, &[offer]);
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
        // SAFETY: writing a valid buffer to a valid fd.
        let n = unsafe { libc::write(server.as_raw_fd(), events.as_ptr().cast(), events.len()) };
        assert_eq!(n as usize, events.len());
        assert!(
            s.conn
                .read(Instant::now() + Duration::from_secs(1))
                .unwrap()
        );
        s.dispatch(&Machine::new()).unwrap();
    }

    /// The requests the surface sent so far, as (object, opcode), and the
    /// fds that came with them.
    fn requests(s: &mut Surface, server: &OwnedFd) -> (Vec<(u32, u16)>, Vec<OwnedFd>) {
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
}
