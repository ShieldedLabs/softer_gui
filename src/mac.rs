//! macOS backend (Apple Silicon): an NSWindow whose content layer scans out of
//! our IOSurfaces (zero-copy), a CVDisplayLink as the vblank source, and the
//! user's keyboard layout through UCKeyTranslate. Talks to the Objective-C
//! runtime and the frameworks directly — no binding crates.
//!
//! ROLES INVERTED versus Linux: AppKit forbids pumping events off the main
//! thread, so the MAIN thread is the event pump (`run` keeps it there), the
//! CVDisplayLink thread is the ring's sole PRODUCER (it drains what the pump
//! collected, then stamps a RENDER once per physical refresh), and the app runs
//! on its own thread as the consumer. Written without a Mac to run it on; the
//! call sequence follows the brevis backend, which was.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex};
use crate::event::*;
use crate::keysym;

type id = *mut core::ffi::c_void;
type SEL = *mut core::ffi::c_void;
type Class = *mut core::ffi::c_void;
type CFTypeRef = *const core::ffi::c_void;
type CFStringRef = CFTypeRef;
type Boolean = u8;

#[repr(C)] #[derive(Clone, Copy, Default)] pub struct NSPoint { x: f64, y: f64 }
#[repr(C)] #[derive(Clone, Copy, Default)] pub struct NSSize { w: f64, h: f64 }
#[repr(C)] #[derive(Clone, Copy, Default)] pub struct NSRect { o: NSPoint, s: NSSize }
#[repr(C)] #[derive(Clone, Copy)] pub struct CVTime { value: i64, scale: i32, flags: i32 }

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const u8) -> Class;
    fn sel_registerName(name: *const u8) -> SEL;
    fn objc_msgSend();
    fn objc_allocateClassPair(sup: Class, name: *const u8, extra: usize) -> Class;
    fn objc_registerClassPair(cls: Class);
    fn class_addMethod(cls: Class, sel: SEL, imp: *const core::ffi::c_void, types: *const u8) -> Boolean;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(r: CFTypeRef);
    fn CFNumberCreate(alloc: CFTypeRef, ty: i64, ptr: *const core::ffi::c_void) -> CFTypeRef;
    fn CFDictionaryCreate(alloc: CFTypeRef, keys: *const CFTypeRef, vals: *const CFTypeRef, n: i64, kcb: *const core::ffi::c_void, vcb: *const core::ffi::c_void) -> CFTypeRef;
    fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
    static kCFTypeDictionaryKeyCallBacks: [u8; 0];
    static kCFTypeDictionaryValueCallBacks: [u8; 0];
}
#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreate(props: CFTypeRef) -> CFTypeRef;
    fn IOSurfaceGetBaseAddress(s: CFTypeRef) -> *mut u8;
    fn IOSurfaceGetBytesPerRow(s: CFTypeRef) -> usize;
    fn IOSurfaceLock(s: CFTypeRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(s: CFTypeRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceIsInUse(s: CFTypeRef) -> Boolean;
    static kIOSurfaceWidth: CFStringRef;
    static kIOSurfaceHeight: CFStringRef;
    static kIOSurfaceBytesPerElement: CFStringRef;
    static kIOSurfaceBytesPerRow: CFStringRef;
    static kIOSurfacePixelFormat: CFStringRef;
}
#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVDisplayLinkCreateWithCGDisplay(display: u32, out: *mut *mut core::ffi::c_void) -> i32;
    fn CVDisplayLinkSetOutputCallback(link: *mut core::ffi::c_void, cb: extern "C" fn(*mut core::ffi::c_void, *const u8, *const u8, u64, *mut u64, *mut core::ffi::c_void) -> i32, user: *mut core::ffi::c_void) -> i32;
    fn CVDisplayLinkStart(link: *mut core::ffi::c_void) -> i32;
    fn CVDisplayLinkStop(link: *mut core::ffi::c_void) -> i32;
    fn CVDisplayLinkSetCurrentCGDisplay(link: *mut core::ffi::c_void, display: u32) -> i32;
    fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link: *mut core::ffi::c_void) -> CVTime;
    fn CVDisplayLinkGetActualOutputVideoRefreshPeriod(link: *mut core::ffi::c_void) -> f64;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" { fn CGMainDisplayID() -> u32; }
#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" { static kCAFilterNearest: id; }
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> CFTypeRef;
    fn TISGetInputSourceProperty(src: CFTypeRef, key: CFStringRef) -> CFTypeRef;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(layout: *const u8, vk: u16, action: u16, mods: u32, kbd_type: u32, options: u32, dead: *mut u32, max: usize, actual: *mut usize, out: *mut u16) -> i32;
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
}

