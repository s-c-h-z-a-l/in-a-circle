#![no_main]
//! `dbus::message::decode` on arbitrary bytes: whatever a session-bus peer
//! (or something impersonating the notification server) sends.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The connection reads the fixed 16-byte prefix, asks `frame_len` how
    // much more to read, then hands exactly that many bytes to `decode`.
    // Drive both the same way; `decode` alone also checks the length itself.
    if let Some(head) = data.get(..16).and_then(|h| <&[u8; 16]>::try_from(h).ok()) {
        let _ = dbus::message::frame_len(head);
    }
    let _ = dbus::message::decode(data);
});
