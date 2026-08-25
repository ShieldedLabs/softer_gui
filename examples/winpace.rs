//! Pacing proof: does display time count the refreshes that ACTUALLY elapsed?
//!
//! This is the one property the whole crate exists for, and the one a Windows
//! backend is most likely to get quietly wrong, because the easy implementation
//! passes 1 to display_tick() every wakeup and looks perfect until the app misses
//! a frame. So we make it miss frames on purpose.
//!
//! Every Nth frame the renderer sleeps well past the vblank. A correct backend
//! reports the gap as 2 (or more) whole periods and display time keeps tracking
//! the wall clock; a backend that always ticks 1 shows a growing negative drift,
//! because it is inventing a slower display than the one it is drawing on.
//!
//! Run it with no argument for the honest case, or `winpace 10` to stall every
//! tenth frame.

#[cfg(not(any(target_os = "windows", cosmo)))]
fn main() { println!("winpace: Windows or a cosmo APE only"); }

#[cfg(any(target_os = "windows", cosmo))]
use softer_gui::*;

/// The flags every program in this repo understands, so the same words work
/// whichever one you are running. A library cannot read these for you: it has no
/// command line, and helping itself to the program's argv is not its business.
fn options_from_args() -> softer_gui::Options {
    use softer_gui::{Backend_, D3dDriver};
    let mut o = softer_gui::Options::default();
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--debug" => o.debug = true,
            "--fullscreen" => o.fullscreen = true,
            "--gdi" => o.backend = Backend_::Gdi,
            "--d3d" => o.backend = Backend_::D3d,
            "--x11" => o.backend = Backend_::X11,
            "--warp" => o.d3d_driver = D3dDriver::Warp,
            "--hardware" => o.d3d_driver = D3dDriver::Hardware,
            _ => {}
        }
    }
    o
}

#[cfg(any(target_os = "windows", cosmo))]
fn main() {
    // Positionals are whatever is not a flag, so the two can be given in any order.
    let nums: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    let stall_every: u64 = nums.first().and_then(|a| a.parse().ok()).unwrap_or(10);
    let seconds: f64 = nums.get(1).and_then(|a| a.parse().ok()).unwrap_or(6.0);

    let Some(mut gui) = softer_gui::open_with("softer_gui winpace", "lol.softer.winpace", 480, 320, options_from_args()) else {
        eprintln!("winpace: could not open a window");
        std::process::exit(1);
    };
    let period = gui.period_fs();
    println!("winpace: period {period} fs ({:.4} Hz), stalling every {stall_every} frames for {seconds}s",
             1e15 / period as f64);

    // --cycle switches rendering path every second, which is how the switch
    // itself gets tested: the histogram below must stay clean across them.
    let cycle = std::env::args().any(|a| a == "--cycle");
    let mut last_cycle = std::time::Instant::now();

    let mut ev = Event::default();
    let mut reported_size = (0u32, 0u32);
    let mut frames = 0u64;
    let mut last_t: u128 = 0;
    let mut first_t: u128 = 0;
    // gaps[n] = how many RENDER-to-RENDER intervals were n whole periods long.
    let mut gaps = [0u64; 12];
    let mut over = 0u64;
    let start = std::time::Instant::now();

    'outer: while start.elapsed().as_secs_f64() < seconds {
        gui.wait();
        while gui.next_event(&mut ev) {
            match ev.kind {
                EVENT_CLOSE => break 'outer,
                EVENT_RENDER => {
                    if first_t == 0 { first_t = ev.t_fs; reported_size = (ev.width, ev.height); }
                    if last_t != 0 {
                        let n = ((ev.t_fs - last_t) / ev.dt_fs as u128) as usize;
                        if n < gaps.len() { gaps[n] += 1 } else { over += 1 }
                    }
                    last_t = ev.t_fs;
                    frames += 1;
                    if cycle && last_cycle.elapsed().as_millis() > 1000 {
                        last_cycle = std::time::Instant::now();
                        gui.cycle_backend();
                    }
                    let mut fb = gui.get_framebuffer();
                    if !fb.ok() { continue; }
                    let c = if frames % 2 == 0 { 0xFF203040 } else { 0xFF304020 };
                    for p in fb.slice().iter_mut() { *p = c; }
                    gui.submit();
                    // Miss the vblank on purpose. It has to be worth several whole
                    // periods: push_render() only coalesces when the PREVIOUS RENDER
                    // is still unconsumed, so a one-period overrun merely queues a
                    // frame and the app catches up with no gap at all. Four periods
                    // guarantees the pump ticks past a RENDER the app has not taken.
                    if stall_every > 0 && frames % stall_every == 0 {
                        std::thread::sleep(std::time::Duration::from_micros(period / 1_000_000_000 * 4));
                    }
                }
                _ => {}
            }
        }
    }

    let wall = start.elapsed().as_secs_f64();
    println!("winpace: window {}x{} at exit (started {}x{})", gui.window_size().0, gui.window_size().1, reported_size.0, reported_size.1);
    let disp = (last_t - first_t) as f64 / 1e15;
    println!("winpace: {frames} frames, wall {wall:.3}s, display {disp:.3}s, drift {:+.1}ms", (disp - wall) * 1e3);
    print!("winpace: gap histogram (periods:count)");
    for (n, c) in gaps.iter().enumerate() { if *c > 0 { print!(" {n}:{c}"); } }
    if over > 0 { print!(" over:{over}"); }
    println!();

    // A stalled frame MUST show up as a multi-period gap; if every gap is one
    // period while we were demonstrably sleeping through vblanks, the backend is
    // inventing frame boundaries instead of counting them.
    let multi: u64 = gaps.iter().skip(2).sum::<u64>() + over;
    let mut bad = 0;
    if stall_every > 0 {
        let ok = multi > 0;
        println!("{} stalled frames are reported as multi-period gaps ({multi} of them)", if ok { "ok  " } else { "FAIL" });
        if !ok { bad += 1; }
    }
    // Display time is not the wall clock and is free to differ, but it must not
    // DRIFT: a backend that under-counts refreshes falls behind without bound.
    let ok = (disp - wall).abs() < 0.25 * wall.max(1.0) / 6.0 + 0.05;
    println!("{} display time tracks the wall clock (drift {:+.1}ms over {wall:.1}s)",
             if ok { "ok  " } else { "FAIL" }, (disp - wall) * 1e3);
    if !ok { bad += 1; }

    println!("{}", if bad == 0 { "winpace: pacing verified" } else { "winpace: FAILURES" });
    std::process::exit(if bad == 0 { 0 } else { 1 });
}
