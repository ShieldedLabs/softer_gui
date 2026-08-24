//! Windows system interface: the `extern "system"` declarations the backend uses,
//! plus dynamic resolution for everything newer than the Vista baseline.
//!
//! This is a SIBLING of sys.rs, not a #[cfg] branch of it. sys.rs issues Linux
//! syscalls directly because the kernel ABI is a stability contract; Windows has
//! no such contract (ntdll's syscall numbers are an implementation detail of each
//! build), so the supported boundary is the DLL export. No binding crate is
//! involved: a binding crate is nothing but transcribed signatures, and these are
//! the fifty this crate actually uses.
//!
//! COMPATIBILITY RULE FOR THIS FILE, and it is load bearing: every function in a
//! `#[link]` block must exist on Windows Vista (most predate it by a decade).
//! Anything newer goes through `Dyn`, which GetProcAddress's it once at startup
//! and leaves a None behind when it is missing, so an old Windows degrades in
//! behaviour instead of failing to load the process. Adding a Windows 10 only
//! import to a link block below would silently raise the floor for the whole
//! crate, and it would do it at load time, before any of our code can react.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]

use core::ffi::c_void;

pub type HANDLE = *mut c_void;
pub type HWND = HANDLE;
pub type HDC = HANDLE;
pub type HBITMAP = HANDLE;
pub type HICON = HANDLE;
pub type HMODULE = HANDLE;
pub type HMONITOR = HANDLE;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type LRESULT = isize;
pub type BOOL = i32;
pub type WNDPROC = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

pub const NULL: HANDLE = core::ptr::null_mut();

// ---- structures, transcribed from the SDK headers -----------------------------
#[repr(C)] #[derive(Clone, Copy, Default, Debug)] pub struct POINT { pub x: i32, pub y: i32 }
#[repr(C)] #[derive(Clone, Copy, Default, Debug)] pub struct RECT { pub left: i32, pub top: i32, pub right: i32, pub bottom: i32 }

#[repr(C)]
pub struct WNDCLASSEXW {
    pub cbSize: u32, pub style: u32, pub lpfnWndProc: Option<WNDPROC>,
    pub cbClsExtra: i32, pub cbWndExtra: i32, pub hInstance: HMODULE,
    pub hIcon: HICON, pub hCursor: HANDLE, pub hbrBackground: HANDLE,
    pub lpszMenuName: *const u16, pub lpszClassName: *const u16, pub hIconSm: HICON,
}

#[repr(C)] #[derive(Clone, Copy)]
pub struct MSG { pub hwnd: HWND, pub message: u32, pub wParam: WPARAM, pub lParam: LPARAM, pub time: u32, pub pt: POINT }
impl Default for MSG {
    fn default() -> MSG { MSG { hwnd: NULL, message: 0, wParam: 0, lParam: 0, time: 0, pt: POINT::default() } }
}

#[repr(C)]
pub struct PAINTSTRUCT { pub hdc: HDC, pub fErase: BOOL, pub rcPaint: RECT, pub fRestore: BOOL, pub fIncUpdate: BOOL, pub rgbReserved: [u8; 32] }
impl Default for PAINTSTRUCT {
    fn default() -> PAINTSTRUCT { unsafe { core::mem::zeroed() } }
}

#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct BITMAPINFOHEADER {
    pub biSize: u32, pub biWidth: i32, pub biHeight: i32, pub biPlanes: u16, pub biBitCount: u16,
    pub biCompression: u32, pub biSizeImage: u32, pub biXPelsPerMeter: i32, pub biYPelsPerMeter: i32,
    pub biClrUsed: u32, pub biClrImportant: u32,
}
/// Header plus room for the colour table GDI never reads at 32bpp BI_RGB.
#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct BITMAPINFO { pub bmiHeader: BITMAPINFOHEADER, pub bmiColors: [u32; 3] }

#[repr(C)]
pub struct ICONINFO { pub fIcon: BOOL, pub xHotspot: u32, pub yHotspot: u32, pub hbmMask: HBITMAP, pub hbmColor: HBITMAP }

#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct MONITORINFO { pub cbSize: u32, pub rcMonitor: RECT, pub rcWork: RECT, pub dwFlags: u32 }

