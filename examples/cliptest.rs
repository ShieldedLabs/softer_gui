//! Clipboard check without a human: `cliptest set TEXT [HOLD_MS]` owns the clipboard
//! for HOLD_MS (default 3000) so another process can paste from it; `cliptest get`
//! prints what is on it; `cliptest both TEXT [WAIT_MS]` sets and reads back in one process;
//! `cliptest big N` does the same with N bytes and prints only whether they match.
//! SOFTER_GUI_X11=1 picks X11. On Wayland the window needs keyboard focus for any of it.
use softer_gui::*;
use std::time::{Duration, Instant};

fn pump(gui: &mut Gui, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    let mut ev = Event::default();
    while Instant::now() < end {
        gui.wait_ms(10);
        while gui.next_event(&mut ev) {
            if ev.kind == EVENT_RENDER {
                let mut fb = gui.get_framebuffer();
                if fb.ok() { fb.slice().fill(0xFF204060); gui.submit(); }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut gui = open("cliptest", "com.example.cliptest", 320, 200).expect("open");
    let clip = gui.clipboard();
    println!("{clip:?} available {}", clip.available());
    // Long enough for the window to map and, on Wayland, take focus.
    pump(&mut gui, 700);
    match args.first().map(|s| s.as_str()) {
        Some("set") => {
            println!("set -> {}", clip.set(&args[1]));
            pump(&mut gui, args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3000));
        }
        Some("get") => println!("get -> {:?}", clip.get()),
        Some("both") => {
            println!("set -> {}", clip.set(&args[1]));
            pump(&mut gui, args.get(2).and_then(|s| s.parse().ok()).unwrap_or(300));
            println!("get -> {:?}", clip.get());
            pump(&mut gui, 300);
        }
        Some("big") => {
            let n: usize = args[1].parse().unwrap();
            let text: String = (0..n).map(|i| (b'a' + (i % 26) as u8) as char).collect();
            println!("set -> {}", clip.set(&text));
            pump(&mut gui, 300);
            let got = clip.get();
            println!("big {n} -> match {}, got {:?} bytes", got.as_deref() == Some(text.as_str()), got.map(|g| g.len()));
        }
        _ => eprintln!("usage: cliptest set TEXT [HOLD_MS] | get | both TEXT | big N"),
    }
}
