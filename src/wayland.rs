//! Wayland backend: an xdg-shell toplevel over a wl_shm memfd pool, paced by
//! wp_presentation feedback (exact vblank sequence + refresh) or, failing that,
//! wl_surface.frame; keyboard text from the compositor's XKB keymap through our
//! own engine; smooth scroll, pinch and rotate from wl_pointer axes and
//! zwp_pointer_gestures. Client-side frame chrome (three stretched pixels over
//! wp_viewporter) when the compositor won't decorate.
//!
//! Wire format: every message is `object id (u32), size<<16 | opcode (u32)` then
//! 32-bit-word args; strings and arrays are length-prefixed and padded; fds ride
//! in SCM_RIGHTS. Not run on this machine (no compositor) — written from the
//! protocol XML and the brevis backend's proven handshake order.

use std::sync::atomic::{AtomicU32, Ordering::*};
use std::sync::{Arc, Mutex};
use crate::event::*;
use crate::shm::{next_pow2, ShmMem};
use crate::sys::{self, Fd};
use crate::xkb;

const BORDER: u32 = 5;
const TOPBAR: u32 = 22;
const CORNER: u32 = 16;
const MAX_OUTPUTS: usize = 8;

// ---- wire ------------------------------------------------------------------
struct Writer { buf: Vec<u8>, fds: Vec<Fd>, next_id: u32 }

pub struct Conn { fd: Fd, w: Mutex<Writer> }

struct Msg { b: Vec<u8> }
impl Msg {
    fn new(id: u32, opcode: u16) -> Msg {
        let mut b = Vec::with_capacity(32);
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&[(opcode & 0xff) as u8, (opcode >> 8) as u8, 0, 0]);
        Msg { b }
    }
    fn u(mut self, v: u32) -> Msg { self.b.extend_from_slice(&v.to_le_bytes()); self }
    fn i(self, v: i32) -> Msg { self.u(v as u32) }
    fn s(mut self, s: &str) -> Msg {
        let n = s.len() + 1;
        self.b.extend_from_slice(&(n as u32).to_le_bytes());
        self.b.extend_from_slice(s.as_bytes()); self.b.push(0);
        while self.b.len() % 4 != 0 { self.b.push(0); }
        self
    }
    fn finish(mut self) -> Vec<u8> {
        let len = self.b.len() as u32;
        let opcode = u16::from_le_bytes([self.b[4], self.b[5]]) as u32;
        self.b[4..8].copy_from_slice(&(opcode | (len << 16)).to_le_bytes());
        self.b
    }
}

impl Conn {
    fn send(&self, m: Msg) { let mut w = self.w.lock().unwrap(); w.buf.extend_from_slice(&m.finish()); }
    fn send_fd(&self, m: Msg, fd: Fd) { let mut w = self.w.lock().unwrap(); w.buf.extend_from_slice(&m.finish()); w.fds.push(fd); }
    fn new_id(&self) -> u32 { let mut w = self.w.lock().unwrap(); let id = w.next_id; w.next_id += 1; id }
    fn flush(&self) {
        let mut w = self.w.lock().unwrap();
        if w.buf.is_empty() { return; }
        let buf = std::mem::take(&mut w.buf);
        let fds = std::mem::take(&mut w.fds);
        if !sys::send_with_fds(self.fd, &buf, &fds) { eprintln!("softer_gui: wayland write failed"); }
    }
}

/// Reads one event's args in order.
struct Args<'a> { b: &'a [u8], o: usize }
impl<'a> Args<'a> {
    fn u(&mut self) -> u32 {
        let v = if self.o + 4 <= self.b.len() { u32::from_le_bytes([self.b[self.o], self.b[self.o + 1], self.b[self.o + 2], self.b[self.o + 3]]) } else { 0 };
        self.o += 4; v
    }
    fn i(&mut self) -> i32 { self.u() as i32 }
    fn s(&mut self) -> String {
        let n = self.u() as usize;
        let start = self.o.min(self.b.len()); let end = (start + n).min(self.b.len());
        let s = String::from_utf8_lossy(&self.b[start..end]).trim_end_matches('\0').to_string();
        self.o += (n + 3) & !3;
        s
    }
    fn array_u32(&mut self) -> Vec<u32> {
        let n = self.u() as usize;
        let mut v = Vec::new();
        let mut o = self.o;
        while o + 4 <= self.o + n && o + 4 <= self.b.len() { v.push(u32::from_le_bytes([self.b[o], self.b[o + 1], self.b[o + 2], self.b[o + 3]])); o += 4; }
        self.o += (n + 3) & !3;
        v
    }
}

// ---- shared between threads --------------------------------------------------
pub struct Shared {
    conn: Conn,
    surface: AtomicU32,
    shm: AtomicU32,
    presentation: AtomicU32,
    frame_cb: AtomicU32,
    bufs: [AtomicU32; 2],
    feedbacks: Mutex<Vec<u32>>,
}

pub struct App {
    sh: Arc<Shared>,
    core: Arc<Core>,
    side: u32,
    buf_w: u32, buf_h: u32,
    generation: u64,
    cur: usize,
    pool: Option<ShmMem>,
    pool_id: u32,
    wl_buf: [u32; 2],
}

