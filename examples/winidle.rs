//! Idle proof: does the frame chain survive an app that has nothing to draw?
//!
//! The README's pacing contract is that a submitted frame's completion is the
//! RENDER for the next one, and that when the app skips a frame the chain restarts
//! on `request_frame()`. winpace never exercises the second half, because it
//! submits every frame, and that is exactly how the capable Windows path came to
//! break it: the swapchain's waitable object is refilled by Present, so the first
//! RENDER an app declined to draw was the last one it was ever sent. The window
//! stayed up, input kept arriving, and nothing was drawn again until a resize.
//!
//! So this skips on purpose, in the two shapes a real app does it.
//!
//! First half, eager: every Nth RENDER is not submitted and the app asks for the
//! next one on the spot. Second half, lazy: nothing is submitted at all and the
//! app only asks once a RENDER is overdue, which is what a damage-tracking
//! renderer with a watchdog looks like when its picture is not changing.
//!
//! Run it with no argument to skip every fifth frame, or `winidle 2 10` to skip
//! every second one for ten seconds. `--gdi` and `--d3d` pick the path.

#[cfg(not(any(target_os = "windows", cosmo)))]
fn main() { println!("winidle: Windows or a cosmo APE only"); }

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

/// How long the lazy half lets a RENDER be overdue before asking for one.
#[cfg(any(target_os = "windows", cosmo))]
const OVERDUE_MS: u128 = 40;
/// The longest silence a working chain can show here: the lazy half asks after
/// OVERDUE_MS and is answered within a refresh or two, so this is several times
/// the worst honest case and far below what a dead chain produces.
#[cfg(any(target_os = "windows", cosmo))]
const SILENCE_LIMIT_MS: u128 = 250;

#[cfg(any(target_os = "windows", cosmo))]
fn main() {
    let nums: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    let skip_every: u64 = nums.first().and_then(|a| a.parse().ok()).unwrap_or(5).max(2);
    let seconds: f64 = nums.get(1).and_then(|a| a.parse().ok()).unwrap_or(6.0);

    let Some(mut gui) = softer_gui::open_with("softer_gui winidle", "lol.softer.winidle", 480, 320, options_from_args()) else {
        eprintln!("winidle: could not open a window");
        std::process::exit(1);
    };
    let period = gui.period_fs();
    println!("winidle: period {period} fs ({:.4} Hz), skipping every {skip_every} frames then all of them, {seconds}s",
             1e15 / period as f64);

    let mut ev = Event::default();
    let (mut frames, mut skipped, mut after_skip) = (0u64, 0u64, 0u64);
    let (mut eager_frames, mut lazy_frames) = (0u64, 0u64);
    let (mut first_t, mut last_t) = (0u128, 0u128);
    let mut last_render = std::time::Instant::now();
    // Longest wall-clock gap between RENDERs, per half: [eager, lazy].
    let mut silence_ms = [0u128; 2];
    let mut was_lazy = false;
    let start = std::time::Instant::now();

    'outer: while start.elapsed().as_secs_f64() < seconds {
        // A timeout, not gui.wait(): against a backend that drops the request this
        // loop would otherwise sleep forever and never get to say so.
        gui.wait_ms(20);
        let lazy = start.elapsed().as_secs_f64() > seconds / 2.0;
        if lazy && !was_lazy {
            // A silence still running at the handover belongs to the eager half.
            silence_ms[0] = silence_ms[0].max(last_render.elapsed().as_millis());
            last_render = std::time::Instant::now();
            was_lazy = true;
        }
        if lazy && last_render.elapsed().as_millis() > OVERDUE_MS { gui.request_frame(); }
        while gui.next_event(&mut ev) {
            match ev.kind {
                EVENT_CLOSE => break 'outer,
                EVENT_RENDER => {
                    if frames != 0 { silence_ms[lazy as usize] = silence_ms[lazy as usize].max(last_render.elapsed().as_millis()); }
                    last_render = std::time::Instant::now();
                    if first_t == 0 { first_t = ev.t_fs; }
                    last_t = ev.t_fs;
                    frames += 1;
                    if skipped != 0 { after_skip += 1; }
                    if lazy { lazy_frames += 1 } else { eager_frames += 1 }

                    if lazy || frames % skip_every == 0 {
                        skipped += 1;
                        if !lazy { gui.request_frame(); }
                        continue;
                    }
                    let mut fb = gui.get_framebuffer();
                    if !fb.ok() { continue; }
                    let c = if frames % 2 == 0 { 0xFF203040 } else { 0xFF304020 };
                    for p in fb.slice().iter_mut() { *p = c; }
                    gui.submit();
                }
                _ => {}
            }
        }
    }
    // The run can end in the middle of a silence, and that one counts most of all.
    silence_ms[was_lazy as usize] = silence_ms[was_lazy as usize].max(last_render.elapsed().as_millis());

    let wall = start.elapsed().as_secs_f64();
    let disp = (last_t - first_t) as f64 / 1e15;
    println!("winidle: {frames} frames ({eager_frames} eager, {lazy_frames} lazy), {skipped} skipped, wall {wall:.3}s, display {disp:.3}s");

    let mut bad = 0;
    let ok = after_skip > 0 && silence_ms[0] < SILENCE_LIMIT_MS;
    println!("{} the chain restarts when the app asks straight after skipping ({after_skip} frames after the first skip, longest silence {} ms)",
             if ok { "ok  " } else { "FAIL" }, silence_ms[0]);
    if !ok { bad += 1; }
    let ok = lazy_frames > 0 && silence_ms[1] < SILENCE_LIMIT_MS;
    println!("{} the chain restarts when the app asks only once a frame is overdue ({lazy_frames} frames, longest silence {} ms)",
             if ok { "ok  " } else { "FAIL" }, silence_ms[1]);
    if !ok { bad += 1; }
    // Reported, not asserted. Whether display time tracks the wall clock is
    // winpace's question, and it has an answer only where the refresh period is
    // a fact: with a variable-refresh panel the compositor slows down under an
    // app that is not presenting, which is exactly the app this is.
    println!("note display time drift {:+.1}ms over {wall:.1}s", (disp - wall) * 1e3);

    println!("{}", if bad == 0 { "winidle: idle pacing verified" } else { "winidle: FAILURES" });
    std::process::exit(if bad == 0 { 0 } else { 1 });
}
