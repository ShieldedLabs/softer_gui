//! The `sys` module for Cosmopolitan builds (`--cfg cosmo`): the same wrappers as
//! sys.rs, but every OS call goes through cosmo's libc instead of a raw Linux
//! syscall. That is the whole point of an APE: cosmo picks the syscall numbers,
//! struct layouts and constants at LOAD time for whatever kernel it lands on,
//! so a raw `syscall` instruction with Linux numbers would be exactly wrong on
//! macOS. Constants that differ across hosts are cosmo's own `extern const`
//! symbols, read at run time; the few that are the same everywhere are literals.

#![allow(dead_code, non_upper_case_globals)]

use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::AtomicU32;

pub type Fd = i32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timespec { pub sec: i64, pub nsec: i64 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PollFd { pub fd: i32, pub events: i16, pub revents: i16 }

#[repr(C)]
pub struct IoVec { pub base: *mut u8, pub len: usize }

/// Cosmo's msghdr is the Linux layout; cmsghdr likewise (len/level/type + data).
#[repr(C)]
pub struct MsgHdr {
    pub name: *mut u8, pub namelen: u32,
    pub iov: *mut IoVec, pub iovlen: usize,
    pub control: *mut u8, pub controllen: usize,
    pub flags: i32,
}

#[repr(C)]
pub struct SockAddrUn { pub family: u16, pub path: [u8; 108] }

mod c {
    use super::*;
    unsafe extern "C" {
    pub fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    pub fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    pub fn close(fd: c_int) -> c_int;
    pub fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    pub fn poll(fds: *mut PollFd, n: u64, timeout_ms: c_int) -> c_int;
    pub fn mmap(addr: *mut c_void, len: u64, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: u64) -> c_int;
    pub fn ftruncate(fd: c_int, len: i64) -> c_int;
    pub fn memfd_create(name: *const c_char, flags: u32) -> c_int;
    pub fn ioctl(fd: c_int, req: u64, ...) -> c_int;
    pub fn socket(domain: c_int, ty: c_int, proto: c_int) -> c_int;
    pub fn connect(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    pub fn sendmsg(fd: c_int, msg: *const MsgHdr, flags: c_int) -> isize;
    pub fn recvmsg(fd: c_int, msg: *mut MsgHdr, flags: c_int) -> isize;
    pub fn clock_gettime(clock: c_int, ts: *mut Timespec) -> c_int;
    pub fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> c_int;
    pub fn __errno_location() -> *mut c_int;
    pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;

    // Load-time constants. Names are cosmo's; values are the running host's.
    pub static SOCK_CLOEXEC: c_int;
    pub static SOL_SOCKET: c_int;
    pub static MSG_NOSIGNAL: c_int;
    pub static MSG_DONTWAIT: c_int;
    pub static MAP_ANONYMOUS: c_int;
    pub static CLOCK_MONOTONIC: c_int;
    pub static POLLIN: i16;
    pub static POLLERR: i16;
    pub static POLLHUP: i16;
    pub static O_CLOEXEC: u32;
    pub static EINTR: c_int;
    pub static F_SETFD: c_int;
    pub static EAGAIN: c_int;
}
}

// Same on every host cosmo supports (cosmo defines these as literals).
pub const AF_UNIX: usize = 1;
pub const SOCK_STREAM: usize = 1;
pub const SCM_RIGHTS: i32 = 1;
pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const MAP_SHARED: usize = 1;
pub const MAP_PRIVATE: usize = 2;
pub const MFD_CLOEXEC: usize = 1;

/// The Linux-numbered sentinels the callers compare against; cosmo's errno is mapped onto them.
pub const EINTR_RET: isize = -4;
pub const EAGAIN_RET: isize = -11;
pub const EINTR: isize = EINTR_RET;
pub const EAGAIN: isize = EAGAIN_RET;

fn errno() -> c_int { unsafe { *c::__errno_location() } }
/// Turn a failed libc call into the negative-errno convention sys.rs callers expect,
/// with EINTR/EAGAIN normalized to Linux's numbers so the callers' comparisons hold.
fn neg(r: isize) -> isize {
    if r >= 0 { return r; }
    let e = errno();
    unsafe {
        if e == c::EINTR { EINTR_RET } else if e == c::EAGAIN { EAGAIN_RET } else { -(e as isize).max(1) }
    }
}
pub fn pollin() -> i16 { unsafe { c::POLLIN } }
pub fn pollerr() -> i16 { unsafe { c::POLLERR } }
pub fn pollhup() -> i16 { unsafe { c::POLLHUP } }

// ---- wrappers (same signatures as sys.rs) ---------------------------------------
pub fn read_fd(fd: Fd, buf: &mut [u8]) -> isize { neg(unsafe { c::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) }) }
pub fn write_fd(fd: Fd, buf: &[u8]) -> isize { neg(unsafe { c::write(fd, buf.as_ptr() as *const _, buf.len()) }) }
pub fn write_all(fd: Fd, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let n = write_fd(fd, buf);
        if n == EINTR_RET || n == EAGAIN_RET { continue; }
        if n <= 0 { return false; }
        buf = &buf[n as usize..];
    }
    true
}
pub fn close_fd(fd: Fd) { unsafe { c::close(fd); } }

