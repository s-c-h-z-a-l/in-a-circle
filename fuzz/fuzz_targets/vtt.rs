#![no_main]
//! `subtitles::read_vtt` on arbitrary bytes, with the checks every subtitle
//! reader shares (see `subtitles/common.rs`): the verdict is stable, an
//! `Ok` is usable and sorted, its cues round-trip through the SRT and
//! WebVTT writers, and each error names what it says. The input is read as
//! WebVTT whatever it is, so a malformed file reaches the reader's rules.

use libfuzzer_sys::fuzz_target;

#[path = "subtitles/common.rs"]
mod common;

fuzz_target!(|data: &[u8]| common::check(data, subtitles::read_vtt));
