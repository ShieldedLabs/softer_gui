//! Windows backend: a GDI DIB section blitted to the window, and a display clock
//! taken from DWM's composition timing rather than from the presentation path.
//!
//! WHY GDI AND NOT D3D11. The obvious Windows answer is a DXGI flip-model
//! swapchain, and on a Windows 10 floor it is the better answer: it is the only
//! path that reports vblank-accurate present statistics for the frames you
//! actually submitted. It is rejected here because it does not exist before
//! Windows 8, and this backend is built to a Vista baseline. GDI's
//! CreateDIBSection plus BitBlt has worked unchanged since Windows 95.
//!
//! THE MOVE THAT MAKES THAT AFFORDABLE: the display clock does not have to come
//! from the presentation path. It is usually assumed it must, and that is what
//! makes GDI look disqualifying, because GDI reports no timing at all, at any
//! Windows version. But DWM will tell anyone who asks:
//! DwmGetCompositionTimingInfo (Vista and up) returns `rateRefresh` as an exact
//! rational, which is disp_period_fs with no float round-trip, and `cRefresh`, a
//! monotonic vblank counter, which is exactly X11 Present's MSC. Delta of
//! cRefresh between our wakeups IS the `frames` argument display_tick() wants.
//! So we keep the crate's central property (display time that advances only at
//! frame boundaries and only in whole refresh periods) on a Vista-era pixel path.
//!
//! Fallbacks, in order, all resolved at runtime so a missing one degrades rather
//! than failing to load: DwmFlush for the vblank wait, else
//! D3DKMTWaitForVerticalBlankEvent (Vista, and works with composition off), else
//! a QPC sleep to the next period boundary. Refresh period from DWM's rational,
//! else EnumDisplaySettings' integer Hz.
//!
//! ONE CPU BUFFER, NOT TWO. Linux needs two because the compositor HOLDS the
//! buffer until it releases it. Windows copies out of ours synchronously and
//! hands it straight back, so there is nothing to alternate, and alternating
//! actively hurts: the buffer you are about to draw frame N into would hold frame
//! N-2, so an incremental renderer would leave stale rectangles flickering
//! between two states. We keep one buffer and pin the key's index to 0.
//! Core::busy[] and Core::in_flight are therefore unused on Windows; that is
//! deliberate, not unimplemented.
//!
//! NO ZERO-COPY HANDOFF EXISTS ON WINDOWS. Not through DIB sections, not through
//! D3D11, not through DirectComposition. DWM composites from surfaces it owns, so
//! every path copies our pixels out. Budget one CPU-to-GPU copy per frame; there
//! is no way around it and looking for one is wasted time.
//!
//! THREADS. Three, matching the macOS shape more than the Linux one: the pump
//! thread creates the HWND and owns the message loop and all window state (Win32
//! message queues are per-thread and a window belongs to its creator); a vblank
//! thread does nothing but wait for the next refresh and signal an event; the app
//! thread only writes pixels. EVERY Win32 and GDI call, the blit included, happens
//! on the pump thread, because a window belongs to the thread that created it and
//! that thread can be parked inside DefWindowProc for a whole modal resize drag.
//! So the app never touches the HWND: outward requests (cursor, fullscreen, icon)
//! go through the flags on Core that the pump polls, exactly as on the other
//! backends, and submit() publishes the frame and posts WM_SOFTER_BLIT.
//!
//! RESIZE. Windows runs a modal loop while a border is dragged, in which
//! DefWindowProc does not return and the pump's own loop stops. A timer, which is
//! the one thing still delivered in that loop, becomes the frame source for the
//! duration, so display time keeps advancing and the app keeps animating rather
//! than re-rendering one frozen timestamp.
//!
//! KNOWN LIMITATIONS, written down so they are not mistaken for bugs: with
//! variable-refresh displays active, "nominal refresh period" is a fiction and the
//! whole-periods-only model degrades. AXIS_ZOOM and AXIS_ROTATE are never emitted;
//! they come from touchpad valuators on Linux and Windows has no equivalent short
//! of parsing raw HID contacts. Dead keys are composed by Windows rather than by
//! keysym.rs, so an accent may produce a different character here than on Linux.

#![allow(non_snake_case)]

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Arc, Condvar, Mutex};
use core::cell::Cell;

use crate::event::*;
use crate::keysym;
use crate::scancode_win::to_evdev;
use crate::sys_win::*;
use crate::win_d3d::D3d;

const FS: u64 = 1_000_000_000_000_000;

/// The frame source during the modal resize loop (see WM_ENTERSIZEMOVE).
const RESIZE_TIMER: usize = 1;
/// Windows clamps timers to about 10 ms, so this asks for "as often as you can".
/// It is deliberately FASTER than any refresh rate: frame_tick gates itself on
/// DWM's refresh counter, so polling too often costs a cheap call and nothing else.
const RESIZE_TIMER_MS: u32 = 8;

fn now_ns() -> u64 { (qpc() as u128 * 1_000_000_000u128 / qpf() as u128) as u64 }

fn debug() -> bool { std::env::var("SOFTER_GUI_DEBUG").is_ok() }

// ---- the one CPU buffer -------------------------------------------------------
/// A square power-of-two DIB section plus the memory DC it is selected into.
/// BitBlt from a selected DIB is GDI's fast path; StretchDIBits would re-validate
/// the BITMAPINFO on every call.
struct Surface { memdc: HDC, bmp: HBITMAP, old: HANDLE, pixels: *mut u32, side: u32 }

impl Surface {
    fn new(side: u32) -> Option<Surface> {
        unsafe {
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = side as i32;
            // Negative height: top-down. Otherwise row 0 is the bottom of the image
            // and every loop the caller writes is upside down.
            bmi.bmiHeader.biHeight = -(side as i32);
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            // hSection null: GDI allocates. The section variant exists for sharing the
            // pixels with another process, which nothing in this crate asks for.
            let bmp = CreateDIBSection(NULL, &bmi, DIB_RGB_COLORS, &mut bits, NULL, 0);
            if bmp.is_null() || bits.is_null() { return None; }
            let memdc = CreateCompatibleDC(NULL);
            if memdc.is_null() { DeleteObject(bmp); return None; }
            let old = SelectObject(memdc, bmp);
            Some(Surface { memdc, bmp, old, pixels: bits as *mut u32, side })
        }
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memdc, self.old);
            DeleteDC(self.memdc);
            DeleteObject(self.bmp);
        }
    }
}
unsafe impl Send for Surface {}

