//! Raw Linux syscalls. No libc declarations, no header crates: the kernel ABI is
//! the only contract. x86_64 and aarch64.

#![allow(dead_code)]

use core::arch::asm;

pub type Fd = i32;

// ---- syscall numbers -------------------------------------------------------
#[cfg(target_arch = "x86_64")]
pub mod nr {
    pub const READ: usize = 0;
    pub const WRITE: usize = 1;
    pub const OPEN: usize = 2;
    pub const CLOSE: usize = 3;
    pub const POLL: usize = 7;
    pub const MMAP: usize = 9;
    pub const MUNMAP: usize = 11;
    pub const IOCTL: usize = 16;
    pub const NANOSLEEP: usize = 35;
    pub const SOCKET: usize = 41;
    pub const CONNECT: usize = 42;
    pub const SENDMSG: usize = 46;
    pub const RECVMSG: usize = 47;
    pub const FTRUNCATE: usize = 77;
    pub const FUTEX: usize = 202;
    pub const CLOCK_GETTIME: usize = 228;
    pub const MEMFD_CREATE: usize = 319;
}
#[cfg(target_arch = "aarch64")]
pub mod nr {
    pub const READ: usize = 63;
    pub const WRITE: usize = 64;
    pub const OPENAT: usize = 56;
    pub const CLOSE: usize = 57;
    pub const PPOLL: usize = 73;
    pub const MMAP: usize = 222;
    pub const MUNMAP: usize = 215;
    pub const IOCTL: usize = 29;
    pub const NANOSLEEP: usize = 101;
    pub const SOCKET: usize = 198;
    pub const CONNECT: usize = 203;
    pub const SENDMSG: usize = 211;
    pub const RECVMSG: usize = 212;
    pub const FTRUNCATE: usize = 46;
    pub const FUTEX: usize = 98;
    pub const CLOCK_GETTIME: usize = 113;
    pub const MEMFD_CREATE: usize = 279;
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn syscall6(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!("syscall", inlateout("rax") n as isize => ret, in("rdi") a, in("rsi") b, in("rdx") c,
             in("r10") d, in("r8") e, in("r9") f, lateout("rcx") _, lateout("r11") _,
             options(nostack));
    }
    ret
}
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn syscall6(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!("svc 0", in("x8") n, inlateout("x0") a as isize => ret, in("x1") b, in("x2") c,
             in("x3") d, in("x4") e, in("x5") f, options(nostack));
    }
    ret
}

#[inline(always)]
pub unsafe fn syscall0(n: usize) -> isize { unsafe { syscall6(n, 0, 0, 0, 0, 0, 0) } }
#[inline(always)]
pub unsafe fn syscall1(n: usize, a: usize) -> isize { unsafe { syscall6(n, a, 0, 0, 0, 0, 0) } }
#[inline(always)]
pub unsafe fn syscall2(n: usize, a: usize, b: usize) -> isize { unsafe { syscall6(n, a, b, 0, 0, 0, 0) } }
#[inline(always)]
pub unsafe fn syscall3(n: usize, a: usize, b: usize, c: usize) -> isize { unsafe { syscall6(n, a, b, c, 0, 0, 0) } }
#[inline(always)]
pub unsafe fn syscall4(n: usize, a: usize, b: usize, c: usize, d: usize) -> isize { unsafe { syscall6(n, a, b, c, d, 0, 0) } }

pub const EINTR: isize = -4;
pub const EAGAIN: isize = -11;

// ---- kernel structs --------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timespec { pub sec: i64, pub nsec: i64 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PollFd { pub fd: i32, pub events: i16, pub revents: i16 }
pub const POLLIN: i16 = 1;
pub const POLLERR: i16 = 8;
pub const POLLHUP: i16 = 16;

#[repr(C)]
pub struct IoVec { pub base: *mut u8, pub len: usize }

#[repr(C)]
pub struct MsgHdr {
    pub name: *mut u8, pub namelen: u32,
    pub iov: *mut IoVec, pub iovlen: usize,
    pub control: *mut u8, pub controllen: usize,
    pub flags: i32,
}

#[repr(C)]
pub struct SockAddrUn { pub family: u16, pub path: [u8; 108] }

pub const AF_UNIX: usize = 1;
pub const SOCK_STREAM: usize = 1;
pub const SOCK_CLOEXEC: usize = 0o2000000;
pub const SOL_SOCKET: i32 = 1;
pub const SCM_RIGHTS: i32 = 1;
pub const MSG_NOSIGNAL: usize = 0x4000;
pub const MSG_DONTWAIT: usize = 0x40;
pub const MSG_CMSG_CLOEXEC: usize = 0x40000000;

pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const MAP_SHARED: usize = 1;
pub const MAP_PRIVATE: usize = 2;
pub const MAP_ANONYMOUS: usize = 0x20;
pub const MFD_CLOEXEC: usize = 1;
pub const CLOCK_MONOTONIC: usize = 1;
pub const FUTEX_WAIT: usize = 0;
pub const FUTEX_WAKE: usize = 1;
pub const FUTEX_PRIVATE: usize = 128;

// ---- wrappers --------------------------------------------------------------
pub fn read(fd: Fd, buf: &mut [u8]) -> isize {
    unsafe { syscall3(nr::READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) }
}
pub fn write(fd: Fd, buf: &[u8]) -> isize {
    unsafe { syscall3(nr::WRITE, fd as usize, buf.as_ptr() as usize, buf.len()) }
}
pub fn write_all(fd: Fd, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let n = write(fd, buf);
        if n == EINTR || n == EAGAIN { continue; }
        if n <= 0 { return false; }
        buf = &buf[n as usize..];
    }
    true
}
pub fn close(fd: Fd) { unsafe { syscall1(nr::CLOSE, fd as usize); } }

pub fn open_read(path: &[u8]) -> Fd {
    let mut p = [0u8; 256];
    if path.len() >= p.len() { return -1; }
    p[..path.len()].copy_from_slice(path);
    #[cfg(target_arch = "x86_64")]
    { unsafe { syscall3(nr::OPEN, p.as_ptr() as usize, 0o2000000, 0) as Fd } }
    #[cfg(target_arch = "aarch64")]
    { unsafe { syscall4(nr::OPENAT, -100isize as usize, p.as_ptr() as usize, 0o2000000, 0) as Fd } }
}

pub fn poll(fds: &mut [PollFd], timeout_ms: i32) -> isize {
    #[cfg(target_arch = "x86_64")]
    { unsafe { syscall3(nr::POLL, fds.as_mut_ptr() as usize, fds.len(), timeout_ms as isize as usize) } }
    #[cfg(target_arch = "aarch64")]
    {
        let ts = Timespec { sec: (timeout_ms / 1000) as i64, nsec: ((timeout_ms % 1000) as i64) * 1_000_000 };
        let tp = if timeout_ms < 0 { 0 } else { &ts as *const _ as usize };
        unsafe { syscall6(nr::PPOLL, fds.as_mut_ptr() as usize, fds.len(), tp, 0, 0, 0) }
    }
}

pub fn mmap(len: usize, prot: usize, flags: usize, fd: Fd, off: usize) -> *mut u8 {
    let r = unsafe { syscall6(nr::MMAP, 0, len, prot, flags, fd as isize as usize, off) };
    if r < 0 && r > -4096 { core::ptr::null_mut() } else { r as *mut u8 }
}
pub fn munmap(p: *mut u8, len: usize) { unsafe { syscall2(nr::MUNMAP, p as usize, len); } }
pub fn ftruncate(fd: Fd, len: usize) -> isize { unsafe { syscall2(nr::FTRUNCATE, fd as usize, len) } }
pub fn memfd_create(name: &[u8]) -> Fd {
    let mut p = [0u8; 64];
    p[..name.len().min(63)].copy_from_slice(&name[..name.len().min(63)]);
    unsafe { syscall2(nr::MEMFD_CREATE, p.as_ptr() as usize, MFD_CLOEXEC) as Fd }
}
pub fn ioctl(fd: Fd, req: usize, arg: usize) -> isize { unsafe { syscall3(nr::IOCTL, fd as usize, req, arg) } }

pub fn socket_unix() -> Fd { unsafe { syscall3(nr::SOCKET, AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0) as Fd } }
/// `path` may begin with a NUL for the abstract namespace.
pub fn connect_unix(fd: Fd, path: &[u8]) -> bool {
    let mut sa = SockAddrUn { family: AF_UNIX as u16, path: [0; 108] };
    if path.len() > 107 { return false; }
    sa.path[..path.len()].copy_from_slice(path);
    let len = 2 + path.len() + if path[0] == 0 { 0 } else { 1 };
    unsafe { syscall3(nr::CONNECT, fd as usize, &sa as *const _ as usize, len) == 0 }
}