impl App {
    fn destroy_wl_buffers(&mut self) {
        for i in 0..2 { if self.wl_buf[i] != 0 { self.sh.conn.send(Msg::new(self.wl_buf[i], 0)); self.wl_buf[i] = 0; self.sh.bufs[i].store(0, Release); } }
    }
    fn make_wl_buffers(&mut self) {
        // Window-sized views with the square pool's stride: on Wayland the attached buffer's
        // dimensions ARE the surface size, so the buffer must be exactly the window size.
        let bufsz = self.side * self.side * 4;
        for i in 0..2 {
            let id = self.sh.conn.new_id();
            self.sh.conn.send(Msg::new(self.pool_id, 0).u(id).i((i as u32 * bufsz) as i32).i(self.buf_w as i32).i(self.buf_h as i32).i((self.side * 4) as i32).u(1));   // XRGB8888
            self.wl_buf[i] = id;
            self.sh.bufs[i].store(id, Release);
        }
    }
    fn ensure_size(&mut self) {
        let w = self.core.win_w.load(Relaxed).max(1); let h = self.core.win_h.load(Relaxed).max(1);
        let want = next_pow2(w.max(h));
        if want > self.side || self.pool.is_none() {
            self.destroy_wl_buffers();
            if self.pool_id != 0 { self.sh.conn.send(Msg::new(self.pool_id, 1)); self.pool_id = 0; }
            self.pool = None;
            self.side = want;
            let mem = match ShmMem::new((want * want * 4 * 2) as usize) { Some(m) => m, None => return };
            let pool = self.sh.conn.new_id();
            self.sh.conn.send_fd(Msg::new(self.sh.shm.load(Relaxed), 0).u(pool).i((want * want * 4 * 2) as i32), mem.fd);
            self.pool_id = pool;
            self.pool = Some(mem);
            self.generation += 1;
            self.cur = 0;
            self.core.busy[0].store(false, Relaxed); self.core.busy[1].store(false, Relaxed);
            self.buf_w = w; self.buf_h = h;
            self.make_wl_buffers();
            self.core.full_redraw.store(true, Relaxed);
        } else if w != self.buf_w || h != self.buf_h {
            // Only the views change; the generation (and the app's per-key cache) stays put.
            self.destroy_wl_buffers();
            self.buf_w = w; self.buf_h = h;
            self.cur = 0;
            self.core.busy[0].store(false, Relaxed); self.core.busy[1].store(false, Relaxed);
            self.make_wl_buffers();
            self.core.full_redraw.store(true, Relaxed);
        }
    }
    pub fn get_framebuffer(&mut self) -> Option<(*mut u32, u32, u64)> {
        self.ensure_size();
        if self.core.busy[self.cur].load(Acquire) { return None; }
        let mem = self.pool.as_ref()?;
        let off = (self.cur as u32 * self.side * self.side * 4) as usize;
        Some((unsafe { mem.ptr.add(off) } as *mut u32, self.side, (self.generation << 1) | self.cur as u64))
    }
    pub fn submit(&mut self) {
        let sh = &self.sh;
        let buf = self.wl_buf[self.cur];
        if buf == 0 { return; }
        let s = sh.surface.load(Relaxed);
        sh.conn.send(Msg::new(s, 1).u(buf).i(0).i(0));                                             // attach
        sh.conn.send(Msg::new(s, 9).i(0).i(0).i(self.buf_w as i32).i(self.buf_h as i32));           // damage_buffer
        let pres = sh.presentation.load(Relaxed);
        if pres != 0 {
            let fb = sh.conn.new_id();
            sh.conn.send(Msg::new(pres, 1).u(s).u(fb));                                              // feedback
            sh.feedbacks.lock().unwrap().push(fb);
        } else if sh.frame_cb.load(Acquire) == 0 {
            let cb = sh.conn.new_id();
            sh.conn.send(Msg::new(s, 3).u(cb));                                                      // frame
            sh.frame_cb.store(cb, Release);
        }
        sh.conn.send(Msg::new(s, 6));                                                                // commit
        self.core.busy[self.cur].store(true, Release);
        self.core.in_flight.fetch_add(1, AcqRel);
        self.cur ^= 1;
        sh.conn.flush();
    }
    pub fn poke(&self) {
        // A sync round-trip makes the pump's poll return so it re-reads the app's flags.
        let cb = self.sh.conn.new_id();
        self.sh.conn.send(Msg::new(1, 0).u(cb));
        self.sh.conn.flush();
    }
}
impl Drop for App {
    fn drop(&mut self) { self.destroy_wl_buffers(); if self.pool_id != 0 { self.sh.conn.send(Msg::new(self.pool_id, 1)); } self.sh.conn.flush(); }
}

// ---- pump ----------------------------------------------------------------------
struct Pump {
    sh: Arc<Shared>,
    core: Arc<Core>,
    rbuf: Vec<u8>, rfill: usize,
    fds: Vec<Fd>,
    dead: bool,
    // globals
    registry: u32, compositor: u32, subcompositor: u32, viewporter: u32, wm_base: u32, seat: u32,
    // xdg-toplevel-icon-v1: the manager global, plus the live icon object and the
    // buffers backing it. Those must outlive the set_icon call — the protocol
    // raises no_buffer if a buffer dies before the icon it belongs to.
    icon_mgr: u32, icon_obj: u32, icon_bufs: Vec<u32>, icon_mem: Option<ShmMem>,
    deco_mgr: u32, cursor_mgr: u32, gestures: u32, presentation: u32,
    outputs: [(u32, u64); MAX_OUTPUTS], n_out: usize, surf_out: u32,
    seat_version: u32,
    // objects
    surface: u32, frame_surf: u32, xdg_surface: u32, toplevel: u32, deco: u32,
    keyboard: u32, pointer: u32, cursor_dev: u32, pinch: u32,
    frame_pool: u32, frame_bufs: [u32; 3], frame_vp: u32,
    white_surf: u32, white_sub: u32, white_vp: u32, inner_surf: u32, inner_sub: u32, inner_vp: u32,
    content_sub: u32,
    // state
    configured: bool, pending_w: u32, pending_h: u32, pending_fullscreen: bool,
    frame_w: u32, frame_h: u32, ml: u32, mr: u32, mt: u32, mb: u32, have_frame: bool, ssd: bool,
    keymap: xkb::Keymap, mods: u8, group: u32,
    ptr_surf: u32, ptr_x: i32, ptr_y: i32, ptr_enter_serial: u32, ptr_serial: u32, cursor_shape: u32,
    cursor_surf: u32, cursor_mem: Option<ShmMem>, frame_mem: Option<ShmMem>,
    axis_v: i32, axis_h: i32, axis_v120: Option<i32>, axis_h120: Option<i32>, axis_dirty: bool,
    pinch_scale: i32,
    last_seq: u64, first_present: bool,
    cursor_applied: bool, fs_applied: bool,
    sync_done: Option<u32>,
}

