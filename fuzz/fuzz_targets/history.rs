#![no_main]
//! The history reader behind `undo` on arbitrary bytes: no panic, no hang.
//! The history is a log on the user's disk, appended by every run.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    engine::journal::fuzz_history(data);
});
