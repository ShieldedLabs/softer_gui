//! softer_gui: CPU-rendered windowing with vblank-exact frame pacing and a single
//! ordered input+render event stream. Design lifted from lingua_brevis
//! fulcra/unix_future_gui.lbc; platform layers speak the X11/Wayland wire
//! protocols directly so the binary links statically. Zero dependencies.
//!
//! Plain polling, no callbacks:
//!
//!   let mut gui = softer_gui::open("title", "com.example.app", 800, 600).unwrap();
//!   let mut ev = Event::default();
//!   loop {
//!       gui.wait();
//!       while gui.next_event(&mut ev) {
//!           match ev.kind {
//!               EVENT_RENDER => {
//!                   let fb = gui.get_framebuffer();          // fb.pixels is null on backpressure: skip the frame
//!                   if !fb.pixels.is_null() { draw(&fb); gui.submit(); }
//!               }
//!               EVENT_CLOSE => return,
//!               _ => apply_input(&ev),
//!           }
//!       }
//!   }
//!
//! The framebuffer is a square power-of-two, bigger than the window; render the
//! window-sized top-left sub-rect with stride = side. `key` is stable per buffer
//! in steady state and changes for both on any realloc, so a renderer can cache
//! "what I drew per key" and patch only the diff.
//!
//! On macOS call `open` from the main thread: the library keeps that thread for
//! AppKit and hands the caller's stack to a new thread, which is what returns
//! from `open`. Nothing else differs.
#![allow(clippy::missing_safety_doc)]

pub mod event;
pub mod keysym;
mod keysym_table;
pub mod xkb;
pub mod icon;
pub mod install;
#[cfg(all(target_os = "linux", not(cosmo)))]
pub mod sys;
/// Cosmopolitan build: the same interface over cosmo's libc, so one binary runs on any host.
#[cfg(cosmo)]
#[path = "sys_cosmo.rs"]
pub mod sys;
#[cfg(target_os = "linux")]
mod shm;
#[cfg(target_os = "linux")]
pub mod x11_conn;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
mod wayland;
// An APE says target_os = "linux" whatever it is running on, so the macOS
// backend is compiled in there too and chosen at run time; its foreign symbols
// come from cosmo_dlopen instead of the linker (see mac_sys.rs).
#[cfg(any(target_os = "macos", cosmo))]
mod mac_sys;
#[cfg(any(target_os = "macos", cosmo))]
mod mac;
// And the same again for Windows: an APE may land there too, so the Win32
// backend is compiled in and chosen at run time, with its imports coming from
// cosmo_dlsym rather than an import table (see the win32! macro in sys_win.rs).
#[cfg(any(target_os = "windows", cosmo))]
pub mod sys_win;
#[cfg(any(target_os = "windows", cosmo))]
mod scancode_win;
#[cfg(any(target_os = "windows", cosmo))]
mod win_d3d;
#[cfg(any(target_os = "windows", cosmo))]
mod win;

pub use event::*;
use std::sync::Arc;
use std::sync::atomic::Ordering::*;

/// Smallest power of two ≥ n (and ≥ 256): the square framebuffer's side.
pub fn next_pow2(n: u32) -> u32 { let mut s = 256u32; while s < n { s <<= 1; } s }

/// Which backend to use, when more than one could serve.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend_ {
    /// Pick the best one that works here. Nearly always what you want.
    #[default]
    Auto,
    /// Linux: force X11 rather than trying Wayland first.
    X11,
    /// Windows: the compatible presenter, GDI on a Vista-era API baseline.
    Gdi,
    /// Windows: the capable presenter, D3D11 flip model, Windows 8.1 and up.
    D3d,
}

/// Which D3D11 device to ask for. Windows only, and only for `Backend_::D3d`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum D3dDriver {
    /// Hardware natively; WARP inside an APE, where the vendor driver crashes.
    #[default]
    Auto,
    Hardware,
    /// Microsoft's software rasteriser. Costs ~0.2 ms of latency here, because
    /// the GPU work in this crate is one copy per frame rather than a scene.
    Warp,
}

/// How to open the window.
///
/// These were environment variables, and for a library that is the conventional
/// place: a library has no command line of its own, and reading the program's
/// argv would collide with whatever the program wants to do with it. So the
/// knobs live here, the application decides where they come from, and the ones
/// shipped in this repo parse them from their own flags.
///
/// `Options::from_env()` keeps the old variables working for a program that has
/// no flags to offer, and is what plain `open()` uses.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Trace what the backend chose and how it is pacing.
    pub debug: bool,
    /// Start borderless fullscreen. On Windows this is worth most of a frame of
    /// latency, because it lets the compositor hand the swapchain straight to
    /// the display controller instead of compositing it.
    pub fullscreen: bool,
    pub backend: Backend_,
    pub d3d_driver: D3dDriver,
}

