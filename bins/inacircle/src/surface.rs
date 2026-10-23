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
pub fn run(m: &mut Machine) -> io::Result<Chosen> {
    let mut s = Surface::connect()?;
    s.setup(m)?;
    s.event_loop(m)
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

/// A drop being read: the pipe, what came so far, the read's deadline.
struct Reading {
    fd: OwnedFd,
    offer: u32,
    data: Vec<u8>,
    until: Instant,
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
    /// The offer of the drop, kept alive until its data is read.
    dropped: Option<u32>,
    /// The drag over us: offer id (0: none offered) and enter serial.
    dnd: Option<(u32, u32)>,
    /// The action last negotiated for the drag's offer.
    dnd_action: u32,
    reading: Option<Reading>,
    busy: [bool; 2],
    painted: [Painted; 2],
    /// What the screen shows; `None` before the wheel's first commit.
    shown: Painted,
    screen: Screen,
    /// A render waits for a free buffer.
    pending: bool,
    inputs: Vec<Input>,
}

impl Surface {
    fn connect() -> io::Result<Surface> {
        Ok(Surface {
            conn: Conn::connect().map_err(|e| {
                io::Error::new(e.kind(), format!("cannot reach the compositor: {e}"))
            })?,
            id: Ids::default(),
            shm: None,
            deadline: Instant::now() + STAGE.saturating_sub(SOFT_MARGIN),
            globals: Globals::default(),
            synced: false,
            size: None,
            offers: Offers::default(),
            dropped: None,
            dnd: None,
            dnd_action: 0,
            reading: None,
            busy: [false; 2],
            painted: [None; 2],
            shown: None,
            screen: Screen::new(),
            pending: false,
            inputs: Vec::new(),
        })
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
                    && self.dropped != Some(offer)
                {
                    self.retire(offer);
                    self.dnd = None;
                }
                Some(Input::DragLeave)
            }
            (Iface::DataDevice, op::wl_data_device::DROP) => {
                self.dropped = self.dnd.map(|d| d.0);
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
            Effect::ReadDrop => self.start_read()?,
            Effect::Arm(d) => {
                self.deadline = Instant::now() + d.saturating_sub(SOFT_MARGIN);
                // SAFETY: alarm(2) has no memory effects; see main.
                unsafe { libc::alarm(d.as_secs().max(1) as u32) };
            }
            Effect::Close => return Ok(Some(None)),
            Effect::Run(files, preset) => return Ok(Some(Some((files, preset)))),
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
        if self.offers.retire(offer, self.dropped) {
            self.conn.req(offer, op::wl_data_offer::DESTROY, &[]);
        }
    }

    fn start_read(&mut self) -> io::Result<()> {
        let offer = match self.dnd {
            Some((offer, _)) if offer != 0 => offer,
            _ => {
                self.inputs.push(Input::DropData(Err("no offer".into())));
                return Ok(());
            }
        };
        let mut fds = [0; 2];
        // SAFETY: a valid 2-element array; checked.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh descriptors we own.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // Only our end is non-blocking; the source writes as it likes.
        // SAFETY: fcntl on a valid fd we own.
        unsafe {
            let fl = libc::fcntl(r.as_raw_fd(), libc::F_GETFL);
            if fl < 0 || libc::fcntl(r.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        self.conn.req(
            offer,
            op::wl_data_offer::RECEIVE,
            &[S(URI_LIST), Fd(w.as_fd())],
        );
        self.flush()?;
        // Our copy of the write end must go, or the read never sees EOF.
        drop(w);
        self.reading = Some(Reading {
            fd: r,
            offer,
            data: Vec::new(),
            until: Instant::now() + READ_DROP,
        });
        Ok(())
    }

    /// Reads what the pipe has; queues `DropData` at EOF, past the size
    /// limit, on error or past the read's deadline.
    fn pump_read(&mut self) {
        let Some(rd) = &mut self.reading else {
            return;
        };
        let mut buf = [0u8; 4096];
        let result = loop {
            // SAFETY: buf is valid for buf.len() bytes.
            let n = unsafe { libc::read(rd.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            match n {
                0 => break Some(Ok(std::mem::take(&mut rd.data))),
                n if n > 0 => {
                    rd.data.extend_from_slice(&buf[..n as usize]);
                    if rd.data.len() > MAX_BYTES {
                        break Some(Err(format!("URI list over {MAX_BYTES} bytes")));
                    }
                }
                _ => {
                    let e = io::Error::last_os_error();
                    match e.kind() {
                        io::ErrorKind::Interrupted => {}
                        io::ErrorKind::WouldBlock if Instant::now() < rd.until => break None,
                        io::ErrorKind::WouldBlock => break Some(Err("URI list timed out".into())),
                        _ => break Some(Err(e.to_string())),
                    }
                }
            }
        };
        if let Some(r) = result {
            let offer = rd.offer;
            self.reading = None;
            // `finish` is a protocol error unless copy was negotiated
            // (niri drops even without it).
            if r.is_ok() && self.dnd_action == DND_COPY && self.dnd.is_some_and(|d| d.0 == offer) {
                self.conn.req(offer, op::wl_data_offer::FINISH, &[]);
            }
            if self.offers.retire(offer, None) {
                self.conn.req(offer, op::wl_data_offer::DESTROY, &[]);
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
        let shm = self.shm.as_mut().expect("built in setup");
        let canvas = shm.canvas(i);
        let painted = match self.painted[i] {
            Some((g, old)) if g == m.generation() => m.wheel().paint_hover(canvas, old, m.hover()),
            _ => m.wheel().paint_full(canvas, m.hover()),
        };
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
        self.shown = want;
        Ok(())
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

/// Live server-created data offers and their MIME types.
#[derive(Default)]
struct Offers(Vec<(u32, Vec<String>)>);

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

    /// Forgets offer `id` unless it is `keep`; true if it was alive and
    /// must now be destroyed (so it is destroyed exactly once).
    fn retire(&mut self, id: u32, keep: Option<u32>) -> bool {
        if keep == Some(id) {
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
        assert!(o.retire(id, None));
        o.mime(id, "late/type".into()).unwrap();
        assert!(o.mimes(id).is_empty());
        o.add(0xff00_1000).unwrap();
    }

    #[test]
    fn offers_are_retired_once_and_never_the_one_kept() {
        let mut o = Offers::default();
        o.add(7).unwrap();
        o.mime(7, URI_LIST.into()).unwrap();
        assert_eq!(o.mimes(7), [URI_LIST]);
        // The offer being read stays.
        assert!(!o.retire(7, Some(7)));
        assert_eq!(o.mimes(7), [URI_LIST]);
        assert!(o.retire(7, Some(8)));
        assert!(!o.retire(7, None), "destroyed once");
        assert!(!o.retire(0, None), "no offer");
    }
}
