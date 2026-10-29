//! Interfaces and opcodes. `tests/opcodes.rs` checks every opcode, and
//! which events carry fds, against the protocol XMLs in `tests/data/`.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Iface {
    Display,
    Registry,
    Callback,
    Compositor,
    Surface,
    Region,
    Shm,
    ShmPool,
    Buffer,
    Seat,
    Pointer,
    Keyboard,
    Subcompositor,
    Subsurface,
    DataDeviceManager,
    DataDevice,
    DataOffer,
    Viewporter,
    Viewport,
    Presentation,
    PresentationFeedback,
    PrimaryManager,
    PrimaryDevice,
    PrimaryOffer,
    LayerShell,
    LayerSurface,
}

impl Iface {
    pub const ALL: [Iface; 26] = [
        Iface::Display,
        Iface::Registry,
        Iface::Callback,
        Iface::Compositor,
        Iface::Surface,
        Iface::Region,
        Iface::Shm,
        Iface::ShmPool,
        Iface::Buffer,
        Iface::Seat,
        Iface::Pointer,
        Iface::Keyboard,
        Iface::Subcompositor,
        Iface::Subsurface,
        Iface::DataDeviceManager,
        Iface::DataDevice,
        Iface::DataOffer,
        Iface::Viewporter,
        Iface::Viewport,
        Iface::Presentation,
        Iface::PresentationFeedback,
        Iface::PrimaryManager,
        Iface::PrimaryDevice,
        Iface::PrimaryOffer,
        Iface::LayerShell,
        Iface::LayerSurface,
    ];

    /// The interface name in the protocol XML (and in `wl_registry.global`).
    pub const fn name(self) -> &'static str {
        match self {
            Iface::Display => "wl_display",
            Iface::Registry => "wl_registry",
            Iface::Callback => "wl_callback",
            Iface::Compositor => "wl_compositor",
            Iface::Surface => "wl_surface",
            Iface::Region => "wl_region",
            Iface::Shm => "wl_shm",
            Iface::ShmPool => "wl_shm_pool",
            Iface::Buffer => "wl_buffer",
            Iface::Seat => "wl_seat",
            Iface::Pointer => "wl_pointer",
            Iface::Keyboard => "wl_keyboard",
            Iface::Subcompositor => "wl_subcompositor",
            Iface::Subsurface => "wl_subsurface",
            Iface::DataDeviceManager => "wl_data_device_manager",
            Iface::DataDevice => "wl_data_device",
            Iface::DataOffer => "wl_data_offer",
            Iface::Viewporter => "wp_viewporter",
            Iface::Viewport => "wp_viewport",
            Iface::Presentation => "wp_presentation",
            Iface::PresentationFeedback => "wp_presentation_feedback",
            Iface::PrimaryManager => "zwp_primary_selection_device_manager_v1",
            Iface::PrimaryDevice => "zwp_primary_selection_device_v1",
            Iface::PrimaryOffer => "zwp_primary_selection_offer_v1",
            Iface::LayerShell => "zwlr_layer_shell_v1",
            Iface::LayerSurface => "zwlr_layer_surface_v1",
        }
    }
}

