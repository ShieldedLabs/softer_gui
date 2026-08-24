//! Icon pixels, and the two container formats the host systems demand of us.
//!
//! Everything here is pure computation over pixel arrays: no syscalls, no
//! platform code. The window backends take [`IconImage`] straight; [`install`]
//! needs PNG for the freedesktop icon theme and ICNS for a macOS bundle, so
//! both encoders live here.
//!
//! Icons are SQUARE. Not a simplification for its own sake: xdg-toplevel-icon-v1
//! rejects a non-square buffer outright, .icns has no non-square chunk type, and
//! nothing anywhere wants a rectangular app icon. Requiring it up front turns a
//! runtime protocol error into an obvious API.

/// One square icon image, borrowed. `argb` is `side * side` pixels in
/// 0xAARRGGBB — the same layout as [`crate::Framebuffer`], so an icon can be
/// drawn with the same code that draws the window.
///
/// Alpha is straight, NOT premultiplied. Backends premultiply where the target
/// wants it (Wayland's ARGB8888 does; _NET_WM_ICON does not).
#[derive(Clone, Copy)]
pub struct IconImage<'a> {
    pub side: u32,
    pub argb: &'a [u32],
}

impl<'a> IconImage<'a> {
    /// None if `argb` is not exactly `side * side`, or `side` is 0.
    pub fn new(side: u32, argb: &'a [u32]) -> Option<IconImage<'a>> {
        if side == 0 || argb.len() != (side as usize) * (side as usize) { return None; }
        Some(IconImage { side, argb })
    }
    pub fn to_owned(&self) -> OwnedIcon { OwnedIcon { side: self.side, argb: self.argb.to_vec() } }
}

/// The same thing, owned — what [`crate::Gui`] keeps so a set outlives the call.
#[derive(Clone)]
pub struct OwnedIcon {
    pub side: u32,
    pub argb: Vec<u32>,
}

impl OwnedIcon {
    pub fn as_image(&self) -> IconImage<'_> { IconImage { side: self.side, argb: &self.argb } }
}

/// Copy a borrowed set, dropping any malformed entry, largest first.
///
/// Largest first because every consumer wants that order: _NET_WM_ICON readers
/// conventionally take the first image that is big enough, and a compositor
/// picking from xdg-toplevel-icon buffers does better downscaling a large one
/// than upscaling a small one.
pub fn own_set(images: &[IconImage]) -> Vec<OwnedIcon> {
    let mut v: Vec<OwnedIcon> = images.iter()
        .filter(|i| i.side > 0 && i.argb.len() == (i.side as usize) * (i.side as usize))
        .map(|i| i.to_owned())
        .collect();
    v.sort_by(|a, b| b.side.cmp(&a.side));
    v
}

// ============================================================================
// PNG
// ============================================================================

fn crc32(data: &[u8]) -> u32 {
    // Built on first use rather than as a const table: 1 KiB of .rodata for a
    // function that runs a handful of times per install is the wrong trade.
    let mut c: u32 = 0xffff_ffff;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 { c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 }; }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    // 5552 is the most bytes that cannot overflow the u32 accumulators before a
    // modulo, which is the standard way to keep this to one division per block.
    for chunk in data.chunks(5552) {
        for &x in chunk { a += x as u32; b += a; }
        a %= 65521; b %= 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A deflate bit sink. Bits go out LSB-first within a byte; Huffman codes are
/// defined MSB-first, so [`Bits::code`] reverses before writing. Getting those
/// two orders the wrong way round is the classic deflate bug.
struct Bits { out: Vec<u8>, acc: u32, n: u32 }

impl Bits {
    fn new() -> Bits { Bits { out: Vec::new(), acc: 0, n: 0 } }
    fn put(&mut self, v: u32, len: u32) {
        self.acc |= v << self.n;
        self.n += len;
        while self.n >= 8 { self.out.push(self.acc as u8); self.acc >>= 8; self.n -= 8; }
    }
    fn code(&mut self, v: u32, len: u32) {
        let mut r = 0u32;
        for i in 0..len { r |= ((v >> i) & 1) << (len - 1 - i); }
        self.put(r, len);
    }
    fn flush(&mut self) { if self.n > 0 { self.out.push(self.acc as u8); self.acc = 0; self.n = 0; } }
}

// RFC 1951 section 3.2.5. Index i is length code 257+i / distance code i.
const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u32; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// The fixed Huffman code for a literal/length symbol (RFC 1951 section 3.2.6).
fn fixed_lit(sym: u32) -> (u32, u32) {
    match sym {
        0..=143 => (0x30 + sym, 8),
        144..=255 => (0x190 + sym - 144, 9),
        256..=279 => (sym - 256, 7),
        _ => (0xc0 + sym - 280, 8),
    }
}

/// Deflate with the FIXED Huffman tables and greedy LZ77 matching.
///
/// Fixed rather than dynamic tables: dynamic would win maybe another 20 % but
/// costs the whole code-length-code encoder for a file written once at install
/// time. The matching is what actually matters here — an icon is mostly flat
/// colour, which becomes a handful of very long matches, and stored blocks made
/// a 1024x1024 .icns entry 4 MiB on its own.
fn deflate(data: &[u8]) -> Vec<u8> {
    const HASH_BITS: usize = 15;
    const WINDOW: usize = 32768;
    let mut head = vec![u32::MAX; 1 << HASH_BITS];
    let mut b = Bits::new();
    b.put(1, 1);   // BFINAL
    b.put(1, 2);   // BTYPE = 01, fixed Huffman

    let h3 = |d: &[u8], i: usize| -> usize {
        // Knuth multiplicative hash over three bytes; collisions only cost a
        // missed match, never correctness, because every candidate is verified.
        let v = (d[i] as u32) << 16 | (d[i + 1] as u32) << 8 | d[i + 2] as u32;
        ((v.wrapping_mul(0x9E37_79B1)) >> (32 - HASH_BITS)) as usize
    };

    let mut i = 0usize;
    while i < data.len() {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if i + 3 <= data.len() {
            let hv = h3(data, i);
            let cand = head[hv];
            head[hv] = i as u32;
            if cand != u32::MAX {
                let c = cand as usize;
                let dist = i - c;
                if dist <= WINDOW && dist > 0 {
                    let max = (data.len() - i).min(258);
                    let mut l = 0usize;
                    while l < max && data[c + l] == data[i + l] { l += 1; }
                    if l >= 3 { best_len = l; best_dist = dist; }
                }
            }
        }
        if best_len >= 3 {
            let li = LEN_BASE.iter().rposition(|&v| v as usize <= best_len).unwrap();
            let (c, n) = fixed_lit(257 + li as u32);
            b.code(c, n);
            b.put((best_len - LEN_BASE[li] as usize) as u32, LEN_EXTRA[li]);
            let di = DIST_BASE.iter().rposition(|&v| v as usize <= best_dist).unwrap();
            b.code(di as u32, 5);   // fixed distance codes are 5 bits, MSB-first
            b.put((best_dist - DIST_BASE[di] as usize) as u32, DIST_EXTRA[di]);
            // Insert the skipped positions so later matches can still find them.
            for k in (i + 1)..(i + best_len).min(data.len().saturating_sub(2)) { head[h3(data, k)] = k as u32; }
            i += best_len;
        } else {
            let (c, n) = fixed_lit(data[i] as u32);
            b.code(c, n);
            i += 1;
        }
    }
    let (c, n) = fixed_lit(256);   // end of block
    b.code(c, n);
    b.flush();
    b.out
}

/// Encode one image as a PNG (8-bit RGBA, no interlacing).
pub fn encode_png(img: &IconImage) -> Vec<u8> {
    let side = img.side as usize;

    // Raw scanlines. Filter 1 (Sub) predicts each byte from the pixel to its
    // left, so a flat run becomes a run of zeros — which is exactly what the
    // LZ77 stage turns into one long match. Filter 0 would leave the colour
    // repeating and cost far more.
    let mut raw = Vec::with_capacity(side * (side * 4 + 1));
    for y in 0..side {
        raw.push(1);
        for x in 0..side {
            let p = img.argb[y * side + x];
            let cur = [(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8];
            let left = if x == 0 { [0u8; 4] } else {
                let q = img.argb[y * side + x - 1];
                [(q >> 16) as u8, (q >> 8) as u8, q as u8, (q >> 24) as u8]
            };
            for k in 0..4 { raw.push(cur[k].wrapping_sub(left[k])); }
        }
    }

    let mut z = vec![0x78, 0x01];   // CMF: deflate, 32 KiB window. FLG: no dict.
    z.extend_from_slice(&deflate(&raw));
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(img.side).to_be_bytes());
    ihdr.extend_from_slice(&(img.side).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);   // 8bpc, RGBA, deflate, adaptive filter, no interlace

    let mut out = Vec::with_capacity(z.len() + 64);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

// ============================================================================
// ICNS
// ============================================================================

/// The .icns chunk type that holds a PNG of this side, or None if macOS has no
/// slot for that size. Sizes outside this set are simply dropped from the
/// bundle rather than rescaled — we do not have a resampler and a bad one would
/// look worse than an absent size.
fn icns_type(side: u32) -> Option<&'static [u8; 4]> {
    Some(match side {
        16 => b"icp4",
        32 => b"icp5",
        64 => b"icp6",
        128 => b"ic07",
        256 => b"ic08",
        512 => b"ic09",
        1024 => b"ic10",
        _ => return None,
    })
}

/// Pack an icon set into a .icns file. Empty if no image had a usable size.
pub fn encode_icns(images: &[OwnedIcon]) -> Vec<u8> {
    let mut body = Vec::new();
    for img in images {
        let Some(ty) = icns_type(img.side) else { continue };
        let png = encode_png(&img.as_image());
        body.extend_from_slice(ty);
        body.extend_from_slice(&((png.len() + 8) as u32).to_be_bytes());
        body.extend_from_slice(&png);
    }
    if body.is_empty() { return Vec::new(); }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

// ============================================================================
// A stand-in icon
// ============================================================================

/// A plain rounded square in `rgb`, so a caller with no art can still exercise
/// the whole path and see something land in the taskbar. Not a logo; a probe.
pub fn placeholder(side: u32, rgb: u32) -> OwnedIcon {
    let n = side as f64;
    let r = n / 5.0;                     // corner radius
    let half = n / 2.0;
    let mut argb = vec![0u32; (side as usize) * (side as usize)];
    for y in 0..side {
        for x in 0..side {
            // Signed distance to a rounded box, evaluated at the pixel centre.
            // The naive version — ramp alpha by how far into the corner BAND a
            // pixel is — fades all four edges, not just the corners, and the
            // halo is glaring at 16px. Only the quadrant where both axes are
            // outside the inset rect is actually curved, which is what taking
            // the length of the clamped-positive offset expresses.
            let qx = (x as f64 + 0.5 - half).abs() - (half - r);
            let qy = (y as f64 + 0.5 - half).abs() - (half - r);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let inside = qx.max(qy).min(0.0);
            let d = outside + inside - r;
            let a = ((0.5 - d).clamp(0.0, 1.0) * 255.0).round() as u32;
            argb[(y * side + x) as usize] = (a << 24) | (rgb & 0x00ff_ffff);
        }
    }
    OwnedIcon { side, argb }
}
