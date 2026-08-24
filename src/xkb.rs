//! A minimal XKB keymap engine: keycode + (real modifier mask, group) -> keysym.
//! Fed either from the XKB text keymap a Wayland compositor hands over, or from
//! the X server's XkbGetMap reply. Key types, symbols and virtual-modifier
//! resolution only — no actions, indicators, geometry or compose; modifier and
//! group STATE is computed by the server on both platforms and arrives with each
//! key event, so nothing here has to model latching or locking.

use crate::keysym;

pub const SHIFT: u8 = 1;
pub const LOCK: u8 = 2;
pub const CONTROL: u8 = 4;
pub const MOD1: u8 = 8;
pub const MOD5: u8 = 128;

#[derive(Clone, Debug, Default)]
pub struct KeyType {
    pub name: String,
    /// Real modifiers this type looks at.
    pub mask: u8,
    /// (real mask, level, active). Matched against `mods & mask`.
    pub entries: Vec<(u8, u8, bool)>,
    pub num_levels: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Key {
    pub groups: u8,
    pub width: u8,
    pub types: [u16; 4],
    /// syms[group * width + level]; 0 = NoSymbol.
    pub syms: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Keymap {
    pub types: Vec<KeyType>,
    /// Indexed by keycode (X keycode = evdev + 8).
    pub keys: Vec<Key>,
}

impl Keymap {
    pub fn keysym(&self, keycode: u32, mods: u8, group: u32) -> u32 {
        let Some(k) = self.keys.get(keycode as usize) else { return 0 };
        if k.groups == 0 || k.width == 0 { return 0; }
        let g = (group % k.groups as u32) as usize;
        let level = match self.types.get(k.types[g.min(3)] as usize) {
            Some(t) => {
                let active = mods & t.mask;
                t.entries.iter().find(|e| e.2 && e.0 == active).map(|e| e.1 as usize).unwrap_or(0)
            }
            None => 0,
        };
        if level >= k.width as usize { return 0; }
        k.syms.get(g * k.width as usize + level).copied().unwrap_or(0)
    }
    /// Layout text for a key press: the keysym's character, or "" for non-text keys.
    pub fn text(&self, keycode: u32, mods: u8, group: u32) -> (u32, String) {
        let sym = self.keysym(keycode, mods, group);
        let mut s = String::new();
        // Control+key yields no text (Ctrl-C is a shortcut); the caller checks Ctrl anyway.
        if let Some(c) = keysym::to_char(sym) { if (c as u32) >= 0x20 && c as u32 != 0x7f { s.push(c); } }
        (sym, s)
    }
}

// ============================================================================
// XKB text keymap (xkb_keymap { xkb_keycodes, xkb_types, xkb_compatibility, xkb_symbols })
// ============================================================================

#[derive(Clone, Debug, PartialEq)]
enum Tok { Id(String), KeyName(String), Str(String), Num(u32), P(char) }

fn tokenize(src: &str) -> Vec<Tok> {
    let b = src.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 4);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() { i += 1; continue; }
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' { while i < b.len() && b[i] != b'\n' { i += 1; } continue; }
        if c == b'#' { while i < b.len() && b[i] != b'\n' { i += 1; } continue; }
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') { i += 1; }
            i += 2; continue;
        }
        if c == b'"' {
            let s = i + 1; i += 1;
            while i < b.len() && b[i] != b'"' { if b[i] == b'\\' { i += 1; } i += 1; }
            out.push(Tok::Str(String::from_utf8_lossy(&b[s..i.min(b.len())]).into_owned()));
            i += 1; continue;
        }
        if c == b'<' {
            let s = i + 1; i += 1;
            while i < b.len() && b[i] != b'>' { i += 1; }
            out.push(Tok::KeyName(String::from_utf8_lossy(&b[s..i.min(b.len())]).into_owned()));
            i += 1; continue;
        }
        if c.is_ascii_digit() {
            let s = i;
            if c == b'0' && i + 1 < b.len() && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                i += 2;
                while i < b.len() && b[i].is_ascii_hexdigit() { i += 1; }
                out.push(Tok::Num(u32::from_str_radix(&src[s + 2..i], 16).unwrap_or(0)));
            } else {
                while i < b.len() && b[i].is_ascii_digit() { i += 1; }
                out.push(Tok::Num(src[s..i].parse().unwrap_or(0)));
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') { i += 1; }
            out.push(Tok::Id(src[s..i].to_string()));
            continue;
        }
        out.push(Tok::P(c as char));
        i += 1;
    }
    out
}

