//! Feed an XKB text keymap on stdin; print what a few keys resolve to under modifier states.
use std::io::Read;
fn main() {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).unwrap();
    let km = softer_gui::xkb::parse_text(&s).expect("parse");
    let show = |name: &str, code: u32| {
        let mut line = format!("{name:5}");
        for (mods, g, label) in [(0u8, 0u32, "base"), (1, 0, "shift"), (2, 0, "lock"), (128, 0, "mod5"), (129, 0, "sh+mod5"), (0, 1, "grp2")] {
            let (sym, txt) = km.text(code, mods, g);
            line += &format!("  {label}={txt:?}/{sym:x}");
        }
        println!("{line}");
    };
    show("q", 24); show("a", 38); show("2", 11); show("4", 13); show("e", 26); show(";", 47); show("KP1", 87); show("bksp", 22); show("lvl3", 108);
}
