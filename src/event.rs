//! The event model: one totally ordered SPSC ring carrying input AND frame
//! boundaries, stamped with DISPLAY time; the pump-side keyboard state machine
//! (absolute button bitmap, layout text, dead keys, WM-driven autorepeat).
//!
//! Display time: 128-bit femtoseconds that advance ONLY at frame boundaries and
//! only in whole refresh periods, so consecutive RENDERs on a 60 Hz monitor are
//! exactly 1/60 apart. It is not the system clock and drifts from it — the two
//! answer different questions ("when will this be shown" vs "what time is it").
//! Every event within one frame shares that frame's time; equal timestamps mean
//! the same frame: re-render, do not step.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::*};
use crate::keysym;

#[cfg(target_os = "linux")]
fn now_ns() -> u64 { crate::sys::clock_monotonic_ns() }
#[cfg(not(target_os = "linux"))]
fn now_ns() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) }

pub const RING: usize = 256;

// ---- evdev codes the library itself refers to ------------------------------
pub const KEY_ESC: u32 = 1;
pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_TAB: u32 = 15;
pub const KEY_ENTER: u32 = 28;
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_RIGHTSHIFT: u32 = 54;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_RIGHTCTRL: u32 = 97;
pub const KEY_RIGHTALT: u32 = 100;
pub const KEY_HOME: u32 = 102;
pub const KEY_UP: u32 = 103;
pub const KEY_PAGEUP: u32 = 104;
pub const KEY_LEFT: u32 = 105;
pub const KEY_RIGHT: u32 = 106;
pub const KEY_END: u32 = 107;
pub const KEY_DOWN: u32 = 108;
pub const KEY_PAGEDOWN: u32 = 109;
pub const KEY_INSERT: u32 = 110;
pub const KEY_DELETE: u32 = 111;
pub const KEY_LEFTMETA: u32 = 125;
pub const KEY_RIGHTMETA: u32 = 126;
pub const KEY_F11: u32 = 87;
pub const KEY_F12: u32 = 88;
pub const KEY_UNKNOWN: u32 = 240;
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

// ---- continuous axes ---------------------------------------------------------
/// Absolute pointer position, 24.8 fixed pixels, window-local. (Also on every RENDER.)
pub const AXIS_MOUSE_X: u32 = 0;
pub const AXIS_MOUSE_Y: u32 = 1;
/// Scroll, 24.8 fixed pixels; positive = content moves up/left (finger down / wheel down).
/// One wheel click is SCROLL_STEP.
pub const AXIS_SCROLL_V: u32 = 2;
pub const AXIS_SCROLL_H: u32 = 3;
/// Touchpad pinch: change of scale factor, 16.16 fixed (+6554 = +10 %).
pub const AXIS_ZOOM: u32 = 4;
/// Touchpad rotate: degrees, 16.16 fixed, clockwise positive.
pub const AXIS_ROTATE: u32 = 5;
/// One wheel click in SCROLL units (15 px in 24.8).
pub const SCROLL_STEP: i32 = 15 * 256;

pub const MODE_DEAD_KEY: u8 = 1;

pub const CP_COPY: u32 = 1;
pub const CP_CUT: u32 = 2;
pub const CP_PASTE: u32 = 3;

