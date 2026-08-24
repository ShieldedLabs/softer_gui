//! softer_gui: CPU-rendered windowing with vblank-exact frame pacing and a single
//! ordered input+render event stream. Design lifted from lingua_brevis
//! fulcra/unix_future_gui.lbc; platform layers speak the X11/Wayland wire
//! protocols directly so the binary links statically. Zero dependencies.
//!
//!   let mut gui = Gui::open("title", 800, 600)?;
//!   loop {
//!       gui.wait();
//!       while let Some(ev) = gui.next_event() {
//!           match ev.kind {
//!               Kind::Render(r) => if let Some(fb) = gui.get_framebuffer() { draw(&fb); gui.submit(); },
//!               Kind::Close => return,
//!               _ => apply_input(ev),
//!           }
//!       }
//!   }
//!
//! The framebuffer is a square power-of-two, bigger than the window; render the
//! window-sized top-left sub-rect with stride = side. `key` is stable per buffer
//! in steady state and changes for both on any realloc, so a renderer can cache
//! "what I drew per key" and patch only the diff.
#![allow(clippy::missing_safety_doc)]

pub mod event;
pub mod keysym;
mod keysym_table;
pub mod xkb;
#[cfg(target_os = "linux")]
pub mod sys;
#[cfg(target_os = "linux")]
mod shm;
#[cfg(target_os = "linux")]
pub mod x11_conn;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "macos")]
mod mac;

pub use event::*;
use std::sync::Arc;

/// Smallest power of two ≥ n (and ≥ 256): the square framebuffer's side.
pub fn next_pow2(n: u32) -> u32 { let mut s = 256u32; while s < n { s <<= 1; } s }
use std::sync::atomic::Ordering::*;

enum Backend {
    #[cfg(target_os = "linux")]
    X11(x11::App),
    #[cfg(target_os = "linux")]
    Wayland(wayland::App),
    #[cfg(target_os = "macos")]
    Mac(mac::App),
}

pub struct Gui {
    core: Arc<Core>,
    back: Backend,
}

/// A back buffer to draw into. Pixels are 0xAARRGGBB with alpha 0xFF; stride is `side`.
pub struct Framebuffer<'a> {
    pub pixels: &'a mut [u32],
    pub side: usize,
    pub width: usize,
    pub height: usize,
    /// (generation << 1) | buffer index. Same key = same memory with the contents you last drew into it.
    pub key: u64,
}

impl Gui {
    /// Open a window. Wayland first, X11 fallback (SOFTER_GUI_X11=1 forces X11).
    /// Linux only: on macOS the main thread must pump AppKit, so use [`run`].
    #[cfg(target_os = "linux")]
    pub fn open(title: &str, width: u32, height: u32) -> Option<Gui> {
        let core = Arc::new(Core::new());
        let force_x11 = std::env::var("SOFTER_GUI_X11").map(|v| v == "1").unwrap_or(false);
        if !force_x11 {
            if let Some(app) = wayland::open(core.clone(), title, width, height) {
                return Some(Gui { core, back: Backend::Wayland(app) });
            }
        }
        let app = x11::open(core.clone(), title, width, height)?;
        Some(Gui { core, back: Backend::X11(app) })
    }

    /// Pop the next event, or None when the ring is empty.
    pub fn next_event(&self) -> Option<Event> { self.core.next_event() }
    /// Sleep until an event arrives.
    pub fn wait(&self) { self.core.wait(0) }
    /// Sleep until an event arrives or `ms` elapse.
    pub fn wait_ms(&self, ms: u64) { self.core.wait(ms * 1_000_000) }

    /// Settle any pending resize and hand out the free back buffer. None means both
    /// buffers are still held by the display server — skip this frame; a fresh
    /// RENDER arrives when one is released.
    pub fn get_framebuffer(&mut self) -> Option<Framebuffer<'_>> {
        let (ptr, side, key) = match &mut self.back {
            #[cfg(target_os = "linux")]
            Backend::X11(a) => a.get_framebuffer()?,
            #[cfg(target_os = "linux")]
            Backend::Wayland(a) => a.get_framebuffer()?,
            #[cfg(target_os = "macos")]
            Backend::Mac(a) => a.get_framebuffer()?,
        };
        let side = side as usize;
        let pixels = unsafe { core::slice::from_raw_parts_mut(ptr, side * side) };
        let w = (self.core.win_w.load(Relaxed) as usize).min(side);
        let h = (self.core.win_h.load(Relaxed) as usize).min(side);
        Some(Framebuffer { pixels, side, width: w, height: h, key })
    }
    /// Flip the buffer from the last get_framebuffer onto the screen at the next vblank.
    pub fn submit(&mut self) {
        match &mut self.back {
            #[cfg(target_os = "linux")]
            Backend::X11(a) => a.submit(),
            #[cfg(target_os = "linux")]
            Backend::Wayland(a) => a.submit(),
            #[cfg(target_os = "macos")]
            Backend::Mac(a) => a.submit(),
        }
    }

    /// Ask for a RENDER even though nothing was submitted last frame (animation wake-up).
    pub fn request_frame(&self) { self.core.wants_frame.store(true, Relaxed); self.poke(); }
    pub fn set_cursor_hidden(&self, hidden: bool) { self.core.cursor_hidden.store(hidden, Relaxed); self.poke(); }
    pub fn set_fullscreen(&self, on: bool) { self.core.wants_fullscreen.store(on, Relaxed); self.poke(); }
    pub fn is_fullscreen(&self) -> bool { self.core.wants_fullscreen.load(Relaxed) }
    pub fn window_size(&self) -> (u32, u32) { (self.core.win_w.load(Relaxed), self.core.win_h.load(Relaxed)) }
    /// The pump sets this when the window's contents must be fully redrawn (resize/realloc).
    pub fn take_full_redraw(&self) -> bool { self.core.full_redraw.swap(false, AcqRel) }
    pub fn period_fs(&self) -> u64 { self.core.period_fs() }

    fn poke(&self) {
        match &self.back {
            #[cfg(target_os = "linux")]
            Backend::Wayland(a) => a.poke(),
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
}

/// Open a window and run `app` against it. On Linux this is `app(Gui::open(..))` on the
/// calling thread. On macOS the calling thread must be the main thread: it becomes the
/// AppKit event pump and `app` runs on a second thread. Returns when `app` returns (or
/// the window closes, on macOS).
pub fn run<F: FnOnce(Gui) + Send + 'static>(title: &str, width: u32, height: u32, app: F) -> bool {
    #[cfg(target_os = "linux")]
    {
        match Gui::open(title, width, height) { Some(g) => { app(g); true } None => false }
    }
    #[cfg(target_os = "macos")]
    {
        let core = Arc::new(Core::new());
        let Some((a, mut pump)) = mac::open(core.clone(), title, width, height) else { return false };
        let gui = Gui { core: core.clone(), back: Backend::Mac(a) };
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done2 = done.clone();
        std::thread::Builder::new().name("softer_gui-app".into()).spawn(move || { app(gui); done2.store(true, Release); }).expect("thread");
        while !done.load(Acquire) && !core.quit.load(Relaxed) {
            pump.pump_once();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        true
    }
}

impl Drop for Gui {
    fn drop(&mut self) { self.core.quit.store(true, Relaxed); self.poke(); }
}
