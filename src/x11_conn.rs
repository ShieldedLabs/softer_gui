//! X11 wire protocol: socket, .Xauthority, setup, request encoding, packet reading.
//! Little-endian host only (the setup announces our byte order; x86_64/aarch64 both LE).

use std::sync::Mutex;
use crate::sys::{self, Fd};

#[allow(dead_code)]
pub struct Setup {
    pub root: u32,
    pub root_visual: u32,
    pub root_depth: u8,
    pub black: u32,
    pub width: u16,
    pub height: u16,
    pub min_keycode: u8,
    pub max_keycode: u8,
    pub id_base: u32,
    pub id_mask: u32,
}

pub struct Writer {
    pub buf: Vec<u8>,
    /// Requests issued so far; the server numbers replies with the low 16 bits.
    pub seq: u64,
    pub next_id: u32,
    id_base: u32,
    id_mask: u32,
}

pub struct Conn {
    pub fd: Fd,
    pub w: Mutex<Writer>,
    pub setup: Setup,
}

fn le16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
fn le32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
pub fn rd16(b: &[u8], o: usize) -> u16 { le16(b, o) }
pub fn rd32(b: &[u8], o: usize) -> u32 { le32(b, o) }
pub fn rd64(b: &[u8], o: usize) -> u64 { le32(b, o) as u64 | ((le32(b, o + 4) as u64) << 32) }
pub fn pad4(n: usize) -> usize { (n + 3) & !3 }

fn read_file(path: &str) -> Option<Vec<u8>> { std::fs::read(path).ok() }

/// The MIT-MAGIC-COOKIE-1 for display `num`, from $XAUTHORITY / ~/.Xauthority.
fn find_cookie(num: u32) -> Option<Vec<u8>> {
    let path = sys::getenv("XAUTHORITY").or_else(|| sys::getenv("HOME").map(|h| h + "/.Xauthority"))?;
    let data = read_file(&path)?;
    let host = read_file("/proc/sys/kernel/hostname").map(|mut h| { while h.last().map(|c| *c == b'\n').unwrap_or(false) { h.pop(); } h });
    let mut best: Option<(u32, Vec<u8>)> = None;   // (score, cookie)
    let mut i = 0usize;
    let rd = |i: &mut usize| -> Option<Vec<u8>> {
        if *i + 2 > data.len() { return None; }
        let n = u16::from_be_bytes([data[*i], data[*i + 1]]) as usize;
        *i += 2;
        if *i + n > data.len() { return None; }
        let v = data[*i..*i + n].to_vec();
        *i += n;
        Some(v)
    };
    while i + 2 <= data.len() {
        let family = u16::from_be_bytes([data[i], data[i + 1]]);
        i += 2;
        let addr = rd(&mut i)?; let number = rd(&mut i)?; let name = rd(&mut i)?; let cookie = rd(&mut i)?;
        if name != b"MIT-MAGIC-COOKIE-1" { continue; }
        let num_ok = number.is_empty() || std::str::from_utf8(&number).ok().and_then(|s| s.parse::<u32>().ok()) == Some(num);
        if !num_ok { continue; }
        let mut score = match family { 256 => 2, 65535 => 1, _ => 0 };
        if score == 0 { continue; }
        if let Some(h) = &host { if &addr == h { score += 2; } }
        if best.as_ref().map(|b| score > b.0).unwrap_or(true) { best = Some((score, cookie)); }
    }
    best.map(|b| b.1)
}