/// Number of fds event `op` of `iface` carries. Every such fd is taken off
/// the queue by [`crate::Conn::event`], used or not, so later fds stay
/// matched to their events. Of the interfaces here only `wl_keyboard.keymap`
/// carries one.
pub fn event_fds(iface: Iface, op: u16) -> usize {
    match (iface, op) {
        (Iface::Keyboard, op::wl_keyboard::KEYMAP) => 1,
        _ => 0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Request,
    Event,
}

/// One opcode as listed in [`op::ALL`]: interface, direction, the message
/// name in upper case, opcode.
pub type Entry = (&'static str, Dir, &'static str, u16);

macro_rules! ops {
    ($($iface:ident {
        requests { $($r:ident = $rv:literal),* $(,)? }
        events { $($e:ident = $ev:literal),* $(,)? }
    })*) => {
        /// Opcodes by interface and message name, e.g.
        /// `op::wl_surface::ATTACH`. Every request and event of the
        /// interfaces whose XML is vendored in `tests/data/`.
        pub mod op {
            use super::{Dir, Entry};
            $(pub mod $iface {
                $(pub const $r: u16 = $rv;)*
                $(pub const $e: u16 = $ev;)*
            })*
            /// The whole table, for the check against the XMLs.
            pub const ALL: &[Entry] = &[
                $($((stringify!($iface), Dir::Request, stringify!($r), $rv),)*
                  $((stringify!($iface), Dir::Event, stringify!($e), $ev),)*)*
            ];
        }
    };
}

ops! {
    wl_display {
        requests { SYNC = 0, GET_REGISTRY = 1 }
        events { ERROR = 0, DELETE_ID = 1 }
    }
    wl_registry {
        requests { BIND = 0 }
        events { GLOBAL = 0, GLOBAL_REMOVE = 1 }
    }
    wl_callback {
        requests {}
        events { DONE = 0 }
    }
    wl_compositor {
        requests { CREATE_SURFACE = 0, CREATE_REGION = 1, RELEASE = 2 }
        events {}
    }
    wl_shm_pool {
        requests { CREATE_BUFFER = 0, DESTROY = 1, RESIZE = 2 }
        events {}
    }
    wl_shm {
        requests { CREATE_POOL = 0, RELEASE = 1 }
        events { FORMAT = 0 }
    }
    wl_buffer {
        requests { DESTROY = 0 }
        events { RELEASE = 0 }
    }
    wl_data_offer {
        requests { ACCEPT = 0, RECEIVE = 1, DESTROY = 2, FINISH = 3, SET_ACTIONS = 4 }
        events { OFFER = 0, SOURCE_ACTIONS = 1, ACTION = 2 }
    }
    wl_data_device {
        requests { START_DRAG = 0, SET_SELECTION = 1, RELEASE = 2 }
        events { DATA_OFFER = 0, ENTER = 1, LEAVE = 2, MOTION = 3, DROP = 4, SELECTION = 5 }
    }
    wl_data_device_manager {
        requests { CREATE_DATA_SOURCE = 0, GET_DATA_DEVICE = 1, RELEASE = 2 }
        events {}
    }
    wl_surface {
        requests { DESTROY = 0, ATTACH = 1, DAMAGE = 2, FRAME = 3, SET_OPAQUE_REGION = 4, SET_INPUT_REGION = 5, COMMIT = 6, SET_BUFFER_TRANSFORM = 7, SET_BUFFER_SCALE = 8, DAMAGE_BUFFER = 9, OFFSET = 10, GET_RELEASE = 11 }
        events { ENTER = 0, LEAVE = 1, PREFERRED_BUFFER_SCALE = 2, PREFERRED_BUFFER_TRANSFORM = 3 }
    }
    wl_seat {
        requests { GET_POINTER = 0, GET_KEYBOARD = 1, GET_TOUCH = 2, RELEASE = 3 }
        events { CAPABILITIES = 0, NAME = 1 }
    }
    wl_pointer {
        requests { SET_CURSOR = 0, RELEASE = 1 }
        events { ENTER = 0, LEAVE = 1, MOTION = 2, BUTTON = 3, AXIS = 4, FRAME = 5, AXIS_SOURCE = 6, AXIS_STOP = 7, AXIS_DISCRETE = 8, AXIS_VALUE120 = 9, AXIS_RELATIVE_DIRECTION = 10, WARP = 11 }
    }
    wl_keyboard {
        requests { RELEASE = 0 }
        events { KEYMAP = 0, ENTER = 1, LEAVE = 2, KEY = 3, MODIFIERS = 4, REPEAT_INFO = 5 }
    }
    wl_region {
        requests { DESTROY = 0, ADD = 1, SUBTRACT = 2 }
        events {}
    }
    wl_subcompositor {
        requests { DESTROY = 0, GET_SUBSURFACE = 1 }
        events {}
    }
    wl_subsurface {
        requests { DESTROY = 0, SET_POSITION = 1, PLACE_ABOVE = 2, PLACE_BELOW = 3, SET_SYNC = 4, SET_DESYNC = 5 }
        events {}
    }
    wp_viewporter {
        requests { DESTROY = 0, GET_VIEWPORT = 1 }
        events {}
    }
    wp_viewport {
        requests { DESTROY = 0, SET_SOURCE = 1, SET_DESTINATION = 2 }
        events {}
    }
    zwlr_layer_shell_v1 {
        requests { GET_LAYER_SURFACE = 0, DESTROY = 1 }
        events {}
    }
    zwlr_layer_surface_v1 {
        requests { SET_SIZE = 0, SET_ANCHOR = 1, SET_EXCLUSIVE_ZONE = 2, SET_MARGIN = 3, SET_KEYBOARD_INTERACTIVITY = 4, GET_POPUP = 5, ACK_CONFIGURE = 6, DESTROY = 7, SET_LAYER = 8, SET_EXCLUSIVE_EDGE = 9 }
        events { CONFIGURE = 0, CLOSED = 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::Iface;

    /// Every variant has a slot in `Iface::ALL`, in order, once. The match
    /// has no wildcard: a new variant does not compile until it is placed
    /// here, and the loop then fails until it is in `ALL` (which the
    /// opcode and fd tests, and the server-id table, walk).
    #[test]
    fn all_lists_every_interface_once_in_order() {
        fn slot(i: Iface) -> usize {
            match i {
                Iface::Display => 0,
                Iface::Registry => 1,
                Iface::Callback => 2,
                Iface::Compositor => 3,
                Iface::Surface => 4,
                Iface::Region => 5,
                Iface::Shm => 6,
                Iface::ShmPool => 7,
                Iface::Buffer => 8,
                Iface::Seat => 9,
                Iface::Pointer => 10,
                Iface::Keyboard => 11,
                Iface::Subcompositor => 12,
                Iface::Subsurface => 13,
                Iface::DataDeviceManager => 14,
                Iface::DataDevice => 15,
                Iface::DataOffer => 16,
                Iface::Viewporter => 17,
                Iface::Viewport => 18,
                Iface::Presentation => 19,
                Iface::PresentationFeedback => 20,
                Iface::PrimaryManager => 21,
                Iface::PrimaryDevice => 22,
                Iface::PrimaryOffer => 23,
                Iface::LayerShell => 24,
                Iface::LayerSurface => 25,
            }
        }
        for (n, i) in Iface::ALL.iter().enumerate() {
            assert_eq!(slot(*i), n, "{i:?} is not at its slot in ALL");
        }
        // One past the last slot is not a variant: ALL is not longer.
        assert_eq!(Iface::ALL.len(), slot(Iface::LayerSurface) + 1);
    }
}