impl Options {
    /// The environment variables this crate used to read, and still honours for
    /// callers that reach `open()` without options of their own.
    pub fn from_env() -> Options {
        let var = |n: &str| std::env::var(n).unwrap_or_default();
        Options {
            debug: std::env::var("SOFTER_GUI_DEBUG").is_ok(),
            fullscreen: std::env::var("SOFTER_GUI_FULLSCREEN").is_ok(),
            backend: match (var("SOFTER_GUI_X11").as_str(), var("SOFTER_GUI_WIN").as_str()) {
                ("1", _) => Backend_::X11,
                (_, "gdi") => Backend_::Gdi,
                (_, "d3d") => Backend_::D3d,
                _ => Backend_::Auto,
            },
            d3d_driver: match var("SOFTER_GUI_D3D_DRIVER").as_str() {
                "hardware" => D3dDriver::Hardware,
                "warp" => D3dDriver::Warp,
                _ => D3dDriver::Auto,
            },
        }
    }
}

enum Backend {
    #[cfg(target_os = "linux")]
    X11(x11::App),
    #[cfg(target_os = "linux")]
    Wayland(wayland::App),
    #[cfg(any(target_os = "macos", cosmo))]
    Mac(mac::App),
    #[cfg(any(target_os = "windows", cosmo))]
    Win(win::App),
}

pub struct Gui {
    core: Arc<Core>,
    back: Backend,
}

/// A back buffer to draw into. Pixels are 0xAARRGGBB with alpha 0xFF; stride is `side`.
/// `pixels` is null when no buffer is free (both still held by the display server).
#[derive(Clone, Copy, Debug)]
pub struct Framebuffer {
    pub pixels: *mut u32,
    pub side: usize,
    pub width: usize,
    pub height: usize,
    /// (generation << 1) | buffer index. Same key = same memory with what you last drew into it.
    pub key: u64,
}
impl Framebuffer {
    pub fn ok(&self) -> bool { !self.pixels.is_null() }
    /// The whole side*side buffer as a slice.
    pub fn slice(&mut self) -> &mut [u32] { if self.pixels.is_null() { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(self.pixels, self.side * self.side) } } }
}

/// Open a window. `app_id` names the application for the desktop (WM_CLASS / xdg app_id,
/// e.g. "com.example.app"; see `install`). Linux: Wayland first, X11 fallback
/// (SOFTER_GUI_X11=1 forces X11). macOS: main thread only. In an APE the host
/// is not known until the program starts, so the backend is picked here rather
/// than by the compiler.
pub fn open(title: &str, app_id: &str, width: u32, height: u32) -> Option<Gui> {
    open_with(title, app_id, width, height, Options::from_env())
}

/// `open`, with the choices made explicitly rather than read from the
/// environment. This is what a program with its own command line should call.
pub fn open_with(title: &str, app_id: &str, width: u32, height: u32, opts: Options) -> Option<Gui> {
    let core = Arc::new(Core::new());
    core.debug.store(opts.debug, Relaxed);
    // An APE carries every backend and picks one here, from what cosmo says the
    // host actually is. A native build has exactly one and the branch is gone at
    // compile time.
    #[cfg(cosmo)]
    if mac_sys::on_macos() {
        let _ = app_id;
        return open_mac(core, title, width, height);
    }
    #[cfg(cosmo)]
    if sys_win::cosmo::is_windows() {
        let app = win::open(core.clone(), title, app_id, width, height, opts)?;
        return Some(Gui { core, back: Backend::Win(app) });
    }
    #[cfg(any(target_os = "linux", cosmo))]
    {
        let force_x11 = opts.backend == Backend_::X11;
        if !force_x11 {
            if let Some(app) = wayland::open(core.clone(), title, app_id, width, height) {
                return Some(Gui { core, back: Backend::Wayland(app) });
            }
        }
        let app = x11::open(core.clone(), title, app_id, width, height)?;
        Some(Gui { core, back: Backend::X11(app) })
    }
    #[cfg(all(target_os = "macos", not(cosmo)))]
    {
        let _ = app_id;
        open_mac(core, title, width, height)
    }
    #[cfg(all(target_os = "windows", not(cosmo)))]
    {
        let app = win::open(core.clone(), title, app_id, width, height, opts)?;
        Some(Gui { core, back: Backend::Win(app) })
    }
}