impl Conn {
    /// Connect to $DISPLAY (":N[.S]"), perform setup. None with a message on failure.
    pub fn open() -> Option<Conn> {
        let disp = sys::getenv("DISPLAY").unwrap_or_else(|| ":0".into());
        let after = disp.rsplit(':').next().unwrap_or("0");
        let num: u32 = after.split('.').next().unwrap_or("0").parse().unwrap_or(0);
        let fd = sys::socket_unix();
        if fd < 0 { eprintln!("softer_gui: socket() failed"); return None; }
        let path = format!("/tmp/.X11-unix/X{num}");
        let mut abstract_path = vec![0u8];
        abstract_path.extend_from_slice(path.as_bytes());
        if !sys::connect_unix(fd, &abstract_path) && !sys::connect_unix(fd, path.as_bytes()) {
            eprintln!("softer_gui: cannot connect to X display {disp}");
            sys::close(fd);
            return None;
        }
        let cookie = find_cookie(num).unwrap_or_default();
        let name: &[u8] = if cookie.is_empty() { b"" } else { b"MIT-MAGIC-COOKIE-1" };
        let mut req = vec![0x6c, 0, 11, 0, 0, 0, name.len() as u8, 0, cookie.len() as u8, 0, 0, 0];
        req.extend_from_slice(name); req.resize(pad4(req.len()), 0);
        req.extend_from_slice(&cookie); req.resize(pad4(req.len()), 0);
        if !sys::write_all(fd, &req) { sys::close(fd); return None; }
        let mut head = [0u8; 8];
        if !read_exact(fd, &mut head) { sys::close(fd); return None; }
        let extra = le16(&head, 6) as usize * 4;
        let mut body = vec![0u8; extra];
        if !read_exact(fd, &mut body) { sys::close(fd); return None; }
        if head[0] != 1 {
            let n = head[1] as usize;
            eprintln!("softer_gui: X setup refused: {}", String::from_utf8_lossy(&body[..n.min(body.len())]));
            sys::close(fd);
            return None;
        }
        // body: release(4) id_base(4) id_mask(4) motion(4) vendor_len(2) max_req(2) roots(1) formats(1)
        //       image_order(1) bitmap_order(1) scanline_unit(1) scanline_pad(1) min_kc(1) max_kc(1) pad(4)
        let id_base = le32(&body, 4); let id_mask = le32(&body, 8);
        let vendor_len = le16(&body, 16) as usize; let nformats = body[21] as usize;
        let min_keycode = body[26]; let max_keycode = body[27];
        let mut o = 32 + pad4(vendor_len) + 8 * nformats;
        if o + 40 > body.len() { eprintln!("softer_gui: short X setup"); sys::close(fd); return None; }
        let setup = Setup {
            root: le32(&body, o), black: le32(&body, o + 12), width: le16(&body, o + 20), height: le16(&body, o + 22),
            root_visual: le32(&body, o + 32), root_depth: body[o + 38], min_keycode, max_keycode, id_base, id_mask,
        };
        o += 40;
        let _ = o;
        Some(Conn { fd, w: Mutex::new(Writer { buf: Vec::with_capacity(4096), seq: 0, next_id: 0, id_base, id_mask }), setup })
    }

    /// Append a request. `body` is everything after the 4-byte header; padded to 4 here.
    /// Returns its sequence number.
    pub fn req(&self, major: u8, minor: u8, body: &[u8]) -> u64 {
        let mut w = self.w.lock().unwrap();
        w.push(major, minor, body)
    }
    pub fn flush(&self) {
        let mut w = self.w.lock().unwrap();
        if !w.buf.is_empty() {
            let ok = sys::write_all(self.fd, &w.buf);
            w.buf.clear();
            if !ok { eprintln!("softer_gui: X connection write failed"); }
        }
    }
    /// A request whose fd travels with it (MIT-SHM AttachFd). Flushes what came before,
    /// then sends this one in a single sendmsg so the server pops the fd in order.
    pub fn req_with_fd(&self, major: u8, minor: u8, body: &[u8], fd: Fd) -> u64 {
        let mut w = self.w.lock().unwrap();
        if !w.buf.is_empty() { let ok = sys::write_all(self.fd, &w.buf); w.buf.clear(); if !ok { eprintln!("softer_gui: X write failed"); } }
        let seq = w.push(major, minor, body);
        let pkt = std::mem::take(&mut w.buf);
        if !sys::send_with_fds(self.fd, &pkt, &[fd]) { eprintln!("softer_gui: X sendmsg failed"); }
        seq
    }
    pub fn new_id(&self) -> u32 { self.w.lock().unwrap().new_id() }
}