impl Pump {
    fn send(&self, m: Msg) { self.sh.conn.send(m); }
    fn new_id(&self) -> u32 { self.sh.conn.new_id() }

    /// Dispatch everything buffered, then read more; with `block`, wait for at least one read.
    fn drain(&mut self, block: bool) -> bool {
        let mut did = false;
        loop {
            while self.rfill >= 8 {
                let id = u32::from_le_bytes([self.rbuf[0], self.rbuf[1], self.rbuf[2], self.rbuf[3]]);
                let word = u32::from_le_bytes([self.rbuf[4], self.rbuf[5], self.rbuf[6], self.rbuf[7]]);
                let len = (word >> 16) as usize; let op = (word & 0xffff) as u16;
                if len < 8 { self.dead = true; return did; }
                if self.rfill < len { if self.rbuf.len() < len { self.rbuf.resize(len, 0); } break; }
                let body = self.rbuf[8..len].to_vec();
                self.rbuf.copy_within(len..self.rfill, 0);
                self.rfill -= len;
                self.dispatch(id, op, &body);
                did = true;
            }
            if self.dead { return did; }
            if self.rbuf.len() - self.rfill < 4096 { let n = self.rbuf.len() * 2; self.rbuf.resize(n, 0); }
            let n = sys::recv_with_fds(self.sh.conn.fd, &mut self.rbuf[self.rfill..], &mut self.fds, !block || did);
            if n == sys::EAGAIN { return did; }
            if n <= 0 { self.dead = true; return did; }
            self.rfill += n as usize;
        }
    }
    /// wl_display.sync and dispatch until its callback returns.
    fn roundtrip(&mut self) -> bool {
        let cb = self.new_id();
        self.send(Msg::new(1, 0).u(cb));
        self.sh.conn.flush();
        self.sync_done = None;
        for _ in 0..10_000 {
            self.drain(true);
            if self.dead { return false; }
            if self.sync_done == Some(cb) { return true; }
        }
        false
    }

    // ---- frame chrome ------------------------------------------------------------
    fn pixel_buffer(&mut self, offset: u32) -> u32 {
        let id = self.new_id();
        self.send(Msg::new(self.frame_pool, 0).u(id).i(offset as i32).i(1).i(1).i(4).u(1));
        id
    }
    fn make_layer(&mut self, empty_region: u32) -> (u32, u32, u32) {
        let surf = self.new_id();
        self.send(Msg::new(self.compositor, 0).u(surf));
        self.send(Msg::new(surf, 5).u(empty_region));                                   // set_input_region: clicks fall through to the frame
        let sub = self.new_id();
        self.send(Msg::new(self.subcompositor, 1).u(sub).u(surf).u(self.frame_surf));
        self.send(Msg::new(sub, 5));                                                    // set_desync
        let vp = self.new_id();
        self.send(Msg::new(self.viewporter, 1).u(vp).u(surf));
        (surf, sub, vp)
    }
    fn make_frame(&mut self) {
        // Three pixels — black, white, black — stretched by viewports: 12 bytes of shared
        // memory for the whole frame, and nothing to repaint on resize.
        let mem = match ShmMem::new(12) { Some(m) => m, None => return };
        unsafe {
            let p = mem.ptr as *mut u32;
            *p = 0xFF000000; *p.add(1) = 0xFFFFFFFF; *p.add(2) = 0xFF000000;
        }
        self.frame_pool = self.new_id();
        self.sh.conn.send_fd(Msg::new(self.sh.shm.load(Relaxed), 0).u(self.frame_pool).i(12), mem.fd);
        self.frame_mem = Some(mem);
        self.frame_bufs = [self.pixel_buffer(0), self.pixel_buffer(4), self.pixel_buffer(8)];
        self.frame_vp = self.new_id();
        self.send(Msg::new(self.viewporter, 1).u(self.frame_vp).u(self.frame_surf));
        let region = self.new_id();
        self.send(Msg::new(self.compositor, 1).u(region));
        // Back-to-front: later subsurfaces stack above earlier ones; the content subsurface comes last.
        let (a, b, c) = self.make_layer(region); self.white_surf = a; self.white_sub = b; self.white_vp = c;
        let (a, b, c) = self.make_layer(region); self.inner_surf = a; self.inner_sub = b; self.inner_vp = c;
    }
    fn paint_layer(&self, surf: u32, vp: u32, buf: u32, w: u32, h: u32) {
        if surf == 0 { return; }
        let (w, h) = (w.max(1), h.max(1));
        self.send(Msg::new(vp, 2).i(w as i32).i(h as i32));             // set_destination
        self.send(Msg::new(surf, 1).u(buf).i(0).i(0));                  // attach
        self.send(Msg::new(surf, 2).i(0).i(0).i(w as i32).i(h as i32)); // damage
        self.send(Msg::new(surf, 6));                                   // commit
    }
    fn place_layer(&self, sub: u32, x: u32, y: u32) { if sub != 0 { self.send(Msg::new(sub, 1).i(x as i32).i(y as i32)); } }
    fn update_frame(&mut self) {
        if !self.have_frame { return; }
        self.send(Msg::new(self.xdg_surface, 3).i(0).i(0).i(self.frame_w as i32).i(self.frame_h as i32));   // set_window_geometry
        let (cw, ch) = (self.core.win_w.load(Relaxed), self.core.win_h.load(Relaxed));
        if self.mt == 0 {
            // Server-side decorated or fullscreen: the bars collapse; the parent still needs a buffer to be mapped.
            self.place_layer(self.white_sub, 0, 0);
            self.paint_layer(self.white_surf, self.white_vp, self.frame_bufs[1], 1, 1);
            self.place_layer(self.inner_sub, 0, 0);
            self.paint_layer(self.inner_surf, self.inner_vp, self.frame_bufs[2], 1, 1);
        } else {
            self.place_layer(self.white_sub, 1, 1);
            self.paint_layer(self.white_surf, self.white_vp, self.frame_bufs[1], self.frame_w.saturating_sub(2), self.frame_h.saturating_sub(2));
            self.place_layer(self.inner_sub, self.ml - 1, self.mt - 1);
            self.paint_layer(self.inner_surf, self.inner_vp, self.frame_bufs[2], cw + 2, ch + 2);
        }
        self.place_layer(self.content_sub, self.ml, self.mt);
        // The parent goes LAST: a subsurface's position is double-buffered state of its parent.
        self.paint_layer(self.frame_surf, self.frame_vp, self.frame_bufs[0], self.frame_w, self.frame_h);
    }
    fn apply_frame_size(&mut self) {
        let cw = if self.frame_w > self.ml + self.mr { self.frame_w - self.ml - self.mr } else { 1 };
        let ch = if self.frame_h > self.mt + self.mb { self.frame_h - self.mt - self.mb } else { 1 };
        if cw != self.core.win_w.load(Relaxed) || ch != self.core.win_h.load(Relaxed) {
            self.core.win_w.store(cw, Relaxed); self.core.win_h.store(ch, Relaxed);
            self.core.full_redraw.store(true, Relaxed);
        }
    }
    fn set_margins(&mut self, bars: bool) {
        if bars && self.have_frame { self.ml = BORDER; self.mr = BORDER; self.mt = TOPBAR; self.mb = BORDER; }
        else { self.ml = 0; self.mr = 0; self.mt = 0; self.mb = 0; }
    }

