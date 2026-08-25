//! The capable Windows presenter: D3D11 into a DXGI flip-model swapchain, paced by
//! the swapchain's waitable object. Windows 8.1 and up; `win.rs` falls back to the
//! GDI path below that, and the two are tuned for different things.
//!
//! WHAT THIS BUYS OVER GDI, and why it is worth a second implementation:
//!
//! * **One copy instead of two.** A GDI blit lands in the window's redirection
//!   surface, which DWM then composites from. A flip-model swapchain hands DWM the
//!   buffer directly, so the redirection copy disappears.
//! * **Per-frame truth.** DWM's `cRefresh` tells you where the COMPOSITOR's clock
//!   is. `GetFrameStatistics.PresentRefreshCount` tells you the refresh at which
//!   YOUR frame was actually shown, which is what X11 Present's MSC gives us on
//!   Linux and what the GDI path can only approximate.
//! * **A wait that means "start now".** The waitable object signals when the
//!   swapchain will accept another frame, so rendering begins at the right moment
//!   and the frame lands on the next vblank instead of queueing behind one. The
//!   GDI path can only be told, afterwards, that a vblank happened.
//!
//! It also needs no vblank thread: the waitable handle goes straight into the
//! pump's existing MsgWaitForMultipleObjectsEx, so the capable path runs with one
//! fewer thread than the compatible one.
//!
//! WHY 8.1 AND NOT 8. Flip model and GetFrameStatistics are Windows 8, but
//! `IDXGISwapChain2::GetFrameLatencyWaitableObject` is 8.1, and that is the whole
//! point of coming here. 8.0 without 8.1 is a rounding error of an install base
//! (8.1 was a free upgrade and 8.0 support ended in 2016), so the extra floor
//! costs nothing real and buys the primitive this crate exists for.
//!
//! NO SHADERS, DELIBERATELY. The usual advice is a fullscreen quad, which means
//! shipping compiled DXBC or calling into d3dcompiler_47.dll, a redistributable
//! that may not be present. We do not need it: the CPU buffer is already at window
//! resolution in its top-left corner, so `CopySubresourceRegion` moves exactly that
//! rectangle into the backbuffer with no scaling and no pipeline state at all. The
//! quad would only be needed to SCALE a fixed-size buffer into a different-sized
//! window, which is not what this crate does.
//!
//! COM WITHOUT A BINDING CRATE. A COM object is a pointer to a pointer to a vtable
//! of `extern "system"` functions taking the object as the first argument, so it
//! transcribes directly. The one rule that bites: entries must be in exact
//! interface order INCLUDING every inherited method, and a wrong slot is not a
//! compile error, it is a call to the wrong function. Since a vtable is just an
//! array of pointers, only the methods we actually call need real signatures; the
//! rest are `Slot` placeholders that exist to occupy the right index. That turns a
//! transcription problem into a counting problem, and the counts are asserted.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use core::ffi::c_void;
use crate::sys_win::*;
use crate::sys_win::win_fn_types;

/// One unused vtable entry. Pointer-sized, so it holds the slot without needing a
/// signature we would only get wrong.
type Slot = *const c_void;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid { pub a: u32, pub b: u16, pub c: u16, pub d: [u8; 8] }

const IID_IDXGIDevice: Guid = Guid { a: 0x54ec77fa, b: 0x1377, c: 0x44e6, d: [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c] };
const IID_IDXGIFactory2: Guid = Guid { a: 0x50c83a1c, b: 0xe072, c: 0x4c48, d: [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0] };
const IID_IDXGISwapChain2: Guid = Guid { a: 0xa8be2ac4, b: 0x199f, c: 0x4946, d: [0xb3, 0x31, 0x79, 0x59, 0x9f, 0xb9, 0x8d, 0xe7] };
const IID_ID3D11Texture2D: Guid = Guid { a: 0x6f15aaf2, b: 0xd208, c: 0x4e89, d: [0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c] };

