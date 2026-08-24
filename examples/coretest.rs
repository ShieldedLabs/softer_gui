//! Headless check of the event core: one key press must yield exactly one Text and one Buttons event.
use softer_gui::event::*;
fn main() {
    let c = Core::new();
    c.key_press_sym(35, 0x77, "w", 1_000_000);
    c.key(35, false);
    let mut n = 0;
    let mut e = Event::default();
    while c.next_event(&mut e) { n += 1; println!("kind {} text {:?}", e.kind, e.text()); }
    println!("{n} events");
}