    // ---- cursor ------------------------------------------------------------------
    fn hit_edges(&self, x: i32, y: i32) -> u32 {
        let (fw, fh) = (self.frame_w as i32, self.frame_h as i32);
        let mut e = 0;
        if y < TOPBAR as i32 {
            if x < CORNER as i32 { e = 1 | 4; } else if x + CORNER as i32 > fw { e = 1 | 8; }
        } else {
            if y + BORDER as i32 > fh { e |= 2; }
            if x < BORDER as i32 { e |= 4; } else if x + BORDER as i32 > fw { e |= 8; }
        }
        e
    }
    fn set_cursor_shape(&mut self, shape: u32) {
        if self.pointer == 0 || shape == self.cursor_shape { return; }
        self.cursor_shape = shape;
        if shape == u32::MAX {
            self.send(Msg::new(self.pointer, 0).u(self.ptr_enter_serial).u(0).i(0).i(0));   // set_cursor(NULL): no cursor
        } else if self.cursor_dev != 0 {
            self.send(Msg::new(self.cursor_dev, 1).u(self.ptr_enter_serial).u(shape));
        } else {
            self.ensure_fallback_cursor();
            self.send(Msg::new(self.pointer, 0).u(self.ptr_enter_serial).u(self.cursor_surf).i(1).i(1));
        }
    }
    /// No cursor-shape protocol: draw a plain arrow into a tiny ARGB shm surface.
    fn ensure_fallback_cursor(&mut self) {
        if self.cursor_surf != 0 { return; }
        const W: usize = 12; const H: usize = 19;
        let mem = match ShmMem::new(W * H * 4) { Some(m) => m, None => return };
        let px = mem.ptr as *mut u32;
        for y in 0..H {
            for x in 0..W {
                let (xi, yi) = (x as i32, y as i32);
                let body = yi < 12 && xi <= yi;                             // the triangle
                let tail = yi >= 12 && yi < H as i32 && xi >= 4 && xi < 8;   // the stem
                let inside = body || tail;
                let edge = inside && (xi == 0 || xi == yi || (yi >= 11 && (xi == 4 || xi == 7)) || yi == H as i32 - 1 || (yi == 11 && xi > 7));
                let c = if !inside { 0 } else if edge { 0xFF000000 } else { 0xFFFFFFFF };
                unsafe { *px.add(y * W + x) = c; }
            }
        }
        let pool = self.new_id();
        self.sh.conn.send_fd(Msg::new(self.sh.shm.load(Relaxed), 0).u(pool).i((W * H * 4) as i32), mem.fd);
        let buf = self.new_id();
        self.send(Msg::new(pool, 0).u(buf).i(0).i(W as i32).i(H as i32).i((W * 4) as i32).u(0));   // ARGB8888
        let surf = self.new_id();
        self.send(Msg::new(self.compositor, 0).u(surf));
        self.send(Msg::new(surf, 1).u(buf).i(0).i(0));
        self.send(Msg::new(surf, 2).i(0).i(0).i(W as i32).i(H as i32));
        self.send(Msg::new(surf, 6));
        self.cursor_surf = surf;
        self.cursor_mem = Some(mem);
    }
    fn update_cursor(&mut self) {
        if self.ptr_surf == 0 { return; }
        if self.core.cursor_hidden.load(Relaxed) { self.set_cursor_shape(u32::MAX); return; }
        if self.ptr_surf != self.frame_surf || self.frame_surf == 0 || self.mt == 0 { self.set_cursor_shape(1); return; }
        let shape = match self.hit_edges(self.ptr_x, self.ptr_y) {
            5 => 21, 9 => 20, 6 => 24, 10 => 23, 1 => 19, 2 => 22, 4 => 25, 8 => 18, _ => 13,
        };
        self.set_cursor_shape(shape);
    }
    fn begin_drag(&mut self) {
        let edges = self.hit_edges(self.ptr_x, self.ptr_y);
        if edges != 0 { self.send(Msg::new(self.toplevel, 6).u(self.seat).u(self.ptr_serial).u(edges)); }
        else { self.send(Msg::new(self.toplevel, 5).u(self.seat).u(self.ptr_serial)); }
        self.sh.conn.flush();
    }
    /// The surface carrying the xdg role — the one whose commit applies
    /// double-buffered toplevel state.
    fn role_surface(&self) -> u32 { if self.have_frame { self.frame_surf } else { self.surface } }

