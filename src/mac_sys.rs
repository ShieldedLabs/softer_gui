//! The macOS backend's foreign symbols, resolved one of two ways.
//!
//! * A native `aarch64-apple-darwin` build links AppKit, IOSurface, CoreVideo
//!   and the Objective-C runtime the ordinary way.
//! * A cosmopolitan build (`--features cosmo`) cannot: it produces one file
//!   that must also start on a Linux box where none of those exist, and the
//!   target spec says `linux`, so `cfg(target_os = "macos")` is false even when
//!   the program is running on a Mac. Every symbol is therefore looked up at
//!   run time through `cosmo_dlopen`, and nothing is touched unless cosmo's
//!   `__hostos` says XNU.
//!
//! Both halves export the same function-shaped API, so `mac.rs` reads the same
//! either way. Data symbols (`kIOSurfaceWidth`, `kCAFilterNearest`, ...) are
//! accessor functions rather than statics for exactly that reason.
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]

pub type id = *mut core::ffi::c_void;
pub type SEL = *mut core::ffi::c_void;
pub type Class = *mut core::ffi::c_void;
pub type CFTypeRef = *const core::ffi::c_void;
pub type CFStringRef = CFTypeRef;
pub type Boolean = u8;

#[repr(C)] #[derive(Clone, Copy, Default)] pub struct NSPoint { pub x: f64, pub y: f64 }
#[repr(C)] #[derive(Clone, Copy, Default)] pub struct NSSize { pub w: f64, pub h: f64 }
#[repr(C)] #[derive(Clone, Copy, Default)] pub struct NSRect { pub o: NSPoint, pub s: NSSize }
#[repr(C)] #[derive(Clone, Copy)] pub struct CVTime { pub value: i64, pub scale: i32, pub flags: i32 }

pub type LinkCallback = extern "C" fn(*mut core::ffi::c_void, *const u8, *const u8, u64, *mut u64, *mut core::ffi::c_void) -> i32;

// ---------------------------------------------------------------- native macOS
#[cfg(not(cosmo))]
mod imp {
    use super::*;

