//! Self-driving Windows input test: opens a window, drives it, and asserts on the
//! event stream. The Windows analog of examples/xtest.rs, except that it drives
//! ITSELF rather than whatever happens to be focused, which is what makes it safe
//! to run unattended.
//!
//! Most of it drives the window procedure directly with PostMessage: pointer,
//! wheel, and every scancode in the table. That is faithful, because the window
//! procedure is the thing under test, and it has no global side effects at all, so
//! it runs anywhere, including on a machine whose desktop belongs to somebody else
//! at the time. The user's cursor never moves and no keystroke escapes.
//!
//! Two things PostMessage cannot fake, because ToUnicodeEx reads the kernel's real
//! keyboard state: whether a modifier reaches the layout, and therefore whether
//! shift actually changes the character. Those use real SendInput, which does put
//! synthetic keystrokes into the session, so they sit behind an interlock: nothing
//! is injected unless GetForegroundWindow() is OUR window, and the test reports
//! SKIP rather than typing into someone else's application.
//!
//! This is what verifies the scancode table end to end, which the research notes
//! flagged as the one piece that should not be trusted without a real keyboard,
//! along with resize/regrow and the modal resize loop.

#![allow(non_snake_case, non_camel_case_types)]

#[cfg(not(target_os = "windows"))]
fn main() { println!("wintest: Windows only"); }

#[cfg(target_os = "windows")]
use softer_gui::*;

#[cfg(target_os = "windows")]
mod inject {
    use core::ffi::c_void;
    pub type HWND = *mut c_void;

    #[repr(C)] #[derive(Clone, Copy)]
    pub struct MOUSEINPUT { pub dx: i32, pub dy: i32, pub mouse_data: u32, pub flags: u32, pub time: u32, pub extra: usize }
    #[repr(C)] #[derive(Clone, Copy)]
    pub struct KEYBDINPUT { pub vk: u16, pub scan: u16, pub flags: u32, pub time: u32, pub extra: usize }
    #[repr(C)] #[derive(Clone, Copy)]
    pub union INPUTU { pub m: MOUSEINPUT, pub k: KEYBDINPUT }
    #[repr(C)] #[derive(Clone, Copy)]
    pub struct INPUT { pub ty: u32, pub u: INPUTU }

    pub const INPUT_KEYBOARD: u32 = 1;
    pub const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
    pub const KEYEVENTF_KEYUP: u32 = 0x0002;
    pub const KEYEVENTF_SCANCODE: u32 = 0x0008;

    #[link(name = "user32")]
    unsafe extern "system" {
        pub fn SendInput(n: u32, inputs: *const INPUT, size: i32) -> u32;
        pub fn FindWindowW(class: *const u16, title: *const u16) -> HWND;
        pub fn SetForegroundWindow(h: HWND) -> i32;
        pub fn GetForegroundWindow() -> HWND;
        pub fn GetWindowThreadProcessId(h: HWND, pid: *mut u32) -> u32;
        pub fn PostMessageW(h: HWND, msg: u32, w: usize, l: isize) -> i32;
        pub fn AttachThreadInput(from: u32, to: u32, attach: i32) -> i32;
        pub fn BringWindowToTop(h: HWND) -> i32;
        pub fn SetFocus(h: HWND) -> HWND;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetCurrentProcessId() -> u32;
    }

    /// Windows refuses SetForegroundWindow from a process that is not already in
    /// front. Borrowing the foreground thread's input queue for the length of the
    /// call is the documented way around it, and it is what every test harness and
    /// installer does. Returns whether we actually ended up in front.
    pub fn force_foreground(hwnd: HWND) -> bool {
        unsafe {
            let fg = GetForegroundWindow();
            let fg_tid = GetWindowThreadProcessId(fg, core::ptr::null_mut());
            // The window belongs to the pump thread, not to us.
            let our_tid = GetWindowThreadProcessId(hwnd, core::ptr::null_mut());
            let attached = fg_tid != 0 && fg_tid != our_tid && AttachThreadInput(our_tid, fg_tid, 1) != 0;
            SetForegroundWindow(hwnd);
            BringWindowToTop(hwnd);
            SetFocus(hwnd);
            if attached { AttachThreadInput(our_tid, fg_tid, 0); }
            GetForegroundWindow() == hwnd
        }
    }

