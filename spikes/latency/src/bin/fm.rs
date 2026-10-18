//! How do files get from a file manager to us, over pure protocols?
//!
//!   fm clip  — take focus with a 1x1 surface; dump every MIME type offered on
//!              the clipboard and the primary selection, and the content of the
//!              file-list types.
//!   fm dnd   — full-screen, click-through-free overlay acting as a drop
//!              target: log when a drag enters (and whether pointer motion was
//!              needed), the offered MIME types, and the dropped payload.
//!              Exits on drop, on any click, or after 15 s. Logs to
//!              $XDG_RUNTIME_DIR/inacircle-dnd.log (it is meant to be spawned
//!              by a compositor keybind during a drag).

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_keyboard::{self, WlKeyboard},
        wl_pointer::{self, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
        wl_shm,
        wl_shm::WlShm,
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
    zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
    zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

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
    t0: u64,
    log: Vec<String>,
    size: (i32, i32),
    configured: bool,
    kbd: bool,
    offers: Vec<(WlDataOffer, Vec<String>)>,
    poffers: Vec<(ZwpPrimarySelectionOfferV1, Vec<String>)>,
    clip: Option<Option<(WlDataOffer, Vec<String>)>>,
    prim: Option<Option<(ZwpPrimarySelectionOfferV1, Vec<String>)>>,
    // dnd
    motions_before_enter: u32,
    pointer_entered: bool,
    dnd_offer: Option<(WlDataOffer, Vec<String>)>,
    dropped: bool,
    quit: bool,
}