    #[link(name = "objc")]
    unsafe extern "C" {
        pub fn objc_getClass(name: *const u8) -> Class;
        pub fn sel_registerName(name: *const u8) -> SEL;
        pub fn objc_msgSend();
        pub fn objc_allocateClassPair(sup: Class, name: *const u8, extra: usize) -> Class;
        pub fn objc_registerClassPair(cls: Class);
        pub fn class_addMethod(cls: Class, sel: SEL, imp: *const core::ffi::c_void, types: *const u8) -> Boolean;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFRelease(r: CFTypeRef);
        pub fn CFNumberCreate(alloc: CFTypeRef, ty: i64, ptr: *const core::ffi::c_void) -> CFTypeRef;
        pub fn CFDictionaryCreate(alloc: CFTypeRef, keys: *const CFTypeRef, vals: *const CFTypeRef, n: i64, kcb: *const core::ffi::c_void, vcb: *const core::ffi::c_void) -> CFTypeRef;
        pub fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
        static kCFTypeDictionaryKeyCallBacks: [u8; 0];
        static kCFTypeDictionaryValueCallBacks: [u8; 0];
    }
    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        pub fn IOSurfaceCreate(props: CFTypeRef) -> CFTypeRef;
        pub fn IOSurfaceGetBaseAddress(s: CFTypeRef) -> *mut u8;
        pub fn IOSurfaceGetBytesPerRow(s: CFTypeRef) -> usize;
        pub fn IOSurfaceLock(s: CFTypeRef, options: u32, seed: *mut u32) -> i32;
        pub fn IOSurfaceUnlock(s: CFTypeRef, options: u32, seed: *mut u32) -> i32;
        pub fn IOSurfaceIsInUse(s: CFTypeRef) -> Boolean;
        static kIOSurfaceWidth: CFStringRef;
        static kIOSurfaceHeight: CFStringRef;
        static kIOSurfaceBytesPerElement: CFStringRef;
        static kIOSurfaceBytesPerRow: CFStringRef;
        static kIOSurfacePixelFormat: CFStringRef;
    }
    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        pub fn CVDisplayLinkCreateWithCGDisplay(display: u32, out: *mut *mut core::ffi::c_void) -> i32;
        pub fn CVDisplayLinkSetOutputCallback(link: *mut core::ffi::c_void, cb: LinkCallback, user: *mut core::ffi::c_void) -> i32;
        pub fn CVDisplayLinkStart(link: *mut core::ffi::c_void) -> i32;
        pub fn CVDisplayLinkStop(link: *mut core::ffi::c_void) -> i32;
        pub fn CVDisplayLinkSetCurrentCGDisplay(link: *mut core::ffi::c_void, display: u32) -> i32;
        pub fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link: *mut core::ffi::c_void) -> CVTime;
        pub fn CVDisplayLinkGetActualOutputVideoRefreshPeriod(link: *mut core::ffi::c_void) -> f64;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" { pub fn CGMainDisplayID() -> u32; }
    #[link(name = "QuartzCore", kind = "framework")]
    unsafe extern "C" { static kCAFilterNearest: id; }
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        pub fn TISCopyCurrentKeyboardLayoutInputSource() -> CFTypeRef;
        pub fn TISGetInputSourceProperty(src: CFTypeRef, key: CFStringRef) -> CFTypeRef;
        pub fn LMGetKbdType() -> u8;
        pub fn UCKeyTranslate(layout: *const u8, vk: u16, action: u16, mods: u32, kbd_type: u32, options: u32, dead: *mut u32, max: usize, actual: *mut usize, out: *mut u16) -> i32;
        static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
    }
    #[link(name = "System")]
    unsafe extern "C" { pub fn pthread_main_np() -> i32; }

    /// Statically linked: the frameworks are there because the loader said so.
    pub fn load() -> bool { true }

    pub fn msg_send_ptr() -> *const () { objc_msgSend as *const () }
    pub fn k_iosurface_width() -> CFStringRef { unsafe { kIOSurfaceWidth } }
    pub fn k_iosurface_height() -> CFStringRef { unsafe { kIOSurfaceHeight } }
    pub fn k_iosurface_bytes_per_element() -> CFStringRef { unsafe { kIOSurfaceBytesPerElement } }
    pub fn k_iosurface_bytes_per_row() -> CFStringRef { unsafe { kIOSurfaceBytesPerRow } }
    pub fn k_iosurface_pixel_format() -> CFStringRef { unsafe { kIOSurfacePixelFormat } }
    pub fn cf_dict_key_callbacks() -> *const core::ffi::c_void { unsafe { kCFTypeDictionaryKeyCallBacks.as_ptr() as *const _ } }
    pub fn cf_dict_value_callbacks() -> *const core::ffi::c_void { unsafe { kCFTypeDictionaryValueCallBacks.as_ptr() as *const _ } }
    pub fn ca_filter_nearest() -> id { unsafe { kCAFilterNearest } }
    pub fn k_tis_unicode_key_layout_data() -> CFStringRef { unsafe { kTISPropertyUnicodeKeyLayoutData } }
}

// ------------------------------------------------------------ cosmopolitan APE
#[cfg(cosmo)]
mod imp {
    use super::*;
    use core::ffi::c_void;
    use std::sync::OnceLock;

    unsafe extern "C" {
        fn cosmo_dlopen(path: *const u8, flags: i32) -> *mut c_void;
        fn cosmo_dlsym(handle: *mut c_void, name: *const u8) -> *mut c_void;
        /// Wraps a foreign function so cosmo can call it: blocks cosmo's signal
        /// emulation and forwards x0-x7/q0-q7, x8 (so struct returns work) and
        /// 256 bytes of stack arguments.
        fn cosmo_dltramp(func: *mut c_void) -> *mut c_void;
    }