/// How pixels reach the screen, and how a frame boundary is learned. The two are
/// chosen together because they are the same decision: the capable path's wait IS
/// its swapchain, and the compatible path's wait is a thread watching the
/// compositor. See the module header for what separates them.
enum Present {
    /// Vista and up. DIB section blitted to the window; vblank thread; DWM cRefresh.
    Gdi,
    /// Windows 8.1 and up. Flip-model swapchain; its waitable object; per-present
    /// statistics. No vblank thread at all.
    D3d(D3d),
}

// ---- shared between the three threads ------------------------------------------
pub struct Shared {
    hwnd: AtomicUsize,
    /// The single CPU buffer. Locked for the blit and for reallocation; the app
    /// draws into the pixels outside the lock, so a WM_PAINT landing mid-draw can
    /// show a torn frame. The next submit corrects it and nothing is corrupted.
    surf: Mutex<Option<Surface>>,
    ready: Mutex<u8>,           // 0 pending, 1 open, 2 failed
    ready_cv: Condvar,
    period_fs: AtomicU64,
    dyn_: Arc<Dyn>,
    /// The app has a frame in the buffer that nobody has put on screen yet.
    dirty: AtomicBool,
    quit_vblank: AtomicBool,
    vblank_ev: AtomicUsize,
    generation: AtomicU32,
}
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

pub struct App {
    sh: Arc<Shared>,
    core: Arc<Core>,
    side: u32,
    generation: u64,
}

impl App {
    /// Settle a pending resize, then hand out the buffer. Never returns null: there
    /// is only one buffer and nothing else is holding it (see the module doc).
    pub fn get_framebuffer(&mut self) -> Option<(*mut u32, u32, u64)> {
        let w = self.core.win_w.load(Relaxed);
        let h = self.core.win_h.load(Relaxed);
        let want = crate::next_pow2(w.max(h));
        let mut g = self.sh.surf.lock().ok()?;
        if want != self.side || g.is_none() {
            *g = Surface::new(want);
            g.as_ref()?;
            self.side = want;
            self.generation += 1;
            self.sh.generation.store(self.generation as u32, Relaxed);
            self.core.full_redraw.store(true, Relaxed);
        }
        let s = g.as_ref()?;
        // Index pinned to 0: one buffer, so the low bit of the key never moves.
        Some((s.pixels, s.side, self.generation << 1))
    }

    /// Publish the finished frame and ask the pump to put it on screen.
    ///
    /// This deliberately does NO GDI of its own. A window belongs to the thread
    /// that created it, and GDI against that window from another thread has to
    /// serialise with whatever the owning thread is doing to it. The case that
    /// makes this more than theory is the modal move/size loop: while the user
    /// holds a border, the window's thread sits inside DefWindowProc and does not
    /// return, so an app-thread blit has nothing to serialise against and no bound
    /// on how long it waits. That is a freeze lasting the whole drag.
    ///
    /// So the rule is absolute, and it is the rule the module header already
    /// states: every Win32 and GDI call happens on the pump thread. We publish the
    /// frame and post; the pump blits from inside its message loop, where it is
    /// safe both normally and mid-drag. The cost is one PostMessage and a thread
    /// hop, comfortably inside a frame (measured: submit() returns in under a
    /// millisecond, including during the modal loop).
    pub fn submit(&mut self) {
        let hwnd = self.sh.hwnd.load(Relaxed) as HWND;
        if hwnd.is_null() { return; }
        self.sh.dirty.store(true, Release);
        unsafe { PostMessageW(hwnd, WM_SOFTER_BLIT, 0, 0) };
    }

    pub fn poke(&self) {
        let h = self.sh.hwnd.load(Relaxed) as HWND;
        if !h.is_null() { unsafe { PostMessageW(h, WM_SOFTER_POKE, 0, 0); } }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.core.quit.store(true, Relaxed);
        self.sh.quit_vblank.store(true, Relaxed);
        self.poke();
    }
}

// ---- the vblank thread ---------------------------------------------------------
/// How we learn that a refresh happened. Chosen once, at startup.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Vsync { Dwm, Kmt, Timer }

fn choose_vsync(d: &Dyn) -> Vsync {
    unsafe {
        if let (Some(flush), Some(enabled)) = (d.dwm_flush, d.dwm_enabled) {
            let mut on: BOOL = 0;
            // Composition is always on from Windows 8; on Vista and 7 the user can
            // turn it off, and DwmFlush then returns immediately forever.
            if enabled(&mut on) >= 0 && on != 0 { let _ = flush; return Vsync::Dwm; }
        }
        if d.kmt_open.is_some() && d.kmt_wait.is_some() { return Vsync::Kmt; }
        Vsync::Timer
    }
}

fn vblank_thread(sh: Arc<Shared>, mode: Vsync) {
    let d = sh.dyn_.clone();
    let ev = sh.vblank_ev.load(Relaxed) as HANDLE;
    // D3DKMT wants an adapter, opened from a screen DC.
    let mut wait = D3DKMT_WAITFORVERTICALBLANKEVENT::default();
    let mut kmt_ok = false;
    if mode == Vsync::Kmt {
        unsafe {
            let dc = GetDC(NULL);
            let mut open = D3DKMT_OPENADAPTERFROMHDC { hDc: dc as usize, ..Default::default() };
            if let Some(f) = d.kmt_open {
                if f(&mut open) >= 0 {
                    wait.hAdapter = open.hAdapter;
                    wait.VidPnSourceId = open.VidPnSourceId;
                    kmt_ok = true;
                }
            }
            ReleaseDC(NULL, dc);
        }
    }
    while !sh.quit_vblank.load(Relaxed) {
        let t0 = qpc();
        let period_ns = sh.period_fs.load(Relaxed) / 1_000_000;
        match mode {
            Vsync::Dwm => { if let Some(f) = d.dwm_flush { unsafe { f(); } } }
            Vsync::Kmt if kmt_ok => { if let Some(f) = d.kmt_wait { unsafe { f(&wait); } } }
            _ => unsafe { Sleep((period_ns / 1_000_000).max(1) as u32) },
        }
        // A vblank wait that returns far too fast means the source is not really
        // pacing us (composition off, window minimised, a stub driver). Back off to
        // a timer for that tick rather than spinning a core at 100 %.
        let dt_ns = ((qpc() - t0) as u128 * 1_000_000_000u128 / qpf() as u128) as u64;
        if dt_ns * 4 < period_ns { unsafe { Sleep(((period_ns - dt_ns * 4) / 1_000_000).max(1) as u32) } }
        unsafe { SetEvent(ev); }
    }
}