#[derive(Clone, Copy, Debug, Default)]
pub struct Render {
    pub width: u32,
    pub height: u32,
    /// UI scale, 16.16 fixed (65536 = 1.0).
    pub scale_fx: u32,
    /// Pointer, 32.32 fixed, window-local. Integer pixel = v >> 32.
    pub cursor_x: i64,
    pub cursor_y: i64,
    /// Nominal frame period (1 s / refresh rate), femtoseconds. Not a measurement.
    pub dt_fs: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Buttons {
    /// Absolute snapshot of all 512 discrete inputs (keys and mouse buttons in one evdev code space).
    pub bits: [u64; 8],
    /// MODE_* flags — armed dead key etc.
    pub modes: u8,
}
impl Buttons {
    #[inline] pub fn get(&self, code: u32) -> bool { (self.bits[(code as usize >> 6) & 7] >> (code & 63)) & 1 != 0 }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Text { pub len: u8, pub chars: [char; 19] }
impl Text {
    pub fn as_chars(&self) -> &[char] { &self.chars[..self.len as usize] }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AxisDiff { pub axis: u32, pub delta: i32 }

#[derive(Clone, Copy, Debug, Default)]
pub struct Axes { pub count: u8, pub axes: [AxisDiff; 11] }
impl Axes {
    pub fn as_slice(&self) -> &[AxisDiff] { &self.axes[..self.count as usize] }
}

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    None,
    Render(Render),
    Buttons(Buttons),
    Text(Text),
    CopyPaste(u32),
    Axes(Axes),
    Close,
}

#[derive(Clone, Copy, Debug)]
pub struct Event {
    /// Display time, femtoseconds.
    pub t_fs: u128,
    pub device: u32,
    pub kind: Kind,
}

// ---- pump-owned keyboard / clock state -------------------------------------
pub struct PumpState {
    pub keybits: [u64; 8],
    pub resync_pending: bool,
    pub render_id_p1: u64,   // (ring index of the last pushed RENDER) + 1, 0 = none
    pub disp_fs: u128,
    pub disp_period_fs: u64,
    pub mode_flags: u8,
    pub dead_sym: u32,       // armed dead keysym, 0 = none
    pub repeat_code: u32,
    pub repeat_text: [char; 4],
    pub repeat_text_len: u8,
    pub repeat_deadline_ns: u64,
    pub repeat_delay_ns: u64,
    pub repeat_interval_ns: u64,
    pub device: u32,
}

/// Everything shared between the pump thread (sole producer) and the app thread
/// (sole consumer). Word-sized atomics only; no locks on the hot path.
pub struct Core {
    ring: [UnsafeCell<Event>; RING],
    head: AtomicU64,
    tail: AtomicU64,
    /// Futex word: low 32 bits of head, bumped on every commit so the consumer can sleep on it.
    head_futex: AtomicU32,
    sleeping: AtomicU32,
    #[cfg(not(target_os = "linux"))]
    park: (std::sync::Mutex<()>, std::sync::Condvar),
    pump: UnsafeCell<PumpState>,

    pub win_w: AtomicU32,
    pub win_h: AtomicU32,
    pub scale_fx: AtomicU32,
    pub cursor_x: AtomicI64,
    pub cursor_y: AtomicI64,
    pub busy: [AtomicBool; 2],
    pub full_redraw: AtomicBool,
    pub cursor_hidden: AtomicBool,
    pub wants_fullscreen: AtomicBool,
    pub wants_frame: AtomicBool,
    pub quit: AtomicBool,
    pub focused: AtomicBool,
    /// Presents submitted but not yet completed by the display server.
    pub in_flight: AtomicU32,
    /// Bumped by the pump to poke the app-side wait (resize, etc.).
    pub title_changed: AtomicBool,
}

unsafe impl Sync for Core {}
unsafe impl Send for Core {}

macro_rules! ps { ($s:expr) => { (unsafe { &mut *$s.pump.get() }) } }

impl Core {
    pub fn new() -> Core {
        let t = now_ns();
        Core {
            ring: core::array::from_fn(|_| UnsafeCell::new(Event { t_fs: 0, device: 0, kind: Kind::None })),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            head_futex: AtomicU32::new(0),
            sleeping: AtomicU32::new(0),
            #[cfg(not(target_os = "linux"))]
            park: (std::sync::Mutex::new(()), std::sync::Condvar::new()),
            pump: UnsafeCell::new(PumpState {
                keybits: [0; 8],
                resync_pending: false,
                render_id_p1: 0,
                disp_fs: 0,
                disp_period_fs: 1_000_000_000_000_000 / 60,
                mode_flags: 0,
                dead_sym: 0,
                repeat_code: 0,
                repeat_text: ['\0'; 4],
                repeat_text_len: 0,
                repeat_deadline_ns: 0,
                repeat_delay_ns: 400_000_000,
                repeat_interval_ns: 33_000_000,
                device: ((t & 0xFFFF) as u32) | 1,
            }),
            win_w: AtomicU32::new(0),
            win_h: AtomicU32::new(0),
            scale_fx: AtomicU32::new(65536),
            cursor_x: AtomicI64::new(0),
            cursor_y: AtomicI64::new(0),
            busy: [AtomicBool::new(false), AtomicBool::new(false)],
            full_redraw: AtomicBool::new(true),
            cursor_hidden: AtomicBool::new(false),
            wants_fullscreen: AtomicBool::new(false),
            wants_frame: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            focused: AtomicBool::new(true),
            in_flight: AtomicU32::new(0),
            title_changed: AtomicBool::new(false),
        }
    }

