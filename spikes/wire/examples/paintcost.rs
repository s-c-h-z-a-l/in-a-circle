//! Where the cold first paint goes: mask build vs page faults.
//! Measurement tool only; the unsafe calls are plain libc on our own fds
//! and mappings, which live until exit.
#[allow(dead_code)] // only the painting half of the module is used
#[path = "../src/wheel.rs"]
mod wheel;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use wheel::{BOX, NONE, Wheel};

fn now() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}
fn flt() -> i64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_minflt
}
fn map(size: usize, populate: bool) -> *mut u8 {
    unsafe {
        let fd = OwnedFd::from_raw_fd(libc::memfd_create(c"x".as_ptr(), libc::MFD_CLOEXEC));
        libc::ftruncate(fd.as_raw_fd(), size as i64);
        let fl = libc::MAP_SHARED | if populate { libc::MAP_POPULATE } else { 0 };
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            fl,
            fd.as_raw_fd(),
            0,
        ) as *mut u8
    }
}
fn step<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let (t, p) = (now(), flt());
    let r = f();
    println!(
        "{name:28} {:6} us  {:4} faults",
        (now() - t) / 1000,
        flt() - p
    );
    r
}
fn main() {
    let size = (BOX * BOX * 4) as usize;
    let w = step("mask build (Wheel::new)", Wheel::new);
    let p = map(size, false);
    step("paint, fresh memfd", || {
        w.paint_full(unsafe { std::slice::from_raw_parts_mut(p, size) }, NONE)
    });
    step("paint again, same pages", || {
        w.paint_full(unsafe { std::slice::from_raw_parts_mut(p, size) }, NONE)
    });
    let q = step("mmap MAP_POPULATE", || map(size, true));
    step("paint, populated memfd", || {
        w.paint_full(unsafe { std::slice::from_raw_parts_mut(q, size) }, NONE)
    });
    for i in 0..5 {
        let w2 = step(&format!("mask build #{i}"), Wheel::new);
        drop(w2);
    }
    let c = unsafe { std::slice::from_raw_parts_mut(q, size) };
    let t = now();
    for k in 0..800u32 {
        w.paint_hover(c, if k == 0 { NONE } else { (k - 1) % 8 }, k % 8);
    }
    println!(
        "{:28} {:6.1} us avg",
        "hover repaint",
        (now() - t) as f64 / 800.0 / 1000.0
    );
}
