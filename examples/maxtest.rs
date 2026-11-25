//! Opens a 640x400 window maximized and checks that the first RENDER already has
//! the maximized size, then that the size still holds a second later. Exits 0 on
//! a pass. `--restored` opens it unmaximized and expects 640x400 throughout.
//! Takes the repo's usual flags (`--x11`, `--gdi`, `--d3d`, `--debug`).
use softer_gui::*;

fn main() {
    let mut o = Options { maximized: true, ..Default::default() };
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--debug" => o.debug = true,
            "--x11" => o.backend = Backend_::X11,
            "--gdi" => o.backend = Backend_::Gdi,
            "--d3d" => o.backend = Backend_::D3d,
            "--restored" => o.maximized = false,
            _ => {}
        }
    }
    let n: Vec<u32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let (w0, h0) = if n.len() == 2 { (n[0], n[1]) } else { (640, 400) };
    let mut gui = open_with("maxtest", "com.example.maxtest", w0, h0, o).expect("open");
    let mut ev = Event::default();
    let start = std::time::Instant::now();
    let mut first: Option<(usize, usize)> = None;
    let mut last = (0, 0);
    let mut frames = 0;
    while start.elapsed().as_millis() < 1500 {
        gui.wait_ms(50);
        while start.elapsed().as_millis() < 1500 && gui.next_event(&mut ev) {
            if ev.kind != EVENT_RENDER { continue; }
            let mut fb = gui.get_framebuffer();
            if !fb.ok() { continue; }
            let (w, h, side) = (fb.width, fb.height, fb.side);
            let px = fb.slice();
            for y in 0..h { px[y * side..y * side + w].fill(0xFF20_4060); }
            gui.submit();
            if first.is_none() { first = Some((w, h)); println!("maxtest: first frame {w}x{h}"); }
            if (w, h) != last { if frames > 0 { println!("maxtest: frame {frames} is {w}x{h}"); } last = (w, h); }
            frames += 1;
        }
    }
    let Some((fw, fh)) = first else { println!("maxtest: FAIL, no frame"); std::process::exit(1) };
    println!("maxtest: {frames} frames, last {}x{}", last.0, last.1);
    let want_big = o.maximized;
    let ok = (fw, fh) == last && if want_big { fw > w0 as usize && fh > h0 as usize } else { (fw, fh) == (w0 as usize, h0 as usize) };
    println!("maxtest: {}", if ok { "pass" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
