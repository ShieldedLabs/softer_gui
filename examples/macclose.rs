//! macOS regression check for EVENT_CLOSE: a window that is merely not on
//! screen (minimized, app hidden, another Space, full screen) must not raise it;
//! a close request (red button, Quit) must raise exactly one.
//!
//!   cargo run --example macclose            interactive: prints every event but RENDER, and RENDERs per second
//!   cargo run --example macclose -- --script     drives minimize/hide/fullscreen/close itself and checks the counts
//!   cargo run --example macclose -- --script --quit   the same, ending in NSApp terminate: (Dock Quit) instead of the red button
//!   cargo run --example macclose -- --script --only=minimize,hide,fullscreen   just those steps (close is the latch: test each alone)
//!
//! The script drives AppKit with performSelectorOnMainThread, which the pump
//! runs on the main thread exactly as it runs the button and menu actions; it
//! needs no Accessibility permission. Exit status 0 is a pass.

#[cfg(not(target_os = "macos"))]
fn main() { println!("macclose: macOS only"); }

#[cfg(target_os = "macos")]
use softer_gui::*;

#[cfg(target_os = "macos")]
mod objc {
    use core::ffi::c_void;
    pub type Id = *mut c_void;
    #[link(name = "objc")]
    unsafe extern "C" { fn objc_getClass(name: *const u8) -> Id; fn sel_registerName(name: *const u8) -> Id; fn objc_msgSend(); }
    pub fn sel(name: &str) -> Id { let s = format!("{name}\0"); unsafe { sel_registerName(s.as_ptr()) } }
    pub fn send0(r: Id, s: &str) -> Id { let f: extern "C" fn(Id, Id) -> Id = unsafe { core::mem::transmute(objc_msgSend as *const c_void) }; f(r, sel(s)) }
    pub fn app() -> Id { send0(unsafe { objc_getClass(b"NSApplication\0".as_ptr()) }, "sharedApplication") }
    /// `[r performSelectorOnMainThread:s withObject:nil waitUntilDone:NO]`: the action runs on the pump's thread.
    pub fn on_main(r: Id, s: &str) {
        let f: extern "C" fn(Id, Id, Id, Id, bool) = unsafe { core::mem::transmute(objc_msgSend as *const c_void) };
        f(r, sel("performSelectorOnMainThread:withObject:waitUntilDone:"), sel(s), core::ptr::null_mut(), false);
    }
}

#[cfg(target_os = "macos")]
struct Run { gui: Gui, closes: u32, renders: u32, frame: u32 }

#[cfg(target_os = "macos")]
impl Run {
    /// Pump events for `ms`, drawing every RENDER; returns (closes, renders) seen in that time.
    fn pump(&mut self, ms: u64, log: bool) -> (u32, u32) {
        let (c0, r0) = (self.closes, self.renders);
        let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        let mut ev = Event::default();
        while std::time::Instant::now() < end {
            self.gui.wait_ms(20);
            while std::time::Instant::now() < end && self.gui.next_event(&mut ev) {
                match ev.kind {
                    EVENT_RENDER => {
                        self.renders += 1;
                        self.frame += 1;
                        let mut fb = self.gui.get_framebuffer();
                        if fb.ok() {
                            // A colour that cycles, so a window that stopped presenting is visible as frozen.
                            let (side, w, h, f) = (fb.side, fb.width, fb.height, self.frame);
                            let px = fb.slice();
                            for y in 0..h { px[y * side..y * side + w].fill(0xff000000 | (f & 0xff) << 16 | 0x4060); }
                            self.gui.submit();
                        }
                    }
                    EVENT_CLOSE => { self.closes += 1; if log { println!("EVENT_CLOSE (#{})", self.closes); } }
                    k if log => println!("event kind {k}"),
                    _ => {}
                }
            }
        }
        (self.closes - c0, self.renders - r0)
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let script = args.iter().any(|a| a == "--script");
    let quit = args.iter().any(|a| a == "--quit");
    // A close is sent once and then latched, so a step that wrongly raised one hides the
    // same fault in every later step: --only runs a subset.
    let only: Option<Vec<String>> = args.iter().find_map(|a| a.strip_prefix("--only=")).map(|s| s.split(',').map(String::from).collect());
    let doing = |what: &str| only.as_ref().is_none_or(|o| o.iter().any(|x| x == what));
    let Some(gui) = open("macclose", "com.shieldedlabs.softer_gui.macclose", 640, 400) else { eprintln!("macclose: no window"); std::process::exit(2) };
    let mut run = Run { gui, closes: 0, renders: 0, frame: 0 };

    if !script {
        println!("macclose: minimize, hide, switch Spaces, go full screen, close, quit; every event but RENDER is printed.");
        loop {
            let (c, r) = run.pump(1000, true);
            println!("{r} RENDER/s{}", if c > 0 { "  <- close requested" } else { "" });
            // An app that honours the request leaves; give it one more second to show a duplicate.
            if run.closes > 0 { let (c, _) = run.pump(1000, true); if c > 0 { println!("FAIL: duplicate EVENT_CLOSE"); } return; }
        }
    }

    let app = objc::app();
    let win = objc::send0(objc::send0(app, "windows"), "firstObject");
    if win.is_null() { eprintln!("macclose: no NSWindow"); std::process::exit(2) }
    let mut fails = 0;
    let mut step = |run: &mut Run, name: &str, ms: u64, want_closes: u32| {
        let (c, r) = run.pump(ms, false);
        let ok = c == want_closes;
        if !ok { fails += 1; }
        println!("{:<4} {name:<34} EVENT_CLOSE {c} (want {want_closes})  RENDER {r} in {ms} ms", if ok { "ok" } else { "FAIL" });
    };
    step(&mut run, "visible", 1500, 0);
    if doing("minimize") {
        objc::on_main(win, "miniaturize:");
        step(&mut run, "miniaturized", 3000, 0);
        objc::on_main(win, "deminiaturize:");
        step(&mut run, "restored from the Dock", 1500, 0);
    }
    if doing("hide") {
        objc::on_main(app, "hide:");
        step(&mut run, "application hidden", 3000, 0);
        objc::on_main(app, "unhide:");
        step(&mut run, "application unhidden", 1500, 0);
    }
    if doing("fullscreen") {
        run.gui.set_fullscreen(true);
        step(&mut run, "full screen (own Space)", 3000, 0);
        run.gui.set_fullscreen(false);
        step(&mut run, "back from full screen", 3000, 0);
    }
    if !doing("close") && !doing("quit") {
    } else if quit || (doing("quit") && !doing("close")) {
        // What the Dock's Quit sends. Without a handler AppKit calls exit() here and nothing below prints.
        objc::on_main(app, "terminate:");
        step(&mut run, "NSApp terminate: (Dock Quit)", 1500, 1);
    } else {
        // What the red button sends.
        objc::on_main(win, "performClose:");
        step(&mut run, "performClose: (red button)", 1500, 1);
    }
    step(&mut run, "after the close request", 1500, 0);
    println!("{}", if fails == 0 { "PASS" } else { "FAIL" });
    std::process::exit(if fails == 0 { 0 } else { 1 });
}