    /// A WM_KEYDOWN/WM_KEYUP posted straight to our own window procedure, with the
    /// lParam Windows would have built. This exercises the scancode table and the
    /// event ordering with no global side effects, so it runs even when the window
    /// cannot take the foreground. It cannot test modifier-dependent text, because
    /// ToUnicodeEx reads the real keyboard state and synthetic messages do not set it.
    pub fn post_key(hwnd: HWND, scan: u32, ext: bool, down: bool) {
        const MAPVK_VSC_TO_VK_EX: u32 = 3;
        let vk = unsafe { softer_gui::sys_win::MapVirtualKeyW(scan | if ext { 0xE000 } else { 0 }, MAPVK_VSC_TO_VK_EX) };
        let mut l: isize = 1 | ((scan as isize & 0xFF) << 16);
        if ext { l |= 1 << 24; }
        let msg = if down { 0x0100 } else { l |= (1 << 30) | (1 << 31); 0x0101 };
        unsafe { PostMessageW(hwnd, msg, vk as usize, l) };
    }
    pub fn post_tap(hwnd: HWND, scan: u32, ext: bool) { post_key(hwnd, scan, ext, true); post_key(hwnd, scan, ext, false); }

    /// One scancode press or release. `ext` sets the E0 flag, which is the half of
    /// the mapping that is not identity with evdev.
    pub fn key(scan: u16, ext: bool, down: bool) {
        let mut flags = KEYEVENTF_SCANCODE;
        if ext { flags |= KEYEVENTF_EXTENDEDKEY; }
        if !down { flags |= KEYEVENTF_KEYUP; }
        let i = INPUT { ty: INPUT_KEYBOARD, u: INPUTU { k: KEYBDINPUT { vk: 0, scan, flags, time: 0, extra: 0 } } };
        unsafe { SendInput(1, &i, core::mem::size_of::<INPUT>() as i32) };
    }
    pub fn tap(scan: u16, ext: bool) { key(scan, ext, true); key(scan, ext, false); }
}

#[cfg(target_os = "windows")]
fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(core::iter::once(0)).collect() }

/// Fill only the window-sized sub-rect and submit. Painting the whole square
/// buffer would be four times the work at 2048 and, unoptimised, slower than the
/// display, which starves the event drain rather than testing anything.
/// Returns how long submit() took, in ms.
#[cfg(target_os = "windows")]
fn paint(gui: &mut Gui) -> u128 {
    let mut fb = gui.get_framebuffer();
    if !fb.ok() { return 0; }
    let (w, h, side) = (fb.width, fb.height, fb.side);
    let px = fb.slice();
    for y in 0..h {
        let row = y * side;
        for x in 0..w { px[row + x] = 0xFF202020; }
    }
    let b = std::time::Instant::now();
    gui.submit();
    b.elapsed().as_millis()
}

/// Pump the gui for `ms`, appending everything that arrives (except RENDER, which
/// would drown the log) and submitting frames so the pacing chain keeps running.
#[cfg(target_os = "windows")]
fn drain(gui: &mut Gui, ms: u64, out: &mut Vec<Event>) {
    let t0 = std::time::Instant::now();
    let mut ev = Event::default();
    while t0.elapsed().as_millis() < ms as u128 {
        gui.wait_ms(5);
        let mut n = 0;
        while gui.next_event(&mut ev) {
            if ev.kind == EVENT_RENDER {
                paint(gui);
            } else {
                out.push(ev);
            }
            // Bound the burst. A consumer slower than the pump would otherwise stay
            // in this loop forever, which is how a slow debug build looks like a hang.
            n += 1;
            if n >= 32 { break; }
        }
    }
}

/// Like drain(), but records the display timestamp of every RENDER instead of
/// discarding it. Used to watch the frame clock during the modal resize loop.
#[cfg(target_os = "windows")]
fn drain_renders(gui: &mut Gui, ms: u64, ts: &mut Vec<u128>) -> (u128, u128) {
    let t0 = std::time::Instant::now();
    let mut ev = Event::default();
    let (mut slow_get, mut slow_submit) = (0u128, 0u128);
    while t0.elapsed().as_millis() < ms as u128 {
        gui.wait_ms(5);
        let mut n = 0;
        while gui.next_event(&mut ev) {
            n += 1;
            if n > 32 { break; }
            if ev.kind == EVENT_RENDER {
                ts.push(ev.t_fs);
                let a = std::time::Instant::now();
                let painted = paint(gui);
                slow_get = slow_get.max(a.elapsed().as_millis());
                slow_submit = slow_submit.max(painted);
            }
        }
    }
    (slow_get, slow_submit)
}