// ---- refresh period ------------------------------------------------------------
/// Nominal period in femtoseconds. DWM's exact rational when available; otherwise
/// the mode's integer Hz, corrected for the /1001 rates whose truncation it is.
fn query_period_fs(d: &Dyn) -> u64 {
    unsafe {
        if let Some(f) = d.dwm_timing {
            let mut t = DWM_TIMING_INFO::default();
            // NULL hwnd: per-window queries were removed after Vista/7, and the
            // composition rate is a property of the display, not of our window.
            if f(NULL, &mut t) >= 0 && t.rateRefresh_num != 0 && t.rateRefresh_den != 0 {
                return FS * t.rateRefresh_den as u64 / t.rateRefresh_num as u64;
            }
        }
        let mut dm = DEVMODEW::default();
        dm.dmSize = core::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(core::ptr::null(), ENUM_CURRENT_SETTINGS, &mut dm) != 0 && dm.dmDisplayFrequency > 1 {
            let hz = dm.dmDisplayFrequency as u64;
            // 59 is always the truncation of 60000/1001, never a real 59 Hz mode.
            let (num, den) = match hz { 23 | 29 | 47 | 59 | 71 | 89 | 119 | 143 => ((hz + 1) * 1000, 1001), _ => (hz * 1000, 1000) };
            return FS * den / num;
        }
    }
    FS / 60
}

// ---- the pump ------------------------------------------------------------------
thread_local! {
    static PUMP: Cell<*const Pump> = const { Cell::new(core::ptr::null()) };
}

/// All mutable pump state is in Cells and every method takes &self, because
/// DefWindowProc re-enters the window procedure (Alt+F4 becomes WM_SYSCOMMAND
/// becomes WM_CLOSE) and a &mut Pump would alias itself when it does.
struct Pump {
    sh: Arc<Shared>,
    core: Arc<Core>,
    hwnd: Cell<HWND>,
    dyn_: Arc<Dyn>,
    cursor: HANDLE,
    /// RefCell, not Cell: the presenter is too big to copy and needs &mut. Nothing
    /// re-enters it (no method here calls DefWindowProc), and every use try_borrows
    /// anyway so a future one cannot deadlock the message loop.
    present: core::cell::RefCell<Present>,
    /// What run() sleeps on: the swapchain's waitable object on the capable path,
    /// the vblank thread's event on the compatible one.
    wait_handle: HANDLE,
    buttons: Cell<u32>,
    last_refresh: Cell<u64>,
    last_tick_ns: Cell<u64>,
    have_refresh: Cell<bool>,
    in_size_move: Cell<bool>,
    cursor_hidden: Cell<bool>,
    fs_applied: Cell<bool>,
    saved_rect: Cell<RECT>,
    saved_style: Cell<isize>,
    saved_ex: Cell<isize>,
    icon_big: Cell<usize>,
    icon_small: Cell<usize>,
    debug: bool,
}

impl Pump {
    // ---- keyboard ---------------------------------------------------------------
    /// Layout-resolved text for this keypress, and whether it armed a dead key.
    ///
    /// ToUnicodeEx has a side effect: it consumes and rewrites the kernel's dead-key
    /// state for the layout. That makes it dangerous to call speculatively or twice
    /// for one key, but calling it exactly once per real WM_KEYDOWN is precisely
    /// what the system does for itself, so composition behaves natively. This is
    /// also why the message loop does NOT call TranslateMessage: TranslateMessage
    /// consumes the same state to make WM_CHAR, and running both would compose
    /// every dead key twice.
    fn text_for(&self, vk: u32, sc: u32, ext: bool) -> (String, bool) {
        unsafe {
            let mut state = [0u8; 256];
            if GetKeyboardState(state.as_mut_ptr()) == 0 { return (String::new(), false); }
            let hkl = GetKeyboardLayout(0);
            let mut buf = [0u16; 8];
            let n = ToUnicodeEx(vk, sc | if ext { 0x100 } else { 0 }, state.as_ptr(), buf.as_mut_ptr(), buf.len() as i32, 0, hkl);
            if n < 0 { return (String::new(), true); }        // negative: a dead key is now armed
            if n == 0 { return (String::new(), false); }
            let s: String = char::decode_utf16(buf[..n as usize].iter().copied())
                .filter_map(|c| c.ok())
                .filter(|c| (*c as u32) >= 0x20 && *c as u32 != 0x7f)
                .collect();
            (s, false)
        }
    }