pub fn open_read(path: &[u8]) -> Fd {
    let mut p = [0u8; 256];
    if path.len() >= p.len() { return -1; }
    p[..path.len()].copy_from_slice(path);
    unsafe { c::open(p.as_ptr() as *const c_char, c::O_CLOEXEC as c_int) }
}

pub fn poll_fds(fds: &mut [PollFd], timeout_ms: i32) -> isize { neg(unsafe { c::poll(fds.as_mut_ptr(), fds.len() as u64, timeout_ms) } as isize) }

pub fn mmap_fd(len: usize, prot: usize, flags: usize, fd: Fd, off: usize) -> *mut u8 {
    let r = unsafe { c::mmap(core::ptr::null_mut(), len as u64, prot as c_int, flags as c_int, fd, off as i64) };
    if r as isize == -1 { core::ptr::null_mut() } else { r as *mut u8 }
}
pub fn munmap_ptr(p: *mut u8, len: usize) { unsafe { c::munmap(p as *mut _, len as u64); } }
pub fn ftruncate_fd(fd: Fd, len: usize) -> isize { neg(unsafe { c::ftruncate(fd, len as i64) } as isize) }
pub fn memfd(name: &[u8]) -> Fd {
    let mut p = [0u8; 64];
    p[..name.len().min(63)].copy_from_slice(&name[..name.len().min(63)]);
    unsafe { c::memfd_create(p.as_ptr() as *const c_char, MFD_CLOEXEC as u32) }
}
pub fn ioctl_fd(fd: Fd, req: usize, arg: usize) -> isize { neg(unsafe { c::ioctl(fd, req as u64, arg) } as isize) }

pub fn socket_unix() -> Fd { unsafe { c::socket(AF_UNIX as c_int, SOCK_STREAM as c_int | c::SOCK_CLOEXEC, 0) } }
pub fn connect_unix(fd: Fd, path: &[u8]) -> bool {
    let mut sa = SockAddrUn { family: AF_UNIX as u16, path: [0; 108] };
    if path.is_empty() || path.len() > 107 { return false; }
    sa.path[..path.len()].copy_from_slice(path);
    let len = 2 + path.len() + if path[0] == 0 { 0 } else { 1 };
    unsafe { c::connect(fd, &sa as *const _ as *const c_void, len as u32) == 0 }
}

pub fn send_with_fds(fd: Fd, data: &[u8], fds: &[Fd]) -> bool {
    let mut iov = IoVec { base: data.as_ptr() as *mut u8, len: data.len() };
    let mut cbuf = [0u64; 4];
    let mut hdr = MsgHdr { name: core::ptr::null_mut(), namelen: 0, iov: &mut iov, iovlen: 1,
                           control: core::ptr::null_mut(), controllen: 0, flags: 0 };
    if !fds.is_empty() {
        let clen = 16 + 4 * fds.len();
        let cb = cbuf.as_mut_ptr() as *mut u8;
        unsafe {
            *(cb as *mut usize) = clen;
            *(cb.add(8) as *mut i32) = c::SOL_SOCKET;
            *(cb.add(12) as *mut i32) = SCM_RIGHTS;
            for (i, f) in fds.iter().enumerate() { *(cb.add(16 + 4 * i) as *mut i32) = *f; }
        }
        hdr.control = cb;
        hdr.controllen = (clen + 7) & !7;
    }
    let mut sent = 0usize;
    loop {
        let r = neg(unsafe { c::sendmsg(fd, &hdr, c::MSG_NOSIGNAL) });
        if r == EINTR_RET || r == EAGAIN_RET { continue; }
        if r < 0 { return false; }
        sent += r as usize;
        if sent >= data.len() { return true; }
        return write_all(fd, &data[sent..]);
    }
}