#[cfg(target_os = "windows")]
fn main() {
    use inject::*;

    let Some(mut gui) = softer_gui::open("softer_gui wintest", "lol.softer.wintest", 640, 480) else {
        eprintln!("wintest: could not open a window");
        std::process::exit(1);
    };
    let mut evs: Vec<Event> = Vec::new();
    drain(&mut gui, 300, &mut evs);

    // Find our own window, and be sure it is ours before touching it.
    let hwnd = unsafe { FindWindowW(wide("softer_gui_window").as_ptr(), core::ptr::null()) };
    if hwnd.is_null() { eprintln!("wintest: FAIL could not find the window"); std::process::exit(1); }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != unsafe { GetCurrentProcessId() } {
        eprintln!("wintest: FAIL found a softer_gui window belonging to another process");
        std::process::exit(1);
    }

    let fails = std::cell::Cell::new(0u32);
    let check = |name: &str, ok: bool| {
        use std::io::Write;
        println!("{} {}", if ok { "ok  " } else { "FAIL" }, name);
        let _ = std::io::stdout().flush();
        if !ok { fails.set(fails.get() + 1); }
    };

    // ---- pointer and wheel, through our own window procedure --------------------
    evs.clear();
    unsafe {
        let pos = (120isize) | (90isize << 16);
        PostMessageW(hwnd, 0x0200, 0, pos);                     // WM_MOUSEMOVE
        PostMessageW(hwnd, 0x0201, 1, pos);                     // WM_LBUTTONDOWN
        PostMessageW(hwnd, 0x0202, 0, pos);                     // WM_LBUTTONUP
        PostMessageW(hwnd, 0x020A, 120usize << 16, pos);        // WM_MOUSEWHEEL, one click forward
        PostMessageW(hwnd, 0x020E, 120usize << 16, pos);        // WM_MOUSEHWHEEL, one click right
    }
    drain(&mut gui, 300, &mut evs);

    // Axes carry 24.8 fixed pixels, so 120 px arrives as 120 << 8.
    let mouse_xy = evs.iter().any(|e| e.kind == EVENT_AXES
        && e.axes().iter().any(|a| a.axis == AXIS_MOUSE_X && a.delta == 120 << 8)
        && e.axes().iter().any(|a| a.axis == AXIS_MOUSE_Y && a.delta == 90 << 8));
    check("pointer position reaches AXIS_MOUSE_X/Y as 24.8 fixed pixels", mouse_xy);

    let btn_down = evs.iter().any(|e| e.kind == EVENT_BUTTONS && e.button(BTN_LEFT));
    let btn_up_after = evs.iter().rev().find(|e| e.kind == EVENT_BUTTONS).map(|e| !e.button(BTN_LEFT)).unwrap_or(false);
    check("BTN_LEFT press appears in the button snapshot", btn_down);
    check("BTN_LEFT release clears it again", btn_up_after);

    // Ours is positive when the content moves up; Windows is positive when the
    // wheel goes forward, so one forward click must arrive as -SCROLL_STEP.
    let wheel_v = evs.iter().any(|e| e.kind == EVENT_AXES && e.axes().iter().any(|a| a.axis == AXIS_SCROLL_V && a.delta == -SCROLL_STEP));
    check("wheel forward is one -SCROLL_STEP on AXIS_SCROLL_V", wheel_v);
    let wheel_h = evs.iter().any(|e| e.kind == EVENT_AXES && e.axes().iter().any(|a| a.axis == AXIS_SCROLL_H && a.delta == SCROLL_STEP));
    check("wheel tilt right is one +SCROLL_STEP on AXIS_SCROLL_H", wheel_h);

    // ---- the scancode table, through our own window procedure -------------------
    // These need no focus, so they run everywhere, including on a machine whose
    // desktop belongs to somebody else at the time.
    let mut table_case = |scan: u32, ext: bool, want: u32, label: &str| {
        let mut evs = Vec::new();
        post_tap(hwnd, scan, ext);
        drain(&mut gui, 200, &mut evs);
        let got = evs.iter().any(|e| e.kind == EVENT_BUTTONS && e.button(want));
        println!("{} {label}", if got { "ok  " } else { "FAIL" });
        (got, evs)
    };

    let (ok, evs_a) = table_case(0x1E, false, 30, "set-1 0x1E is evdev 30 (identity, as the table claims)");
    if !ok { fails.set(fails.get() + 1); }
    // ToUnicodeEx still resolves the unmodified character from an all-zero state,
    // so the text path is exercised here even without real key state.
    let a_text = evs_a.iter().any(|e| e.kind == EVENT_TEXT && e.text().iter().any(|c| c.is_alphabetic()));
    check("ToUnicodeEx resolved layout text for it", a_text);
    // The order key_press_sym documents and relies on: text, then the snapshot.
    let i_text = evs_a.iter().position(|e| e.kind == EVENT_TEXT);
    let i_btn = evs_a.iter().position(|e| e.kind == EVENT_BUTTONS && e.button(30));
    check("TEXT is ordered before the BUTTONS snapshot", matches!((i_text, i_btn), (Some(t), Some(b)) if t < b));

    for (scan, ext, want, label) in [
        (0x39u32, false, 57u32, "set-1 0x39 is KEY_SPACE (57)"),
        (0x01, false, KEY_ESC, "set-1 0x01 is KEY_ESC (1)"),
        (0x1C, false, KEY_ENTER, "set-1 0x1C is KEY_ENTER (28)"),
        (0x2A, false, KEY_LEFTSHIFT, "set-1 0x2A is KEY_LEFTSHIFT (42)"),
        (0x57, false, KEY_F11, "set-1 0x57 is KEY_F11 (87), the top of the identity range"),
        // The extended block is where evdev and set-1 genuinely diverge.
        (0x4B, true, KEY_LEFT, "E0 4B is KEY_LEFT (105)"),
        (0x48, true, KEY_UP, "E0 48 is KEY_UP (103)"),
        (0x1D, true, KEY_RIGHTCTRL, "E0 1D is KEY_RIGHTCTRL (97), not the left one"),
        (0x53, true, KEY_DELETE, "E0 53 is KEY_DELETE (111), not the keypad dot"),
        (0x5B, true, KEY_LEFTMETA, "E0 5B is KEY_LEFTMETA (125)"),
    ] {
        let (ok, _) = table_case(scan, ext, want, label);
        if !ok { fails.set(fails.get() + 1); }
    }

    // ---- real injected input, behind the focus interlock ------------------------
    let focused = force_foreground(hwnd);
    drain(&mut gui, 150, &mut evs);
    if !focused {
        println!("SKIP keyboard: could not take the foreground, and injecting keystrokes");
        println!("SKIP keyboard: without it would type into somebody else's window.");
    } else {
        // Shift changing the character is the one thing only real input can prove:
        // it means modifier state actually reached ToUnicodeEx.
        //
        // Retried, because injected input races window activation: a window can be
        // foreground before it is ready to receive, and the first burst then lands
        // nowhere. Retrying tests the same property without the flake.
        let mut shifted = false;
        for _ in 0..3 {
            evs.clear();
            key(0x2A, false, true);                 // left shift down
            tap(0x1E, false);
            key(0x2A, false, false);                // left shift up
            drain(&mut gui, 300, &mut evs);
            shifted = evs.iter().filter(|e| e.kind == EVENT_TEXT)
                .flat_map(|e| e.text().to_vec()).any(|c| c.is_uppercase());
            if shifted { break; }
        }
        // Another application can take the foreground between the interlock and
        // here, and then the keystrokes never arrived. That is the harness losing a
        // race, not the backend failing, so say so rather than crying wolf.
        let still_ours = unsafe { GetForegroundWindow() } == hwnd;
        if !shifted {
            let got: String = evs.iter().filter(|e| e.kind == EVENT_TEXT).flat_map(|e| e.text().to_vec()).collect();
            println!("     (diag: foreground still ours = {still_ours}, text received = {got:?})");
        }
        if !shifted && !still_ours {
            println!("SKIP injected shift: lost the foreground mid-test");
        } else {
            check("injected shift+key produces an uppercase character", shifted);
            let lower = { evs.clear(); tap(0x1E, false); drain(&mut gui, 300, &mut evs);
                evs.iter().filter(|e| e.kind == EVENT_TEXT).flat_map(|e| e.text().to_vec()).any(|c| c.is_lowercase()) };
            check("and the same key without shift produces a lowercase one", lower);
        }
    }

    // ---- resize and buffer regrow -----------------------------------------------
    // 640x480 sits inside a 1024 buffer; 1200x900 needs 2048, so this crosses a
    // power-of-two boundary and must reallocate, bump the generation, and ask for
    // a full redraw. That is the path where a backend silently keeps handing out
    // the old, too-small buffer.
    let gen_before = gui.get_framebuffer().key >> 1;
    let side_before = gui.get_framebuffer().side;
    unsafe {
        let mut r = softer_gui::sys_win::RECT { left: 0, top: 0, right: 1200, bottom: 900 };
        softer_gui::sys_win::AdjustWindowRectEx(&mut r, softer_gui::sys_win::WS_OVERLAPPEDWINDOW, 0, 0);
        softer_gui::sys_win::SetWindowPos(hwnd as *mut _, core::ptr::null_mut(), 0, 0,
                                          r.right - r.left, r.bottom - r.top,
                                          softer_gui::sys_win::SWP_NOMOVE | softer_gui::sys_win::SWP_NOZORDER);
    }
    evs.clear();
    drain(&mut gui, 400, &mut evs);
    let (w, h) = gui.window_size();
    check("resize is reflected in window_size()", w == 1200 && h == 900);
    let fb = gui.get_framebuffer();
    check("the framebuffer grew to the next power of two", fb.side == 2048 && side_before == 1024);
    check("crossing the boundary bumped the generation", fb.key >> 1 != gen_before);
    check("and the window-sized sub-rect matches the new client area", fb.width == 1200 && fb.height == 900);

    // ---- the modal resize loop --------------------------------------------------
    // SC_SIZE enters the REAL modal loop, where DefWindowProc does not return and
    // the pump's own loop stops running. Any RENDER that arrives in here therefore
    // came from the resize timer, which is exactly the path under test. Escape ends
    // it. This needs the foreground, because Escape has to reach the modal loop.
    if focused {
        let period = gui.period_fs() as u128;
        let mut ts: Vec<u128> = Vec::new();
        // Watchdog. The modal loop has to be ended from ANOTHER thread: the whole
        // point of this test is that the app thread might be stuck, and leaving the
        // escape to a stuck thread is how a test wedges a machine.
        let h_usize = hwnd as usize;
        let dog = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            tap(0x01, false);                                          // Escape
            unsafe { PostMessageW(h_usize as HWND, 0x001F, 0, 0) };    // WM_CANCELMODE
        });
        unsafe { PostMessageW(hwnd, 0x0112, 0xF000, 0) };       // WM_SYSCOMMAND, SC_SIZE
        let t0 = std::time::Instant::now();
        let (slow_get, slow_submit) = drain_renders(&mut gui, 1200, &mut ts);
        let secs = t0.elapsed().as_secs_f64();
        let _ = dog.join();
        drain(&mut gui, 250, &mut evs);

        let n = ts.len();
        check("RENDER keeps arriving inside the modal resize loop", n >= 20);
        check("and the display timestamp advances instead of repeating",
              n >= 2 && ts.windows(2).all(|w| w[1] > w[0]));
        // The gate must stop the 8 ms timer running the clock at timer speed: a
        // broken gate shows up right here as roughly double the frame count.
        let expected = secs / (period as f64 / 1e15);
        check("and it advances at the display rate, not the timer rate",
              n as f64 <= expected * 1.35);
        println!("     ({n} renders in {secs:.2}s, slowest get {slow_get} ms, slowest submit {slow_submit} ms)");
    } else {
        println!("SKIP modal resize loop: needs the foreground, because Escape is");
        println!("SKIP modal resize loop: what gets back out of the loop.");
    }

    // ---- close ------------------------------------------------------------------
    evs.clear();
    unsafe { PostMessageW(hwnd, 0x0010, 0, 0) };                // WM_CLOSE
    drain(&mut gui, 300, &mut evs);
    check("WM_CLOSE arrives as EVENT_CLOSE rather than destroying the window",
          evs.iter().any(|e| e.kind == EVENT_CLOSE));

    println!("{}", if fails.get() == 0 { "wintest: all checks passed" } else { "wintest: FAILURES" });
    std::process::exit(if fails.get() == 0 { 0 } else { 1 });
}