    /// The keysym Core needs, which it uses for exactly one thing we must preserve:
    /// deciding copy/cut/paste from the CHARACTER a key produces rather than its
    /// position, so the shortcut follows the layout on AZERTY or Dvorak.
    /// MAPVK_VK_TO_CHAR gives the unmodified character without touching dead-key
    /// state, which ToUnicodeEx with Ctrl cleared could not do safely.
    fn sym_for(&self, vk: u32) -> u32 {
        let c = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) };
        // High bit set means the key is a dead key. Windows composes those itself,
        // so we must not hand Core a dead keysym or it would arm its own engine too.
        if c == 0 || c & 0x8000_0000 != 0 { return 0; }
        char::from_u32(c & 0x7FFF_FFFF).and_then(keysym::from_char).unwrap_or(0)
    }

    fn key_down(&self, vk: u32, l: LPARAM) {
        // lParam bit 30 is the previous key state: set means this is the OS's own
        // autorepeat. We drop those and let Core::repeat_tick be the only repeat
        // engine, or every held key would produce two streams of input.
        if l & 0x4000_0000 != 0 { return; }
        let sc = ((l >> 16) & 0xFF) as u32;
        let ext = (l >> 24) & 1 != 0;
        let code = to_evdev(sc, ext, vk);
        let ctrl = self.core.key_down(KEY_LEFTCTRL) || self.core.key_down(KEY_RIGHTCTRL);
        let alt = self.core.key_down(KEY_LEFTALT) || self.core.key_down(KEY_RIGHTALT);
        // AltGr arrives as Ctrl+Alt and must still produce text; Ctrl or Alt alone
        // must not, and asking ToUnicodeEx would consume dead-key state for nothing.
        let want_text = (ctrl && alt) || (!ctrl && !alt);
        let (text, dead) = if want_text { self.text_for(vk, sc, ext) } else { (String::new(), false) };
        let sym = self.sym_for(vk);
        self.core.set_dead_flag(dead);
        self.core.key_press_sym(code, sym, &text, now_ns());
    }

    fn key_up(&self, vk: u32, l: LPARAM) {
        let sc = ((l >> 16) & 0xFF) as u32;
        let ext = (l >> 24) & 1 != 0;
        self.core.key(to_evdev(sc, ext, vk), false);
    }

    fn refresh_repeat(&self) {
        unsafe {
            let mut delay_idx: u32 = 1;
            let mut speed_idx: u32 = 31;
            SystemParametersInfoW(SPI_GETKEYBOARDDELAY, 0, &mut delay_idx as *mut u32 as *mut _, 0);
            SystemParametersInfoW(SPI_GETKEYBOARDSPEED, 0, &mut speed_idx as *mut u32 as *mut _, 0);
            // Documented ranges: delay index 0..3 is 250 ms to 1000 ms in 250 ms
            // steps; speed index 0..31 is about 2.5 to 30 repeats per second.
            let delay_ms = 250 * (delay_idx.min(3) as u64 + 1);
            let cps_x10 = 25 + (275 * speed_idx.min(31) as u64) / 31;
            let interval_ms = (10_000 / cps_x10).max(1);
            self.core.set_repeat(delay_ms, interval_ms);
            if self.debug { eprintln!("softer_gui: repeat delay {delay_ms} ms interval {interval_ms} ms"); }
        }
    }

    // ---- pointer ----------------------------------------------------------------
    fn button(&self, code: u32, down: bool) {
        let bit = 1u32 << (code & 31);
        let mut held = self.buttons.get();
        if down { held |= bit } else { held &= !bit }
        self.buttons.set(held);
        unsafe {
            // Without capture, a drag that leaves the window silently stops reporting.
            if down { SetCapture(self.hwnd.get()); } else if held == 0 { ReleaseCapture(); }
        }
        self.core.key(code, down);
    }

    fn motion(&self, l: LPARAM) {
        let x = (l & 0xFFFF) as u16 as i16 as i64;
        let y = ((l >> 16) & 0xFFFF) as u16 as i16 as i64;
        // Core wants 32.32 fixed; WM_MOUSEMOVE only carries whole client pixels, so
        // the crate's sub-pixel precision is real but never populated on this path.
        self.core.pointer_abs(x << 32, y << 32);
    }

    // ---- scale ------------------------------------------------------------------
    fn dpi(&self) -> u32 {
        unsafe {
            if let Some(f) = self.dyn_.dpi_for_window {
                let d = f(self.hwnd.get());
                if d != 0 { return d; }
            }
            let dc = GetDC(self.hwnd.get());
            if !dc.is_null() {
                let d = GetDeviceCaps(dc, LOGPIXELSX);
                ReleaseDC(self.hwnd.get(), dc);
                if d > 0 { return d as u32; }
            }
            96
        }
    }
    fn update_scale(&self) {
        let fx = self.dpi() * 65536 / 96;
        if fx != self.core.scale_fx.swap(fx, Relaxed) { self.core.full_redraw.store(true, Relaxed); }
    }

    fn update_size(&self) {
        let mut r = RECT::default();
        unsafe { GetClientRect(self.hwnd.get(), &mut r) };
        let (w, h) = ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32);
        if w == 0 || h == 0 { return; }
        if w != self.core.win_w.load(Relaxed) || h != self.core.win_h.load(Relaxed) {
            self.core.win_w.store(w, Relaxed);
            self.core.win_h.store(h, Relaxed);
            self.core.full_redraw.store(true, Relaxed);
        }
    }

    // ---- outward state the app asked for ----------------------------------------
    fn apply_cursor(&self) {
        let want = self.core.cursor_hidden.load(Relaxed);
        if want != self.cursor_hidden.get() {
            self.cursor_hidden.set(want);
            unsafe { SetCursor(if want { NULL } else { self.cursor }); }
        }
    }

    fn apply_fullscreen(&self) {
        let want = self.core.wants_fullscreen.load(Relaxed);
        if want == self.fs_applied.get() { return; }
        self.fs_applied.set(want);
        let h = self.hwnd.get();
        unsafe {
            if want {
                let mut r = RECT::default();
                GetWindowRect(h, &mut r);
                self.saved_rect.set(r);
                self.saved_style.set(get_window_long(h, GWL_STYLE));
                self.saved_ex.set(get_window_long(h, GWL_EXSTYLE));
                // Borderless fullscreen window, not a mode change: it alt-tabs
                // cleanly and modern compositors give it the same fast path anyway.
                let mut mi = MONITORINFO { cbSize: core::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
                if GetMonitorInfoW(mon, &mut mi) == 0 { return; }
                set_window_long(h, GWL_STYLE, (WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS) as isize);
                set_window_long(h, GWL_EXSTYLE, WS_EX_APPWINDOW as isize);
                let m = mi.rcMonitor;
                SetWindowPos(h, NULL, m.left, m.top, m.right - m.left, m.bottom - m.top, SWP_NOZORDER | SWP_FRAMECHANGED);
            } else {
                set_window_long(h, GWL_STYLE, self.saved_style.get());
                set_window_long(h, GWL_EXSTYLE, self.saved_ex.get());
                let r = self.saved_rect.get();
                SetWindowPos(h, NULL, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_FRAMECHANGED);
            }
        }
        self.update_size();
    }

    /// Build an HICON straight from our ARGB pixels. No PE resource is involved,
    /// which is the point: the icon travels in the binary's data, not its
    /// resource directory, so a single-file build needs no post-processing.
    fn make_icon(img: &crate::icon::OwnedIcon) -> HICON {
        unsafe {
            let side = img.side as i32;
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = side;
            bmi.bmiHeader.biHeight = -side;          // top-down, as icon.rs stores it
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let color = CreateDIBSection(NULL, &bmi, DIB_RGB_COLORS, &mut bits, NULL, 0);
            if color.is_null() || bits.is_null() { return NULL; }
            // 0xAARRGGBB is B,G,R,A in memory, which is exactly the DIB's layout.
            core::ptr::copy_nonoverlapping(img.argb.as_ptr(), bits as *mut u32, img.argb.len());
            // A 32bpp icon still needs a mask bitmap; the alpha channel does the
            // real work, so an all-zero (fully opaque) mask is correct.
            let mask = CreateBitmap(side, side, 1, 1, core::ptr::null());
            let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
            let icon = CreateIconIndirect(&info);
            DeleteObject(color);
            DeleteObject(mask);
            icon
        }
    }

    fn apply_icon(&self) {
        let Some(set) = self.core.take_icon() else { return };
        let h = self.hwnd.get();
        unsafe {
            // Largest first (own_set sorts); Windows scales for the small slot.
            let big = set.first().map(Self::make_icon).unwrap_or(NULL);
            let small = set.last().map(Self::make_icon).unwrap_or(NULL);
            SendMessageW(h, WM_SETICON, ICON_BIG, big as LPARAM);
            SendMessageW(h, WM_SETICON, ICON_SMALL, small as LPARAM);
            let (ob, os) = (self.icon_big.replace(big as usize), self.icon_small.replace(small as usize));
            if ob != 0 { DestroyIcon(ob as HICON); }
            if os != 0 { DestroyIcon(os as HICON); }
        }
    }

    /// Put the app's most recent frame on screen from the PUMP thread, which is the
    /// only thread that can do it without blocking while the modal loop is running.
    /// A no-op when there is nothing new, so the main loop can call it every pass.
    fn blit(&self) {
        if !self.sh.dirty.load(Acquire) { return; }
        let (w, h) = (self.core.win_w.load(Relaxed) as i32, self.core.win_h.load(Relaxed) as i32);
        if w <= 0 || h <= 0 { return; }
        // try_lock, never lock. The pump must not block on a mutex the app holds:
        // the app can be part-way through reallocating the buffer, and a pump that
        // waits is a window that stops answering Windows. Leaving `dirty` set and
        // re-posting means the frame is retried rather than dropped.
        let Ok(g) = self.sh.surf.try_lock() else {
            unsafe { PostMessageW(self.hwnd.get(), WM_SOFTER_BLIT, 0, 0) };
            return;
        };
        let Some(s) = g.as_ref() else { return };
        self.sh.dirty.store(false, Release);
        let Ok(mut pres) = self.present.try_borrow_mut() else { return };
        match &mut *pres {
            Present::D3d(d) => {
                // A failed present means the device went away (driver update, GPU
                // reset, hibernate). Drop to the compatible path rather than dying:
                // it needs no device and the window keeps working.
                if !d.present(s.pixels as *const u32, s.side, w.min(s.side as i32) as u32, h.min(s.side as i32) as u32) {
                    if self.debug { eprintln!("softer_gui: d3d present failed, falling back to GDI"); }
                    *pres = Present::Gdi;
                    self.core.full_redraw.store(true, Relaxed);
                }
            }
            Present::Gdi => unsafe {
                let dc = GetDC(self.hwnd.get());
                if dc.is_null() { return; }
                BitBlt(dc, 0, 0, w.min(s.side as i32), h.min(s.side as i32), s.memdc, 0, 0, SRCCOPY);
                GdiFlush();
                ReleaseDC(self.hwnd.get(), dc);
            }
        }
    }

    // ---- the frame boundary -----------------------------------------------------
    /// One refresh passed. This is the only place display time moves.
    ///
    /// `gated` means "advance ONLY if a refresh boundary has demonstrably passed".
    /// The vblank thread already guarantees that, so the normal path passes false
    /// and keeps its long-standing behaviour of always advancing at least one
    /// period. The resize timer cannot guarantee it (it fires about twice per
    /// refresh, and Windows does not promise even that), so it passes true and lets
    /// this function decide. Getting that backwards would run display time at the
    /// timer's rate instead of the display's, which is worse than freezing it.
    fn frame_tick(&self, gated: bool) {
        let mut frames = 1u64;
        let mut counted = false;
        // The capable path contributes two things here and takes its refresh count
        // from DWM below, which is not the arrangement that looks obvious.
        //
        // The obvious one is to pace off the swapchain's own PresentRefreshCount,
        // since that is the true per-present MSC. Measured, it is not usable as a
        // per-frame clock: DXGI updates those counters in bursts, sitting still for
        // about sixty calls and then jumping sixty at once, so the drop accounting
        // is always a beat behind. DWM's cRefresh is live every frame and gives
        // exactly one period per frame (measured: 480 intervals of 1, no exceptions).
        //
        // So each source is used for what it is actually good at. The swapchain
        // supplies the WAIT, which is the thing DWM cannot give, and the count of
        // presents still in flight. DWM supplies the CLOCK. What the swapchain
        // measures stays the fallback for a host with no DWM composition.
        let mut d3d_frames: Option<u64> = None;
        if let Ok(mut pres) = self.present.try_borrow_mut() {
            if let Present::D3d(d) = &mut *pres {
                d3d_frames = Some(d.frames_elapsed());
                // Presents submitted but not yet shown: the same quantity the X11
                // backend keeps from PresentCompleteNotify, and the reason
                // Core::in_flight is meaningful on this path and inert on the other.
                self.core.in_flight.store(d.in_flight, Relaxed);
                if gated && d.occluded { return; }
            }
        }
        if let Some(f) = self.dyn_.dwm_timing {
            let mut t = DWM_TIMING_INFO::default();
            if unsafe { f(NULL, &mut t) } >= 0 && t.cRefresh != 0 {
                if t.rateRefresh_num != 0 && t.rateRefresh_den != 0 {
                    let p = FS * t.rateRefresh_den as u64 / t.rateRefresh_num as u64;
                    if p != self.sh.period_fs.load(Relaxed) {
                        self.sh.period_fs.store(p, Relaxed);
                        self.core.set_period_fs(p);
                    }
                }
                if self.have_refresh.get() {
                    // cRefresh is a monotonic vblank counter: its delta IS the number
                    // of whole refresh periods that elapsed, the same thing X11
                    // Present's MSC delta gives us.
                    frames = t.cRefresh.saturating_sub(self.last_refresh.get());
                    counted = true;
                }
                self.last_refresh.set(t.cRefresh);
                self.have_refresh.set(true);
            }
        }
        if counted {
            // The counter has not moved, so there is no frame boundary to stamp.
            // This is the whole reason the resize timer may poll as fast as it likes.
            if frames == 0 && gated { return; }
            // Clamped because a suspend or a mode change can jump the counter
            // arbitrarily, and display time must not absorb a discontinuity.
            frames = frames.clamp(1, 8);
        } else if let Some(n) = d3d_frames {
            // No DWM clock, but there is a swapchain: use what it measured.
            frames = n.clamp(1, 8);
        } else if gated {
            // Neither counter to consult, so gate on the wall clock instead.
            // Coarser, but it still refuses to invent frames.
            let now = now_ns();
            let period_ns = self.sh.period_fs.load(Relaxed) / 1_000_000;
            if now.saturating_sub(self.last_tick_ns.get()) < period_ns { return; }
        }
        self.last_tick_ns.set(now_ns());
        self.core.display_tick(frames);
        self.core.push_render();
    }

    // ---- window procedure --------------------------------------------------------
    fn message(&self, h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_ERASEBKGND => return 1,          // we paint every pixel; let GDI not flicker
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                unsafe { BeginPaint(h, &mut ps) };
                let (cw, ch) = (self.core.win_w.load(Relaxed) as i32, self.core.win_h.load(Relaxed) as i32);
                // try_lock, never lock: if the app is mid-reallocation we skip this
                // repaint rather than stalling the message loop. Nothing is lost;
                // the app submits again and WM_SOFTER_BLIT puts it up.
                if let Ok(g) = self.sh.surf.try_lock() {
                    if let Some(s) = g.as_ref() {
                        unsafe { BitBlt(ps.hdc, 0, 0, cw.min(s.side as i32), ch.min(s.side as i32), s.memdc, 0, 0, SRCCOPY); }
                    }
                }
                unsafe { EndPaint(h, &ps) };
                return 0;
            }
            WM_SIZE => {
                if w != SIZE_MINIMIZED {
                    self.update_size();
                    if let Ok(mut pres) = self.present.try_borrow_mut() {
                        if let Present::D3d(d) = &mut *pres {
                            d.resize(self.core.win_w.load(Relaxed), self.core.win_h.load(Relaxed));
                        }
                    }
                    // A size step inside the drag is a chance to advance the clock
                    // early rather than waiting for the next timer tick, which keeps
                    // motion smooth exactly while the mouse is moving.
                    if self.in_size_move.get() { self.frame_tick(true); self.blit(); }
                }
                return 0;
            }
            WM_ENTERSIZEMOVE => {
                self.in_size_move.set(true);
                // Windows runs a MODAL loop while the user drags a border or a
                // corner: DefWindowProc does not return until the drag ends, so
                // run() below stops looping and nothing drives the frame clock. A
                // timer is the one thing Windows still delivers inside that loop, so
                // it becomes the frame source for the duration. Without it the app
                // keeps redrawing one frozen timestamp: the window resizes but the
                // content stops moving, which reads as a hang.
                unsafe { SetTimer(h, RESIZE_TIMER, RESIZE_TIMER_MS, core::ptr::null()) };
                return 0;
            }
            WM_EXITSIZEMOVE => {
                self.in_size_move.set(false);
                unsafe { KillTimer(h, RESIZE_TIMER) };
                self.update_size();
                self.blit();
                return 0;
            }
            WM_TIMER if w == RESIZE_TIMER => {
                // Everything run() would be doing for us, for as long as it cannot.
                self.core.service_resync();
                self.apply_cursor();
                self.core.repeat_tick(now_ns());
                self.frame_tick(true);
                self.blit();
                return 0;
            }
            WM_CLOSE => { self.core.push_close(); return 0; }   // the app decides, not us
            WM_DESTROY => { self.core.quit.store(true, Relaxed); return 0; }
            WM_SETFOCUS => { self.core.focused.store(true, Relaxed); return 0; }
            WM_KILLFOCUS => {
                // Alt+Tab can steal focus mid-keypress and the WM_KEYUP never comes.
                // Keys left set would make our own autorepeat run forever.
                self.core.focused.store(false, Relaxed);
                self.core.release_all_keys(true);
                return 0;
            }
            WM_ACTIVATEAPP => { self.core.focused.store(w != 0, Relaxed); return 0; }
            WM_SETCURSOR => {
                // Low word of lParam is the hit-test code; 1 is HTCLIENT.
                if (l & 0xFFFF) == 1 {
                    unsafe { SetCursor(if self.core.cursor_hidden.load(Relaxed) { NULL } else { self.cursor }) };
                    return 1;
                }
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                self.key_down(w as u32, l);
                // Alt+F4 and the window menu still have to work, so system keys fall
                // through to DefWindowProc; plain keys do not need it.
                if msg == WM_KEYDOWN { return 0; }
            }
            WM_KEYUP | WM_SYSKEYUP => {
                self.key_up(w as u32, l);
                if msg == WM_KEYUP { return 0; }
            }
            WM_SYSCOMMAND => {
                // Plain Alt opening the window menu steals focus and beeps; the
                // screensaver starting under a game is worse.
                let sc = w & 0xFFF0;
                if sc == SC_KEYMENU || sc == SC_SCREENSAVE || sc == SC_MONITORPOWER { return 0; }
            }
            WM_MOUSEMOVE => { self.motion(l); return 0; }
            WM_LBUTTONDOWN => { self.button(BTN_LEFT, true); return 0; }
            WM_LBUTTONUP => { self.button(BTN_LEFT, false); return 0; }
            WM_RBUTTONDOWN => { self.button(BTN_RIGHT, true); return 0; }
            WM_RBUTTONUP => { self.button(BTN_RIGHT, false); return 0; }
            WM_MBUTTONDOWN => { self.button(BTN_MIDDLE, true); return 0; }
            WM_MBUTTONUP => { self.button(BTN_MIDDLE, false); return 0; }
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let code = if (w >> 16) & 0xFFFF == 1 { BTN_SIDE } else { BTN_EXTRA };
                self.button(code, msg == WM_XBUTTONDOWN);
                return 1;
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = ((w >> 16) & 0xFFFF) as u16 as i16 as i32;
                // 120 units is one click and one click is SCROLL_STEP, so the scale
                // is exact and a high-resolution wheel's sub-click deltas convert
                // without rounding or a leftover accumulator.
                let v = delta * (SCROLL_STEP / WHEEL_DELTA);
                // Ours is positive when the content moves up or left; Windows is
                // positive when the wheel goes forward or the tilt goes right.
                let axis = if msg == WM_MOUSEWHEEL { AXIS_SCROLL_V } else { AXIS_SCROLL_H };
                let delta = if msg == WM_MOUSEWHEEL { -v } else { v };
                self.core.push_axes(&[AxisDiff { axis, delta }]);
                return 0;
            }
            WM_DPICHANGED => {
                self.update_scale();
                // lParam is the rectangle Windows suggests for the new DPI; honouring
                // it is what keeps the window the same physical size across monitors.
                let r = unsafe { &*(l as *const RECT) };
                unsafe { SetWindowPos(h, NULL, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
                return 0;
            }
            WM_DISPLAYCHANGE => {
                let p = query_period_fs(&self.dyn_);
                self.sh.period_fs.store(p, Relaxed);
                self.core.set_period_fs(p);
                self.have_refresh.set(false);       // counters may have discontinued
                return 0;
            }
            WM_SETTINGCHANGE => { self.refresh_repeat(); return 0; }
            WM_SOFTER_BLIT => { self.blit(); return 0; }
            WM_SOFTER_POKE => return 0,
            _ => {}
        }
        unsafe { DefWindowProcW(h, msg, w, l) }
    }

    fn run(&self) {
        PUMP.with(|c| c.set(self as *const Pump));
        let vblank = self.wait_handle;
        loop {
            if self.core.quit.load(Relaxed) { break; }
            self.core.service_resync();
            // Drain every pending message. TranslateMessage is deliberately absent:
            // it would consume the dead-key state that text_for() needs (see there).
            let mut m = MSG::default();
            while unsafe { PeekMessageW(&mut m, NULL, 0, 0, PM_REMOVE) } != 0 {
                unsafe { DispatchMessageW(&m) };
                if self.core.quit.load(Relaxed) { break; }
            }
            if self.core.quit.load(Relaxed) { break; }
            self.apply_cursor();
            self.apply_fullscreen();
            self.apply_icon();
            // NOT self.blit() here. Outside the modal loop the app thread does its
            // own blit, and touching this window through GDI from the pump while it
            // is NOT processing a message deadlocks against another thread calling
            // SetWindowPos: that call holds the window lock and waits for us to pump,
            // while GetDC here waits for that same lock. Pump-side blitting stays
            // inside message handlers, which is exactly where the modal loop needs it.
            let ms = self.core.repeat_tick(now_ns());
            // The structural analog of poll() in the Linux pump. MWMO_INPUTAVAILABLE
            // is not optional: without it a message that arrives between the drain
            // above and this wait is marked already-seen and we sleep through it,
            // which shows up as a window that occasionally stops responding.
            let r = unsafe { MsgWaitForMultipleObjectsEx(1, &vblank, ms as u32, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
            if r == WAIT_OBJECT_0 { self.frame_tick(false); }
        }
        unsafe {
            let b = self.icon_big.get(); if b != 0 { DestroyIcon(b as HICON); }
            let s = self.icon_small.get(); if s != 0 { DestroyIcon(s as HICON); }
            let h = self.hwnd.get();
            if !h.is_null() { DestroyWindow(h); }
        }
        PUMP.with(|c| c.set(core::ptr::null()));
    }
}

/// Two shims over one body, because the ABI is not the same in both builds and an
/// ABI cannot be selected by attribute. See WNDPROC in sys_win.rs.
#[cfg(not(all(cosmo, target_arch = "x86_64")))]
unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { wndproc_impl(h, msg, w, l) }
}
#[cfg(all(cosmo, target_arch = "x86_64"))]
unsafe extern "win64" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { wndproc_impl(h, msg, w, l) }
}

