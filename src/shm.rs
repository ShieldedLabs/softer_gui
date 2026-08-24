//! memfd-backed pixel memory shared with the display server (X11 SHM-by-fd, wl_shm).
use crate::sys::{self, Fd};

pub struct ShmMem {
    pub fd: Fd,
    pub ptr: *mut u8,
    pub len: usize,
}
unsafe impl Send for ShmMem {}
unsafe impl Sync for ShmMem {}

impl ShmMem {
    pub fn new(len: usize) -> Option<ShmMem> {
        let fd = sys::memfd_create(b"softer_gui");
        if fd < 0 { eprintln!("softer_gui: memfd_create failed"); return None; }
        if sys::ftruncate(fd, len) != 0 { sys::close(fd); return None; }
        let ptr = sys::mmap(len, sys::PROT_READ | sys::PROT_WRITE, sys::MAP_SHARED, fd, 0);
        if ptr.is_null() { sys::close(fd); return None; }
        Some(ShmMem { fd, ptr, len })
    }
}
impl Drop for ShmMem {
    fn drop(&mut self) {
        sys::munmap(self.ptr, self.len);
        sys::close(self.fd);
    }
}

pub use crate::next_pow2;