impl Writer {
    pub fn push(&mut self, major: u8, minor: u8, body: &[u8]) -> u64 {
        let len = 4 + pad4(body.len());
        self.buf.push(major); self.buf.push(minor);
        self.buf.extend_from_slice(&((len / 4) as u16).to_le_bytes());
        self.buf.extend_from_slice(body);
        self.buf.resize(self.buf.len() + pad4(body.len()) - body.len(), 0);
        self.seq += 1;
        self.seq
    }
    pub fn new_id(&mut self) -> u32 {
        // Walk the mask's set bits: ids are base | (n shifted into the mask).
        let n = self.next_id; self.next_id += 1;
        let mut id = 0u32; let mut bit = 0; let mut nn = n;
        while nn != 0 && bit < 32 {
            if self.id_mask & (1 << bit) != 0 { id |= (nn & 1) << bit; nn >>= 1; }
            bit += 1;
        }
        self.id_base | id
    }
}

pub fn read_exact(fd: Fd, buf: &mut [u8]) -> bool {
    let mut got = 0;
    while got < buf.len() {
        let n = sys::read(fd, &mut buf[got..]);
        if n == sys::EINTR { continue; }
        if n <= 0 { return false; }
        got += n as usize;
    }
    true
}

/// Reader side: only the pump thread. Buffers socket bytes and cuts them into packets.
pub struct Reader {
    fd: Fd,
    buf: Vec<u8>,
    fill: usize,
    /// Events read while waiting for a reply, handled afterwards in order.
    pub queued: std::collections::VecDeque<Vec<u8>>,
    pub dead: bool,
}

impl Reader {
    pub fn new(fd: Fd) -> Reader { Reader { fd, buf: vec![0; 1 << 16], fill: 0, queued: Default::default(), dead: false } }

    fn packet_len(&self) -> Option<usize> {
        if self.fill < 32 { return None; }
        let b = &self.buf[..self.fill];
        let ty = b[0] & 0x7f;
        let extra = if ty == 1 || ty == 35 { le32(b, 4) as usize * 4 } else { 0 };
        Some(32 + extra)
    }
    fn fill_some(&mut self, nonblock: bool) -> bool {
        if self.fill == self.buf.len() { self.buf.resize(self.buf.len() * 2, 0); }
        let mut fds = Vec::new();
        let n = sys::recv_with_fds(self.fd, &mut self.buf[self.fill..], &mut fds, nonblock);
        if n == sys::EAGAIN { return false; }
        if n <= 0 { self.dead = true; return false; }
        self.fill += n as usize;
        true
    }
    /// Next complete packet. With `nonblock`, returns None the moment the socket has
    /// nothing more — the caller must loop this to None BEFORE polling the fd, or a
    /// packet already buffered here would sit unhandled through the whole poll timeout.
    pub fn next_packet(&mut self, nonblock: bool) -> Option<Vec<u8>> {
        if let Some(p) = self.queued.pop_front() { return Some(p); }
        self.socket_packet(nonblock)
    }
    /// Straight from the socket, never from `queued` (wait_reply fills that and must not re-read it).
    fn socket_packet(&mut self, nonblock: bool) -> Option<Vec<u8>> {
        loop {
            if self.dead { return None; }
            if let Some(len) = self.packet_len() {
                if self.fill >= len {
                    let p = self.buf[..len].to_vec();
                    self.buf.copy_within(len..self.fill, 0);
                    self.fill -= len;
                    return Some(p);
                }
                if self.buf.len() < len { self.buf.resize(len, 0); }
            }
            if !self.fill_some(nonblock) { return None; }
        }
    }
    pub fn has_buffered(&self) -> bool { !self.queued.is_empty() || self.packet_len().map(|l| self.fill >= l).unwrap_or(false) }

    /// Block until the reply for `seq` arrives; events seen meanwhile are queued.
    /// Returns None on an X error for that request (printed) or a dead connection.
    pub fn wait_reply(&mut self, seq: u64) -> Option<Vec<u8>> {
        loop {
            let p = self.socket_packet(false)?;
            let ty = p[0] & 0x7f;
            if ty == 1 && le16(&p, 2) == (seq & 0xffff) as u16 { return Some(p); }
            if ty == 0 {
                if le16(&p, 2) == (seq & 0xffff) as u16 { eprintln!("softer_gui: X error {} for request major {} minor {}", p[1], p[10], le16(&p, 8)); return None; }
                self.queued.push_back(p);
                continue;
            }
            if ty == 1 { continue; }   // a reply nobody waits for
            self.queued.push_back(p);
        }
    }
}
