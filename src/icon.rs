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

/// Encode one image as a PNG (8-bit RGBA, no interlacing).
///
/// The zlib stream uses STORED deflate blocks — i.e. no compression at all. A
/// real deflate encoder is several hundred lines and a Huffman table, to save
/// perhaps 60 % of a file that is measured in tens of kilobytes and written
/// once, at install time, to the user's home directory. Stored blocks are
/// perfectly legal zlib and every PNG reader accepts them.
pub fn encode_png(img: &IconImage) -> Vec<u8> {
    let side = img.side as usize;

    // Raw scanlines: one filter byte (0 = None) then RGBA, top row first.
    let mut raw = Vec::with_capacity(side * (side * 4 + 1));
    for y in 0..side {
        raw.push(0);
        for x in 0..side {
            let p = img.argb[y * side + x];
            raw.push((p >> 16) as u8);   // R
            raw.push((p >> 8) as u8);    // G
            raw.push(p as u8);           // B
            raw.push((p >> 24) as u8);   // A
        }
    }

    let mut z = vec![0x78, 0x01];   // CMF: deflate, 32 KiB window. FLG: no dict, fastest.
    for (i, block) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(if last { 1 } else { 0 });
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    if raw.is_empty() { z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]); }
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