    const RTLD_LAZY: i32 = 1;

    struct Syms {
        objc_getClass: *mut c_void,
        sel_registerName: *mut c_void,
        objc_msgSend: *mut c_void,
        objc_allocateClassPair: *mut c_void,
        objc_registerClassPair: *mut c_void,
        class_addMethod: *mut c_void,
        CFRelease: *mut c_void,
        CFNumberCreate: *mut c_void,
        CFDictionaryCreate: *mut c_void,
        CFDataGetBytePtr: *mut c_void,
        kCFTypeDictionaryKeyCallBacks: *const c_void,
        kCFTypeDictionaryValueCallBacks: *const c_void,
        IOSurfaceCreate: *mut c_void,
        IOSurfaceGetBaseAddress: *mut c_void,
        IOSurfaceGetBytesPerRow: *mut c_void,
        IOSurfaceLock: *mut c_void,
        IOSurfaceUnlock: *mut c_void,
        IOSurfaceIsInUse: *mut c_void,
        kIOSurfaceWidth: CFStringRef,
        kIOSurfaceHeight: CFStringRef,
        kIOSurfaceBytesPerElement: CFStringRef,
        kIOSurfaceBytesPerRow: CFStringRef,
        kIOSurfacePixelFormat: CFStringRef,
        CVDisplayLinkCreateWithCGDisplay: *mut c_void,
        CVDisplayLinkSetOutputCallback: *mut c_void,
        CVDisplayLinkStart: *mut c_void,
        CVDisplayLinkStop: *mut c_void,
        CVDisplayLinkSetCurrentCGDisplay: *mut c_void,
        CVDisplayLinkGetNominalOutputVideoRefreshPeriod: *mut c_void,
        CVDisplayLinkGetActualOutputVideoRefreshPeriod: *mut c_void,
        CGMainDisplayID: *mut c_void,
        kCAFilterNearest: id,
        TISCopyCurrentKeyboardLayoutInputSource: *mut c_void,
        TISGetInputSourceProperty: *mut c_void,
        LMGetKbdType: *mut c_void,
        UCKeyTranslate: *mut c_void,
        kTISPropertyUnicodeKeyLayoutData: CFStringRef,
        pthread_main_np: *mut c_void,
    }
    unsafe impl Send for Syms {}
    unsafe impl Sync for Syms {}

    static SYMS: OnceLock<Option<Syms>> = OnceLock::new();

    fn dlopen(path: &str) -> *mut c_void {
        let c = format!("{path}\0");
        unsafe { cosmo_dlopen(c.as_ptr(), RTLD_LAZY) }
    }
    /// A callable foreign function: resolved, then trampolined.
    fn f(h: *mut c_void, name: &str) -> *mut c_void {
        let c = format!("{name}\0");
        let p = unsafe { cosmo_dlsym(h, c.as_ptr()) };
        if p.is_null() { return p }
        unsafe { cosmo_dltramp(p) }
    }
    /// The address of a foreign variable (no trampoline: it is not code).
    fn v(h: *mut c_void, name: &str) -> *const c_void {
        let c = format!("{name}\0");
        unsafe { cosmo_dlsym(h, c.as_ptr()) as *const c_void }
    }
    /// A foreign `CFStringRef` *variable*: dlsym gives its address, the key is
    /// one dereference further in.
    fn vderef(h: *mut c_void, name: &str) -> CFTypeRef {
        let p = v(h, name);
        if p.is_null() { return core::ptr::null() }
        unsafe { *(p as *const CFTypeRef) }
    }