/// 220 bytes, asserted below: EnumDisplaySettingsW validates dmSize against it.
#[repr(C)] #[derive(Clone, Copy)]
pub struct DEVMODEW {
    pub dmDeviceName: [u16; 32],
    pub dmSpecVersion: u16, pub dmDriverVersion: u16, pub dmSize: u16, pub dmDriverExtra: u16,
    pub dmFields: u32,
    /// The printer/display union; this crate reads none of it.
    pub dmUnion1: [u32; 4],
    pub dmColor: i16, pub dmDuplex: i16, pub dmYResolution: i16, pub dmTTOption: i16, pub dmCollate: i16,
    pub dmFormName: [u16; 32],
    pub dmLogPixels: u16,
    pub dmBitsPerPel: u32, pub dmPelsWidth: u32, pub dmPelsHeight: u32,
    pub dmDisplayFlags: u32, pub dmDisplayFrequency: u32,
    pub dmICMMethod: u32, pub dmICMIntent: u32, pub dmMediaType: u32, pub dmDitherType: u32,
    pub dmReserved1: u32, pub dmReserved2: u32, pub dmPanningWidth: u32, pub dmPanningHeight: u32,
}
impl Default for DEVMODEW {
    fn default() -> DEVMODEW { unsafe { core::mem::zeroed() } }
}
const _: () = assert!(core::mem::size_of::<DEVMODEW>() == 220);

/// dwmapi.h's DWM_TIMING_INFO. Every field is present because cbSize must equal
/// the real size or the call returns E_INVALIDARG; only the first handful are read.
#[repr(C)] #[derive(Clone, Copy)]
pub struct DWM_TIMING_INFO {
    pub cbSize: u32,
    pub rateRefresh_num: u32, pub rateRefresh_den: u32,
    pub qpcRefreshPeriod: u64,
    pub rateCompose_num: u32, pub rateCompose_den: u32,
    pub qpcVBlank: u64,
    pub cRefresh: u64,
    pub cDXRefresh: u32,
    pub qpcCompose: u64,
    pub cFrame: u64,
    pub cDXPresent: u32,
    pub cRefreshFrame: u64,
    pub cFrameSubmitted: u64,
    pub cDXPresentSubmitted: u32,
    pub cFrameConfirmed: u64,
    pub cDXPresentConfirmed: u32,
    pub cRefreshConfirmed: u64,
    pub cDXRefreshConfirmed: u32,
    pub cFramesLate: u64,
    pub cFramesOutstanding: u32,
    pub cFrameDisplayed: u64,
    pub qpcFrameDisplayed: u64,
    pub cRefreshFrameDisplayed: u64,
    pub cFrameComplete: u64,
    pub qpcFrameComplete: u64,
    pub cFramePending: u64,
    pub qpcFramePending: u64,
    pub cFramesDisplayed: u64,
    pub cFramesComplete: u64,
    pub cFramesPending: u64,
    pub cFramesAvailable: u64,
    pub cFramesDropped: u64,
    pub cFramesMissed: u64,
    pub cRefreshNextDisplayed: u64,
    pub cRefreshNextPresented: u64,
    pub cRefreshesDisplayed: u64,
    pub cRefreshesPresented: u64,
    pub cRefreshStarted: u64,
    pub cPixelsReceived: u64,
    pub cPixelsDrawn: u64,
    pub cBuffersEmpty: u64,
}
impl Default for DWM_TIMING_INFO {
    fn default() -> DWM_TIMING_INFO {
        let mut t: DWM_TIMING_INFO = unsafe { core::mem::zeroed() };
        t.cbSize = core::mem::size_of::<DWM_TIMING_INFO>() as u32;
        t
    }
}

/// D3DKMT structures for the DWM-less vblank wait. Both are small and stable.
#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct D3DKMT_OPENADAPTERFROMHDC { pub hDc: usize, pub hAdapter: u32, pub luid_low: u32, pub luid_high: i32, pub VidPnSourceId: u32 }
#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct D3DKMT_WAITFORVERTICALBLANKEVENT { pub hAdapter: u32, pub hDevice: u32, pub VidPnSourceId: u32 }

