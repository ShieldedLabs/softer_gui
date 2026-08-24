//! Keysym ↔ name/char, keypad and modifier classification, dead-key composition.
use crate::keysym_table::{KEYSYMS, KEYSYM_ALIASES};

/// Keysym by X name (`a`, `Shift_L`, `dead_macron`, `U20AC`, `0x1000000`).
pub fn from_name(name: &str) -> Option<u32> {
    if let Some(i) = KEYSYMS.iter().position(|k| k.0 == name) { return Some(KEYSYMS[i].1); }
    if let Some(i) = KEYSYM_ALIASES.iter().position(|k| k.0 == name) { return Some(KEYSYM_ALIASES[i].1); }
    if let Some(h) = name.strip_prefix("0x").or_else(|| name.strip_prefix("0X")) {
        return u32::from_str_radix(h, 16).ok();
    }
    if let Some(h) = name.strip_prefix('U') {
        if let Ok(v) = u32::from_str_radix(h, 16) { return Some(0x0100_0000 | v); }
    }
    if name.chars().all(|c| c.is_ascii_digit()) { return name.parse().ok(); }
    None
}

/// Unicode character for a keysym, if it represents one (control chars included).
pub fn to_char(sym: u32) -> Option<char> {
    if (0x20..=0x7e).contains(&sym) || (0xa0..=0xff).contains(&sym) { return char::from_u32(sym); }
    if sym & 0xff00_0000 == 0x0100_0000 { return char::from_u32(sym & 0x00ff_ffff); }
    let i = KEYSYMS.binary_search_by_key(&sym, |k| k.1).ok()?;
    let u = KEYSYMS[i].2;
    if u == 0 { None } else { char::from_u32(u) }
}

pub fn is_modifier(sym: u32) -> bool {
    (0xffe1..=0xffee).contains(&sym) || (0xfe01..=0xfe13).contains(&sym) || sym == 0xff7e || sym == 0xff7f
}
pub fn is_keypad(sym: u32) -> bool { (0xff80..=0xffbd).contains(&sym) || (0x11000000..=0x1100ffff).contains(&sym) }

pub fn lower(sym: u32) -> u32 {
    match to_char(sym) { Some(c) => { let mut l = c.to_lowercase(); let r = l.next().unwrap_or(c); if l.next().is_some() { sym } else { from_char(r).unwrap_or(sym) } } None => sym }
}
pub fn upper(sym: u32) -> u32 {
    match to_char(sym) { Some(c) => { let mut u = c.to_uppercase(); let r = u.next().unwrap_or(c); if u.next().is_some() { sym } else { from_char(r).unwrap_or(sym) } } None => sym }
}
pub fn from_char(c: char) -> Option<u32> {
    let u = c as u32;
    if (0x20..=0x7e).contains(&u) || (0xa0..=0xff).contains(&u) { return Some(u); }
    if let Some(k) = KEYSYMS.iter().find(|k| k.2 == u && k.1 < 0x0100_0000) { return Some(k.1); }
    Some(0x0100_0000 | u)
}
/// `a`/`A` style pair as XKB's automatic-type logic sees it.
pub fn is_case_pair(lo: u32, hi: u32) -> bool {
    lo != hi && lower(hi) == lo && upper(lo) == hi && to_char(lo).map(|c| c.is_alphabetic()).unwrap_or(false)
}

/// The spacing character of a dead keysym (0xfe50..0xfe93), if we compose with it.
pub fn dead_key_base(sym: u32) -> Option<char> {
    Some(match sym {
        0xfe50 => '`', 0xfe51 => '\u{b4}', 0xfe52 => '^', 0xfe53 => '~', 0xfe54 => '\u{af}',
        0xfe55 => '\u{2d8}', 0xfe56 => '\u{2d9}', 0xfe57 => '\u{a8}', 0xfe58 => '\u{2da}',
        0xfe59 => '\u{2dd}', 0xfe5a => '\u{2c7}', 0xfe5b => '\u{b8}', 0xfe5c => '\u{2db}',
        _ => return None,
    })
}

