//! Context-detection cost, pure Wayland protocols: map a 1x1 layer surface
//! with exclusive keyboard focus, then read what the compositor offers on
//! focus: clipboard (wl_data_device), primary selection (zwp v1), and the
//! activated toplevel (wlr-foreign-toplevel). Prints one `M ctx` line.
//!
//! Usage: ctx [--no-toplevel]

use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::WlDataDeviceManager,
        wl_data_offer::{self, WlDataOffer},
        wl_keyboard::{self, WlKeyboard},
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
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, KeyboardInteractivity, ZwlrLayerSurfaceV1},
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
struct Toplevel {
    app_id: String,
    activated: bool,
}

#[derive(Default)]
struct State {
    configured: bool,
    kbd_enter: u64,
    /// Offers being described (mime types arrive before the selection event).
    clip_mimes: Vec<(WlDataOffer, Vec<String>)>,
    prim_mimes: Vec<(ZwpPrimarySelectionOfferV1, Vec<String>)>,
    clip: Option<(WlDataOffer, Vec<String>, u64)>,
    clip_seen: bool,
    prim: Option<(ZwpPrimarySelectionOfferV1, Vec<String>, u64)>,
    prim_seen: bool,
    toplevels: Vec<(ZwlrForeignToplevelHandleV1, Toplevel)>,
}

const PREFERRED: [&str; 4] = [
    "text/uri-list",
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
];

fn pick(mimes: &[String]) -> Option<&str> {
    PREFERRED
        .iter()
        .copied()
        .find(|p| mimes.iter().any(|m| m == p))
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: valid 2-element array.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: fresh descriptors we own.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn read_all(r: OwnedFd) -> usize {
    let mut f = std::fs::File::from(r);
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    buf.len()
}

fn main() {
    let t_main = mono();
    let toplevel = !std::env::args().any(|a| a == "--no-toplevel");
    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut eq) = registry_queue_init::<State>(&conn).unwrap();
    let qh = eq.handle();
    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
    let seat: WlSeat = globals.bind(&qh, 1..=9, ()).unwrap();
    let ddm: WlDataDeviceManager = globals.bind(&qh, 3..=3, ()).unwrap();
    let psm: Option<ZwpPrimarySelectionDeviceManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let ftm: Option<ZwlrForeignToplevelManagerV1> = if toplevel {
        globals.bind(&qh, 1..=3, ()).ok()
    } else {
        None
    };
    let _kbd = seat.get_keyboard(&qh, ());
    let _dd = ddm.get_data_device(&seat, &qh, ());
    let _pd = psm.as_ref().map(|m| m.get_device(&seat, &qh, ()));
    let t_registry = mono();

    // Foreign toplevel: initial state arrives right after bind; a roundtrip
    // guarantees we've seen all of it.
    let mut st = State::default();
    let t_tl = if ftm.is_some() {
        eq.roundtrip(&mut st).unwrap();
        mono()
    } else {
        0
    };
    let active = st
        .toplevels
        .iter()
        .find(|(_, t)| t.activated)
        .map_or("none".to_string(), |(_, t)| t.app_id.clone());

    // 1x1 transparent surface with exclusive keyboard focus.
    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Overlay,
        "inacircle-ctx".into(),
        &qh,
        (),
    );
    layer.set_size(1, 1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    surface.commit();
    // SAFETY: static C string; result checked.
    let fd = unsafe { libc::memfd_create(c"ctx".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: valid fd.
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 4) }, 0);
    let pool = shm.create_pool(fd.as_fd(), 4, &qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, &qh, ());
    while !st.configured {
        eq.blocking_dispatch(&mut st).unwrap();
    }
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    let t_commit = mono();

    // Wait for focus and both selection events (or their absence: the
    // compositor sends selection(None) too).
    while st.kbd_enter == 0 || !st.clip_seen || (psm.is_some() && !st.prim_seen) {
        eq.blocking_dispatch(&mut st).unwrap();
    }
    let t_offers = mono();

    let fetch = |name: &str, recv: &dyn Fn(&str, &OwnedFd), mimes: &[String]| -> String {
        let Some(m) = pick(mimes) else {
            return format!("{name}=none");
        };
        let t = mono();
        let (r, w) = pipe();
        recv(m, &w);
        drop(w);
        conn.flush().unwrap();
        let n = read_all(r);
        format!(
            "{name}_mime={m} {name}_bytes={n} {name}_read_us={}",
            (mono() - t) / 1000
        )
    };
    let clip = match &st.clip {
        Some((o, mimes, _)) => fetch("clip", &|m, w| o.receive(m.into(), w.as_fd()), mimes),
        None => "clip=empty".into(),
    };
    let prim = match &st.prim {
        Some((o, mimes, _)) => fetch("prim", &|m, w| o.receive(m.into(), w.as_fd()), mimes),
        None => "prim=empty".into(),
    };
    let t_end = mono();

    let us = |a: u64, b: u64| {
        if a == 0 || b == 0 {
            -1
        } else {
            (b as i64 - a as i64) / 1000
        }
    };
    println!(
        "M ctx registry_us={} toplevels={} toplevel_us={} active={active} configure_to_commit_us={} \
         commit_to_kbd_enter_us={} commit_to_offers_us={} {clip} {prim} main_to_end_us={}",
        us(t_main, t_registry),
        st.toplevels.len(),
        us(t_registry, t_tl),
        us(t_tl.max(t_registry), t_commit),
        us(t_commit, st.kbd_enter),
        us(t_commit, t_offers),
        us(t_main, t_end),
    );
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
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = ev {
            ls.ack_configure(serial);
            st.configured = true;
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
            && st.kbd_enter == 0
        {
            st.kbd_enter = mono();
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
            wl_data_device::Event::DataOffer { id } => st.clip_mimes.push((id, Vec::new())),
            wl_data_device::Event::Selection { id } => {
                st.clip_seen = true;
                st.clip = id.and_then(|o| {
                    let i = st.clip_mimes.iter().position(|(x, _)| *x == o)?;
                    let (o, m) = st.clip_mimes.swap_remove(i);
                    Some((o, m, mono()))
                });
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
        if let wl_data_offer::Event::Offer { mime_type } = ev
            && let Some((_, m)) = st.clip_mimes.iter_mut().find(|(x, _)| x == o)
        {
            m.push(mime_type);
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
                st.prim_mimes.push((offer, Vec::new()))
            }
            zwp_primary_selection_device_v1::Event::Selection { id } => {
                st.prim_seen = true;
                st.prim = id.and_then(|o| {
                    let i = st.prim_mimes.iter().position(|(x, _)| *x == o)?;
                    let (o, m) = st.prim_mimes.swap_remove(i);
                    Some((o, m, mono()))
                });
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
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = ev
            && let Some((_, m)) = st.prim_mimes.iter_mut().find(|(x, _)| x == o)
        {
            m.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        st: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        ev: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = ev {
            st.toplevels.push((toplevel, Toplevel::default()));
        }
    }
    event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        st: &mut Self,
        h: &ZwlrForeignToplevelHandleV1,
        ev: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some((_, t)) = st.toplevels.iter_mut().find(|(x, _)| x == h) else {
            return;
        };
        match ev {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => t.app_id = app_id,
            zwlr_foreign_toplevel_handle_v1::Event::State { state } => {
                // Array of u32 (native endian); 2 = activated.
                t.activated = state
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == 2);
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
delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore WlDataDeviceManager);
delegate_noop!(State: ignore ZwpPrimarySelectionDeviceManagerV1);
delegate_noop!(State: ignore ZwlrLayerShellV1);