// ---- objc_msgSend, typed per call shape ------------------------------------------
fn cls(name: &str) -> Class { let c = format!("{name}\0"); unsafe { objc_getClass(c.as_ptr()) } }
fn sel(name: &str) -> SEL { let s = format!("{name}\0"); unsafe { sel_registerName(s.as_ptr()) } }
fn send0(r: id, s: SEL) -> id { let f: extern "C" fn(id, SEL) -> id = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(r, s) }
fn send1(r: id, s: SEL, a: id) -> id { let f: extern "C" fn(id, SEL, id) -> id = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(r, s, a) }
fn send_u(r: id, s: SEL, a: u64) -> id { let f: extern "C" fn(id, SEL, u64) -> id = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(r, s, a) }
fn send_f(r: id, s: SEL, a: f64) -> id { let f: extern "C" fn(id, SEL, f64) -> id = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(r, s, a) }
fn send_bool(r: id, s: SEL, a: bool) -> id { send_u(r, s, a as u64) }
fn ret_f64(r: id, s: SEL) -> f64 { let f: extern "C" fn(id, SEL) -> f64 = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(r, s) }
fn ret_f32(r: id, s: SEL) -> f32 { let f: extern "C" fn(id, SEL) -> f32 = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(r, s) }
fn ret_u64(r: id, s: SEL) -> u64 { send0(r, s) as u64 }
fn ret_rect(r: id, s: SEL) -> NSRect { let f: extern "C" fn(id, SEL) -> NSRect = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(r, s) }
fn ret_point(r: id, s: SEL) -> NSPoint { let f: extern "C" fn(id, SEL) -> NSPoint = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(r, s) }
fn send_rect(r: id, s: SEL, rect: NSRect) -> id { let f: extern "C" fn(id, SEL, NSRect) -> id = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(r, s, rect) }
fn nsdata(bytes: &[u8]) -> id {
    let f: extern "C" fn(id, SEL, *const u8, u64) -> id = unsafe { core::mem::transmute(objc_msgSend as *const ()) };
    f(cls("NSData"), sel("dataWithBytes:length:"), bytes.as_ptr(), bytes.len() as u64)
}
fn nsstring(s: &str) -> id { let c = format!("{s}\0"); let f: extern "C" fn(id, SEL, *const u8) -> id = unsafe { core::mem::transmute(objc_msgSend as *const ()) }; f(cls("NSString"), sel("stringWithUTF8String:"), c.as_ptr()) }

extern "C" fn yes_imp(_this: id, _sel: SEL) -> Boolean { 1 }

// ---- IOSurface buffers -------------------------------------------------------------
struct Surface { surf: CFTypeRef, addr: *mut u8 }
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

fn make_surface(side: u32) -> Option<Surface> {
    unsafe {
        let nums: [i32; 5] = [side as i32, side as i32, 4, (side * 4) as i32, 0x42475241];   // 'BGRA'
        let keys: [CFStringRef; 5] = [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfacePixelFormat];
        let mut vals: [CFTypeRef; 5] = [core::ptr::null(); 5];
        for i in 0..5 { vals[i] = CFNumberCreate(core::ptr::null(), 3, &nums[i] as *const i32 as *const _); }   // kCFNumberSInt32Type
        let props = CFDictionaryCreate(core::ptr::null(), keys.as_ptr(), vals.as_ptr(), 5, kCFTypeDictionaryKeyCallBacks.as_ptr() as *const _, kCFTypeDictionaryValueCallBacks.as_ptr() as *const _);
        let surf = IOSurfaceCreate(props);
        CFRelease(props);
        for v in vals { CFRelease(v); }
        if surf.is_null() { return None; }
        if IOSurfaceGetBytesPerRow(surf) != (side * 4) as usize { CFRelease(surf); return None; }
        Some(Surface { surf, addr: IOSurfaceGetBaseAddress(surf) })
    }
}

// ---- what the main-thread pump hands to the producer ---------------------------------
enum Raw {
    KeyDown { code: u32, sym: u32, text: String, dead: bool },
    KeyUp(u32),
    Button { code: u32, down: bool },
    Scroll { v: i32, h: i32 },
    Zoom(i32),
    Rotate(i32),
    Focus(bool),
    Close,
}

pub struct Shared {
    core: Arc<Core>,
    raw: Mutex<Vec<Raw>>,
    win: AtomicU64, view: AtomicU64, layer: AtomicU64,
    scale: AtomicU32,
    link: AtomicU64,
    pend_free: Mutex<Vec<Surface>>,
    start: std::time::Instant,
    sel_set_contents: SEL, sel_contents_rect: SEL, sel_flush: SEL, sel_bounds: SEL, sel_mouse_loc: SEL,
    sel_backing_scale: SEL, sel_contents_scale: SEL,
    cls_catransaction: Class,
}
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

pub struct App {
    sh: Arc<Shared>,
    core: Arc<Core>,
    side: u32,
    generation: u64,
    cur: usize,
    surfaces: [Option<Surface>; 2],
}