    fn resolve() -> Option<Syms> {
        if !crate::mac_sys::on_macos() { return None }
        let objc = dlopen("/usr/lib/libobjc.A.dylib");
        // Loading AppKit is what registers NSWindow and friends with the
        // Objective-C runtime; without it objc_getClass returns null.
        let appkit = dlopen("/System/Library/Frameworks/AppKit.framework/AppKit");
        let cf = dlopen("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation");
        let iosurf = dlopen("/System/Library/Frameworks/IOSurface.framework/IOSurface");
        let cv = dlopen("/System/Library/Frameworks/CoreVideo.framework/CoreVideo");
        let cg = dlopen("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics");
        let qc = dlopen("/System/Library/Frameworks/QuartzCore.framework/QuartzCore");
        let carbon = dlopen("/System/Library/Frameworks/Carbon.framework/Carbon");
        let sys = dlopen("/usr/lib/libSystem.B.dylib");
        if objc.is_null() || appkit.is_null() || cf.is_null() || iosurf.is_null()
            || cv.is_null() || cg.is_null() || qc.is_null() { return None }

        let s = Syms {
            objc_getClass: f(objc, "objc_getClass"),
            sel_registerName: f(objc, "sel_registerName"),
            objc_msgSend: f(objc, "objc_msgSend"),
            objc_allocateClassPair: f(objc, "objc_allocateClassPair"),
            objc_registerClassPair: f(objc, "objc_registerClassPair"),
            class_addMethod: f(objc, "class_addMethod"),
            CFRelease: f(cf, "CFRelease"),
            CFNumberCreate: f(cf, "CFNumberCreate"),
            CFDictionaryCreate: f(cf, "CFDictionaryCreate"),
            CFDataGetBytePtr: f(cf, "CFDataGetBytePtr"),
            kCFTypeDictionaryKeyCallBacks: v(cf, "kCFTypeDictionaryKeyCallBacks"),
            kCFTypeDictionaryValueCallBacks: v(cf, "kCFTypeDictionaryValueCallBacks"),
            IOSurfaceCreate: f(iosurf, "IOSurfaceCreate"),
            IOSurfaceGetBaseAddress: f(iosurf, "IOSurfaceGetBaseAddress"),
            IOSurfaceGetBytesPerRow: f(iosurf, "IOSurfaceGetBytesPerRow"),
            IOSurfaceLock: f(iosurf, "IOSurfaceLock"),
            IOSurfaceUnlock: f(iosurf, "IOSurfaceUnlock"),
            IOSurfaceIsInUse: f(iosurf, "IOSurfaceIsInUse"),
            kIOSurfaceWidth: vderef(iosurf, "kIOSurfaceWidth"),
            kIOSurfaceHeight: vderef(iosurf, "kIOSurfaceHeight"),
            kIOSurfaceBytesPerElement: vderef(iosurf, "kIOSurfaceBytesPerElement"),
            kIOSurfaceBytesPerRow: vderef(iosurf, "kIOSurfaceBytesPerRow"),
            kIOSurfacePixelFormat: vderef(iosurf, "kIOSurfacePixelFormat"),
            CVDisplayLinkCreateWithCGDisplay: f(cv, "CVDisplayLinkCreateWithCGDisplay"),
            CVDisplayLinkSetOutputCallback: f(cv, "CVDisplayLinkSetOutputCallback"),
            CVDisplayLinkStart: f(cv, "CVDisplayLinkStart"),
            CVDisplayLinkStop: f(cv, "CVDisplayLinkStop"),
            CVDisplayLinkSetCurrentCGDisplay: f(cv, "CVDisplayLinkSetCurrentCGDisplay"),
            CVDisplayLinkGetNominalOutputVideoRefreshPeriod: f(cv, "CVDisplayLinkGetNominalOutputVideoRefreshPeriod"),
            CVDisplayLinkGetActualOutputVideoRefreshPeriod: f(cv, "CVDisplayLinkGetActualOutputVideoRefreshPeriod"),
            CGMainDisplayID: f(cg, "CGMainDisplayID"),
            kCAFilterNearest: vderef(qc, "kCAFilterNearest") as id,
            TISCopyCurrentKeyboardLayoutInputSource: f(carbon, "TISCopyCurrentKeyboardLayoutInputSource"),
            TISGetInputSourceProperty: f(carbon, "TISGetInputSourceProperty"),
            LMGetKbdType: f(carbon, "LMGetKbdType"),
            UCKeyTranslate: f(carbon, "UCKeyTranslate"),
            kTISPropertyUnicodeKeyLayoutData: vderef(carbon, "kTISPropertyUnicodeKeyLayoutData"),
            pthread_main_np: f(sys, "pthread_main_np"),
        };
        // The window cannot be built without these; the keyboard ones are
        // allowed to be missing (dead keys degrade, the app still runs).
        if s.objc_getClass.is_null() || s.objc_msgSend.is_null() || s.IOSurfaceCreate.is_null()
            || s.CVDisplayLinkCreateWithCGDisplay.is_null() { return None }
        Some(s)
    }