struct ModSpec { real: u8, vmods: u32 }

struct TextKeymap {
    keycodes: Vec<(String, u32)>,
    aliases: Vec<(String, String)>,
    vmod_names: Vec<String>,
    vmod_explicit: Vec<Option<u8>>,   // from `virtual_modifiers X=Mod5`
    vmod_real: Vec<u8>,               // resolved
    types: Vec<(String, ModSpec, Vec<(ModSpec, u8)>, u8)>,   // name, modifiers, map entries (mods, level), num levels
    interprets: Vec<(u32, u32)>,      // keysym -> vmod index
    keys: Vec<TextKey>,
    modmap: Vec<(u8, String)>,        // real mod, key name
}

#[derive(Default, Clone)]
struct TextKey { name: String, types: [Option<String>; 4], syms: [Vec<u32>; 4], groups: u8, vmods: u32 }

struct Parser<'a> { t: &'a [Tok], i: usize }
impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> { self.t.get(self.i) }
    fn next(&mut self) -> Option<Tok> { let r = self.t.get(self.i).cloned(); self.i += 1; r }
    fn is_p(&self, c: char) -> bool { matches!(self.peek(), Some(Tok::P(x)) if *x == c) }
    fn is_id(&self, s: &str) -> bool { matches!(self.peek(), Some(Tok::Id(x)) if x == s) }
    fn expect_p(&mut self, c: char) -> bool { if self.is_p(c) { self.i += 1; true } else { false } }
    /// Skip to just past the matching `;` of the current statement, honoring braces.
    fn skip_stmt(&mut self) {
        let mut depth = 0i32;
        while let Some(t) = self.next() {
            match t {
                Tok::P('{') => depth += 1,
                Tok::P('}') => { depth -= 1; if depth < 0 { self.i -= 1; return; } }
                Tok::P(';') if depth <= 0 => return,
                _ => {}
            }
        }
    }
    fn skip_block(&mut self) {   // at '{' ... matching '}'
        if !self.expect_p('{') { return; }
        let mut depth = 1;
        while let Some(t) = self.next() {
            match t { Tok::P('{') => depth += 1, Tok::P('}') => { depth -= 1; if depth == 0 { return; } } _ => {} }
        }
    }
}