    fn apply_icon(&mut self) {
        let Some(set) = self.core.take_icon() else { return };
        if self.icon_mgr == 0 || self.toplevel == 0 { return; }

        let old_obj = self.icon_obj;
        let old_bufs = std::mem::take(&mut self.icon_bufs);
        let old_mem = self.icon_mem.take();
        self.icon_obj = 0;

        if set.is_empty() {
            // A null icon resets the toplevel to its default — which is the icon
            // from the .desktop file matching our app_id, if there is one.
            self.send(Msg::new(self.icon_mgr, 2).u(self.toplevel).u(0));
        } else {
            // One pool for every size. wl_shm ARGB8888 is PREMULTIPLIED, unlike
            // _NET_WM_ICON, so the pixels are converted on the way in.
            let total: usize = set.iter().map(|i| i.argb.len() * 4).sum();
            if let Some(mem) = ShmMem::new(total) {
                let pool = self.new_id();
                self.sh.conn.send_fd(Msg::new(self.sh.shm.load(Relaxed), 0).u(pool).i(total as i32), mem.fd);
                let icon = self.new_id();
                self.send(Msg::new(self.icon_mgr, 1).u(icon));   // create_icon
                let mut off = 0usize;
                for img in &set {
                    let n = img.argb.len();
                    unsafe {
                        let dst = mem.ptr.add(off) as *mut u32;
                        for (i, &p) in img.argb.iter().enumerate() {
                            let a = p >> 24;
                            let mul = |c: u32| ((c & 0xff) * a + 127) / 255;
                            *dst.add(i) = (a << 24) | (mul(p >> 16) << 16) | (mul(p >> 8) << 8) | mul(p);
                        }
                    }
                    let buf = self.new_id();
                    self.send(Msg::new(pool, 0).u(buf).i(off as i32).i(img.side as i32).i(img.side as i32).i((img.side * 4) as i32).u(0));
                    self.send(Msg::new(icon, 2).u(buf).i(1));   // add_buffer(buffer, scale)
                    self.icon_bufs.push(buf);
                    off += n * 4;
                }
                self.send(Msg::new(self.icon_mgr, 2).u(self.toplevel).u(icon));   // set_icon
                self.send(Msg::new(pool, 1));                                     // pool.destroy
                self.icon_obj = icon;
                self.icon_mem = Some(mem);
            }
        }
        // set_icon is double-buffered on the toplevel: nothing happens until the
        // role surface commits, and an idle app may not commit for a long time.
        self.send(Msg::new(self.role_surface(), 6));
        // Only now is the previous icon unreferenced.
        if old_obj != 0 { self.send(Msg::new(old_obj, 0)); }
        for b in old_bufs { self.send(Msg::new(b, 0)); }
        drop(old_mem);
        self.sh.conn.flush();
    }

    fn apply_fullscreen(&mut self) {
        let want = self.core.wants_fullscreen.load(Relaxed);
        if want == self.fs_applied || self.toplevel == 0 { return; }
        self.fs_applied = want;
        if want { self.send(Msg::new(self.toplevel, 11).u(0)); } else { self.send(Msg::new(self.toplevel, 12)); }
    }
    fn flush_axes(&mut self) {
        if !self.axis_dirty { return; }
        self.axis_dirty = false;
        let mut d = Vec::new();
        // value120 (wheels, exact 1/120 clicks) wins over the continuous value when both arrived.
        let v = match self.axis_v120 { Some(v) => v * SCROLL_STEP / 120, None => self.axis_v };
        let h = match self.axis_h120 { Some(v) => v * SCROLL_STEP / 120, None => self.axis_h };
        if v != 0 { d.push(AxisDiff { axis: AXIS_SCROLL_V, delta: v }); }
        if h != 0 { d.push(AxisDiff { axis: AXIS_SCROLL_H, delta: h }); }
        self.axis_v = 0; self.axis_h = 0; self.axis_v120 = None; self.axis_h120 = None;
        if !d.is_empty() { self.core.push_axes(&d); }
    }
    fn frame_done(&mut self, frames: u64) {
        if self.core.in_flight.load(Acquire) > 0 { self.core.in_flight.fetch_sub(1, AcqRel); }
        self.core.display_tick(frames);
        self.core.push_render();
    }