#[cfg(any(target_os = "macos", cosmo))]
fn open_mac(core: Arc<Core>, title: &str, width: u32, height: u32) -> Option<Gui> {
    let (app, pump) = mac::open(core.clone(), title, width, height)?;
    // From here on we are a different kernel thread: the main thread now belongs to AppKit.
    if !mac::takeover_main_thread(pump) { return None; }
    Some(Gui { core, back: Backend::Mac(app) })
}

impl Gui {
    /// Copy the next event into `ev`; false when there is none.
    pub fn next_event(&mut self, ev: &mut Event) -> bool { self.core.next_event(ev) }

    /// Sleep until an event arrives.
    pub fn wait(&mut self) { self.wait_ms(0) }
    /// Sleep until an event arrives or `ms` elapse (0 = no limit).
    pub fn wait_ms(&mut self, ms: u64) { self.core.wait(ms * 1_000_000) }

    /// Settle any pending resize and hand out the free back buffer. `pixels` is null
    /// when both buffers are still held by the display server — skip this frame; a
    /// fresh RENDER arrives when one is released.
    pub fn get_framebuffer(&mut self) -> Framebuffer {
        let got = match &mut self.back {
            #[cfg(target_os = "linux")]
            Backend::X11(a) => a.get_framebuffer(),
            #[cfg(target_os = "linux")]
            Backend::Wayland(a) => a.get_framebuffer(),
            #[cfg(any(target_os = "macos", cosmo))]
            Backend::Mac(a) => a.get_framebuffer(),
            #[cfg(any(target_os = "windows", cosmo))]
            Backend::Win(a) => a.get_framebuffer(),
        };
        let Some((ptr, side, key)) = got else { return Framebuffer { pixels: core::ptr::null_mut(), side: 0, width: 0, height: 0, key: 0 } };
        let side = side as usize;
        let w = (self.core.win_w.load(Relaxed) as usize).min(side);
        let h = (self.core.win_h.load(Relaxed) as usize).min(side);
        Framebuffer { pixels: ptr, side, width: w, height: h, key }
    }
    /// Flip the buffer from the last get_framebuffer onto the screen at the next vblank.
    pub fn submit(&mut self) {
        match &mut self.back {
            #[cfg(target_os = "linux")]
            Backend::X11(a) => a.submit(),
            #[cfg(target_os = "linux")]
            Backend::Wayland(a) => a.submit(),
            #[cfg(any(target_os = "macos", cosmo))]
            Backend::Mac(a) => a.submit(),
            #[cfg(any(target_os = "windows", cosmo))]
            Backend::Win(a) => a.submit(),
        }
    }

    /// Ask for a RENDER even though nothing was submitted last frame (animation wake-up).
    pub fn request_frame(&mut self) { self.core.wants_frame.store(true, Relaxed); self.poke(); }
    pub fn set_cursor_hidden(&mut self, hidden: bool) { self.core.cursor_hidden.store(hidden, Relaxed); self.poke(); }
    /// Switch to the next rendering path and keep the window. Windows has more
    /// than one (GDI, D3D11 on the hardware driver, D3D11 on WARP) and they differ
    /// in latency, so being able to change without restarting is how you compare
    /// them on the same picture. A no-op everywhere else.
    pub fn cycle_backend(&mut self) { self.core.wants_cycle.store(true, Relaxed); self.poke(); }
    pub fn set_fullscreen(&mut self, on: bool) { self.core.wants_fullscreen.store(on, Relaxed); self.poke(); }
    pub fn is_fullscreen(&self) -> bool { self.core.wants_fullscreen.load(Relaxed) }
    pub fn window_size(&self) -> (u32, u32) { (self.core.win_w.load(Relaxed), self.core.win_h.load(Relaxed)) }
    /// Set by the pump when the window's contents must be fully redrawn (resize/realloc); cleared on read.
    pub fn take_full_redraw(&mut self) -> bool { self.core.full_redraw.swap(false, AcqRel) }
    /// Set the window icon: square 0xAARRGGBB images, any sizes (largest is preferred by the desktop).
    pub fn set_icon(&mut self, images: &[icon::IconImage]) { self.core.set_icon(icon::own_set(images)); self.poke(); }
    /// Nominal frame period, femtoseconds.
    pub fn period_fs(&self) -> u64 { self.core.period_fs() }

    fn poke(&mut self) {
        match &self.back {
            #[cfg(target_os = "linux")]
            Backend::Wayland(a) => a.poke(),
            #[cfg(any(target_os = "windows", cosmo))]
            Backend::Win(a) => a.poke(),
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
}

impl Drop for Gui {
    fn drop(&mut self) { self.core.quit.store(true, Relaxed); self.poke(); }
}
