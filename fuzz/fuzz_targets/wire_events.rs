#![no_main]
//! The Wayland event framing and argument readers on arbitrary bytes: no
//! panic, no hang. The input is whatever the compositor writes to the
//! socket. The seeds are the messages the fake compositor sends, laid out for
//! the object ids `wire::fuzz_events` registers.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    wire::fuzz_events(data);
});
