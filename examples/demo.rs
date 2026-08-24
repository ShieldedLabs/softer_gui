//! A bouncing box, a yellow box under the mouse, scroll/pinch readouts, a typed-text
//! row, and a frame-time HUD — plus pacing statistics on stdout (frames, dropped
//! vblanks, display-vs-wall drift). ESC or the close button quits; F11 toggles
//! fullscreen; hold the left mouse button to hide the native cursor.
use softer_gui::*;

fn fill(fb: &mut Framebuffer, x0: i64, y0: i64, x1: i64, y1: i64, c: u32) {
    let (w, h) = (fb.width as i64, fb.height as i64);
    let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(w), y1.min(h));
    for y in y0..y1 { let row = y as usize * fb.side; for x in x0..x1 { fb.pixels[row + x as usize] = c; } }
}

// 3x5 digit glyphs for the HUD, one bit per pixel, rows top to bottom.
const DIGITS: [u16; 10] = [0b111_101_101_101_111, 0b010_110_010_010_111, 0b111_001_111_100_111, 0b111_001_111_001_111, 0b101_101_111_001_001,
                           0b111_100_111_001_111, 0b111_100_111_101_111, 0b111_001_001_001_001, 0b111_101_111_101_111, 0b111_101_111_001_111];
fn digit(fb: &mut Framebuffer, x: i64, y: i64, d: u8, s: i64, c: u32) {
    let g = DIGITS[d as usize % 10];
    for r in 0..5 { for col in 0..3 { if g >> (14 - (r * 3 + col)) & 1 != 0 { fill(fb, x + col * s, y + r * s, x + col * s + s, y + r * s + s, c); } } }
}
fn number(fb: &mut Framebuffer, x: i64, y: i64, mut v: u64, s: i64, c: u32) {
    let mut ds = Vec::new();
    if v == 0 { ds.push(0); }
    while v > 0 { ds.push((v % 10) as u8); v /= 10; }
    for (i, d) in ds.iter().rev().enumerate() { digit(fb, x + i as i64 * 4 * s, y, *d, s, c); }
}

fn main() {
    if !softer_gui::run("softer_gui demo", 640, 480, app) { eprintln!("could not open a window"); }
}