impl App {
    fn ensure_size(&mut self) {
        let w = self.core.win_w.load(Relaxed); let h = self.core.win_h.load(Relaxed);
        let want = crate::next_pow2(w.max(h));
        if want != self.side || self.surfaces[0].is_none() {
            // Keep the old (still-bound) surfaces alive and on screen; the next submit flips
            // old->new in ONE transaction and frees them — detaching first was a grey flash.
            let mut pend = self.sh.pend_free.lock().unwrap();
            for s in self.surfaces.iter_mut() { if let Some(s) = s.take() { if pend.is_empty() || pend.len() < 2 { pend.push(s); } else { unsafe { CFRelease(s.surf); } } } }
            drop(pend);
            self.side = want;
            self.generation += 1;
            self.cur = 0;
            self.surfaces[0] = make_surface(want);
            self.surfaces[1] = make_surface(want);
            self.core.full_redraw.store(true, Relaxed);
        }
    }
    pub fn get_framebuffer(&mut self) -> Option<(*mut u32, u32, u64)> {
        self.ensure_size();
        // Double-buffered, no triple buffer: the buffer returned was shown two vblanks ago.
        // IOSurfaceLock WAITS for the rare sub-ms release lag rather than tearing, so no
        // racy IOSurfaceIsInUse pre-check (that dropped 6-18% of flips below 120 Hz).
        let s = self.surfaces[self.cur].as_ref()?;
        let mut seed = 0u32;
        unsafe { IOSurfaceLock(s.surf, 0, &mut seed); }
        Some((s.addr as *mut u32, self.side, (self.generation << 1) | self.cur as u64))
    }
    pub fn submit(&mut self) {
        let Some(s) = self.surfaces[self.cur].as_ref() else { return };
        let mut seed = 0u32;
        unsafe { IOSurfaceUnlock(s.surf, 0, &mut seed); }
        let sh = &self.sh;
        let layer = sh.layer.load(Relaxed) as id;
        send1(layer, sh.sel_set_contents, s.surf as id);
        // Show only the window-sized top-left sub-rect of the square surface; contentsRect's
        // y origin is bottom-based, so origin y = 1 - hfrac.
        let (w, h) = (self.core.win_w.load(Relaxed) as f64, self.core.win_h.load(Relaxed) as f64);
        let side = self.side as f64;
        send_rect(layer, sh.sel_contents_rect, NSRect { o: NSPoint { x: 0.0, y: 1.0 - h / side }, s: NSSize { w: w / side, h: h / side } });
        send0(sh.cls_catransaction, sh.sel_flush);
        let mut pend = sh.pend_free.lock().unwrap();
        for s in pend.drain(..) { unsafe { CFRelease(s.surf); } }
        self.core.in_flight.store(1, Relaxed);
        self.cur ^= 1;
    }
    pub fn poke(&self) {}
}

// ---- keyboard --------------------------------------------------------------------------
fn vk_to_evdev(vk: u16) -> u32 {
    match vk {
        0 => 30, 1 => 31, 2 => 32, 3 => 33, 4 => 35, 5 => 34, 6 => 44, 7 => 45, 8 => 46, 9 => 47, 11 => 48, 12 => 16, 13 => 17, 14 => 18, 15 => 19,
        16 => 21, 17 => 20, 18 => 2, 19 => 3, 20 => 4, 21 => 5, 22 => 7, 23 => 6, 24 => 13, 25 => 10, 26 => 8, 27 => 12, 28 => 9, 29 => 11,
        30 => 27, 31 => 24, 32 => 22, 33 => 26, 34 => 23, 35 => 25, 37 => 38, 38 => 36, 39 => 40, 40 => 37, 41 => 39, 42 => 43, 43 => 51, 44 => 53,
        45 => 49, 46 => 50, 47 => 52, 50 => 41, 10 => 86,
        36 => KEY_ENTER, 48 => KEY_TAB, 49 => 57, 51 => KEY_BACKSPACE, 53 => KEY_ESC, 114 => KEY_INSERT, 115 => KEY_HOME, 116 => KEY_PAGEUP,
        117 => KEY_DELETE, 119 => KEY_END, 121 => KEY_PAGEDOWN, 123 => KEY_LEFT, 124 => KEY_RIGHT, 126 => KEY_UP, 125 => KEY_DOWN,
        122 => 59, 120 => 60, 99 => 61, 118 => 62, 96 => 63, 97 => 64, 98 => 65, 100 => 66, 101 => 67, 109 => 68, 103 => KEY_F11, 111 => KEY_F12,
        56 => KEY_LEFTSHIFT, 60 => KEY_RIGHTSHIFT, 59 => KEY_LEFTCTRL, 62 => KEY_RIGHTCTRL, 58 => KEY_LEFTALT, 61 => KEY_RIGHTALT, 55 => KEY_LEFTMETA, 54 => KEY_RIGHTMETA, 57 => 58,
        65 => 83, 67 => 55, 69 => 78, 71 => 69, 75 => 98, 76 => 96, 78 => 74, 81 => 117, 82 => 82, 83 => 79, 84 => 80, 85 => 81, 86 => 75, 87 => 76, 88 => 77, 89 => 71, 91 => 72, 92 => 73,
        _ => KEY_UNKNOWN,
    }
}

struct Layout { data: *const u8, kbd_type: u32, dead: u32 }