    // ---- producer side (pump thread only) --------------------------------------

    /// Reserve a slot; None when the ring is full (the event is dropped and a resync is owed).
    fn reserve(&self, kind: Kind) -> Option<u64> {
        let head = self.head.load(Relaxed);
        let tail = self.tail.load(Acquire);
        if head - tail >= RING as u64 {
            ps!(self).resync_pending = true;
            return None;
        }
        let (t_fs, device) = (ps!(self).disp_fs, ps!(self).device);
        unsafe { *self.ring[(head as usize) & (RING - 1)].get() = Event { t_fs, device, kind }; }
        Some(head)
    }
    fn commit(&self, id: u64) {
        self.head.store(id + 1, Release);
        self.head_futex.fetch_add(1, Release);
        if self.sleeping.load(Acquire) != 0 {
            #[cfg(target_os = "linux")]
            crate::sys::futex_wake(&self.head_futex, 1);
            #[cfg(not(target_os = "linux"))]
            { let _g = self.park.0.lock().unwrap(); self.park.1.notify_one(); }
        }
    }
    fn push(&self, kind: Kind) -> bool {
        match self.reserve(kind) { Some(id) => { self.commit(id); true } None => false }
    }

    pub fn push_close(&self) { self.push(Kind::Close); }
    pub fn push_copypaste(&self, action: u32) { self.push(Kind::CopyPaste(action)); }

    pub fn push_button_snapshot(&self) {
        let b = Buttons { bits: ps!(self).keybits, modes: ps!(self).mode_flags };
        self.push(Kind::Buttons(b));
    }

    /// Pay off an owed snapshot once there is room again. Call at the top of every pump pass.
    pub fn service_resync(&self) {
        if ps!(self).resync_pending && self.head.load(Relaxed) - self.tail.load(Acquire) < RING as u64 {
            ps!(self).resync_pending = false;
            self.push_button_snapshot();
        }
    }

    /// Set/clear one discrete input and publish the absolute snapshot.
    pub fn key(&self, code: u32, pressed: bool) {
        if code >= 512 { return; }
        let w = &mut ps!(self).keybits[(code >> 6) as usize];
        let bit = 1u64 << (code & 63);
        if pressed { *w |= bit } else { *w &= !bit }
        self.push_button_snapshot();
    }
    #[inline] pub fn key_down(&self, code: u32) -> bool {
        code < 512 && (ps!(self).keybits[(code >> 6) as usize] >> (code & 63)) & 1 != 0
    }
    /// Release+press so the bit ends SET and the app sees a fresh edge (autorepeat of a nav key).
    pub fn key_refire(&self, code: u32) { self.key(code, false); self.key(code, true); }

    /// Focus lost: every key is logically up. Left set, our autorepeat would run forever.
    pub fn release_all_keys(&self, keyboard_only: bool) {
        let mut held = 0u64;
        let n = if keyboard_only { 4 } else { 8 };
        for i in 0..n { held |= ps!(self).keybits[i]; ps!(self).keybits[i] = 0; }
        ps!(self).repeat_code = 0;
        if held != 0 { self.push_button_snapshot(); }
    }

    pub fn push_text(&self, chars: &[char]) {
        let mut t = Text::default();
        for c in chars.iter().take(19) { t.chars[t.len as usize] = *c; t.len += 1; }
        if t.len > 0 { self.push(Kind::Text(t)); }
    }
    /// One Text event per codepoint (streams must not reduce last-wins). Control chars dropped.
    pub fn push_text_str(&self, s: &str) -> Option<char> {
        let mut last = None;
        for c in s.chars() {
            if (c as u32) < 0x20 || c as u32 == 0x7f { continue; }
            self.push_text(&[c]);
            last = Some(c);
        }
        last
    }

    pub fn push_axes(&self, diffs: &[AxisDiff]) {
        let mut a = Axes::default();
        for d in diffs.iter().take(11) { a.axes[a.count as usize] = *d; a.count += 1; }
        if a.count > 0 { self.push(Kind::Axes(a)); }
    }