    fn syms() -> &'static Syms {
        SYMS.get_or_init(resolve).as_ref().expect("macOS frameworks not loaded")
    }

    /// Resolve everything now; false if this host has no AppKit.
    pub fn load() -> bool { SYMS.get_or_init(resolve).is_some() }

    macro_rules! call {
        ($field:ident, $t:ty) => {{ let p = syms().$field; unsafe { core::mem::transmute::<*mut c_void, $t>(p) } }};
    }

    pub unsafe fn objc_getClass(name: *const u8) -> Class { call!(objc_getClass, extern "C" fn(*const u8) -> Class)(name) }
    pub unsafe fn sel_registerName(name: *const u8) -> SEL { call!(sel_registerName, extern "C" fn(*const u8) -> SEL)(name) }
    pub unsafe fn objc_allocateClassPair(sup: Class, name: *const u8, extra: usize) -> Class { call!(objc_allocateClassPair, extern "C" fn(Class, *const u8, usize) -> Class)(sup, name, extra) }
    pub unsafe fn objc_registerClassPair(cls: Class) { call!(objc_registerClassPair, extern "C" fn(Class))(cls) }
    pub unsafe fn class_addMethod(cls: Class, sel: SEL, imp: *const c_void, types: *const u8) -> Boolean { call!(class_addMethod, extern "C" fn(Class, SEL, *const c_void, *const u8) -> Boolean)(cls, sel, imp, types) }

    pub unsafe fn CFRelease(r: CFTypeRef) { call!(CFRelease, extern "C" fn(CFTypeRef))(r) }
    pub unsafe fn CFNumberCreate(a: CFTypeRef, ty: i64, p: *const c_void) -> CFTypeRef { call!(CFNumberCreate, extern "C" fn(CFTypeRef, i64, *const c_void) -> CFTypeRef)(a, ty, p) }
    pub unsafe fn CFDictionaryCreate(a: CFTypeRef, k: *const CFTypeRef, v: *const CFTypeRef, n: i64, kcb: *const c_void, vcb: *const c_void) -> CFTypeRef { call!(CFDictionaryCreate, extern "C" fn(CFTypeRef, *const CFTypeRef, *const CFTypeRef, i64, *const c_void, *const c_void) -> CFTypeRef)(a, k, v, n, kcb, vcb) }
    pub unsafe fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8 { call!(CFDataGetBytePtr, extern "C" fn(CFTypeRef) -> *const u8)(d) }

    pub unsafe fn IOSurfaceCreate(props: CFTypeRef) -> CFTypeRef { call!(IOSurfaceCreate, extern "C" fn(CFTypeRef) -> CFTypeRef)(props) }
    pub unsafe fn IOSurfaceGetBaseAddress(s: CFTypeRef) -> *mut u8 { call!(IOSurfaceGetBaseAddress, extern "C" fn(CFTypeRef) -> *mut u8)(s) }
    pub unsafe fn IOSurfaceGetBytesPerRow(s: CFTypeRef) -> usize { call!(IOSurfaceGetBytesPerRow, extern "C" fn(CFTypeRef) -> usize)(s) }
    pub unsafe fn IOSurfaceLock(s: CFTypeRef, o: u32, seed: *mut u32) -> i32 { call!(IOSurfaceLock, extern "C" fn(CFTypeRef, u32, *mut u32) -> i32)(s, o, seed) }
    pub unsafe fn IOSurfaceUnlock(s: CFTypeRef, o: u32, seed: *mut u32) -> i32 { call!(IOSurfaceUnlock, extern "C" fn(CFTypeRef, u32, *mut u32) -> i32)(s, o, seed) }
    pub unsafe fn IOSurfaceIsInUse(s: CFTypeRef) -> Boolean { call!(IOSurfaceIsInUse, extern "C" fn(CFTypeRef) -> Boolean)(s) }

    pub unsafe fn CVDisplayLinkCreateWithCGDisplay(d: u32, out: *mut *mut c_void) -> i32 { call!(CVDisplayLinkCreateWithCGDisplay, extern "C" fn(u32, *mut *mut c_void) -> i32)(d, out) }
    pub unsafe fn CVDisplayLinkSetOutputCallback(l: *mut c_void, cb: LinkCallback, u: *mut c_void) -> i32 { call!(CVDisplayLinkSetOutputCallback, extern "C" fn(*mut c_void, LinkCallback, *mut c_void) -> i32)(l, cb, u) }
    pub unsafe fn CVDisplayLinkStart(l: *mut c_void) -> i32 { call!(CVDisplayLinkStart, extern "C" fn(*mut c_void) -> i32)(l) }
    pub unsafe fn CVDisplayLinkStop(l: *mut c_void) -> i32 { call!(CVDisplayLinkStop, extern "C" fn(*mut c_void) -> i32)(l) }
    pub unsafe fn CVDisplayLinkSetCurrentCGDisplay(l: *mut c_void, d: u32) -> i32 { call!(CVDisplayLinkSetCurrentCGDisplay, extern "C" fn(*mut c_void, u32) -> i32)(l, d) }
    pub unsafe fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(l: *mut c_void) -> CVTime { call!(CVDisplayLinkGetNominalOutputVideoRefreshPeriod, extern "C" fn(*mut c_void) -> CVTime)(l) }
    pub unsafe fn CVDisplayLinkGetActualOutputVideoRefreshPeriod(l: *mut c_void) -> f64 { call!(CVDisplayLinkGetActualOutputVideoRefreshPeriod, extern "C" fn(*mut c_void) -> f64)(l) }

    pub unsafe fn CGMainDisplayID() -> u32 { call!(CGMainDisplayID, extern "C" fn() -> u32)() }

    pub unsafe fn TISCopyCurrentKeyboardLayoutInputSource() -> CFTypeRef {
        if syms().TISCopyCurrentKeyboardLayoutInputSource.is_null() { return core::ptr::null() }
        call!(TISCopyCurrentKeyboardLayoutInputSource, extern "C" fn() -> CFTypeRef)()
    }
    pub unsafe fn TISGetInputSourceProperty(src: CFTypeRef, key: CFStringRef) -> CFTypeRef {
        if syms().TISGetInputSourceProperty.is_null() { return core::ptr::null() }
        call!(TISGetInputSourceProperty, extern "C" fn(CFTypeRef, CFStringRef) -> CFTypeRef)(src, key)
    }
    pub unsafe fn LMGetKbdType() -> u8 {
        if syms().LMGetKbdType.is_null() { return 0 }
        call!(LMGetKbdType, extern "C" fn() -> u8)()
    }
    pub unsafe fn UCKeyTranslate(layout: *const u8, vk: u16, action: u16, mods: u32, kbd: u32, opts: u32, dead: *mut u32, max: usize, actual: *mut usize, out: *mut u16) -> i32 {
        if syms().UCKeyTranslate.is_null() { return -1 }
        call!(UCKeyTranslate, extern "C" fn(*const u8, u16, u16, u32, u32, u32, *mut u32, usize, *mut usize, *mut u16) -> i32)(layout, vk, action, mods, kbd, opts, dead, max, actual, out)
    }
    pub unsafe fn pthread_main_np() -> i32 {
        if syms().pthread_main_np.is_null() { return 1 }
        call!(pthread_main_np, extern "C" fn() -> i32)()
    }

    pub fn msg_send_ptr() -> *const () { syms().objc_msgSend as *const () }
    pub fn k_iosurface_width() -> CFStringRef { syms().kIOSurfaceWidth }
    pub fn k_iosurface_height() -> CFStringRef { syms().kIOSurfaceHeight }
    pub fn k_iosurface_bytes_per_element() -> CFStringRef { syms().kIOSurfaceBytesPerElement }
    pub fn k_iosurface_bytes_per_row() -> CFStringRef { syms().kIOSurfaceBytesPerRow }
    pub fn k_iosurface_pixel_format() -> CFStringRef { syms().kIOSurfacePixelFormat }
    pub fn cf_dict_key_callbacks() -> *const c_void { syms().kCFTypeDictionaryKeyCallBacks }
    pub fn cf_dict_value_callbacks() -> *const c_void { syms().kCFTypeDictionaryValueCallBacks }
    pub fn ca_filter_nearest() -> id { syms().kCAFilterNearest }
    pub fn k_tis_unicode_key_layout_data() -> CFStringRef { syms().kTISPropertyUnicodeKeyLayoutData }
}