// ---- structures ------------------------------------------------------------------
#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct DXGI_SAMPLE_DESC { pub Count: u32, pub Quality: u32 }

#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct DXGI_SWAP_CHAIN_DESC1 {
    pub Width: u32, pub Height: u32, pub Format: u32, pub Stereo: BOOL,
    pub SampleDesc: DXGI_SAMPLE_DESC, pub BufferUsage: u32, pub BufferCount: u32,
    pub Scaling: u32, pub SwapEffect: u32, pub AlphaMode: u32, pub Flags: u32,
}

#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct D3D11_TEXTURE2D_DESC {
    pub Width: u32, pub Height: u32, pub MipLevels: u32, pub ArraySize: u32,
    pub Format: u32, pub SampleDesc: DXGI_SAMPLE_DESC, pub Usage: u32,
    pub BindFlags: u32, pub CPUAccessFlags: u32, pub MiscFlags: u32,
}

#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct D3D11_BOX { pub left: u32, pub top: u32, pub front: u32, pub right: u32, pub bottom: u32, pub back: u32 }

#[repr(C)] #[derive(Clone, Copy, Default)]
pub struct DXGI_FRAME_STATISTICS {
    pub PresentCount: u32, pub PresentRefreshCount: u32, pub SyncRefreshCount: u32,
    pub SyncQPCTime: i64, pub SyncGPUTime: i64,
}

// ---- constants -------------------------------------------------------------------
const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
const DXGI_USAGE_RENDER_TARGET_OUTPUT: u32 = 0x20;
const DXGI_SCALING_NONE: u32 = 1;
const DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL: u32 = 3;
const DXGI_SWAP_EFFECT_FLIP_DISCARD: u32 = 4;
const DXGI_ALPHA_MODE_IGNORE: u32 = 3;
/// 0x40. Not 0x800, which is ALLOW_TEARING: that one is also a valid flag, so
/// creation succeeds and you get a swapchain with no waitable object at all.
const DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT: u32 = 0x40;
const DXGI_MWA_NO_ALT_ENTER: u32 = 2;
const D3D_DRIVER_TYPE_HARDWARE: u32 = 1;
const D3D_DRIVER_TYPE_WARP: u32 = 5;
const D3D11_SDK_VERSION: u32 = 7;
const D3D11_USAGE_DEFAULT: u32 = 0;
/// Returned by Present when the window is entirely hidden; not an error.
const DXGI_STATUS_OCCLUDED: i32 = 0x087A0001u32 as i32;
const DXGI_ERROR_DEVICE_REMOVED: i32 = 0x887A0005u32 as i32;
const DXGI_ERROR_DEVICE_RESET: i32 = 0x887A0007u32 as i32;
const DXGI_ERROR_FRAME_STATISTICS_DISJOINT: i32 = 0x887A000Bu32 as i32;

const FEATURE_LEVELS: [u32; 6] = [0xb000, 0xa100, 0xa000, 0x9300, 0x9200, 0x9100];

// ---- method signatures, ABI-correct on both build kinds --------------------------
win_fn_types! {
    FnRelease = fn(*mut c_void) -> u32;
    FnQueryInterface = fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32;
    FnGetParent = fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32;
    FnGetAdapter = fn(*mut c_void, *mut *mut c_void) -> i32;
    FnCreateTexture2D = fn(*mut c_void, *const D3D11_TEXTURE2D_DESC, *const c_void, *mut *mut c_void) -> i32;
    FnUpdateSubresource = fn(*mut c_void, *mut c_void, u32, *const D3D11_BOX, *const c_void, u32, u32);
    FnCopySubresourceRegion = fn(*mut c_void, *mut c_void, u32, u32, u32, u32, *mut c_void, u32, *const D3D11_BOX);
    FnMakeWindowAssociation = fn(*mut c_void, HWND, u32) -> i32;
    FnCreateSwapChainForHwnd = fn(*mut c_void, *mut c_void, HWND, *const DXGI_SWAP_CHAIN_DESC1, *const c_void, *mut c_void, *mut *mut c_void) -> i32;
    FnPresent = fn(*mut c_void, u32, u32) -> i32;
    FnGetBuffer = fn(*mut c_void, u32, *const Guid, *mut *mut c_void) -> i32;
    FnResizeBuffers = fn(*mut c_void, u32, u32, u32, u32, u32) -> i32;
    FnGetFrameStatistics = fn(*mut c_void, *mut DXGI_FRAME_STATISTICS) -> i32;
    FnSetMaximumFrameLatency = fn(*mut c_void, u32) -> i32;
    FnGetFrameLatencyWaitableObject = fn(*mut c_void) -> HANDLE;
    FnD3D11CreateDevice = fn(*mut c_void, u32, HMODULE, u32, *const u32, u32, u32, *mut *mut c_void, *mut u32, *mut *mut c_void) -> i32;
}