    /// A RENDER means "a frame boundary passed, draw the current state" — it is not
    /// a unit of queued work, so a new one is dropped while the previous is still
    /// unconsumed. The pump owns render_id_p1 and only reads the app's tail; a
    /// stale-low tail can only make us drop one extra RENDER, never stall the clock.
    pub fn push_render(&self) -> bool {
        if ps!(self).render_id_p1 != 0 && ps!(self).render_id_p1 - 1 >= self.tail.load(Acquire) { return false; }
        let r = Render {
            width: self.win_w.load(Relaxed),
            height: self.win_h.load(Relaxed),
            scale_fx: self.scale_fx.load(Relaxed),
            cursor_x: self.cursor_x.load(Relaxed),
            cursor_y: self.cursor_y.load(Relaxed),
            dt_fs: ps!(self).disp_period_fs,
        };
        if let Some(id) = self.reserve(Kind::Render(r)) {
            ps!(self).render_id_p1 = id + 1;
            self.commit(id);
            self.wants_frame.store(false, Relaxed);
            true
        } else { false }
    }
    pub fn render_pending(&self) -> bool {
        ps!(self).render_id_p1 != 0 && ps!(self).render_id_p1 - 1 >= self.tail.load(Acquire)
    }

    /// The ONLY way display time moves: never from a wall clock reading.
    pub fn display_tick(&self, frames: u64) {
        ps!(self).disp_fs += ps!(self).disp_period_fs as u128 * frames as u128;
    }
    pub fn set_period_fs(&self, fs: u64) { if fs != 0 { ps!(self).disp_period_fs = fs; } }
    pub fn period_fs(&self) -> u64 { ps!(self).disp_period_fs }

    /// Publish the pointer position (32.32 window-local) and emit it as an axes event.
    pub fn pointer_abs(&self, x_fx: i64, y_fx: i64) {
        self.cursor_x.store(x_fx, Relaxed);
        self.cursor_y.store(y_fx, Relaxed);
        self.push_axes(&[AxisDiff { axis: AXIS_MOUSE_X, delta: (x_fx >> 24) as i32 },
                         AxisDiff { axis: AXIS_MOUSE_Y, delta: (y_fx >> 24) as i32 }]);
    }

    // ---- layout-aware key handling (shared by every backend) ------------------
    /// A key press with its resolved keysym and layout text. Order is load-bearing:
    /// text/dead-key first (it may arm or disarm the dead mode the snapshot must
    /// carry), then the button snapshot, then arm autorepeat.
    pub fn key_press_sym(&self, code: u32, sym: u32, text: &str, now_ns: u64) {
        let mut produced: Option<char> = None;
        let mut emitted_dead = false;
        if sym != 0 {
            if keysym::is_modifier(sym) {
                // Modifiers pass through an armed dead key untouched.
            } else if let Some(base) = keysym::dead_key_base(sym) {
                // Arm (or re-arm) the dead key; pressing the same dead key twice yields the bare accent.
                if ps!(self).dead_sym == sym {
                    ps!(self).dead_sym = 0; ps!(self).mode_flags &= !MODE_DEAD_KEY;
                    produced = self.push_text_str(&base.to_string());
                    emitted_dead = true;
                } else {
                    ps!(self).dead_sym = sym; ps!(self).mode_flags |= MODE_DEAD_KEY;
                }
            } else if ps!(self).dead_sym != 0 {
                let dead = ps!(self).dead_sym;
                ps!(self).dead_sym = 0; ps!(self).mode_flags &= !MODE_DEAD_KEY;
                let ctrl = self.key_down(KEY_LEFTCTRL) || self.key_down(KEY_RIGHTCTRL);
                if !ctrl {
                    if let Some(c) = text.chars().next() {
                        let out = keysym::dead_compose(dead, c).unwrap_or(c);
                        produced = self.push_text_str(&out.to_string());
                        emitted_dead = true;
                    }
                }
            }
        }
        if !emitted_dead && ps!(self).dead_sym == 0 {
            let ctrl = self.key_down(KEY_LEFTCTRL) || self.key_down(KEY_RIGHTCTRL)
                || self.key_down(KEY_LEFTMETA) && cfg!(target_os = "macos");
            if ctrl {
                // Layout-aware: the key that PRODUCES c/x/v triggers copy/cut/paste wherever it sits.
                match keysym::to_char(sym).map(|c| c.to_ascii_lowercase()) {
                    Some('c') => self.push_copypaste(CP_COPY),
                    Some('x') => self.push_copypaste(CP_CUT),
                    Some('v') => self.push_copypaste(CP_PASTE),
                    _ => {}
                }
            } else {
                produced = self.push_text_str(text);
            }
        }
        self.key(code, true);
        self.repeat_arm(code, produced, now_ns);
    }