    // ---- dispatch ------------------------------------------------------------------
    fn dispatch(&mut self, id: u32, op: u16, body: &[u8]) {
        let mut a = Args { b: body, o: 0 };
        let now = sys::clock_monotonic_ns();
        self.core.service_resync();
        if id == 1 {   // wl_display
            if op == 0 { let obj = a.u(); let code = a.u(); let msg = a.s(); eprintln!("softer_gui: wayland error on object {obj} code {code}: {msg}"); self.dead = true; }
            return;   // delete_id: ids are never reused
        }
        if id == self.registry {
            if op == 0 {   // global(name, interface, version)
                let name = a.u(); let iface = a.s(); let version = a.u();
                let bind = |p: &Pump, v: u32| -> u32 {
                    let nid = p.new_id();
                    p.send(Msg::new(p.registry, 0).u(name).s(&iface).u(v).u(nid));
                    nid
                };
                match iface.as_str() {
                    "wl_compositor" => self.compositor = bind(self, version.min(6)),
                    "wl_subcompositor" => self.subcompositor = bind(self, 1),
                    "wl_shm" => { let s = bind(self, 1); self.sh.shm.store(s, Relaxed); }
                    "xdg_wm_base" => self.wm_base = bind(self, version.min(6)),
                    "wl_seat" => { self.seat_version = version.min(9); self.seat = bind(self, self.seat_version); }
                    "zxdg_decoration_manager_v1" => self.deco_mgr = bind(self, 1),
                    "wp_viewporter" => self.viewporter = bind(self, 1),
                    "wp_cursor_shape_manager_v1" => self.cursor_mgr = bind(self, 1),
                    "xdg_toplevel_icon_manager_v1" => self.icon_mgr = bind(self, 1),
                    "zwp_pointer_gestures_v1" => self.gestures = bind(self, version.min(3)),
                    "wp_presentation" => { let p = bind(self, version.min(2)); self.presentation = p; self.sh.presentation.store(p, Relaxed); }
                    "wl_output" => if self.n_out < MAX_OUTPUTS { let o = bind(self, version.min(4)); self.outputs[self.n_out] = (o, 0); self.n_out += 1; },
                    _ => {}
                }
            }
            return;
        }
        // The frame callback
        let frame_cb = self.sh.frame_cb.load(Acquire);
        if op == 0 && frame_cb != 0 && id == frame_cb {
            self.sh.frame_cb.store(0, Release);
            // One committed frame = one refresh period; dropped frames are invisible here.
            if self.presentation == 0 { self.frame_done(1); }
            return;
        }
        // wp_presentation_feedback: exact vblank sequence and refresh, like X11's CompleteNotify.
        {
            let mut fbs = self.sh.feedbacks.lock().unwrap();
            if let Some(pos) = fbs.iter().position(|f| *f == id) {
                match op {
                    0 => return,   // sync_output
                    1 => {   // presented(tv_sec_hi, tv_sec_lo, tv_nsec, refresh, seq_hi, seq_lo, flags)
                        fbs.remove(pos); drop(fbs);
                        a.u(); a.u(); a.u();
                        let refresh = a.u(); let seq = ((a.u() as u64) << 32) | a.u() as u64;
                        if refresh != 0 { self.core.set_period_fs(refresh as u64 * 1_000_000); }
                        let frames = if self.first_present || seq <= self.last_seq { 1 } else { seq - self.last_seq };
                        self.first_present = false; self.last_seq = seq;
                        self.frame_done(frames);
                    }
                    _ => { fbs.remove(pos); drop(fbs); self.frame_done(1); }   // discarded
                }
                return;
            }
        }
        for i in 0..self.n_out {
            if id == self.outputs[i].0 {
                if op == 1 {   // mode(flags, w, h, refresh mHz): the compositor's exact rate
                    let flags = a.u(); a.u(); a.u(); let refresh = a.u();
                    if flags & 1 != 0 && refresh != 0 {
                        let per = 1_000_000_000_000_000_000u64 / refresh as u64;
                        self.outputs[i].1 = per;
                        if self.surf_out == id || self.surf_out == 0 { self.core.set_period_fs(per); }
                    }
                }
                return;
            }
        }
        if id == self.surface || (self.frame_surf != 0 && id == self.frame_surf) {
            match op {
                0 => { let o = a.u(); self.surf_out = o; for i in 0..self.n_out { if self.outputs[i].0 == o && self.outputs[i].1 != 0 { self.core.set_period_fs(self.outputs[i].1); } } }
                1 => { if self.surf_out == a.u() { self.surf_out = 0; } }
                _ => {}
            }
            return;
        }
        if id == self.wm_base { if op == 0 { let serial = a.u(); self.send(Msg::new(self.wm_base, 3).u(serial)); } return; }
        if id == self.deco && self.deco != 0 {
            if op == 0 { let mode = a.u(); self.ssd = mode == 2; }
            return;
        }
        if id == self.xdg_surface {
            if op == 0 {   // configure(serial)
                let serial = a.u();
                self.send(Msg::new(self.xdg_surface, 4).u(serial));   // ack_configure
                self.set_margins(!self.ssd && !self.pending_fullscreen);
                // A zero size means "you choose"; keep ours.
                if self.pending_w != 0 && self.pending_h != 0 { self.frame_w = self.pending_w; self.frame_h = self.pending_h; }
                else { self.frame_w = self.core.win_w.load(Relaxed) + self.ml + self.mr; self.frame_h = self.core.win_h.load(Relaxed) + self.mt + self.mb; }
                self.apply_frame_size();
                self.configured = true;
                self.update_frame();
                self.core.full_redraw.store(true, Relaxed);
                if self.core.in_flight.load(Acquire) == 0 { self.core.push_render(); }
                self.sh.conn.flush();
            }
            return;
        }
        if id == self.toplevel {
            match op {
                0 => { self.pending_w = a.u(); self.pending_h = a.u(); let states = a.array_u32(); self.pending_fullscreen = states.contains(&2); }
                1 => self.core.push_close(),
                _ => {}
            }
            return;
        }
        if id == self.seat {
            if op == 0 {   // capabilities
                let caps = a.u();
                if caps & 1 != 0 && self.pointer == 0 {
                    self.pointer = self.new_id(); self.send(Msg::new(self.seat, 0).u(self.pointer));
                    if self.cursor_mgr != 0 { self.cursor_dev = self.new_id(); self.send(Msg::new(self.cursor_mgr, 1).u(self.cursor_dev).u(self.pointer)); }
                    if self.gestures != 0 { self.pinch = self.new_id(); self.send(Msg::new(self.gestures, 1).u(self.pinch).u(self.pointer)); }
                }
                if caps & 2 != 0 && self.keyboard == 0 { self.keyboard = self.new_id(); self.send(Msg::new(self.seat, 1).u(self.keyboard)); }
            }
            return;
        }
        if id == self.keyboard && self.keyboard != 0 {
            match op {
                0 => {   // keymap(format, fd, size)
                    let format = a.u(); let size = a.u() as usize;
                    let fd = if self.fds.is_empty() { -1 } else { self.fds.remove(0) };
                    if fd >= 0 {
                        if format == 1 {
                            let p = sys::mmap(size, sys::PROT_READ, sys::MAP_PRIVATE, fd, 0);
                            if !p.is_null() {
                                let text = unsafe { core::slice::from_raw_parts(p, size) };
                                let s = String::from_utf8_lossy(text);
                                match xkb::parse_text(&s) { Some(km) => self.keymap = km, None => eprintln!("softer_gui: could not parse the compositor's keymap") }
                                sys::munmap(p, size);
                            }
                        }
                        sys::close(fd);
                    }
                }
                1 => { self.core.focused.store(true, Relaxed); a.u(); a.u(); for k in a.array_u32() { self.core.key(k, true); } }
                2 => { self.core.focused.store(false, Relaxed); self.core.release_all_keys(false); }   // every key is logically up
                3 => {   // key(serial, time, key, state)
                    a.u(); a.u(); let key = a.u(); let state = a.u();
                    if state == 1 {
                        let (sym, text) = self.keymap.text(key + 8, self.mods, self.group);
                        self.core.key_press_sym(key, sym, &text, now);
                    } else { self.core.key(key, false); }
                }
                4 => {   // modifiers(serial, depressed, latched, locked, group)
                    a.u(); let d = a.u(); let l = a.u(); let k = a.u(); let g = a.u();
                    self.mods = ((d | l | k) & 0xff) as u8; self.group = g;
                }
                5 => {   // repeat_info(rate, delay)
                    let rate = a.i(); let delay = a.i();
                    if rate <= 0 { self.core.set_repeat(delay.max(0) as u64, 0); } else { self.core.set_repeat(delay.max(0) as u64, (1000 / rate).max(1) as u64); }
                }
                _ => {}
            }
            return;
        }
        if id == self.pointer && self.pointer != 0 {
            match op {
                0 => {   // enter(serial, surface, x, y)
                    self.ptr_enter_serial = a.u(); self.ptr_surf = a.u(); let x = a.i(); let y = a.i();
                    self.ptr_x = x >> 8; self.ptr_y = y >> 8;
                    self.cursor_shape = 0;   // the compositor reset it; re-send whatever we want
                    if self.ptr_surf == self.surface { self.core.pointer_abs((x as i64) << 24, (y as i64) << 24); }
                    self.update_cursor();
                }
                1 => { self.ptr_surf = 0; self.cursor_shape = 0; }
                2 => {   // motion(time, x, y)
                    a.u(); let x = a.i(); let y = a.i();
                    self.ptr_x = x >> 8; self.ptr_y = y >> 8;
                    if self.ptr_surf == self.surface { self.core.pointer_abs((x as i64) << 24, (y as i64) << 24); }
                    self.update_cursor();
                }
                3 => {   // button(serial, time, button, state)
                    self.ptr_serial = a.u(); a.u(); let button = a.u(); let state = a.u();
                    if self.ptr_surf == self.frame_surf && self.frame_surf != 0 && self.mt != 0 {
                        if button == BTN_LEFT && state == 1 { self.begin_drag(); }
                    } else {
                        self.core.key(button, state == 1);
                    }
                }
                4 => {   // axis(time, axis, value 24.8)
                    a.u(); let axis = a.u(); let v = a.i();
                    if axis == 0 { self.axis_v += v; } else { self.axis_h += v; }
                    self.axis_dirty = true;
                    if self.seat_version < 5 { self.flush_axes(); }
                }
                5 => self.flush_axes(),   // frame
                9 => {   // axis_value120(axis, value120)
                    let axis = a.u(); let v = a.i();
                    if axis == 0 { self.axis_v120 = Some(self.axis_v120.unwrap_or(0) + v); } else { self.axis_h120 = Some(self.axis_h120.unwrap_or(0) + v); }
                    self.axis_dirty = true;
                }
                _ => {}   // axis_source, axis_stop, axis_discrete, axis_relative_direction
            }
            return;
        }
        if id == self.pinch && self.pinch != 0 {
            match op {
                0 => { self.pinch_scale = 1 << 16; }
                1 => {   // update(time, dx, dy, scale 24.8, rotation 24.8 degrees)
                    a.u(); a.i(); a.i(); let scale = a.i(); let rot = a.i();
                    let s16 = scale << 8; let dz = s16 - self.pinch_scale; self.pinch_scale = s16;
                    let mut d = Vec::new();
                    if dz != 0 { d.push(AxisDiff { axis: AXIS_ZOOM, delta: dz }); }
                    if rot != 0 { d.push(AxisDiff { axis: AXIS_ROTATE, delta: rot << 8 }); }
                    if !d.is_empty() { self.core.push_axes(&d); }
                }
                _ => {}
            }
            return;
        }
        // wl_buffer.release
        if op == 0 {
            for i in 0..2 {
                let b = self.sh.bufs[i].load(Acquire);
                if b != 0 && id == b {
                    self.core.busy[i].store(false, Release);
                    if self.core.in_flight.load(Acquire) == 0 { self.core.push_render(); }
                    return;
                }
            }
        }
        // Any other callback.done is a sync round-trip (or a poke) completing.
        if op == 0 && body.len() == 4 { self.sync_done = Some(id); }
    }