impl TextKeymap {
    fn vmod_index(&mut self, name: &str) -> u32 {
        if let Some(i) = self.vmod_names.iter().position(|n| n == name) { return i as u32; }
        self.vmod_names.push(name.to_string());
        self.vmod_explicit.push(None);
        (self.vmod_names.len() - 1) as u32
    }
    fn real_mod(name: &str) -> Option<u8> {
        Some(match name {
            "Shift" | "shift" => SHIFT, "Lock" | "lock" => LOCK, "Control" | "control" | "Ctrl" => CONTROL,
            "Mod1" | "mod1" => 8, "Mod2" | "mod2" => 16, "Mod3" | "mod3" => 32, "Mod4" | "mod4" => 64, "Mod5" | "mod5" => 128,
            "none" | "None" => 0,
            _ => return None,
        })
    }
    /// `A+B+C` of real and virtual modifier names, consuming tokens until something that isn't `+`.
    fn parse_mods(&mut self, p: &mut Parser) -> ModSpec {
        let mut m = ModSpec { real: 0, vmods: 0 };
        loop {
            match p.next() {
                Some(Tok::Id(n)) => {
                    if let Some(r) = Self::real_mod(&n) { m.real |= r; }
                    else if n == "all" { m.real |= 0xff; }
                    else { m.vmods |= 1 << self.vmod_index(&n); }
                }
                _ => { p.i -= 1; break; }
            }
            if !p.expect_p('+') { break; }
        }
        m
    }
    fn resolve(&self, m: &ModSpec) -> (u8, bool) {
        let mut r = m.real;
        let mut unresolved_only = m.real == 0 && m.vmods != 0;
        for i in 0..self.vmod_names.len() {
            if m.vmods & (1 << i) != 0 {
                let rm = self.vmod_real.get(i).copied().unwrap_or(0);
                r |= rm;
                if rm != 0 { unresolved_only = false; }
            }
        }
        (r, !unresolved_only)
    }
    fn parse_vmods_decl(&mut self, p: &mut Parser) {
        // virtual_modifiers A, B=Mod5, C;
        loop {
            match p.next() {
                Some(Tok::Id(n)) => {
                    let idx = self.vmod_index(&n) as usize;
                    if p.expect_p('=') {
                        let m = self.parse_mods(p);
                        self.vmod_explicit[idx] = Some(m.real);
                    }
                }
                Some(Tok::P(',')) => {}
                _ => break,
            }
        }
    }
    fn parse_level(p: &mut Parser) -> u8 {
        match p.next() {
            Some(Tok::Id(n)) => n.strip_prefix("Level").and_then(|x| x.parse::<u8>().ok()).map(|x| x.saturating_sub(1)).unwrap_or(0),
            Some(Tok::Num(n)) => (n as u8).saturating_sub(1),
            _ => 0,
        }
    }
    fn parse_types(&mut self, p: &mut Parser) {
        if !p.expect_p('{') { return; }
        while !p.is_p('}') && p.peek().is_some() {
            if p.is_id("virtual_modifiers") { p.i += 1; self.parse_vmods_decl(p); continue; }
            if p.is_id("type") {
                p.i += 1;
                let name = match p.next() { Some(Tok::Str(s)) => s, _ => String::new() };
                let mut mods = ModSpec { real: 0, vmods: 0 };
                let mut entries = Vec::new();
                let mut levels = 1u8;
                if p.expect_p('{') {
                    while !p.is_p('}') && p.peek().is_some() {
                        if p.is_id("modifiers") { p.i += 1; p.expect_p('='); mods = self.parse_mods(p); p.skip_stmt(); }
                        else if p.is_id("map") {
                            p.i += 1; p.expect_p('[');
                            let m = self.parse_mods(p);
                            p.expect_p(']'); p.expect_p('=');
                            let l = Self::parse_level(p);
                            levels = levels.max(l + 1);
                            entries.push((m, l));
                            p.skip_stmt();
                        }
                        else if p.is_id("level_name") {
                            p.i += 1; p.expect_p('[');
                            let l = Self::parse_level(p);
                            levels = levels.max(l + 1);
                            p.skip_stmt();
                        }
                        else { p.skip_stmt(); }
                    }
                    p.expect_p('}');
                }
                p.skip_stmt();
                self.types.push((name, mods, entries, levels));
                continue;
            }
            p.skip_stmt();
        }
        p.expect_p('}');
    }
    fn parse_compat(&mut self, p: &mut Parser) {
        if !p.expect_p('{') { return; }
        while !p.is_p('}') && p.peek().is_some() {
            if p.is_id("virtual_modifiers") { p.i += 1; self.parse_vmods_decl(p); continue; }
            if p.is_id("interpret") {
                p.i += 1;
                if p.is_p('.') { p.skip_stmt(); continue; }   // interpret.useModMapMods= ...
                let sym = match p.next() { Some(Tok::Id(n)) => keysym::from_name(&n).unwrap_or(0), Some(Tok::Num(n)) => n, _ => 0 };
                // optional +Pred(mods)
                if p.expect_p('+') { p.next(); if p.expect_p('(') { while !p.is_p(')') && p.peek().is_some() { p.i += 1; } p.expect_p(')'); } }
                let mut vmod: Option<u32> = None;
                if p.expect_p('{') {
                    while !p.is_p('}') && p.peek().is_some() {
                        if p.is_id("virtualModifier") || p.is_id("virtualmodifier") {
                            p.i += 1; p.expect_p('=');
                            if let Some(Tok::Id(n)) = p.next() { vmod = Some(self.vmod_index(&n)); }
                            p.skip_stmt();
                        } else { p.skip_stmt(); }
                    }
                    p.expect_p('}');
                }
                p.skip_stmt();
                if let (Some(v), true) = (vmod, sym != 0) { self.interprets.push((sym, v)); }
                continue;
            }
            p.skip_stmt();
        }
        p.expect_p('}');
    }
    fn parse_symlist(p: &mut Parser) -> Vec<u32> {
        // [ a, A, { x, y }, NoSymbol ]
        let mut v = Vec::new();
        if !p.expect_p('[') { return v; }
        while !p.is_p(']') && p.peek().is_some() {
            match p.next() {
                Some(Tok::Id(n)) => v.push(if n == "NoSymbol" { 0 } else { keysym::from_name(&n).unwrap_or(0) }),
                // A bare decimal digit in a symbol list is the keysym NAMED that digit ("2" = 0x32);
                // hex literals and larger decimals are raw keysym values.
                Some(Tok::Num(n)) => v.push(if n < 10 { 0x30 + n } else { n }),
                Some(Tok::P('{')) => {
                    let mut first = None;
                    while !p.is_p('}') && p.peek().is_some() {
                        match p.next() {
                            Some(Tok::Id(n)) if first.is_none() => first = Some(keysym::from_name(&n).unwrap_or(0)),
                            Some(Tok::Num(n)) if first.is_none() => first = Some(if n < 10 { 0x30 + n } else { n }),
                            _ => {}
                        }
                    }
                    p.expect_p('}');
                    v.push(first.unwrap_or(0));
                }
                Some(Tok::P(',')) => {}
                _ => {}
            }
        }
        p.expect_p(']');
        v
    }
    fn group_index(p: &mut Parser) -> usize {
        // [Group1] / [1]
        let mut g = 0;
        if p.expect_p('[') {
            match p.next() {
                Some(Tok::Id(n)) => g = n.strip_prefix("Group").and_then(|x| x.parse::<usize>().ok()).unwrap_or(1).saturating_sub(1),
                Some(Tok::Num(n)) => g = (n as usize).saturating_sub(1),
                _ => {}
            }
            p.expect_p(']');
        }
        g.min(3)
    }
    fn parse_symbols(&mut self, p: &mut Parser) {
        if !p.expect_p('{') { return; }
        while !p.is_p('}') && p.peek().is_some() {
            if p.is_id("key") {
                p.i += 1;
                let name = match p.next() { Some(Tok::KeyName(n)) => n, _ => { p.skip_stmt(); continue; } };
                let mut k = TextKey { name, ..Default::default() };
                if p.expect_p('{') {
                    while !p.is_p('}') && p.peek().is_some() {
                        if p.is_p('[') {
                            k.syms[0] = Self::parse_symlist(p);
                        } else if p.is_id("type") {
                            p.i += 1;
                            let g = if p.is_p('[') { Some(Self::group_index(p)) } else { None };
                            p.expect_p('=');
                            if let Some(Tok::Str(s)) = p.next() {
                                match g { Some(g) => k.types[g] = Some(s), None => { for t in k.types.iter_mut() { *t = Some(s.clone()); } } }
                            }
                        } else if p.is_id("symbols") {
                            p.i += 1;
                            let g = Self::group_index(p);
                            p.expect_p('=');
                            k.syms[g] = Self::parse_symlist(p);
                        } else if p.is_id("vmods") || p.is_id("virtualMods") || p.is_id("virtualmods") {
                            p.i += 1; p.expect_p('=');
                            let m = self.parse_mods(p);
                            k.vmods |= m.vmods;
                        } else if p.is_id("actions") {
                            p.i += 1;
                            Self::group_index(p); p.expect_p('=');
                            if p.expect_p('[') { let mut d = 1; while d > 0 { match p.next() { Some(Tok::P('[')) => d += 1, Some(Tok::P(']')) => d -= 1, None => break, _ => {} } } }
                        } else {
                            // repeat= Yes, locking=..., etc.
                            p.i += 1;
                            while !p.is_p(',') && !p.is_p('}') && p.peek().is_some() { p.i += 1; }
                        }
                        p.expect_p(',');
                    }
                    p.expect_p('}');
                }
                p.skip_stmt();
                k.groups = (0..4).rev().find(|&g| !k.syms[g].is_empty()).map(|g| g as u8 + 1).unwrap_or(0);
                self.keys.push(k);
                continue;
            }
            if p.is_id("modifier_map") {
                p.i += 1;
                let real = match p.next() { Some(Tok::Id(n)) => Self::real_mod(&n).unwrap_or(0), _ => 0 };
                if p.expect_p('{') {
                    while !p.is_p('}') && p.peek().is_some() {
                        match p.next() {
                            Some(Tok::KeyName(n)) => self.modmap.push((real, n)),
                            Some(Tok::Id(n)) => {   // a keysym instead of a key name
                                if let Some(sym) = keysym::from_name(&n) {
                                    let names: Vec<String> = self.keys.iter().filter(|k| k.syms[0].first() == Some(&sym)).map(|k| k.name.clone()).collect();
                                    for nm in names { self.modmap.push((real, nm)); }
                                }
                            }
                            _ => {}
                        }
                    }
                    p.expect_p('}');
                }
                p.skip_stmt();
                continue;
            }
            p.skip_stmt();
        }
        p.expect_p('}');
    }
    fn parse_keycodes(&mut self, p: &mut Parser) {
        if !p.expect_p('{') { return; }
        while !p.is_p('}') && p.peek().is_some() {
            match p.peek().cloned() {
                Some(Tok::KeyName(n)) => {
                    p.i += 1;
                    if p.expect_p('=') { if let Some(Tok::Num(c)) = p.next() { self.keycodes.push((n, c)); } }
                    p.skip_stmt();
                }
                Some(Tok::Id(ref s)) if s == "alias" => {
                    p.i += 1;
                    if let Some(Tok::KeyName(a)) = p.next() {
                        if p.expect_p('=') { if let Some(Tok::KeyName(b)) = p.next() { self.aliases.push((a, b)); } }
                    }
                    p.skip_stmt();
                }
                _ => p.skip_stmt(),
            }
        }
        p.expect_p('}');
    }
    fn keycode_of(&self, name: &str) -> Option<u32> {
        if let Some(k) = self.keycodes.iter().find(|k| k.0 == name) { return Some(k.1); }
        let target = self.aliases.iter().find(|a| a.0 == name)?;
        self.keycodes.iter().find(|k| k.0 == target.1).map(|k| k.1)
    }
    fn resolve_vmods(&mut self) {
        let n = self.vmod_names.len();
        self.vmod_real = vec![0u8; n];
        for i in 0..n { if let Some(r) = self.vmod_explicit[i] { self.vmod_real[i] = r; } }
        // Bind a vmod to the real modifier of every key in modifier_map whose keysyms an
        // `interpret` names it for, or that declares it in vmods= directly.
        for (real, keyname) in &self.modmap {
            let Some(key) = self.keys.iter().find(|k| &k.name == keyname) else { continue };
            for i in 0..n { if key.vmods & (1 << i) != 0 { self.vmod_real[i] |= real; } }
            for g in 0..4 {
                for sym in &key.syms[g] {
                    for (isym, v) in &self.interprets {
                        if isym == sym && self.vmod_explicit[*v as usize].is_none() { self.vmod_real[*v as usize] |= real; }
                    }
                }
            }
        }
    }
    fn automatic_type(syms: &[u32]) -> &'static str {
        let alpha = |a: u32, b: u32| keysym::is_case_pair(a, b);
        match syms.len() {
            0 | 1 => "ONE_LEVEL",
            2 => if alpha(syms[0], syms[1]) { "ALPHABETIC" } else if keysym::is_keypad(syms[0]) || keysym::is_keypad(syms[1]) { "KEYPAD" } else { "TWO_LEVEL" },
            3 | 4 => {
                let s2 = syms.get(2).copied().unwrap_or(0); let s3 = syms.get(3).copied().unwrap_or(0);
                if alpha(syms[0], syms[1]) { if alpha(s2, s3) { "FOUR_LEVEL_ALPHABETIC" } else { "FOUR_LEVEL_SEMIALPHABETIC" } }
                else if keysym::is_keypad(syms[0]) || keysym::is_keypad(syms[1]) { "FOUR_LEVEL_KEYPAD" }
                else { "FOUR_LEVEL" }
            }
            _ => "FOUR_LEVEL",   // wider keys keep the four-level mapping; extra levels are unreachable
        }
    }
    fn build(mut self) -> Keymap {
        self.resolve_vmods();
        let mut km = Keymap::default();
        for (name, mods, entries, levels) in &self.types {
            let (mask, _) = self.resolve(mods);
            let mut t = KeyType { name: name.clone(), mask, entries: Vec::new(), num_levels: *levels };
            for (m, l) in entries {
                let (em, ok) = self.resolve(m);
                t.entries.push((em & mask, *l, ok));
            }
            km.types.push(t);
        }
        let type_index = |km: &Keymap, name: &str| km.types.iter().position(|t| t.name == name).map(|i| i as u16);
        km.keys.resize(256, Key::default());
        for k in &self.keys {
            let Some(code) = self.keycode_of(&k.name) else { continue };
            if code >= 256 || k.groups == 0 { continue; }
            let width = (0..k.groups as usize).map(|g| k.syms[g].len()).max().unwrap_or(0).min(255);
            let mut key = Key { groups: k.groups, width: width as u8, types: [0; 4], syms: vec![0; width * k.groups as usize] };
            for g in 0..k.groups as usize {
                let syms = if k.syms[g].is_empty() { &k.syms[0] } else { &k.syms[g] };
                for (l, s) in syms.iter().enumerate() { key.syms[g * width + l] = *s; }
                let tname: String = match &k.types[g] { Some(t) => t.clone(), None => Self::automatic_type(syms).to_string() };
                key.types[g] = type_index(&km, &tname).or_else(|| type_index(&km, "ONE_LEVEL")).unwrap_or(0);
            }
            km.keys[code as usize] = key;
        }
        km
    }
}