pub fn recv_with_fds(fd: Fd, buf: &mut [u8], fds_out: &mut Vec<Fd>, nonblock: bool) -> isize {
    let mut iov = IoVec { base: buf.as_mut_ptr(), len: buf.len() };
    let mut cbuf = [0u64; 36];
    let mut hdr = MsgHdr { name: core::ptr::null_mut(), namelen: 0, iov: &mut iov, iovlen: 1,
                           control: cbuf.as_mut_ptr() as *mut u8, controllen: 288, flags: 0 };
    // No MSG_CMSG_CLOEXEC on every host: mark received fds ourselves below.
    let flags = unsafe { if nonblock { c::MSG_DONTWAIT } else { 0 } };
    let r = loop {
        let r = neg(unsafe { c::recvmsg(fd, &mut hdr, flags) });
        if r == EINTR_RET { continue; }
        break r;
    };
    if r > 0 && hdr.controllen >= 16 {
        let cb = cbuf.as_ptr() as *const u8;
        let mut off = 0usize;
        while off + 16 <= hdr.controllen {
            let clen = unsafe { *(cb.add(off) as *const usize) };
            let level = unsafe { *(cb.add(off + 8) as *const i32) };
            let ty = unsafe { *(cb.add(off + 12) as *const i32) };
            if clen < 16 { break; }
            if level == unsafe { c::SOL_SOCKET } && ty == SCM_RIGHTS {
                let n = (clen - 16) / 4;
                for i in 0..n {
                    let f = unsafe { *(cb.add(off + 16 + 4 * i) as *const i32) };
                    unsafe { c::fcntl(f, c::F_SETFD, 1 as c_int); }   // FD_CLOEXEC is 1 everywhere
                    fds_out.push(f);
                }
            }
            off += (clen + 7) & !7;
        }
    }
    r
}

pub fn clock_monotonic_ns() -> u64 {
    let mut ts = Timespec::default();
    unsafe { c::clock_gettime(c::CLOCK_MONOTONIC, &mut ts); }
    ts.sec as u64 * 1_000_000_000 + ts.nsec as u64
}
pub fn nanosleep_ns(ns: u64) {
    let ts = Timespec { sec: (ns / 1_000_000_000) as i64, nsec: (ns % 1_000_000_000) as i64 };
    unsafe { c::nanosleep(&ts, core::ptr::null_mut()); }
}

// Futex: not portable. The event core uses a condvar when `cosmo` is set (see event.rs);
// these exist so the module surface matches.
pub fn futex_wait(_addr: &AtomicU32, _expected: u32, timeout_ns: u64) { nanosleep_ns(if timeout_ns == 0 { 1_000_000 } else { timeout_ns.min(1_000_000) }); }
pub fn futex_wake(_addr: &AtomicU32, _n: usize) {}

pub fn getenv(name: &str) -> Option<String> { std::env::var(name).ok() }

// The names the rest of the crate uses.
pub fn read(fd: Fd, buf: &mut [u8]) -> isize { read_fd(fd, buf) }
pub fn write(fd: Fd, buf: &[u8]) -> isize { write_fd(fd, buf) }
pub fn close(fd: Fd) { close_fd(fd) }
pub fn poll(fds: &mut [PollFd], timeout_ms: i32) -> isize { poll_fds(fds, timeout_ms) }
pub fn mmap(len: usize, prot: usize, flags: usize, fd: Fd, off: usize) -> *mut u8 { mmap_fd(len, prot, flags, fd, off) }
pub fn munmap(p: *mut u8, len: usize) { munmap_ptr(p, len) }
pub fn ftruncate(fd: Fd, len: usize) -> isize { ftruncate_fd(fd, len) }
pub fn memfd_create(name: &[u8]) -> Fd { memfd(name) }
pub fn ioctl(fd: Fd, req: usize, arg: usize) -> isize { ioctl_fd(fd, req, arg) }