/// sendmsg with up to 4 fds attached via SCM_RIGHTS. Blocks until everything is written.
pub fn send_with_fds(fd: Fd, data: &[u8], fds: &[Fd]) -> bool {
    let mut iov = IoVec { base: data.as_ptr() as *mut u8, len: data.len() };
    let mut cbuf = [0u64; 4]; // cmsghdr (16) + up to 4 fds (16) = 32 bytes
    let mut hdr = MsgHdr { name: core::ptr::null_mut(), namelen: 0, iov: &mut iov, iovlen: 1,
                           control: core::ptr::null_mut(), controllen: 0, flags: 0 };
    if !fds.is_empty() {
        let clen = 16 + 4 * fds.len();
        let cb = cbuf.as_mut_ptr() as *mut u8;
        unsafe {
            *(cb as *mut usize) = clen;                       // cmsg_len
            *(cb.add(8) as *mut i32) = SOL_SOCKET;            // cmsg_level
            *(cb.add(12) as *mut i32) = SCM_RIGHTS;           // cmsg_type
            for (i, f) in fds.iter().enumerate() { *(cb.add(16 + 4 * i) as *mut i32) = *f; }
        }
        hdr.control = cb;
        hdr.controllen = (clen + 7) & !7;
    }
    let mut sent = 0usize;
    loop {
        let r = unsafe { syscall3(nr::SENDMSG, fd as usize, &hdr as *const _ as usize, MSG_NOSIGNAL) };
        if r == EINTR || r == EAGAIN { continue; }
        if r < 0 { return false; }
        sent += r as usize;
        if sent >= data.len() { return true; }
        // The fds went with the first chunk; finish the bytes plainly.
        return write_all(fd, &data[sent..]);
    }
}

/// recvmsg collecting any SCM_RIGHTS fds into `fds_out`. Returns bytes read (0 = closed, <0 = error).
pub fn recv_with_fds(fd: Fd, buf: &mut [u8], fds_out: &mut Vec<Fd>, nonblock: bool) -> isize {
    let mut iov = IoVec { base: buf.as_mut_ptr(), len: buf.len() };
    let mut cbuf = [0u64; 36]; // room for 64 fds
    let mut hdr = MsgHdr { name: core::ptr::null_mut(), namelen: 0, iov: &mut iov, iovlen: 1,
                           control: cbuf.as_mut_ptr() as *mut u8, controllen: 288, flags: 0 };
    let flags = MSG_CMSG_CLOEXEC | if nonblock { MSG_DONTWAIT } else { 0 };
    let r = loop {
        let r = unsafe { syscall3(nr::RECVMSG, fd as usize, &mut hdr as *mut _ as usize, flags) };
        if r == EINTR { continue; }
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
            if level == SOL_SOCKET && ty == SCM_RIGHTS {
                let n = (clen - 16) / 4;
                for i in 0..n { fds_out.push(unsafe { *(cb.add(off + 16 + 4 * i) as *const i32) }); }
            }
            off += (clen + 7) & !7;
        }
    }
    r
}

pub fn clock_monotonic_ns() -> u64 {
    let mut ts = Timespec::default();
    unsafe { syscall2(nr::CLOCK_GETTIME, CLOCK_MONOTONIC, &mut ts as *mut _ as usize); }
    ts.sec as u64 * 1_000_000_000 + ts.nsec as u64
}
pub fn nanosleep_ns(ns: u64) {
    let ts = Timespec { sec: (ns / 1_000_000_000) as i64, nsec: (ns % 1_000_000_000) as i64 };
    unsafe { syscall2(nr::NANOSLEEP, &ts as *const _ as usize, 0); }
}

/// Wait while `*addr == expected` (kernel-side compare), at most `timeout_ns` (0 = forever).
pub fn futex_wait(addr: &core::sync::atomic::AtomicU32, expected: u32, timeout_ns: u64) {
    let ts = Timespec { sec: (timeout_ns / 1_000_000_000) as i64, nsec: (timeout_ns % 1_000_000_000) as i64 };
    let tp = if timeout_ns == 0 { 0 } else { &ts as *const _ as usize };
    unsafe { syscall4(nr::FUTEX, addr as *const _ as usize, FUTEX_WAIT | FUTEX_PRIVATE, expected as usize, tp); }
}
pub fn futex_wake(addr: &core::sync::atomic::AtomicU32, n: usize) {
    unsafe { syscall3(nr::FUTEX, addr as *const _ as usize, FUTEX_WAKE | FUTEX_PRIVATE, n); }
}

pub fn getenv(name: &str) -> Option<String> { std::env::var(name).ok() }