/// Compose an armed dead key with a base character. Latin-1 + Latin Extended-A coverage.
pub fn dead_compose(dead: u32, base: char) -> Option<char> {
    let t: &[(char, char)] = match dead {
        0xfe50 => &[('a','à'),('e','è'),('i','ì'),('o','ò'),('u','ù'),('A','À'),('E','È'),('I','Ì'),('O','Ò'),('U','Ù'),('n','ǹ'),('N','Ǹ'),('w','ẁ'),('W','Ẁ'),('y','ỳ'),('Y','Ỳ')],
        0xfe51 => &[('a','á'),('e','é'),('i','í'),('o','ó'),('u','ú'),('y','ý'),('A','Á'),('E','É'),('I','Í'),('O','Ó'),('U','Ú'),('Y','Ý'),('c','ć'),('C','Ć'),('n','ń'),('N','Ń'),('s','ś'),('S','Ś'),('z','ź'),('Z','Ź'),('l','ĺ'),('L','Ĺ'),('r','ŕ'),('R','Ŕ'),('w','ẃ'),('W','Ẃ'),('g','ǵ'),('G','Ǵ')],
        0xfe52 => &[('a','â'),('e','ê'),('i','î'),('o','ô'),('u','û'),('A','Â'),('E','Ê'),('I','Î'),('O','Ô'),('U','Û'),('c','ĉ'),('C','Ĉ'),('g','ĝ'),('G','Ĝ'),('h','ĥ'),('H','Ĥ'),('j','ĵ'),('J','Ĵ'),('s','ŝ'),('S','Ŝ'),('w','ŵ'),('W','Ŵ'),('y','ŷ'),('Y','Ŷ')],
        0xfe53 => &[('a','ã'),('o','õ'),('n','ñ'),('A','Ã'),('O','Õ'),('N','Ñ'),('i','ĩ'),('I','Ĩ'),('u','ũ'),('U','Ũ'),('e','ẽ'),('E','Ẽ'),('y','ỹ'),('Y','Ỹ')],
        0xfe54 => &[('a','ā'),('e','ē'),('i','ī'),('o','ō'),('u','ū'),('A','Ā'),('E','Ē'),('I','Ī'),('O','Ō'),('U','Ū'),('y','ȳ'),('Y','Ȳ'),('g','ḡ'),('G','Ḡ')],
        0xfe55 => &[('a','ă'),('A','Ă'),('e','ĕ'),('E','Ĕ'),('g','ğ'),('G','Ğ'),('i','ĭ'),('I','Ĭ'),('o','ŏ'),('O','Ŏ'),('u','ŭ'),('U','Ŭ')],
        0xfe56 => &[('c','ċ'),('C','Ċ'),('e','ė'),('E','Ė'),('g','ġ'),('G','Ġ'),('I','İ'),('z','ż'),('Z','Ż'),('i','ı')],
        0xfe57 => &[('a','ä'),('e','ë'),('i','ï'),('o','ö'),('u','ü'),('y','ÿ'),('A','Ä'),('E','Ë'),('I','Ï'),('O','Ö'),('U','Ü'),('Y','Ÿ')],
        0xfe58 => &[('a','å'),('A','Å'),('u','ů'),('U','Ů')],
        0xfe59 => &[('o','ő'),('O','Ő'),('u','ű'),('U','Ű')],
        0xfe5a => &[('c','č'),('C','Č'),('d','ď'),('D','Ď'),('e','ě'),('E','Ě'),('n','ň'),('N','Ň'),('r','ř'),('R','Ř'),('s','š'),('S','Š'),('t','ť'),('T','Ť'),('z','ž'),('Z','Ž'),('a','ǎ'),('A','Ǎ'),('i','ǐ'),('I','Ǐ'),('o','ǒ'),('O','Ǒ'),('u','ǔ'),('U','Ǔ'),('g','ǧ'),('G','Ǧ'),('k','ǩ'),('K','Ǩ')],
        0xfe5b => &[('c','ç'),('C','Ç'),('s','ş'),('S','Ş'),('t','ţ'),('T','Ţ'),('g','ģ'),('G','Ģ'),('k','ķ'),('K','Ķ'),('l','ļ'),('L','Ļ'),('n','ņ'),('N','Ņ'),('r','ŗ'),('R','Ŗ')],
        0xfe5c => &[('a','ą'),('A','Ą'),('e','ę'),('E','Ę'),('i','į'),('I','Į'),('u','ų'),('U','Ų')],
        _ => return None,
    };
    if base == ' ' { return dead_key_base(dead); }
    t.iter().find(|p| p.0 == base).map(|p| p.1)
}