impl Layout {
    fn new() -> Layout {
        unsafe {
            let src = TISCopyCurrentKeyboardLayoutInputSource();
            let mut data = core::ptr::null();
            if !src.is_null() {
                let d = TISGetInputSourceProperty(src, kTISPropertyUnicodeKeyLayoutData);
                if !d.is_null() { data = CFDataGetBytePtr(d); }
            }
            Layout { data, kbd_type: LMGetKbdType() as u32, dead: 0 }
        }
    }
    /// Text for a vk under NSEvent modifier flags; `dead_out` reports an armed dead key.
    fn translate(&mut self, vk: u16, nsmods: u64, carry_dead: bool) -> (String, bool) {
        if self.data.is_null() { return (String::new(), false); }
        // shift (1<<17 -> 2) and option (1<<19 -> 8) coincide after >>16; caps (1<<16) -> 4. Cmd/Ctrl left out.
        let mods = (((nsmods >> 16) & 10) | (((nsmods >> 16) & 1) << 2)) as u32;
        let mut dead = if carry_dead { self.dead } else { 0 };
        let mut out = [0u16; 8];
        let mut n = 0usize;
        let st = unsafe { UCKeyTranslate(self.data, vk, 0, mods, self.kbd_type, 0, &mut dead, 8, &mut n, out.as_mut_ptr()) };
        if st != 0 { return (String::new(), false); }
        if carry_dead { self.dead = dead; }
        if dead != 0 { return (String::new(), true); }
        let s: String = char::decode_utf16(out[..n].iter().copied()).filter_map(|c| c.ok())
            .filter(|c| (*c as u32) >= 0x20 && *c as u32 != 0x7f && !(0xF700..=0xF8FF).contains(&(*c as u32))).collect();
        (s, false)
    }
}

// ---- CVDisplayLink: the vblank source AND the ring producer -------------------------------
extern "C" fn link_callback(link: *mut core::ffi::c_void, _now: *const u8, _out: *const u8, _f: u64, _fo: *mut u64, user: *mut core::ffi::c_void) -> i32 {
    let sh: &Shared = unsafe { &*(user as *const Shared) };
    let core = &sh.core;
    // Nominal period: an exact rational for the mode; the actual one jitters ±µs and made motion shimmer.
    let t = unsafe { CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link) };
    if t.scale != 0 && t.flags & 1 == 0 { core.set_period_fs((t.value as u128 * 1_000_000_000_000_000u128 / t.scale as u128) as u64); }
    else { let p = unsafe { CVDisplayLinkGetActualOutputVideoRefreshPeriod(link) }; if p > 0.0 { core.set_period_fs((p * 1e15) as u64); } }
    core.display_tick(1);
    core.service_resync();
    let now = sh.start.elapsed().as_nanos() as u64;

    // Window geometry and pointer, read straight off the window (pollable state, no events needed).
    let win = sh.win.load(Relaxed) as id; let view = sh.view.load(Relaxed) as id;
    if !win.is_null() {
        let scale = ret_f64(win, sh.sel_backing_scale).max(1.0);
        let sc = scale as u32;
        if sc != sh.scale.load(Relaxed) { sh.scale.store(sc, Relaxed); core.scale_fx.store((scale * 65536.0) as u32, Relaxed); core.full_redraw.store(true, Relaxed); }
        let b = ret_rect(view, sh.sel_bounds);
        let (w, h) = ((b.s.w * scale) as u32, (b.s.h * scale) as u32);
        if w != 0 && h != 0 && (w != core.win_w.load(Relaxed) || h != core.win_h.load(Relaxed)) { core.win_w.store(w, Relaxed); core.win_h.store(h, Relaxed); core.full_redraw.store(true, Relaxed); }
        let p = ret_point(win, sh.sel_mouse_loc);
        let x_fx = (p.x * scale * 4294967296.0) as i64;
        let y_fx = ((h as f64 - p.y * scale) * 4294967296.0) as i64;
        if x_fx != core.cursor_x.load(Relaxed) || y_fx != core.cursor_y.load(Relaxed) { core.pointer_abs(x_fx, y_fx); }
    }
    let raw: Vec<Raw> = std::mem::take(&mut *sh.raw.lock().unwrap());
    for r in raw {
        match r {
            Raw::KeyDown { code, sym, text, dead } => {
                core.set_dead_flag(dead);
                core.key_press_sym(code, sym, &text, now);
            }
            Raw::KeyUp(code) => core.key(code, false),
            Raw::Button { code, down } => core.key(code, down),
            Raw::Scroll { v, h } => { let mut d = Vec::new(); if v != 0 { d.push(AxisDiff { axis: AXIS_SCROLL_V, delta: v }); } if h != 0 { d.push(AxisDiff { axis: AXIS_SCROLL_H, delta: h }); } core.push_axes(&d); }
            Raw::Zoom(z) => core.push_axes(&[AxisDiff { axis: AXIS_ZOOM, delta: z }]),
            Raw::Rotate(a) => core.push_axes(&[AxisDiff { axis: AXIS_ROTATE, delta: a }]),
            Raw::Focus(f) => { core.focused.store(f, Relaxed); if !f { core.release_all_keys(true); } }
            Raw::Close => core.push_close(),
        }
    }
    core.repeat_tick(now);
    core.in_flight.store(0, Relaxed);
    core.push_render();
    0
}