/// Parse an XKB_V1 text keymap. Tolerant: unknown statements are skipped.
pub fn parse_text(src: &str) -> Option<Keymap> {
    let toks = tokenize(src);
    let mut p = Parser { t: &toks, i: 0 };
    let mut km = TextKeymap { keycodes: Vec::new(), aliases: Vec::new(), vmod_names: Vec::new(), vmod_explicit: Vec::new(), vmod_real: Vec::new(),
                              types: Vec::new(), interprets: Vec::new(), keys: Vec::new(), modmap: Vec::new() };
    // xkb_keymap { section "name" { ... }; ... };
    let mut found = false;
    while let Some(t) = p.next() {
        if let Tok::Id(s) = t {
            match s.as_str() {
                "xkb_keymap" => { p.expect_p('{'); }
                "xkb_keycodes" => { while !p.is_p('{') && p.peek().is_some() { p.i += 1; } km.parse_keycodes(&mut p); p.skip_stmt(); found = true; }
                "xkb_types" => { while !p.is_p('{') && p.peek().is_some() { p.i += 1; } km.parse_types(&mut p); p.skip_stmt(); }
                "xkb_compatibility" | "xkb_compat" => { while !p.is_p('{') && p.peek().is_some() { p.i += 1; } km.parse_compat(&mut p); p.skip_stmt(); }
                "xkb_symbols" => { while !p.is_p('{') && p.peek().is_some() { p.i += 1; } km.parse_symbols(&mut p); p.skip_stmt(); }
                "xkb_geometry" => { while !p.is_p('{') && p.peek().is_some() { p.i += 1; } p.skip_block(); p.skip_stmt(); }
                _ => {}
            }
        }
    }
    if !found { return None; }
    Some(km.build())
}