pub use imp::*;

// ------------------------------------------------------------ which OS is this
#[cfg(cosmo)]
unsafe extern "C" {
    /// cosmopolitan's record of the OS it actually started on:
    /// 1 Linux, 2 Metal, 4 Windows, 8 XNU, 16 OpenBSD, 32 FreeBSD, 64 NetBSD.
    static __hostos: i32;
}

/// Is this process running on macOS? A compile-time fact in a native build, a
/// run-time one in an APE.
#[cfg(cosmo)]
pub fn on_macos() -> bool { unsafe { __hostos == 8 } }
#[cfg(not(cosmo))]
pub fn on_macos() -> bool { cfg!(target_os = "macos") }

/// Is this process running on Linux?
#[cfg(cosmo)]
pub fn on_linux() -> bool { unsafe { __hostos == 1 } }
#[cfg(not(cosmo))]
pub fn on_linux() -> bool { cfg!(target_os = "linux") }

/// Take Rust's stack-overflow handler out of the way (cosmo builds only).
///
/// std installs an `SA_SIGINFO` handler for SIGSEGV/SIGBUS to turn a guard-page
/// hit into "thread has overflowed its stack". Under cosmopolitan on macOS the
/// handler gets called with a NULL `siginfo_t`, so its first act -- reading
/// `info->si_addr` -- faults at address 0x10, *inside a fault handler*. The
/// process then wedges instead of dying, and the symptom is a hang in whichever
/// Apple call happened to be running, which is about as misleading as a symptom
/// gets: it cost most of a debugging session here.
///
/// Restoring the default disposition does not fix anything -- it makes a crash
/// look like a crash. Guard-page overflow detection is no loss under cosmo,
/// which does not set up the guard pages std is looking for anyway.
#[cfg(cosmo)]
pub fn drop_std_fault_handlers() {
    unsafe extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
        static SIGSEGV: i32;
        static SIGBUS: i32;
    }
    const SIG_DFL: usize = 0;
    unsafe {
        signal(SIGSEGV, SIG_DFL);
        signal(SIGBUS, SIG_DFL);
    }
}
#[cfg(not(cosmo))]
pub fn drop_std_fault_handlers() {}