// ---- main-thread pump ------------------------------------------------------------------
pub struct Pump {
    sh: Arc<Shared>,
    app: id, win: id, layer: id,
    layout: Layout,
    sel_next_event: SEL, sel_send_event: SEL, sel_type: SEL, sel_keycode: SEL, sel_is_repeat: SEL, sel_modifier_flags: SEL,
    sel_is_visible: SEL, sel_is_key_window: SEL, sel_screen: SEL, sel_device_desc: SEL, sel_object_for_key: SEL, sel_unsigned_int: SEL,
    sel_scroll_x: SEL, sel_scroll_y: SEL, sel_precise: SEL, sel_magnification: SEL, sel_rotation: SEL, sel_button_number: SEL,
    date_distant_past: id, run_loop_mode: id, screen_number_key: id,
    cls_nscursor: Class, sel_hide: SEL, sel_unhide: SEL, sel_toggle_fs: SEL,
    cursor_applied: bool, fs_applied: bool, focused: bool, close_sent: bool, display: u32, nsmods: u64,
}

impl Pump {
    fn push(&self, r: Raw) { self.sh.raw.lock().unwrap().push(r); }

    /// One pass: dequeue every pending NSEvent, apply the app's outward state. Call from the main thread.
    /// macOS has no per-window icon — the titlebar proxy icon represents a
    /// document, not the app. The Dock icon is the equivalent, and unlike the
    /// bundle's CFBundleIconFile it can be set at runtime, which is the only
    /// route open to an unbundled binary.
    fn apply_icon(&self) {
        let Some(set) = self.sh.core.take_icon() else { return };
        // Largest first (own_set sorts); AppKit downscales for the Dock.
        let Some(img) = set.first() else {
            send1(self.app, sel("setApplicationIconImage:"), core::ptr::null_mut());
            return;
        };
        // PNG through NSImage rather than NSBitmapImageRep: the latter needs a
        // ten-argument initialiser whose signature we would have to get exactly
        // right through objc_msgSend, for no gain.
        let png = crate::icon::encode_png(&img.as_image());
        let data = nsdata(&png);
        if data.is_null() { return; }
        let obj = send1(send0(cls("NSImage"), sel("alloc")), sel("initWithData:"), data);
        if obj.is_null() { return; }
        send1(self.app, sel("setApplicationIconImage:"), obj);
    }

    /// Block up to `ms` for an NSEvent to become available (peek, no dequeue); pump_once then takes it.
    pub fn block_ms(&mut self, ms: u64) {
        let date = send_f(cls("NSDate"), sel("dateWithTimeIntervalSinceNow:"), ms as f64 / 1000.0);
        let f: extern "C" fn(id, SEL, u64, id, id, bool) -> id = unsafe { core::mem::transmute(objc_msgSend as *const ()) };
        f(self.app, self.sel_next_event, u64::MAX, date, self.run_loop_mode, false);
    }