// ============================================================================
// XkbGetMap reply (X11 wire). Types' map entries already carry EFFECTIVE real masks.
// ============================================================================
pub fn parse_x11_getmap(r: &[u8]) -> Option<Keymap> {
    if r.len() < 40 { return None; }
    let rd16 = |o: usize| u16::from_le_bytes([r[o], r[o + 1]]);
    let rd32 = |o: usize| u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
    let present = rd16(12);
    let first_type = r[14] as usize; let n_types = r[15] as usize; let total_types = r[16] as usize;
    let first_sym = r[17] as usize; let n_syms = r[20] as usize;
    let mut o = 40usize;
    let mut km = Keymap::default();
    km.types.resize(total_types.max(first_type + n_types), KeyType::default());
    if present & 1 != 0 {
        for ti in 0..n_types {
            if o + 8 > r.len() { return None; }
            let mask = r[o]; let n_levels = r[o + 4]; let n_entries = r[o + 5] as usize; let preserve = r[o + 6] != 0;
            o += 8;
            let mut t = KeyType { name: String::new(), mask, entries: Vec::new(), num_levels: n_levels };
            for _ in 0..n_entries {
                if o + 8 > r.len() { return None; }
                let active = r[o] != 0; let emask = r[o + 1]; let level = r[o + 2];
                t.entries.push((emask & mask, level, active));
                o += 8;
            }
            if preserve { o += 4 * n_entries; }
            km.types[first_type + ti] = t;
        }
    }
    km.keys.resize(256, Key::default());
    if present & 2 != 0 {
        for ki in 0..n_syms {
            if o + 8 > r.len() { return None; }
            let kt = [r[o] as u16, r[o + 1] as u16, r[o + 2] as u16, r[o + 3] as u16];
            let group_info = r[o + 4]; let width = r[o + 5]; let nsyms = rd16(o + 6) as usize;
            o += 8;
            let groups = group_info & 0x0f;
            let mut key = Key { groups, width, types: kt, syms: Vec::with_capacity(nsyms) };
            for _ in 0..nsyms { if o + 4 > r.len() { return None; } key.syms.push(rd32(o)); o += 4; }
            let code = first_sym + ki;
            if code < 256 { km.keys[code] = key; }
        }
    }
    Some(km)
}
