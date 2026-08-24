//! PS/2 set-1 scancode to evdev code.
//!
//! event.rs's canonical code space is evdev, and evdev's numbering for the main
//! keyboard block was lifted from set-1 in the first place, so the base range is
//! not merely "close to" identity, it IS identity for 0x01..=0x58 (ESC=1 through
//! F12=88), holes included: set-1 0x54/0x55 are unassigned and so is evdev 84.
//! That is a fact worth stating loudly, because it is the reason this file is
//! twenty lines of table instead of two hundred.
//!
//! The E0-prefixed keys are where the two diverge and need a real table.
//!
//! We map from the SCANCODE, never from the virtual-key code: VK codes are
//! layout-dependent for the alphanumeric block (VK_Q is at position A on AZERTY),
//! and the whole point of the evdev layer is that it is positional. The single
//! exception is scancode 0x45, which Windows hands to both NumLock and Pause;
//! there the VK is the only thing that separates them.

use crate::event::*;

/// Extended (E0-prefixed) set-1 make codes. Index is the low byte after the E0.
/// 0 means "we have no evdev code for this", and the caller substitutes KEY_UNKNOWN.
const EXT: [u16; 128] = {
    let mut t = [0u16; 128];
    t[0x1C] = 96;               // KEY_KPENTER
    t[0x1D] = 97;               // KEY_RIGHTCTRL
    t[0x35] = 98;               // KEY_KPSLASH
    t[0x37] = 99;               // KEY_SYSRQ (PrintScreen sends E0 2A E0 37)
    t[0x38] = 100;              // KEY_RIGHTALT
    t[0x46] = 119;             // KEY_PAUSE (Ctrl+Break)
    t[0x47] = 102;              // KEY_HOME
    t[0x48] = 103;              // KEY_UP
    t[0x49] = 104;              // KEY_PAGEUP
    t[0x4B] = 105;              // KEY_LEFT
    t[0x4D] = 106;              // KEY_RIGHT
    t[0x4F] = 107;              // KEY_END
    t[0x50] = 108;              // KEY_DOWN
    t[0x51] = 109;              // KEY_PAGEDOWN
    t[0x52] = 110;              // KEY_INSERT
    t[0x53] = 111;              // KEY_DELETE
    t[0x5B] = 125;              // KEY_LEFTMETA
    t[0x5C] = 126;              // KEY_RIGHTMETA
    t[0x5D] = 127;              // KEY_COMPOSE (the menu key)
    t[0x5E] = 116;              // KEY_POWER
    t[0x5F] = 142;              // KEY_SLEEP
    t[0x63] = 143;              // KEY_WAKEUP
    // Consumer-control keys, as an ordinary keyboard's media row reports them.
    t[0x10] = 165;              // KEY_PREVIOUSSONG
    t[0x19] = 163;              // KEY_NEXTSONG
    t[0x20] = 113;              // KEY_MUTE
    t[0x21] = 140;              // KEY_CALC
    t[0x22] = 164;              // KEY_PLAYPAUSE
    t[0x24] = 166;              // KEY_STOPCD
    t[0x2E] = 114;              // KEY_VOLUMEDOWN
    t[0x30] = 115;              // KEY_VOLUMEUP
    t[0x32] = 150;              // KEY_WWW
    t[0x65] = 217;              // KEY_SEARCH
    t[0x66] = 156;              // KEY_BOOKMARKS
    t[0x67] = 173;              // KEY_REFRESH
    t[0x68] = 128;              // KEY_STOP
    t[0x69] = 159;              // KEY_FORWARD
    t[0x6A] = 158;              // KEY_BACK
    t[0x6B] = 144;              // KEY_COMPUTER
    t[0x6C] = 155;              // KEY_MAIL
    t[0x6D] = 226;              // KEY_MEDIA
    t
};

/// Non-extended codes above the identity range: the Japanese and Korean block.
fn base_high(sc: u32) -> u32 {
    match sc {
        0x70 => 93,             // KEY_KATAKANAHIRAGANA
        0x73 => 89,             // KEY_RO
        0x77 => 91,             // KEY_HIRAGANA
        0x78 => 90,             // KEY_KATAKANA
        0x79 => 92,             // KEY_HENKAN
        0x7B => 94,             // KEY_MUHENKAN
        0x7D => 124,            // KEY_YEN
        0x7E => 121,            // KEY_KPCOMMA
        _ => KEY_UNKNOWN,
    }
}

/// `sc` is lParam bits 16..23, `ext` is lParam bit 24, `vk` disambiguates 0x45 only.
pub fn to_evdev(sc: u32, ext: bool, vk: u32) -> u32 {
    if ext {
        let e = EXT[(sc & 0x7F) as usize] as u32;
        return if e == 0 { KEY_UNKNOWN } else { e };
    }
    // The one place the scancode is genuinely ambiguous: NumLock and Pause both
    // arrive as 0x45 (Pause's real sequence is E1 1D 45, and the E1 does not reach us).
    if sc == 0x45 { return if vk == crate::sys_win::VK_PAUSE { 119 } else { 69 }; }
    match sc {
        0x01..=0x53 | 0x56..=0x58 => sc,        // identity, see the module doc
        _ => base_high(sc),
    }
}