    pub fn pump_once(&mut self) {
        let core = &self.sh.core;
        loop {
            let f: extern "C" fn(id, SEL, u64, id, id, bool) -> id = unsafe { core::mem::transmute(objc_msgSend as *const ()) };
            let ev = f(self.app, self.sel_next_event, u64::MAX, self.date_distant_past, self.run_loop_mode, true);
            if ev.is_null() { break; }
            let ty = ret_u64(ev, self.sel_type);
            match ty {
                10 => {   // KeyDown: swallowed (our content view has no keyDown:, sendEvent: would beep)
                    let vk = (ret_u64(ev, self.sel_keycode) & 0xffff) as u16;
                    let nsmods = ret_u64(ev, self.sel_modifier_flags);
                    if ret_u64(ev, self.sel_is_repeat) & 1 != 0 { continue; }   // we run our own repeat timer
                    let cmd = (nsmods >> 20) & 1 != 0;
                    let (text, dead) = self.layout.translate(vk, nsmods, !cmd);
                    // The keysym is the layout character without Cmd: it is what Cmd-C/X/V match on.
                    let (probe, _) = self.layout.translate(vk, nsmods & !(1 << 20), false);
                    let sym = probe.chars().next().and_then(keysym::from_char).unwrap_or(0);
                    self.push(Raw::KeyDown { code: vk_to_evdev(vk), sym, text: if cmd { String::new() } else { text }, dead });
                }
                11 => { let vk = (ret_u64(ev, self.sel_keycode) & 0xffff) as u16; self.push(Raw::KeyUp(vk_to_evdev(vk))); }
                12 => {   // FlagsChanged: modifier keys press/release from the flag bits
                    let vk = (ret_u64(ev, self.sel_keycode) & 0xffff) as u16;
                    let nsmods = ret_u64(ev, self.sel_modifier_flags);
                    let mask = match vk { 56 | 60 => 1 << 17, 59 | 62 => 1 << 18, 58 | 61 => 1 << 19, 55 | 54 => 1 << 20, 57 => 1 << 16, _ => 0 };
                    if mask != 0 { let down = nsmods & mask != 0; self.push(if down { Raw::Button { code: vk_to_evdev(vk), down: true } } else { Raw::KeyUp(vk_to_evdev(vk)) }); }
                    self.nsmods = nsmods;
                    send1(self.app, self.sel_send_event, ev);
                }
                1 | 2 | 3 | 4 | 25 | 26 => {
                    let code = match ty { 1 | 2 => BTN_LEFT, 3 | 4 => BTN_RIGHT, _ => match ret_u64(ev, self.sel_button_number) { 2 => BTN_MIDDLE, 3 => BTN_SIDE, _ => BTN_EXTRA } };
                    self.push(Raw::Button { code, down: matches!(ty, 1 | 3 | 25) });
                    send1(self.app, self.sel_send_event, ev);
                }
                22 => {   // ScrollWheel: precise deltas are pixels (points × scale), else lines × SCROLL_STEP
                    let dx = ret_f64(ev, self.sel_scroll_x); let dy = ret_f64(ev, self.sel_scroll_y);
                    let precise = ret_u64(ev, self.sel_precise) & 1 != 0;
                    let scale = self.sh.scale.load(Relaxed).max(1) as f64;
                    let (v, h) = if precise { (-dy * scale * 256.0, -dx * scale * 256.0) } else { (-dy * SCROLL_STEP as f64, -dx * SCROLL_STEP as f64) };
                    self.push(Raw::Scroll { v: v as i32, h: h as i32 });
                    send1(self.app, self.sel_send_event, ev);
                }
                30 => { let m = ret_f64(ev, self.sel_magnification); self.push(Raw::Zoom((m * 65536.0) as i32)); send1(self.app, self.sel_send_event, ev); }
                18 => { let r = ret_f32(ev, self.sel_rotation); self.push(Raw::Rotate((-r as f64 * 65536.0) as i32)); send1(self.app, self.sel_send_event, ev); }
                _ => { send1(self.app, self.sel_send_event, ev); }
            }
        }
        // Outward state: cursor, fullscreen, icon (AppKit surgery belongs to this thread).
        self.apply_icon();
        let hidden = core.cursor_hidden.load(Relaxed);
        if hidden != self.cursor_applied { self.cursor_applied = hidden; send0(self.cls_nscursor, if hidden { self.sel_hide } else { self.sel_unhide }); }
        let fs = core.wants_fullscreen.load(Relaxed);
        if fs != self.fs_applied { self.fs_applied = fs; send1(self.win, self.sel_toggle_fs, core::ptr::null_mut()); }
        let focused = ret_u64(self.win, self.sel_is_key_window) & 1 != 0;
        if focused != self.focused { self.focused = focused; self.push(Raw::Focus(focused)); }
        if !self.close_sent && ret_u64(self.win, self.sel_is_visible) & 1 == 0 { self.close_sent = true; self.push(Raw::Close); }
        // Follow the window to another monitor: retarget the link so its cadence and period match.
        let screen = send0(self.win, self.sel_screen);
        if !screen.is_null() {
            let desc = send0(screen, self.sel_device_desc);
            let num = send1(desc, self.sel_object_for_key, self.screen_number_key);
            if !num.is_null() {
                let d = ret_u64(num, self.sel_unsigned_int) as u32;
                if d != 0 && d != self.display { self.display = d; unsafe { CVDisplayLinkSetCurrentCGDisplay(self.sh.link.load(Relaxed) as *mut _, d); } }
            }
        }
    }
}