    fn run(mut self) {
        let fd = self.sh.conn.fd;
        loop {
            if self.core.quit.load(Relaxed) || self.dead { break; }
            self.core.service_resync();
            self.drain(false);
            if self.dead { break; }
            let now = sys::clock_monotonic_ns();
            let mut timeout = self.core.repeat_tick(now);
            let hidden = self.core.cursor_hidden.load(Relaxed);
            if hidden != self.cursor_applied { self.cursor_applied = hidden; self.update_cursor(); }
            self.apply_icon();
            self.apply_fullscreen();
            if self.core.wants_frame.load(Relaxed) {
                if self.core.in_flight.load(Acquire) == 0 && !self.core.render_pending() && self.configured { self.core.push_render(); }
                timeout = timeout.min(4);
            }
            self.sh.conn.flush();
            let mut pfd = [sys::PollFd { fd, events: sys::POLLIN, revents: 0 }];
            let r = sys::poll(&mut pfd, timeout);
            if r < 0 && r != sys::EINTR { break; }
            if pfd[0].revents & (sys::POLLERR | sys::POLLHUP) != 0 { break; }
        }
        self.core.push_close();
    }
}

fn connect() -> Option<Fd> {
    if let Some(s) = sys::getenv("WAYLAND_SOCKET") { if let Ok(fd) = s.parse::<i32>() { return Some(fd); } }
    let name = sys::getenv("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
    let path = if name.starts_with('/') { name } else { format!("{}/{}", sys::getenv("XDG_RUNTIME_DIR")?, name) };
    let fd = sys::socket_unix();
    if fd < 0 { return None; }
    if !sys::connect_unix(fd, path.as_bytes()) { sys::close(fd); return None; }
    Some(fd)
}

pub fn open(core: Arc<Core>, title: &str, app_id: &str, width: u32, height: u32) -> Option<App> {
    let fd = connect()?;
    let sh = Arc::new(Shared {
        conn: Conn { fd, w: Mutex::new(Writer { buf: Vec::new(), fds: Vec::new(), next_id: 2 }) },
        surface: AtomicU32::new(0), shm: AtomicU32::new(0), presentation: AtomicU32::new(0),
        frame_cb: AtomicU32::new(0), bufs: [AtomicU32::new(0), AtomicU32::new(0)],
        feedbacks: Mutex::new(Vec::new()),
    });
    let mut p = Pump {
        sh: sh.clone(), core: core.clone(), rbuf: vec![0; 1 << 16], rfill: 0, fds: Vec::new(), dead: false,
        registry: 0, compositor: 0, subcompositor: 0, viewporter: 0, wm_base: 0, seat: 0, deco_mgr: 0, cursor_mgr: 0, gestures: 0, presentation: 0,
        icon_mgr: 0, icon_obj: 0, icon_bufs: Vec::new(), icon_mem: None,
        outputs: [(0, 0); MAX_OUTPUTS], n_out: 0, surf_out: 0, seat_version: 0,
        surface: 0, frame_surf: 0, xdg_surface: 0, toplevel: 0, deco: 0, keyboard: 0, pointer: 0, cursor_dev: 0, pinch: 0,
        frame_pool: 0, frame_bufs: [0; 3], frame_vp: 0, white_surf: 0, white_sub: 0, white_vp: 0, inner_surf: 0, inner_sub: 0, inner_vp: 0, content_sub: 0,
        configured: false, pending_w: 0, pending_h: 0, pending_fullscreen: false,
        frame_w: 0, frame_h: 0, ml: 0, mr: 0, mt: 0, mb: 0, have_frame: false, ssd: false,
        keymap: xkb::Keymap::default(), mods: 0, group: 0,
        ptr_surf: 0, ptr_x: 0, ptr_y: 0, ptr_enter_serial: 0, ptr_serial: 0, cursor_shape: 0, cursor_surf: 0, cursor_mem: None, frame_mem: None,
        axis_v: 0, axis_h: 0, axis_v120: None, axis_h120: None, axis_dirty: false, pinch_scale: 1 << 16,
        last_seq: 0, first_present: true, cursor_applied: false, fs_applied: false, sync_done: None,
    };
    // wl_display.get_registry, one round-trip for the globals, a second for what binding them triggers
    // (seat capabilities, output modes).
    p.registry = p.new_id();
    p.send(Msg::new(1, 1).u(p.registry));
    if !p.roundtrip() { sys::close(fd); return None; }
    if p.compositor == 0 || sh.shm.load(Relaxed) == 0 || p.wm_base == 0 {
        eprintln!("softer_gui: wayland compositor lacks wl_compositor/wl_shm/xdg_wm_base");
        sys::close(fd);
        return None;
    }
    if !p.roundtrip() { sys::close(fd); return None; }

    core.win_w.store(width, Relaxed); core.win_h.store(height, Relaxed);
    p.have_frame = p.subcompositor != 0 && p.viewporter != 0;

    // Content surface always; the frame surface takes the xdg role when we can build chrome.
    p.surface = p.new_id();
    p.send(Msg::new(p.compositor, 0).u(p.surface));
    sh.surface.store(p.surface, Relaxed);
    let role = if p.have_frame { p.frame_surf = p.new_id(); p.send(Msg::new(p.compositor, 0).u(p.frame_surf)); p.frame_surf } else { p.surface };
    p.xdg_surface = p.new_id();
    p.send(Msg::new(p.wm_base, 2).u(p.xdg_surface).u(role));
    p.toplevel = p.new_id();
    p.send(Msg::new(p.xdg_surface, 1).u(p.toplevel));
    p.send(Msg::new(p.toplevel, 2).s(title));
    // app_id is how a Wayland compositor finds our .desktop entry, and therefore
    // our icon when xdg-toplevel-icon-v1 is absent. It must equal the desktop
    // file basename.
    p.send(Msg::new(p.toplevel, 3).s(app_id));
    // Ask for server-side decorations; a compositor that answers client_side (or has no
    // manager at all — Mutter) gets our bevel instead.
    if p.deco_mgr != 0 {
        p.deco = p.new_id();
        p.send(Msg::new(p.deco_mgr, 1).u(p.deco).u(p.toplevel));
        p.send(Msg::new(p.deco, 1).u(2));
    }
    p.set_margins(true);
    p.frame_w = width + p.ml + p.mr; p.frame_h = height + p.mt + p.mb;
    if p.have_frame {
        p.make_frame();
        p.content_sub = p.new_id();
        p.send(Msg::new(p.subcompositor, 1).u(p.content_sub).u(p.surface).u(p.frame_surf));
        p.send(Msg::new(p.content_sub, 5));   // desync: per-frame content commits stand alone
        p.send(Msg::new(p.content_sub, 1).i(p.ml as i32).i(p.mt as i32));
    }
    // The initial commit carries no buffer; the first configure must be acked before anything is attached.
    p.send(Msg::new(role, 6));
    p.sh.conn.flush();
    for _ in 0..100 { if p.configured || !p.roundtrip() { break; } }
    if !p.configured { eprintln!("softer_gui: wayland surface never configured"); return None; }

    std::thread::Builder::new().name("softer_gui-wl-pump".into()).spawn(move || p.run()).ok()?;
    Some(App { sh, core, side: 0, buf_w: 0, buf_h: 0, generation: 0, cur: 0, pool: None, pool_id: 0, wl_buf: [0; 2] })
}