// ---- constants ----------------------------------------------------------------
pub const CS_OWNDC: u32 = 0x0020;
pub const CS_HREDRAW: u32 = 0x0002;
pub const CS_VREDRAW: u32 = 0x0001;
pub const CS_DBLCLKS: u32 = 0x0008;

pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF0000;
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_CLIPCHILDREN: u32 = 0x0200_0000;
pub const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
pub const WS_EX_APPWINDOW: u32 = 0x0004_0000;

pub const SW_SHOW: i32 = 5;
pub const SW_HIDE: i32 = 0;

pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_FRAMECHANGED: u32 = 0x0020;
pub const SWP_NOOWNERZORDER: u32 = 0x0200;

pub const GWL_STYLE: i32 = -16;
pub const GWL_EXSTYLE: i32 = -20;

pub const PM_REMOVE: u32 = 0x0001;
pub const QS_ALLINPUT: u32 = 0x04FF;
pub const MWMO_INPUTAVAILABLE: u32 = 0x0004;
pub const INFINITE: u32 = 0xFFFF_FFFF;
pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_TIMEOUT: u32 = 258;

pub const WM_DESTROY: u32 = 0x0002;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_SETFOCUS: u32 = 0x0007;
pub const WM_KILLFOCUS: u32 = 0x0008;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_SETTINGCHANGE: u32 = 0x001A;
pub const WM_ACTIVATEAPP: u32 = 0x001C;
pub const WM_SETCURSOR: u32 = 0x0020;
pub const WM_SETICON: u32 = 0x0080;
pub const WM_NCCREATE: u32 = 0x0081;
pub const WM_DISPLAYCHANGE: u32 = 0x007E;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_KEYUP: u32 = 0x0101;
pub const WM_SYSKEYDOWN: u32 = 0x0104;
pub const WM_SYSKEYUP: u32 = 0x0105;
pub const WM_SYSCOMMAND: u32 = 0x0112;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_MBUTTONDOWN: u32 = 0x0207;
pub const WM_MBUTTONUP: u32 = 0x0208;
pub const WM_MOUSEWHEEL: u32 = 0x020A;
pub const WM_XBUTTONDOWN: u32 = 0x020B;
pub const WM_XBUTTONUP: u32 = 0x020C;
pub const WM_MOUSEHWHEEL: u32 = 0x020E;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_ENTERSIZEMOVE: u32 = 0x0231;
pub const WM_EXITSIZEMOVE: u32 = 0x0232;
pub const WM_DPICHANGED: u32 = 0x02E0;
pub const WM_USER: u32 = 0x0400;
/// Posted by the app thread to wake the pump out of its wait (poke()).
pub const WM_SOFTER_POKE: u32 = WM_USER + 1;
/// Posted by the app thread when it has a finished frame for the pump to put up.
pub const WM_SOFTER_BLIT: u32 = WM_USER + 2;

pub const SC_KEYMENU: usize = 0xF100;
pub const SC_SCREENSAVE: usize = 0xF140;
pub const SC_MONITORPOWER: usize = 0xF170;

pub const SIZE_MINIMIZED: usize = 1;

pub const ICON_SMALL: usize = 0;
pub const ICON_BIG: usize = 1;

pub const IDC_ARROW: usize = 32512;

pub const MONITOR_DEFAULTTONEAREST: u32 = 0x0002;

pub const BI_RGB: u32 = 0;
pub const DIB_RGB_COLORS: u32 = 0;
pub const SRCCOPY: u32 = 0x00CC_0020;
pub const LOGPIXELSX: i32 = 88;
pub const VREFRESH: i32 = 116;

pub const ENUM_CURRENT_SETTINGS: u32 = 0xFFFF_FFFF;

pub const SPI_GETKEYBOARDDELAY: u32 = 0x0016;
pub const SPI_GETKEYBOARDSPEED: u32 = 0x000A;

pub const MAPVK_VK_TO_CHAR: u32 = 2;
pub const MAPVK_VSC_TO_VK_EX: u32 = 3;

pub const VK_SHIFT: u32 = 0x10;
pub const VK_CONTROL: u32 = 0x11;
pub const VK_MENU: u32 = 0x12;
pub const VK_PAUSE: u32 = 0x13;
pub const VK_NUMLOCK: u32 = 0x90;
pub const VK_F4: u32 = 0x73;