    /// Platforms whose layout engine composes dead keys itself (macOS) report the armed state here.
    pub fn set_dead_flag(&self, armed: bool) {
        if armed { ps!(self).mode_flags |= MODE_DEAD_KEY; } else { ps!(self).mode_flags &= !MODE_DEAD_KEY; }
        ps!(self).dead_sym = 0;
    }

    fn repeat_arm(&self, code: u32, produced: Option<char>, now_ns: u64) {
        let nav = matches!(code, KEY_BACKSPACE | KEY_DELETE | KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN
                                | KEY_HOME | KEY_END | KEY_PAGEUP | KEY_PAGEDOWN | KEY_TAB | KEY_ENTER);
        if produced.is_none() && !nav { return; }
        if ps!(self).repeat_interval_ns == 0 { return; }
        ps!(self).repeat_code = code;
        ps!(self).repeat_text_len = 0;
        if let Some(c) = produced { ps!(self).repeat_text[0] = c; ps!(self).repeat_text_len = 1; }
        ps!(self).repeat_deadline_ns = now_ns + ps!(self).repeat_delay_ns;
    }

    /// Run after every dispatch pass. Returns the poll timeout in ms (≤100) to wake in time.
    pub fn repeat_tick(&self, now_ns: u64) -> i32 {
        if ps!(self).repeat_code != 0 {
            if !self.key_down(ps!(self).repeat_code) { ps!(self).repeat_code = 0; }
            else if now_ns >= ps!(self).repeat_deadline_ns {
                ps!(self).repeat_deadline_ns = now_ns + ps!(self).repeat_interval_ns;
                if ps!(self).repeat_text_len > 0 {
                    let t: Vec<char> = ps!(self).repeat_text[..ps!(self).repeat_text_len as usize].to_vec();
                    self.push_text(&t);
                } else {
                    let c = ps!(self).repeat_code;
                    self.key_refire(c);
                }
            }
        }
        if ps!(self).repeat_code == 0 { return 100; }
        let left = ps!(self).repeat_deadline_ns.saturating_sub(now_ns);
        ((left / 1_000_000) as i32).clamp(1, 100)
    }
    pub fn set_repeat(&self, delay_ms: u64, interval_ms: u64) {
        ps!(self).repeat_delay_ns = delay_ms * 1_000_000;
        ps!(self).repeat_interval_ns = interval_ms * 1_000_000;
    }

    // ---- consumer side (app thread only) ----------------------------------------
    pub fn next_event(&self) -> Option<Event> {
        let tail = self.tail.load(Relaxed);
        if tail == self.head.load(Acquire) { return None; }
        let ev = unsafe { *self.ring[(tail as usize) & (RING - 1)].get() };
        self.tail.store(tail + 1, Release);
        Some(ev)
    }
    pub fn has_events(&self) -> bool { self.tail.load(Relaxed) != self.head.load(Acquire) }

    /// Sleep until an event arrives (futex on the head counter), at most `timeout_ns` (0 = forever).
    pub fn wait(&self, timeout_ns: u64) {
        let seen = self.head_futex.load(Acquire);
        if self.has_events() { return; }
        self.sleeping.store(1, Release);
        #[cfg(target_os = "linux")]
        if !self.has_events() { crate::sys::futex_wait(&self.head_futex, seen, timeout_ns); }
        #[cfg(not(target_os = "linux"))]
        {
            let g = self.park.0.lock().unwrap();
            if !self.has_events() && self.head_futex.load(Acquire) == seen {
                let d = if timeout_ns == 0 { std::time::Duration::from_secs(3600) } else { std::time::Duration::from_nanos(timeout_ns) };
                let _ = self.park.1.wait_timeout(g, d).unwrap();
            }
        }
        self.sleeping.store(0, Release);
    }
}