// ---- vtables ---------------------------------------------------------------------
// Every one of these is "IUnknown, then each base interface in order, then this
// interface". The comments carry the slot index so a future edit can be checked
// against the SDK header without re-deriving the count.

#[repr(C)]
struct IUnknownVtbl { query_interface: FnQueryInterface, add_ref: Slot, release: FnRelease }

/// IUnknown(0..2), IDXGIObject(3..6), IDXGIDevice(7).
#[repr(C)]
struct IDXGIDeviceVtbl {
    base: IUnknownVtbl,
    _obj: [Slot; 4],                    // SetPrivateData, SetPrivateDataInterface, GetPrivateData, GetParent
    get_adapter: FnGetAdapter,          // 7
}

/// IUnknown(0..2), IDXGIObject(3..6). GetParent is the last of those.
#[repr(C)]
struct IDXGIObjectVtbl {
    base: IUnknownVtbl,
    _priv: [Slot; 3],                   // SetPrivateData, SetPrivateDataInterface, GetPrivateData
    get_parent: FnGetParent,            // 6
}

/// IUnknown(0..2), IDXGIObject(3..6), IDXGIFactory(7..11), IDXGIFactory1(12..13), IDXGIFactory2(14..).
#[repr(C)]
struct IDXGIFactory2Vtbl {
    base: IUnknownVtbl,
    _obj: [Slot; 4],
    _enum_adapters: Slot,                            // 7
    make_window_association: FnMakeWindowAssociation, // 8
    _rest_factory: [Slot; 3],                        // 9 GetWindowAssociation, 10 CreateSwapChain, 11 CreateSoftwareAdapter
    _factory1: [Slot; 2],                            // 12 EnumAdapters1, 13 IsCurrent
    _stereo: Slot,                                   // 14 IsWindowedStereoEnabled
    create_swap_chain_for_hwnd: FnCreateSwapChainForHwnd, // 15
}

/// IUnknown(0..2), ID3D11Device(3..). CreateTexture2D is the third creator.
#[repr(C)]
struct ID3D11DeviceVtbl {
    base: IUnknownVtbl,
    _create_buffer: Slot,               // 3
    _create_texture1d: Slot,            // 4
    create_texture2d: FnCreateTexture2D, // 5
}

/// IUnknown(0..2), ID3D11DeviceChild(3..6), ID3D11DeviceContext(7..).
/// CopySubresourceRegion is 46 and UpdateSubresource is 48, so slots 7..45 are
/// thirty-nine placeholders. That count is the whole risk in this file; it is
/// asserted below against the offset of the first typed member.
#[repr(C)]
struct ID3D11DeviceContextVtbl {
    base: IUnknownVtbl,
    _child: [Slot; 4],                  // 3..6
    _unused: [Slot; 39],                // 7..45
    copy_subresource_region: FnCopySubresourceRegion, // 46
    _copy_resource: Slot,               // 47
    update_subresource: FnUpdateSubresource, // 48
}