/// Open the window (main thread only). Returns the consumer-side App and the main-thread Pump.
pub fn open(core: Arc<Core>, title: &str, width: u32, height: u32) -> Option<(App, Pump)> {
    unsafe {
        let app = send0(cls("NSApplication"), sel("sharedApplication"));
        send_u(app, sel("setActivationPolicy:"), 0);
        // An NSWindow subclass whose canBecomeKeyWindow answers YES, so a borderless (fullscreen) window keeps the keyboard.
        let mut win_cls = cls("NSWindow");
        let fs_cls = objc_allocateClassPair(win_cls, b"SofterGuiWindow\0".as_ptr(), 0);
        if !fs_cls.is_null() {
            class_addMethod(fs_cls, sel("canBecomeKeyWindow"), yes_imp as *const _, b"c@:\0".as_ptr());
            objc_registerClassPair(fs_cls);
            win_cls = fs_cls;
        }
        let win = send0(win_cls, sel("alloc"));
        let f: extern "C" fn(id, SEL, NSRect, u64, u64, bool) -> id = core::mem::transmute(objc_msgSend as *const ());
        let win = f(win, sel("initWithContentRect:styleMask:backing:defer:"), NSRect { o: NSPoint { x: 0.0, y: 0.0 }, s: NSSize { w: width as f64, h: height as f64 } }, 15, 2, false);
        if win.is_null() { return None; }
        send_bool(win, sel("setReleasedWhenClosed:"), false);
        send1(win, sel("setTitle:"), nsstring(title));
        send_u(win, sel("setCollectionBehavior:"), 1 << 7);   // FullScreenPrimary: toggleFullScreen: works
        let view = send0(win, sel("contentView"));
        send_bool(view, sel("setWantsLayer:"), true);
        let layer = send0(view, sel("layer"));
        let sel_backing_scale = sel("backingScaleFactor");
        let sel_contents_scale = sel("setContentsScale:");
        let scale = ret_f64(win, sel_backing_scale).max(1.0);
        // Native device pixels: the layer maps surface pixels 1:1, never smoothed.
        send_f(layer, sel_contents_scale, scale);
        send1(layer, sel("setMagnificationFilter:"), kCAFilterNearest);
        send1(layer, sel("setMinificationFilter:"), kCAFilterNearest);
        send_bool(win, sel("setAcceptsMouseMovedEvents:"), true);

        core.win_w.store((width as f64 * scale) as u32, Relaxed);
        core.win_h.store((height as f64 * scale) as u32, Relaxed);
        core.scale_fx.store((scale * 65536.0) as u32, Relaxed);
        {
            let delay = ret_f64(cls("NSEvent"), sel("keyRepeatDelay")); let interval = ret_f64(cls("NSEvent"), sel("keyRepeatInterval"));
            if delay > 0.0 && interval > 0.0 { core.set_repeat((delay * 1000.0) as u64, (interval * 1000.0) as u64); }
        }

        let sh = Arc::new(Shared {
            core: core.clone(), raw: Mutex::new(Vec::new()),
            win: AtomicU64::new(win as u64), view: AtomicU64::new(view as u64), layer: AtomicU64::new(layer as u64),
            scale: AtomicU32::new(scale as u32), link: AtomicU64::new(0), pend_free: Mutex::new(Vec::new()), start: std::time::Instant::now(),
            sel_set_contents: sel("setContents:"), sel_contents_rect: sel("setContentsRect:"), sel_flush: sel("flush"), sel_bounds: sel("bounds"),
            sel_mouse_loc: sel("mouseLocationOutsideOfEventStream"), sel_backing_scale, sel_contents_scale,
            cls_catransaction: cls("CATransaction"),
        });

        let mut app_side = App { sh: sh.clone(), core: core.clone(), side: 0, generation: 0, cur: 0, surfaces: [None, None] };
        app_side.ensure_size();
        // Bind buffer 0 so the window shows something before the first flip; draw the first frame into 1.
        if let Some(s) = app_side.surfaces[0].as_ref() { send1(layer, sh.sel_set_contents, s.surf as id); }
        app_side.cur = 1;

        send1(win, sel("makeKeyAndOrderFront:"), core::ptr::null_mut());
        send0(win, sel("center"));
        send_bool(app, sel("activateIgnoringOtherApps:"), true);
        send0(app, sel("finishLaunching"));

        // The display link: exactly one callback per physical refresh, on its own thread.
        let mut link: *mut core::ffi::c_void = core::ptr::null_mut();
        if CVDisplayLinkCreateWithCGDisplay(CGMainDisplayID(), &mut link) != 0 || link.is_null() { eprintln!("softer_gui: CVDisplayLink creation failed"); return None; }
        sh.link.store(link as u64, Relaxed);
        CVDisplayLinkSetOutputCallback(link, link_callback, Arc::as_ptr(&sh) as *mut _);
        std::mem::forget(sh.clone());   // the callback's reference lives as long as the process
        CVDisplayLinkStart(link);

        let pump = Pump {
            sh, app, win, layer, layout: Layout::new(),
            sel_next_event: sel("nextEventMatchingMask:untilDate:inMode:dequeue:"), sel_send_event: sel("sendEvent:"), sel_type: sel("type"),
            sel_keycode: sel("keyCode"), sel_is_repeat: sel("isARepeat"), sel_modifier_flags: sel("modifierFlags"),
            sel_is_visible: sel("isVisible"), sel_is_key_window: sel("isKeyWindow"), sel_screen: sel("screen"), sel_device_desc: sel("deviceDescription"),
            sel_object_for_key: sel("objectForKey:"), sel_unsigned_int: sel("unsignedIntValue"),
            sel_scroll_x: sel("scrollingDeltaX"), sel_scroll_y: sel("scrollingDeltaY"), sel_precise: sel("hasPreciseScrollingDeltas"),
            sel_magnification: sel("magnification"), sel_rotation: sel("rotation"), sel_button_number: sel("buttonNumber"),
            date_distant_past: send0(cls("NSDate"), sel("distantPast")), run_loop_mode: nsstring("kCFRunLoopDefaultMode"), screen_number_key: nsstring("NSScreenNumber"),
            cls_nscursor: cls("NSCursor"), sel_hide: sel("hide"), sel_unhide: sel("unhide"), sel_toggle_fs: sel("toggleFullScreen:"),
            cursor_applied: false, fs_applied: false, focused: true, close_sent: false, display: 0, nsmods: 0,
        };
        Some((app_side, pump))
    }
}