unsafe fn wndproc_impl(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let p = PUMP.with(|c| c.get());
    // Messages arrive during CreateWindowExW, before the pointer is published.
    if p.is_null() { return unsafe { DefWindowProcW(h, msg, w, l) }; }
    unsafe { (*p).message(h, msg, w, l) }
}

// ---- open ------------------------------------------------------------------------
pub fn open(core: Arc<Core>, title: &str, app_id: &str, width: u32, height: u32) -> Option<App> {
    let _ = app_id;                 // Windows has no WM_CLASS analog worth setting here
    set_dpi_aware();
    let d = Arc::new(Dyn::load());
    let period = query_period_fs(&d);
    core.set_period_fs(period);
    core.win_w.store(width, Relaxed);
    core.win_h.store(height, Relaxed);

    let vblank_ev = unsafe { CreateEventW(core::ptr::null_mut(), 0, 0, core::ptr::null()) };
    if vblank_ev.is_null() { return None; }

    let sh = Arc::new(Shared {
        hwnd: AtomicUsize::new(0),
        surf: Mutex::new(None),
        ready: Mutex::new(0),
        ready_cv: Condvar::new(),
        period_fs: AtomicU64::new(period),
        dyn_: d.clone(),
        dirty: AtomicBool::new(false),
        quit_vblank: AtomicBool::new(false),
        vblank_ev: AtomicUsize::new(vblank_ev as usize),
        generation: AtomicU32::new(0),
    });

    let title = title.to_string();
    let sh2 = sh.clone();
    let core2 = core.clone();
    std::thread::Builder::new().name("softer_gui-win-pump".into()).spawn(move || {
        let dbg = debug();
        let ok = unsafe {
            let inst = GetModuleHandleW(core::ptr::null());
            let class = wide("softer_gui_window");
            let cursor = LoadCursorW(NULL, IDC_ARROW);
            let wc = WNDCLASSEXW {
                cbSize: core::mem::size_of::<WNDCLASSEXW>() as u32,
    // Deliberately NOT CS_OWNDC: one DC shared between the pump and
                // the app thread is a deadlock waiting to happen (see App::submit).
                style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0, cbWndExtra: 0,
                hInstance: inst,
                hIcon: NULL, hCursor: cursor, hbrBackground: NULL,
                lpszMenuName: core::ptr::null(),
                lpszClassName: class.as_ptr(),
                hIconSm: NULL,
            };
            // A second window in one process re-registers the class; that is a
            // benign failure and CreateWindowExW still finds the first registration.
            RegisterClassExW(&wc);
            let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS;
            // CreateWindowExW sizes the whole window; we were given a client size.
            let mut r = RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
            AdjustWindowRectEx(&mut r, style, 0, 0);
            let t = wide(&title);
            let hwnd = CreateWindowExW(0, class.as_ptr(), t.as_ptr(), style,
                                       i32::MIN, i32::MIN,            // CW_USEDEFAULT
                                       r.right - r.left, r.bottom - r.top,
                                       NULL, NULL, inst, core::ptr::null_mut());
            if hwnd.is_null() { None } else { Some((hwnd, cursor)) }
        };

        let Some((hwnd, cursor)) = ok else {
            *sh2.ready.lock().unwrap() = 2;
            sh2.ready_cv.notify_all();
            return;
        };
        sh2.hwnd.store(hwnd as usize, Relaxed);

        // Which of the two Windows backends this run gets. The capable one is
        // tried first and any failure at all falls through to the compatible one,
        // because every reason it can fail (pre-8.1, no d3d11.dll, a driver that
        // will not make a flip-model swapchain, a remote session) has the same
        // answer. SOFTER_GUI_WIN=gdi forces the compatible path, which is how the
        // Vista-era code stays testable on a machine that is not Vista.
        let want = std::env::var("SOFTER_GUI_WIN").unwrap_or_default();
        // Both builds try the capable path. An APE gets it through WARP rather
        // than the vendor driver, for the reason set out in win_d3d.rs; that is a
        // driver problem, not a D3D11 or cosmo one, and WARP measures as well
        // there as hardware does natively.
        let try_d3d = want != "gdi";
        let present = if !try_d3d { Present::Gdi } else {
            match D3d::new(hwnd, dbg) {
                Some(d) => Present::D3d(d),
                None => {
                    if want == "d3d" { eprintln!("softer_gui: d3d requested but unavailable; using GDI"); }
                    Present::Gdi
                }
            }
        };
        let use_d3d = matches!(present, Present::D3d(_));
        // The capable path waits on the swapchain itself, so it needs no vblank
        // thread; the compatible one does. This is the one structural difference
        // between them beyond the pixels.
        let wait_handle = match &present {
            Present::D3d(d) => d.waitable,
            Present::Gdi => sh2.vblank_ev.load(Relaxed) as HANDLE,
        };

        let pump = Pump {
            sh: sh2.clone(), core: core2, hwnd: Cell::new(hwnd), dyn_: d.clone(),
            cursor,
            present: core::cell::RefCell::new(present),
            wait_handle,
            buttons: Cell::new(0), last_refresh: Cell::new(0), last_tick_ns: Cell::new(0), have_refresh: Cell::new(false),
            in_size_move: Cell::new(false), cursor_hidden: Cell::new(false), fs_applied: Cell::new(false),
            saved_rect: Cell::new(RECT::default()), saved_style: Cell::new(0), saved_ex: Cell::new(0),
            icon_big: Cell::new(0), icon_small: Cell::new(0),
            debug: dbg,
        };
        pump.refresh_repeat();
        pump.update_scale();
        unsafe { ShowWindow(hwnd, SW_SHOW); UpdateWindow(hwnd); }
        pump.update_size();

        let vb = if use_d3d {
            if dbg { eprintln!("softer_gui: presenter d3d11 flip-model, waitable object, period {} fs ({:.3} Hz)", period, FS as f64 / period as f64); }
            None
        } else {
            let mode = choose_vsync(&d);
            if dbg { eprintln!("softer_gui: presenter gdi, vsync {:?}, period {} fs ({:.3} Hz)", mode, period, FS as f64 / period as f64); }
            let shv = sh2.clone();
            std::thread::Builder::new().name("softer_gui-win-vblank".into()).spawn(move || vblank_thread(shv, mode)).ok()
        };

        *sh2.ready.lock().unwrap() = 1;
        sh2.ready_cv.notify_all();

        pump.run();
        sh2.quit_vblank.store(true, Relaxed);
        if let Some(vb) = vb { let _ = vb.join(); }
        unsafe { CloseHandle(sh2.vblank_ev.load(Relaxed) as HANDLE); }
    }).ok()?;

    // Wait for the window to exist before handing the app a Gui it can draw into.
    let mut g = sh.ready.lock().ok()?;
    while *g == 0 { g = sh.ready_cv.wait(g).ok()?; }
    if *g != 1 { return None; }
    drop(g);

    Some(App { sh, core, side: 0, generation: 0 })
}