/// IUnknown(0..2), IDXGIObject(3..6), IDXGIDeviceSubObject(7), IDXGISwapChain(8..17),
/// IDXGISwapChain1(18..28), IDXGISwapChain2(29..).
#[repr(C)]
struct IDXGISwapChain2Vtbl {
    base: IUnknownVtbl,
    _obj: [Slot; 4],                    // 3..6
    _get_device: Slot,                  // 7
    present: FnPresent,                 // 8
    get_buffer: FnGetBuffer,            // 9
    _fullscreen: [Slot; 2],             // 10 SetFullscreenState, 11 GetFullscreenState
    _get_desc: Slot,                    // 12
    resize_buffers: FnResizeBuffers,    // 13
    _resize_target: Slot,               // 14
    _containing_output: Slot,           // 15
    get_frame_statistics: FnGetFrameStatistics, // 16
    _last_present_count: Slot,          // 17
    _sc1: [Slot; 11],                   // 18..28
    _source_size: [Slot; 2],            // 29 SetSourceSize, 30 GetSourceSize
    set_maximum_frame_latency: FnSetMaximumFrameLatency, // 31
    _get_max_latency: Slot,             // 32
    get_frame_latency_waitable_object: FnGetFrameLatencyWaitableObject, // 33
}

// The counting, checked. A vtable entry is one pointer, so the byte offset of a
// typed member divided by the pointer size is its slot index.
const PTR: usize = core::mem::size_of::<usize>();
const _: () = assert!(core::mem::offset_of!(ID3D11DeviceContextVtbl, copy_subresource_region) == 46 * PTR);
const _: () = assert!(core::mem::offset_of!(ID3D11DeviceContextVtbl, update_subresource) == 48 * PTR);
const _: () = assert!(core::mem::offset_of!(IDXGISwapChain2Vtbl, present) == 8 * PTR);
const _: () = assert!(core::mem::offset_of!(IDXGISwapChain2Vtbl, resize_buffers) == 13 * PTR);
const _: () = assert!(core::mem::offset_of!(IDXGISwapChain2Vtbl, get_frame_statistics) == 16 * PTR);
const _: () = assert!(core::mem::offset_of!(IDXGISwapChain2Vtbl, set_maximum_frame_latency) == 31 * PTR);
const _: () = assert!(core::mem::offset_of!(IDXGISwapChain2Vtbl, get_frame_latency_waitable_object) == 33 * PTR);
const _: () = assert!(core::mem::offset_of!(IDXGIFactory2Vtbl, create_swap_chain_for_hwnd) == 15 * PTR);
const _: () = assert!(core::mem::offset_of!(IDXGIFactory2Vtbl, make_window_association) == 8 * PTR);
const _: () = assert!(core::mem::offset_of!(ID3D11DeviceVtbl, create_texture2d) == 5 * PTR);
const _: () = assert!(core::mem::offset_of!(IDXGIDeviceVtbl, get_adapter) == 7 * PTR);
const _: () = assert!(core::mem::offset_of!(IDXGIObjectVtbl, get_parent) == 6 * PTR);

#[inline]
unsafe fn vt<T>(p: *mut c_void) -> *const T { unsafe { *(p as *const *const T) } }

unsafe fn release(p: *mut c_void) {
    if !p.is_null() { unsafe { ((*vt::<IUnknownVtbl>(p)).release)(p) }; }
}

// ---- the presenter ----------------------------------------------------------------
pub struct D3d {
    device: *mut c_void,
    ctx: *mut c_void,
    swap: *mut c_void,
    tex: *mut c_void,
    /// Side of the texture, which matches the CPU buffer's square side.
    tex_side: u32,
    /// The pump waits on this instead of running a vblank thread.
    pub waitable: HANDLE,
    /// Last PresentRefreshCount, the analog of X11 Present's MSC, and the
    /// PresentCount that went with it. Both are needed: see frames_elapsed.
    last_refresh: u32,
    last_present: u32,
    have_refresh: bool,
    /// Refreshes that passed without one of our frames in them, not yet charged to
    /// display time. Paid off a few at a time so one hiccup cannot jump the clock.
    owed: u64,
    /// Presents we have handed to DXGI, against PresentCount for how many have
    /// actually been shown. Their difference is Core::in_flight, which on X11 comes
    /// from PresentCompleteNotify and had no meaning on the GDI path at all.
    submitted: u32,
    /// Presents submitted but not yet shown; published to Core::in_flight.
    pub in_flight: u32,
    /// QPC at the moment each present was handed to DXGI, indexed by present
    /// number. Only needed to answer "how long was that frame in the pipe".
    submit_qpc: [i64; 256],
    /// Submit-to-scanout samples, microseconds.
    lat_n: u32, lat_sum: i64, lat_min: i64, lat_max: i64,
    last_stat_present: u32,
    pub occluded: bool,
    debug: bool,
    dbg_n: u32,
}