// ---- owning the main thread ------------------------------------------------------------
// AppKit must be pumped on the main thread, and the app wants a plain polling API on
// whatever thread called open(). So open() on the main thread does a register/stack
// handoff: the caller's context (callee-saved registers, sp, return address) is captured;
// a fresh pthread adopts it and RETURNS from open() as the application, running on the
// original main-thread stack; the real main thread meanwhile moves to a private stack
// and pumps AppKit forever. Thread identity is invisible to the app — it is only the
// kernel's notion of "main thread" that AppKit cares about, and that stays where it is.
//
// The handoff is one naked function so that NOT ONE Rust instruction runs on the shared
// stack between capturing the context and leaving it: capture, publish the flag, switch
// sp, branch to the pump. The adopting thread returns from `handoff` with 1.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn handoff(ctx: *mut u64, flag: *const u32, new_sp: u64, pump: extern "C" fn(*mut core::ffi::c_void) -> !, arg: *mut core::ffi::c_void) -> u64 {
    core::arch::naked_asm!(
        "stp x19, x20, [x0, #0]",
        "stp x21, x22, [x0, #16]",
        "stp x23, x24, [x0, #32]",
        "stp x25, x26, [x0, #48]",
        "stp x27, x28, [x0, #64]",
        "stp x29, x30, [x0, #80]",
        "mov x9, sp",
        "str x9, [x0, #96]",
        "stp d8, d9, [x0, #104]",
        "stp d10, d11, [x0, #120]",
        "stp d12, d13, [x0, #136]",
        "stp d14, d15, [x0, #152]",
        "mov w9, #1",
        "stlr w9, [x1]",          // publish: the adopting thread may now resume on this stack
        "mov sp, x2",             // leave it
        "mov x0, x4",
        "br x3",                  // pump(arg), never returns
    )
}
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn resume(ctx: *const u64) -> ! {
    core::arch::naked_asm!(
        "ldp x19, x20, [x0, #0]",
        "ldp x21, x22, [x0, #16]",
        "ldp x23, x24, [x0, #32]",
        "ldp x25, x26, [x0, #48]",
        "ldp x27, x28, [x0, #64]",
        "ldp x29, x30, [x0, #80]",
        "ldr x9, [x0, #96]",
        "mov sp, x9",
        "ldp d8, d9, [x0, #104]",
        "ldp d10, d11, [x0, #120]",
        "ldp d12, d13, [x0, #136]",
        "ldp d14, d15, [x0, #152]",
        "mov x0, #1",
        "ret",                    // returns from handoff() on the original stack, as the app
    )
}

#[link(name = "System")]
unsafe extern "C" { fn pthread_main_np() -> i32; }

extern "C" fn pump_forever(arg: *mut core::ffi::c_void) -> ! {
    let pump: &mut Pump = unsafe { &mut *(arg as *mut Pump) };
    loop {
        pump.pump_once();
        pump.block_ms(4);
    }
}

/// Give the main thread to AppKit. Must be called on the main thread; returns (once) on
/// a new thread that has taken over the caller's stack and context.
#[cfg(target_arch = "aarch64")]
pub fn takeover_main_thread(pump: Pump) -> bool {
    if unsafe { pthread_main_np() } == 0 { eprintln!("softer_gui: open() must be called on the main thread on macOS"); return false; }
    let ctx: &'static mut [u64; 24] = Box::leak(Box::new([0u64; 24]));
    let flag: &'static std::sync::atomic::AtomicU32 = Box::leak(Box::new(std::sync::atomic::AtomicU32::new(0)));
    let pump_ptr = Box::into_raw(Box::new(pump)) as *mut core::ffi::c_void;
    // 4 MB private stack for the pump; the Vec is leaked on purpose.
    let stack: &'static mut Vec<u8> = Box::leak(Box::new(vec![0u8; 4 << 20]));
    let top = (stack.as_mut_ptr() as u64 + stack.len() as u64) & !15;
    let ctx_addr = ctx as *mut [u64; 24] as usize;
    let flag_ref: &'static std::sync::atomic::AtomicU32 = flag;
    std::thread::Builder::new().name("softer_gui-app".into()).stack_size(64 << 10).spawn(move || {
        while flag_ref.load(Acquire) == 0 { std::thread::yield_now(); }
        unsafe { resume(ctx_addr as *const u64) }
    }).expect("thread");
    let r = unsafe { handoff(ctx.as_mut_ptr(), flag.as_ptr(), top, pump_forever, pump_ptr) };
    r == 1
}
#[cfg(not(target_arch = "aarch64"))]
pub fn takeover_main_thread(_pump: Pump) -> bool { eprintln!("softer_gui: macOS backend is Apple Silicon only"); false }