pub const WHEEL_DELTA: i32 = 120;

pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE: isize = -3;

// ---- Vista-and-older imports ---------------------------------------------------
#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetModuleHandleW(name: *const u16) -> HMODULE;
    pub fn LoadLibraryW(name: *const u16) -> HMODULE;
    pub fn GetProcAddress(m: HMODULE, name: *const u8) -> *const c_void;
    pub fn QueryPerformanceCounter(v: *mut i64) -> BOOL;
    pub fn QueryPerformanceFrequency(v: *mut i64) -> BOOL;
    pub fn CreateEventW(sa: *mut c_void, manual: BOOL, initial: BOOL, name: *const u16) -> HANDLE;
    pub fn SetEvent(h: HANDLE) -> BOOL;
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn Sleep(ms: u32);
    pub fn GetLastError() -> u32;
}

#[link(name = "user32")]
unsafe extern "system" {
    pub fn RegisterClassExW(c: *const WNDCLASSEXW) -> u16;
    pub fn CreateWindowExW(ex: u32, class: *const u16, title: *const u16, style: u32,
                           x: i32, y: i32, w: i32, h: i32,
                           parent: HWND, menu: HANDLE, inst: HMODULE, param: *mut c_void) -> HWND;
    pub fn DestroyWindow(h: HWND) -> BOOL;
    pub fn DefWindowProcW(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT;
    pub fn ShowWindow(h: HWND, cmd: i32) -> BOOL;
    pub fn UpdateWindow(h: HWND) -> BOOL;
    pub fn PeekMessageW(m: *mut MSG, h: HWND, min: u32, max: u32, remove: u32) -> BOOL;
    pub fn TranslateMessage(m: *const MSG) -> BOOL;
    pub fn DispatchMessageW(m: *const MSG) -> LRESULT;
    pub fn PostMessageW(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> BOOL;
    pub fn SendMessageW(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT;
    pub fn MsgWaitForMultipleObjectsEx(n: u32, handles: *const HANDLE, ms: u32, mask: u32, flags: u32) -> u32;
    pub fn GetClientRect(h: HWND, r: *mut RECT) -> BOOL;
    pub fn GetWindowRect(h: HWND, r: *mut RECT) -> BOOL;
    pub fn AdjustWindowRectEx(r: *mut RECT, style: u32, menu: BOOL, ex: u32) -> BOOL;
    pub fn SetWindowPos(h: HWND, after: HWND, x: i32, y: i32, w: i32, ht: i32, flags: u32) -> BOOL;
    pub fn GetDC(h: HWND) -> HDC;
    pub fn ReleaseDC(h: HWND, dc: HDC) -> i32;
    pub fn BeginPaint(h: HWND, ps: *mut PAINTSTRUCT) -> HDC;
    pub fn EndPaint(h: HWND, ps: *const PAINTSTRUCT) -> BOOL;
    pub fn InvalidateRect(h: HWND, r: *const RECT, erase: BOOL) -> BOOL;
    pub fn ValidateRect(h: HWND, r: *const RECT) -> BOOL;
    pub fn SetWindowTextW(h: HWND, s: *const u16) -> BOOL;
    pub fn SetCapture(h: HWND) -> HWND;
    pub fn ReleaseCapture() -> BOOL;
    pub fn ShowCursor(show: BOOL) -> i32;
    pub fn SetCursor(c: HANDLE) -> HANDLE;
    pub fn LoadCursorW(inst: HMODULE, name: usize) -> HANDLE;
    pub fn GetKeyboardState(state: *mut u8) -> BOOL;
    pub fn GetKeyboardLayout(thread: u32) -> HANDLE;
    pub fn ToUnicodeEx(vk: u32, sc: u32, state: *const u8, buf: *mut u16, n: i32, flags: u32, hkl: HANDLE) -> i32;
    pub fn MapVirtualKeyW(code: u32, map: u32) -> u32;
    pub fn SystemParametersInfoW(action: u32, param: u32, ptr: *mut c_void, ini: u32) -> BOOL;
    pub fn MonitorFromWindow(h: HWND, flags: u32) -> HMONITOR;
    pub fn GetMonitorInfoW(m: HMONITOR, info: *mut MONITORINFO) -> BOOL;
    pub fn EnumDisplaySettingsW(name: *const u16, mode: u32, dm: *mut DEVMODEW) -> BOOL;
    pub fn CreateIconIndirect(info: *const ICONINFO) -> HICON;
    pub fn DestroyIcon(i: HICON) -> BOOL;
    pub fn ClientToScreen(h: HWND, p: *mut POINT) -> BOOL;
    pub fn ScreenToClient(h: HWND, p: *mut POINT) -> BOOL;
    pub fn GetCursorPos(p: *mut POINT) -> BOOL;
    pub fn PostQuitMessage(code: i32);
    pub fn SetTimer(h: HWND, id: usize, ms: u32, proc_: *const c_void) -> usize;
    pub fn KillTimer(h: HWND, id: usize) -> BOOL;
    #[cfg(target_pointer_width = "64")]
    pub fn GetWindowLongPtrW(h: HWND, index: i32) -> isize;
    #[cfg(target_pointer_width = "64")]
    pub fn SetWindowLongPtrW(h: HWND, index: i32, v: isize) -> isize;
    #[cfg(target_pointer_width = "32")]
    pub fn GetWindowLongW(h: HWND, index: i32) -> i32;
    #[cfg(target_pointer_width = "32")]
    pub fn SetWindowLongW(h: HWND, index: i32, v: i32) -> i32;
}

/// One spelling for the window-style accessors on both widths: Win32 makes the
/// 64-bit names macros over the 32-bit ones and only exports Ptr on 64-bit.
#[cfg(target_pointer_width = "64")]
pub unsafe fn get_window_long(h: HWND, i: i32) -> isize { unsafe { GetWindowLongPtrW(h, i) } }
#[cfg(target_pointer_width = "64")]
pub unsafe fn set_window_long(h: HWND, i: i32, v: isize) -> isize { unsafe { SetWindowLongPtrW(h, i, v) } }
#[cfg(target_pointer_width = "32")]
pub unsafe fn get_window_long(h: HWND, i: i32) -> isize { unsafe { GetWindowLongW(h, i) as isize } }
#[cfg(target_pointer_width = "32")]
pub unsafe fn set_window_long(h: HWND, i: i32, v: isize) -> isize { unsafe { SetWindowLongW(h, i, v as i32) as isize } }

#[link(name = "gdi32")]
unsafe extern "system" {
    pub fn CreateDIBSection(dc: HDC, bmi: *const BITMAPINFO, usage: u32, bits: *mut *mut c_void, section: HANDLE, offset: u32) -> HBITMAP;
    pub fn CreateCompatibleDC(dc: HDC) -> HDC;
    pub fn CreateBitmap(w: i32, h: i32, planes: u32, bits_per_pixel: u32, bits: *const c_void) -> HBITMAP;
    pub fn DeleteDC(dc: HDC) -> BOOL;
    pub fn DeleteObject(o: HANDLE) -> BOOL;
    pub fn SelectObject(dc: HDC, o: HANDLE) -> HANDLE;
    pub fn BitBlt(dst: HDC, x: i32, y: i32, w: i32, h: i32, src: HDC, sx: i32, sy: i32, rop: u32) -> BOOL;
    pub fn GdiFlush() -> BOOL;
    pub fn GetDeviceCaps(dc: HDC, index: i32) -> i32;
}

// ---- dynamically resolved: everything newer than the baseline ------------------
pub type FnDwmGetCompositionTimingInfo = unsafe extern "system" fn(HWND, *mut DWM_TIMING_INFO) -> i32;
pub type FnDwmFlush = unsafe extern "system" fn() -> i32;
pub type FnDwmIsCompositionEnabled = unsafe extern "system" fn(*mut BOOL) -> i32;
pub type FnD3DKMTOpenAdapterFromHdc = unsafe extern "system" fn(*mut D3DKMT_OPENADAPTERFROMHDC) -> i32;
pub type FnD3DKMTWaitForVerticalBlankEvent = unsafe extern "system" fn(*const D3DKMT_WAITFORVERTICALBLANKEVENT) -> i32;
pub type FnSetProcessDpiAwarenessContext = unsafe extern "system" fn(isize) -> BOOL;
pub type FnSetProcessDpiAwareness = unsafe extern "system" fn(u32) -> i32;
pub type FnSetProcessDPIAware = unsafe extern "system" fn() -> BOOL;
pub type FnGetDpiForWindow = unsafe extern "system" fn(HWND) -> u32;

/// Resolved once at open(); a None member means "this Windows does not have it"
/// and the caller takes its documented fallback. Never assume a member is present.
pub struct Dyn {
    pub dwm_timing: Option<FnDwmGetCompositionTimingInfo>,
    pub dwm_flush: Option<FnDwmFlush>,
    pub dwm_enabled: Option<FnDwmIsCompositionEnabled>,
    pub kmt_open: Option<FnD3DKMTOpenAdapterFromHdc>,
    pub kmt_wait: Option<FnD3DKMTWaitForVerticalBlankEvent>,
    pub dpi_for_window: Option<FnGetDpiForWindow>,
}

pub fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(core::iter::once(0)).collect() }

unsafe fn sym(m: HMODULE, name: &[u8]) -> *const c_void {
    if m.is_null() { return core::ptr::null(); }
    debug_assert!(name.last() == Some(&0));
    unsafe { GetProcAddress(m, name.as_ptr()) }
}

impl Dyn {
    pub fn load() -> Dyn {
        unsafe {
            let dwm = LoadLibraryW(wide("dwmapi.dll").as_ptr());
            let gdi = GetModuleHandleW(wide("gdi32.dll").as_ptr());
            let user = GetModuleHandleW(wide("user32.dll").as_ptr());
            Dyn {
                dwm_timing: core::mem::transmute::<*const c_void, Option<FnDwmGetCompositionTimingInfo>>(sym(dwm, b"DwmGetCompositionTimingInfo\0")),
                dwm_flush: core::mem::transmute::<*const c_void, Option<FnDwmFlush>>(sym(dwm, b"DwmFlush\0")),
                dwm_enabled: core::mem::transmute::<*const c_void, Option<FnDwmIsCompositionEnabled>>(sym(dwm, b"DwmIsCompositionEnabled\0")),
                kmt_open: core::mem::transmute::<*const c_void, Option<FnD3DKMTOpenAdapterFromHdc>>(sym(gdi, b"D3DKMTOpenAdapterFromHdc\0")),
                kmt_wait: core::mem::transmute::<*const c_void, Option<FnD3DKMTWaitForVerticalBlankEvent>>(sym(gdi, b"D3DKMTWaitForVerticalBlankEvent\0")),
                dpi_for_window: core::mem::transmute::<*const c_void, Option<FnGetDpiForWindow>>(sym(user, b"GetDpiForWindow\0")),
            }
        }
    }
}

/// The best DPI awareness this Windows offers, newest first. Must run before the
/// first window exists. Without it Windows lies about window sizes and stretches
/// our output, which for a crate whose premise is exact pixels is not survivable.
pub fn set_dpi_aware() {
    unsafe {
        let user = GetModuleHandleW(wide("user32.dll").as_ptr());
        let f: Option<FnSetProcessDpiAwarenessContext> = core::mem::transmute(sym(user, b"SetProcessDpiAwarenessContext\0"));
        if let Some(f) = f {
            // v2 (Windows 10 1703) scales the non-client area too; v1 is the 8.1 behaviour.
            if f(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 { return; }
            if f(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) != 0 { return; }
        }
        let shcore = LoadLibraryW(wide("shcore.dll").as_ptr());
        let f: Option<FnSetProcessDpiAwareness> = core::mem::transmute(sym(shcore, b"SetProcessDpiAwareness\0"));
        if let Some(f) = f { if f(2) >= 0 { return; } }        // 2 = PROCESS_PER_MONITOR_DPI_AWARE
        let f: Option<FnSetProcessDPIAware> = core::mem::transmute(sym(user, b"SetProcessDPIAware\0"));
        if let Some(f) = f { f(); }                             // Vista: system-DPI aware, the best it has
    }
}

pub fn qpc() -> i64 { let mut v = 0i64; unsafe { QueryPerformanceCounter(&mut v) }; v }
pub fn qpf() -> i64 { let mut v = 0i64; unsafe { QueryPerformanceFrequency(&mut v) }; if v == 0 { 1 } else { v } }