fn app(mut gui: Gui) {
    let period = gui.period_fs();
    println!("demo: window {}x{}, period {} fs ({:.4} Hz)", gui.window_size().0, gui.window_size().1, period, 1e15 / period as f64);

    let (mut bx, mut by) = (60.0f64, 60.0f64);
    let (mut vx, mut vy) = (240.0f64, 180.0f64);   // px/s
    let mut last_t: u128 = 0;
    let mut buttons = Buttons::default();
    let mut typed: Vec<char> = Vec::new();
    let (mut scroll_x, mut scroll_y) = (0i64, 0i64);
    let mut zoom = 1.0f64;
    let mut angle = 0.0f64;
    let (mut mx, mut my) = (0i64, 0i64);
    let mut frames = 0u64; let mut skipped = 0u64; let mut dropped_vblanks = 0u64; let mut same_t = 0u64;
    let start_wall = std::time::Instant::now();
    let mut start_disp: Option<u128> = None;
    let mut last_report = std::time::Instant::now();
    let mut last_gen = 0u64;
    let mut ft_last = std::time::Instant::now(); let mut ft_us = 0u64; let mut ft_max = 0u64;

    'outer: loop {
        gui.wait();
        while let Some(ev) = gui.next_event() {
            match ev.kind {
                Kind::Close => break 'outer,
                Kind::Buttons(b) => {
                    if b.get(KEY_ESC) { break 'outer; }
                    if b.get(KEY_F11) && !buttons.get(KEY_F11) { let f = !gui.is_fullscreen(); gui.set_fullscreen(f); }
                    if b.get(KEY_BACKSPACE) && !buttons.get(KEY_BACKSPACE) { typed.pop(); }
                    if b.get(BTN_LEFT) != buttons.get(BTN_LEFT) { gui.set_cursor_hidden(b.get(BTN_LEFT)); println!("left button {} at {},{}", b.get(BTN_LEFT), mx, my); }
                    buttons = b;
                }
                Kind::Text(t) => { for c in t.as_chars() { typed.push(*c); } println!("text: {:?}", typed.iter().collect::<String>()); }
                Kind::CopyPaste(a) => println!("copypaste {a}"),
                Kind::Axes(a) => for d in a.as_slice() {
                    if d.axis >= AXIS_SCROLL_V { println!("axis {} delta {}", d.axis, d.delta); }
                    match d.axis {
                        AXIS_MOUSE_X => mx = d.delta as i64 >> 8,
                        AXIS_MOUSE_Y => my = d.delta as i64 >> 8,
                        AXIS_SCROLL_V => scroll_y += d.delta as i64,
                        AXIS_SCROLL_H => scroll_x += d.delta as i64,
                        AXIS_ZOOM => zoom *= 1.0 + d.delta as f64 / 65536.0,
                        AXIS_ROTATE => angle += d.delta as f64 / 65536.0,
                        _ => {}
                    }
                },
                Kind::Render(r) => {
                    if start_disp.is_none() { start_disp = Some(ev.t_fs); }
                    // Step by DISPLAY time: exact multiples of the period, catch-up over drops.
                    if last_t != 0 {
                        if ev.t_fs == last_t { same_t += 1; }
                        let dt_fs = ev.t_fs - last_t;
                        let n = dt_fs / r.dt_fs as u128;
                        if n > 1 { dropped_vblanks += (n - 1) as u64; if std::env::var("SOFTER_GUI_DEBUG").is_ok() { eprintln!("[{}] render gap of {n} periods", (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()) % 100000); } }
                        let dt = dt_fs as f64 / 1e15;
                        bx += vx * dt; by += vy * dt;
                        let (w, h) = (r.width as f64, r.height as f64);
                        let hs = 25.0 * zoom;
                        if bx < hs { bx = hs; vx = -vx } if bx > w - hs { bx = w - hs; vx = -vx }
                        if by < hs { by = hs; vy = -vy } if by > h - hs { by = h - hs; vy = -vy }
                    }
                    last_t = ev.t_fs;
                    let Some(mut fb) = gui.get_framebuffer() else { skipped += 1; continue };
                    if fb.key >> 1 != last_gen { last_gen = fb.key >> 1; println!("framebuffer generation {last_gen}: side {} window {}x{}", fb.side, fb.width, fb.height); }
                    frames += 1;
                    let now = std::time::Instant::now();
                    ft_us = now.duration_since(ft_last).as_micros() as u64; ft_last = now;
                    if ft_us > ft_max { ft_max = ft_us; }
                    // Background, scroll-driven grid so scrolling is visible.
                    let (w, h) = (fb.width, fb.height);
                    for y in 0..h {
                        let row = y * fb.side;
                        let gy = ((y as i64 + (scroll_y >> 8)) & 63) < 2;
                        for x in 0..w {
                            let gx = ((x as i64 + (scroll_x >> 8)) & 63) < 2;
                            fb.pixels[row + x] = if gx || gy { 0xFF303040 } else { 0xFF181820 };
                        }
                    }
                    let hs = (25.0 * zoom) as i64;
                    fill(&mut fb, bx as i64 - hs, by as i64 - hs, bx as i64 + hs, by as i64 + hs, 0xFF40C0FF);
                    // Rotation readout: a bar around the box.
                    let (s, c) = (angle.to_radians().sin(), angle.to_radians().cos());
                    for i in -hs..hs { let px = bx + c * i as f64; let py = by + s * i as f64; fill(&mut fb, px as i64, py as i64, px as i64 + 2, py as i64 + 2, 0xFFFFFFFF); }
                    let (cx, cy) = ((r.cursor_x >> 32), (r.cursor_y >> 32));
                    fill(&mut fb, cx - 8, cy - 8, cx + 8, cy + 8, 0xFFFFFF00);
                    fill(&mut fb, mx - 3, my - 3, mx + 3, my + 3, 0xFFFF4040);
                    // Typed text as coloured cells (no font in the demo).
                    for (i, ch) in typed.iter().enumerate() {
                        let hsh = (*ch as u32).wrapping_mul(2654435761);
                        fill(&mut fb, 16 + i as i64 * 12, h as i64 - 40, 26 + i as i64 * 12, h as i64 - 24, 0xFF000000 | (hsh >> 8));
                    }
                    fill(&mut fb, 16 + typed.len() as i64 * 12, h as i64 - 42, 18 + typed.len() as i64 * 12, h as i64 - 22, if buttons.modes & MODE_DEAD_KEY != 0 { 0xFFFFA020 } else { 0xFF30FF30 });
                    // HUD: frame time us, max, frames.
                    number(&mut fb, 8, 8, ft_us, 2, 0xFFFFFFFF);
                    number(&mut fb, 8, 22, ft_max, 2, 0xFFFF8080);
                    number(&mut fb, 8, 36, frames, 2, 0xFF80FF80);
                    // 1px magenta border: resize correctness check.
                    fill(&mut fb, 0, 0, w as i64, 1, 0xFFFF00FF); fill(&mut fb, 0, h as i64 - 1, w as i64, h as i64, 0xFFFF00FF);
                    fill(&mut fb, 0, 0, 1, h as i64, 0xFFFF00FF); fill(&mut fb, w as i64 - 1, 0, w as i64, h as i64, 0xFFFF00FF);
                    gui.submit();
                }
                Kind::None => {}
            }
        }
        if last_report.elapsed().as_secs() >= 2 {
            last_report = std::time::Instant::now();
            ft_max = 0;
            let wall = start_wall.elapsed().as_secs_f64();
            let disp = start_disp.map(|s| (last_t - s) as f64 / 1e15).unwrap_or(0.0);
            println!("stats: frames {frames} skipped {skipped} dropped_vblanks {dropped_vblanks} same_t {same_t} wall {wall:.2}s display {disp:.2}s drift {:+.1}ms ft {ft_us}us", (disp - wall) * 1e3);
        }
    }
    println!("demo: quit after {frames} frames");
}