unsafe impl Send for D3d {}

impl D3d {
    /// Try to stand up the capable path. Returns None on anything at all, because
    /// every failure here has the same answer: use the GDI path instead.
    pub fn new(hwnd: HWND, debug: bool) -> Option<D3d> {
        unsafe {
            let d3d11 = LoadLibraryW(wide("d3d11.dll").as_ptr());
            if d3d11.is_null() { return None; }
            let create: Option<FnD3D11CreateDevice> =
                core::mem::transmute(GetProcAddress(d3d11, b"D3D11CreateDevice\0".as_ptr()));
            let create = create?;

            // Hardware first, then WARP: a software rasteriser still gets us the
            // flip model and its timing, which is the reason we are here at all.
            let mut device: *mut c_void = core::ptr::null_mut();
            let mut ctx: *mut c_void = core::ptr::null_mut();
            let mut got_level: u32 = 0;
            let mut hr = -1;
            // Which driver to ask for, and this is not a preference: inside a
            // cosmopolitan APE the hardware driver kills the process.
            //
            // Measured on Windows 11 with an NVIDIA GPU: from an APE,
            // D3D11CreateDevice with D3D_DRIVER_TYPE_HARDWARE raises
            // STATUS_BREAKPOINT and never returns, with the library loaded, the
            // entry point resolved and the ABI correct. The same call with
            // D3D_DRIVER_TYPE_WARP succeeds and runs the whole flip-model path.
            // So it is the vendor user-mode driver's initialisation that objects
            // to something about a cosmo process, not D3D11 and not cosmo's
            // loader. Native builds are unaffected and use hardware as usual.
            //
            // The ordinary hardware-then-WARP fallback cannot save us here,
            // because the failure is a crash rather than an error return and
            // there is nothing to fall back FROM. An APE therefore never asks
            // for the hardware driver at all. SOFTER_GUI_D3D_DRIVER=hardware
            // overrides that for anyone wanting to retest it on other hardware.
            let want_drv = std::env::var("SOFTER_GUI_D3D_DRIVER").unwrap_or_default();
            let drivers: &[u32] = match want_drv.as_str() {
                "warp" => &[D3D_DRIVER_TYPE_WARP],
                "hardware" => &[D3D_DRIVER_TYPE_HARDWARE],
                _ if cfg!(cosmo) => &[D3D_DRIVER_TYPE_WARP],
                _ => &[D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP],
            };
            for &driver in drivers {
                hr = create(core::ptr::null_mut(), driver, NULL, 0,
                            FEATURE_LEVELS.as_ptr(), FEATURE_LEVELS.len() as u32,
                            D3D11_SDK_VERSION, &mut device, &mut got_level, &mut ctx);
                if hr >= 0 { if debug { eprintln!("softer_gui: d3d11 driver {driver}, feature level {got_level:#x}"); } break; }
            }
            if hr < 0 || device.is_null() || ctx.is_null() { return None; }

            // device -> IDXGIDevice -> adapter -> factory. Going through the device's
            // own factory matters: a swapchain from a different factory is undefined.
            let mut dxgi_dev: *mut c_void = core::ptr::null_mut();
            if ((*vt::<IUnknownVtbl>(device)).query_interface)(device, &IID_IDXGIDevice, &mut dxgi_dev) < 0 {
                release(ctx); release(device); return None;
            }
            let mut adapter: *mut c_void = core::ptr::null_mut();
            let ok = ((*vt::<IDXGIDeviceVtbl>(dxgi_dev)).get_adapter)(dxgi_dev, &mut adapter) >= 0;
            release(dxgi_dev);
            if !ok { release(ctx); release(device); return None; }
            let mut factory: *mut c_void = core::ptr::null_mut();
            let ok = ((*vt::<IDXGIObjectVtbl>(adapter)).get_parent)(adapter, &IID_IDXGIFactory2, &mut factory) >= 0;
            release(adapter);
            if !ok { release(ctx); release(device); return None; }

            let mut r = RECT::default();
            GetClientRect(hwnd, &mut r);
            let (w, h) = ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32);

            // FLIP_DISCARD is Windows 10; FLIP_SEQUENTIAL is the 8.1 spelling and is
            // what makes this path work at its stated floor. Try the better one first.
            let mut swap1: *mut c_void = core::ptr::null_mut();
            let mut made = false;
            for effect in [DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL] {
                let desc = DXGI_SWAP_CHAIN_DESC1 {
                    Width: w, Height: h,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,   // matches our 0xAARRGGBB memory order
                    Stereo: 0,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,                        // flip model requires at least two
                    Scaling: DXGI_SCALING_NONE,
                    SwapEffect: effect,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                    Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
                };
                if ((*vt::<IDXGIFactory2Vtbl>(factory)).create_swap_chain_for_hwnd)(
                        factory, device, hwnd, &desc, core::ptr::null(), core::ptr::null_mut(), &mut swap1) >= 0 {
                    made = true;
                    if debug { eprintln!("softer_gui: swap effect {effect}"); }
                    break;
                }
            }
            if !made {
                ((*vt::<IDXGIFactory2Vtbl>(factory)).make_window_association)(factory, hwnd, DXGI_MWA_NO_ALT_ENTER);
                release(factory); release(ctx); release(device);
                return None;
            }
            // Stop DXGI turning Alt+Enter into its own fullscreen transition; this
            // crate does borderless fullscreen itself.
            ((*vt::<IDXGIFactory2Vtbl>(factory)).make_window_association)(factory, hwnd, DXGI_MWA_NO_ALT_ENTER);
            release(factory);

            // IDXGISwapChain2 is the 8.1 interface and the only source of the
            // waitable object; without it there is no reason to prefer this path.
            let mut swap: *mut c_void = core::ptr::null_mut();
            let hr_qi = ((*vt::<IUnknownVtbl>(swap1)).query_interface)(swap1, &IID_IDXGISwapChain2, &mut swap);
            release(swap1);
            if hr_qi < 0 {
                if debug { eprintln!("softer_gui: IDXGISwapChain2 QI failed hr={hr_qi:#x} (pre-8.1?)"); }
                release(ctx); release(device); return None;
            }

            let scv = vt::<IDXGISwapChain2Vtbl>(swap);
            // One frame of latency: render, present, and be woken for the next.
            ((*scv).set_maximum_frame_latency)(swap, 1);
            let waitable = ((*scv).get_frame_latency_waitable_object)(swap);
            if waitable.is_null() {
                if debug { eprintln!("softer_gui: no frame-latency waitable object"); }
                release(swap); release(ctx); release(device); return None;
            }

            Some(D3d { device, ctx, swap, tex: core::ptr::null_mut(), tex_side: 0,
                       waitable, last_refresh: 0, last_present: 0, have_refresh: false, owed: 0, submitted: 0, in_flight: 0,
                       submit_qpc: [0; 256], lat_n: 0, lat_sum: 0, lat_min: i64::MAX, lat_max: 0,
                       last_stat_present: 0,
                       occluded: false, debug, dbg_n: 0 })
        }
    }

    /// The upload texture mirrors the CPU buffer, so it is reallocated on the same
    /// power-of-two boundaries and never per frame.
    unsafe fn ensure_texture(&mut self, side: u32) -> bool {
        if !self.tex.is_null() && self.tex_side == side { return true; }
        unsafe {
            release(self.tex);
            self.tex = core::ptr::null_mut();
            let desc = D3D11_TEXTURE2D_DESC {
                Width: side, Height: side, MipLevels: 1, ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                // DEFAULT plus UpdateSubresource, not DYNAMIC plus Map(WRITE_DISCARD):
                // discard hands back a fresh buffer with undefined contents, which
                // forces a full-frame copy every frame and throws away exactly the
                // incremental property the damage box exists for.
                Usage: D3D11_USAGE_DEFAULT, BindFlags: 0, CPUAccessFlags: 0, MiscFlags: 0,
            };
            let mut tex: *mut c_void = core::ptr::null_mut();
            if ((*vt::<ID3D11DeviceVtbl>(self.device)).create_texture2d)(self.device, &desc, core::ptr::null(), &mut tex) < 0 {
                return false;
            }
            self.tex = tex;
            self.tex_side = side;
            true
        }
    }

    /// Upload the window-sized rectangle and flip it. `pixels` is the top-left
    /// corner of a `side` by `side` buffer; only `w` by `h` of it is on screen.
    pub fn present(&mut self, pixels: *const u32, side: u32, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 { return true; }
        unsafe {
            if !self.ensure_texture(side) { return false; }
            let ctxv = vt::<ID3D11DeviceContextVtbl>(self.ctx);

            // Only the visible rectangle is uploaded, not the whole square. The
            // pointer addresses the first pixel OF THE BOX while the pitch stays the
            // FULL buffer pitch; getting that pair backwards is the classic diagonal
            // smear, instantly recognisable once seen.
            let box_ = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
            ((*ctxv).update_subresource)(self.ctx, self.tex, 0, &box_, pixels as *const c_void, side * 4, 0);

            let scv = vt::<IDXGISwapChain2Vtbl>(self.swap);
            let mut back: *mut c_void = core::ptr::null_mut();
            if ((*scv).get_buffer)(self.swap, 0, &IID_ID3D11Texture2D, &mut back) < 0 { return false; }
            // No shader, no quad: the source rectangle already matches the
            // destination, so this is a straight blit on the GPU.
            ((*ctxv).copy_subresource_region)(self.ctx, back, 0, 0, 0, 0, self.tex, 0, &box_);
            release(back);

            // Stamped before the call: this is the moment the frame stops being
            // ours and starts waiting on the display.
            self.submit_qpc[(self.submitted.wrapping_add(1) & 255) as usize] = qpc();
            let hr = ((*scv).present)(self.swap, 1, 0);
            self.submitted = self.submitted.wrapping_add(1);
            self.occluded = hr == DXGI_STATUS_OCCLUDED;
            if hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET { return false; }
            true
        }
    }

    /// Whole refresh periods that the app should advance by for this frame.
    ///
    /// The obvious implementation, "delta of PresentRefreshCount since last call",
    /// is wrong here and it took a measurement to see why: DXGI does not update
    /// these counters every frame. Measured on Windows 11, PresentRefreshCount sits
    /// still for about sixty calls and then jumps by sixty at once. Treating that
    /// delta as elapsed frames ticks the clock 1 while it is stalled and then 60 in
    /// one go, which is how display time ran 931 ms AHEAD of the wall clock over
    /// eight seconds while every individual frame looked fine.
    ///
    /// What the counters actually give, and what makes this the closest match to
    /// X11 Present of anything on Windows, is a PAIR: PresentRefreshCount is the
    /// MSC, and PresentCount says how many of our frames went into those refreshes.
    /// One present should occupy one refresh, so any excess of refreshes over
    /// presents is exactly the vblanks we missed, whenever the counters get around
    /// to telling us. So: charge one period for the frame in hand, and add the
    /// shortfall when it shows up. Steady state is 1 per frame regardless of when
    /// DXGI feels like updating, and a real drop is still counted, just late.
    pub fn frames_elapsed(&mut self, period_qpc: i64) -> u64 {
        unsafe {
            let mut st = DXGI_FRAME_STATISTICS::default();
            let hr = ((*vt::<IDXGISwapChain2Vtbl>(self.swap)).get_frame_statistics)(self.swap, &mut st);
            if hr == DXGI_ERROR_FRAME_STATISTICS_DISJOINT {
                // A mode change or fullscreen transition discontinued the counters.
                // Re-baseline rather than letting display time absorb the jump.
                self.have_refresh = false;
                self.owed = 0;
                return 1;
            }
            if hr >= 0 && st.PresentRefreshCount != 0 {
                if self.have_refresh {
                    let d_ref = st.PresentRefreshCount.wrapping_sub(self.last_refresh) as u64;
                    let d_pres = st.PresentCount.wrapping_sub(self.last_present) as u64;
                    // Refreshes that went by with no frame of ours in them.
                    if d_ref > d_pres { self.owed += d_ref - d_pres; }
                }
                self.last_refresh = st.PresentRefreshCount;
                self.last_present = st.PresentCount;
                self.have_refresh = true;
            }
            // Submit-to-scanout, for the one present DXGI just told us about.
            //
            // Pacing does not imply latency: the gap histogram says no frame was
            // dropped and the clock is right, and says nothing about how many
            // vblanks a frame sat in the pipeline first. These statistics can
            // answer that from inside the process. PresentRefreshCount is the
            // vblank an image went up at, and SyncRefreshCount with SyncQPCTime is
            // a vblank/QPC pair the scheduler sampled, so walking back from the
            // pair by the refresh period turns the first into an instant to
            // difference against when Present was called. Same correlation
            // PresentMon makes.
            if period_qpc > 0 && st.PresentCount != self.last_stat_present && st.PresentCount != 0 {
                self.last_stat_present = st.PresentCount;
                let behind = st.SyncRefreshCount as i64 - st.PresentRefreshCount as i64;
                if (0..600).contains(&behind) {
                    let shown_qpc = st.SyncQPCTime - behind * period_qpc;
                    // PresentCount and our own counter are the same numbering:
                    // measured, their difference holds at the frames in flight.
                    let sent = self.submit_qpc[(st.PresentCount & 255) as usize];
                    if sent != 0 && shown_qpc > sent {
                        let us = (shown_qpc - sent) * 1_000_000 / qpf();
                        // A whole second of "latency" is a stale ring slot, not a frame.
                        if us < 1_000_000 {
                            self.lat_n += 1;
                            self.lat_sum += us;
                            self.lat_min = self.lat_min.min(us);
                            self.lat_max = self.lat_max.max(us);
                            if self.debug && self.lat_n % 8 == 0 {
                                eprintln!("softer_gui: latency submit->scanout n={} mean={}us min={}us max={}us",
                                          self.lat_n, self.lat_sum / self.lat_n as i64, self.lat_min, self.lat_max);
                            }
                        }
                    }
                }
            }

            // One period for this frame, plus a little of what is owed.
            let take = self.owed.min(7);
            self.owed -= take;
            self.in_flight = self.submitted.wrapping_sub(st.PresentCount);
            1 + take
        }
    }

    /// Backbuffers must match the client area or DXGI stretches; all references to
    /// them have to be gone first, which they are because `present` releases the one
    /// it takes every frame.
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 { return; }
        unsafe {
            let hr = ((*vt::<IDXGISwapChain2Vtbl>(self.swap)).resize_buffers)(
                self.swap, 0, w, h, 0, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT);
            let _ = hr;
            self.have_refresh = false;
        }
    }
}

impl Drop for D3d {
    fn drop(&mut self) {
        // Reverse creation order, and the waitable handle belongs to the swapchain.
        unsafe {
            release(self.tex);
            release(self.swap);
            release(self.ctx);
            release(self.device);
        }
    }
}