impl State {
    fn note(&mut self, s: String) {
        let ms = (mono() - self.t0) as f64 / 1e6;
        self.log.push(format!("[{ms:8.2} ms] {s}"));
    }
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: valid 2-element array.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: fresh descriptors we own.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn read_capped(r: OwnedFd, cap: usize) -> String {
    let mut buf = Vec::new();
    std::fs::File::from(r)
        .take(cap as u64)
        .read_to_end(&mut buf)
        .ok();
    String::from_utf8_lossy(&buf)
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\0', "\\0")
}

const FILE_TYPES: [&str; 4] = [
    "text/uri-list",
    "x-special/gnome-copied-files",
    "application/x-kde-cutselection",
    "text/plain;charset=utf-8",
];

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let dnd = mode == "dnd";
    let mut st = State {
        t0: mono(),
        ..Default::default()
    };

    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut eq) = registry_queue_init::<State>(&conn).unwrap();
    let qh = eq.handle();
    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
    let seat: WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
    let ddm: WlDataDeviceManager = globals.bind(&qh, 3..=3, ()).unwrap();
    let psm: ZwpPrimarySelectionDeviceManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ()).unwrap();
    let _ptr = seat.get_pointer(&qh, ());
    let _kbd = seat.get_keyboard(&qh, ());
    let _dd = ddm.get_data_device(&seat, &qh, ());
    let _pd = psm.get_device(&seat, &qh, ());

    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Overlay,
        "inacircle-fm".into(),
        &qh,
        (),
    );
    if dnd {
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    } else {
        layer.set_size(1, 1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    }
    surface.commit();
    while !st.configured {
        eq.blocking_dispatch(&mut st).unwrap();
    }
    // 1x1 buffer; in dnd mode scaled to the whole output, faintly tinted so
    // the user sees the target is up.
    // SAFETY: static C string; checked.
    let fd = unsafe { libc::memfd_create(c"fm".as_ptr(), libc::MFD_CLOEXEC) };
    // SAFETY: fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let tint: u32 = if dnd { 0x2010_1018 } else { 0 };
    std::fs::File::from(fd.try_clone().unwrap())
        .write_all(&tint.to_ne_bytes())
        .unwrap();
    let pool = shm.create_pool(fd.as_fd(), 4, &qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, &qh, ());
    let _vp = if dnd {
        let v = viewporter.get_viewport(&surface, &qh, ());
        v.set_destination(st.size.0, st.size.1);
        Some(v)
    } else {
        None
    };
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    conn.flush().unwrap();
    st.note("mapped".into());

    if dnd {
        let deadline = mono() + 15_000_000_000;
        while !st.quit && mono() < deadline {
            conn.flush().unwrap();
            if let Some(g) = eq.prepare_read() {
                let mut pfd = libc::pollfd {
                    fd: g.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                if unsafe { libc::poll(&mut pfd, 1, 50) } > 0 {
                    g.read().ok();
                }
            }
            eq.dispatch_pending(&mut st).unwrap();
            if st.dropped {
                if let Some((o, mimes)) = st.dnd_offer.take() {
                    let payload = mimes.iter().find(|m| *m == "text/uri-list").map(|m| {
                        let (r, w) = pipe();
                        o.receive(m.clone(), w.as_fd());
                        conn.flush().unwrap();
                        drop(w);
                        read_capped(r, 2000)
                    });
                    o.finish();
                    o.destroy();
                    conn.flush().unwrap();
                    st.note(format!("payload text/uri-list = {payload:?}"));
                }
                st.quit = true;
            }
        }
        if !st.quit {
            st.note("timeout (15 s), no drop".into());
        }
        let path = format!(
            "{}/inacircle-dnd.log",
            std::env::var("XDG_RUNTIME_DIR").unwrap()
        );
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "--- dnd run").unwrap();
        for l in &st.log {
            writeln!(f, "{l}").unwrap();
        }
        return;
    }

    while !st.kbd || st.clip.is_none() || st.prim.is_none() {
        eq.blocking_dispatch(&mut st).unwrap();
    }
    let clip = st.clip.take().unwrap();
    let prim = st.prim.take().unwrap();
    println!(
        "clipboard: {}",
        clip.as_ref().map_or("EMPTY".into(), |(_, m)| m.join(" | "))
    );
    if let Some((o, mimes)) = &clip {
        for t in FILE_TYPES.iter().filter(|t| mimes.iter().any(|m| m == *t)) {
            let (r, w) = pipe();
            o.receive(t.to_string(), w.as_fd());
            conn.flush().unwrap();
            drop(w);
            println!("  {t}: {:?}", read_capped(r, 600));
        }
    }
    println!(
        "primary:   {}",
        prim.as_ref().map_or("EMPTY".into(), |(_, m)| m.join(" | "))
    );
    if let Some((o, mimes)) = &prim {
        for t in FILE_TYPES.iter().filter(|t| mimes.iter().any(|m| m == *t)) {
            let (r, w) = pipe();
            o.receive(t.to_string(), w.as_fd());
            conn.flush().unwrap();
            drop(w);
            println!("  {t}: {:?}", read_capped(r, 600));
        }
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
        match ev {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                ls.ack_configure(serial);
                st.size = (width as i32, height as i32);
                st.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => st.quit = true,
            _ => {}
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
        if let wl_keyboard::Event::Enter { .. } = ev {
            st.kbd = true;
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
        match ev {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } if !st.pointer_entered => {
                st.pointer_entered = true;
                st.note(format!(
                    "pointer enter @{surface_x:.0},{surface_y:.0} (no drag in progress)"
                ));
            }
            wl_pointer::Event::Button { .. } => {
                st.note("click: abort".into());
                st.quit = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        st: &mut Self,
        _: &WlDataDevice,
        ev: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            wl_data_device::Event::DataOffer { id } => st.offers.push((id, Vec::new())),
            wl_data_device::Event::Selection { id } => {
                st.clip = Some(id.and_then(|o| {
                    let i = st.offers.iter().position(|(x, _)| *x == o)?;
                    Some(st.offers.swap_remove(i))
                }));
            }
            wl_data_device::Event::Enter {
                serial, x, y, id, ..
            } => {
                let found = id.and_then(|o| {
                    let i = st.offers.iter().position(|(x, _)| *x == o)?;
                    Some(st.offers.swap_remove(i))
                });
                let mimes = found.as_ref().map_or(String::new(), |(_, m)| m.join(" | "));
                let motions = st.motions_before_enter;
                st.note(format!(
                    "DND ENTER @{x:.0},{y:.0} after {motions} pointer-motion events; mimes: {mimes}"
                ));
                if let Some((o, m)) = &found {
                    let want = m.iter().find(|m| *m == "text/uri-list").cloned();
                    o.accept(serial, want);
                    o.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
                }
                st.dnd_offer = found;
            }
            wl_data_device::Event::Motion { .. } => {}
            wl_data_device::Event::Leave => st.note("dnd leave".into()),
            wl_data_device::Event::Drop => {
                st.note("DROP".into());
                st.dropped = true;
            }
            _ => {}
        }
    }
    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(
        st: &mut Self,
        o: &WlDataOffer,
        ev: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            wl_data_offer::Event::Offer { mime_type } => {
                if let Some((_, m)) = st.offers.iter_mut().find(|(x, _)| x == o) {
                    m.push(mime_type);
                }
            }
            wl_data_offer::Event::SourceActions { source_actions } => {
                st.note(format!("source actions: {source_actions:?}"));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for State {
    fn event(
        st: &mut Self,
        _: &ZwpPrimarySelectionDeviceV1,
        ev: zwp_primary_selection_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            zwp_primary_selection_device_v1::Event::DataOffer { offer } => {
                st.poffers.push((offer, Vec::new()))
            }
            zwp_primary_selection_device_v1::Event::Selection { id } => {
                st.prim = Some(id.and_then(|o| {
                    let i = st.poffers.iter().position(|(x, _)| *x == o)?;
                    Some(st.poffers.swap_remove(i))
                }));
            }
            _ => {}
        }
    }
    event_created_child!(State, ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE => (ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<ZwpPrimarySelectionOfferV1, ()> for State {
    fn event(
        st: &mut Self,
        o: &ZwpPrimarySelectionOfferV1,
        ev: zwp_primary_selection_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = ev {
            if let Some((_, m)) = st.poffers.iter_mut().find(|(x, _)| x == o) {
                m.push(mime_type);
            }
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore WlDataDeviceManager);
delegate_noop!(State: ignore ZwpPrimarySelectionDeviceManagerV1);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore ZwlrLayerShellV1);
