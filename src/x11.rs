//! X11 backend: a window presented from memfd-backed shared pixmaps via the
//! Present extension, paced by PresentCompleteNotify's msc (a real vblank
//! counter), with the refresh period queried exactly from RandR. Keyboard text
//! comes from the server's XKB map through our own engine; pointer, smooth
//! scroll and touchpad gestures from XInput 2.
//!
//! Threads: the pump thread owns the socket's READ side and all protocol state
//! here; the app thread only writes requests (get_framebuffer/submit) through
//! the connection's mutex-guarded writer.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};
use crate::event::*;
use crate::shm::{next_pow2, ShmMem};
use crate::sys;
use crate::x11_conn::{rd16, rd32, rd64, Conn, Reader};
use crate::xkb;

// Core opcodes.
const CREATE_WINDOW: u8 = 1;
const MAP_WINDOW: u8 = 8;
const INTERN_ATOM: u8 = 16;
const CHANGE_PROPERTY: u8 = 18;
const SEND_EVENT: u8 = 25;
const FREE_PIXMAP: u8 = 54;
const QUERY_EXTENSION: u8 = 98;

// Atoms we intern once.
struct Atoms { wm_protocols: u32, wm_delete: u32, net_wm_sync: u32, net_wm_sync_counter: u32, net_wm_state: u32, net_wm_fullscreen: u32, net_wm_name: u32, utf8_string: u32, net_wm_pid: u32 }

struct Ext { present: u8, shm: u8, sync: u8, xfixes: u8, randr: u8, rr_event: u8, xkb: u8, xkb_event: u8, xi: u8 }

/// State both threads reach: the connection and the pixmap ids the pump matches idle notifies against.
pub struct Shared {
    pub conn: Conn,
    win: u32,
    ext: Ext,
    sync_counter: u32,
    sync_want: AtomicU64,
    sync_done: AtomicU64,
    pixmap: [AtomicU32; 2],
    present_serial: AtomicU32,
}

struct Buf { mem: ShmMem, seg: u32, pixmap: u32 }

/// App-thread side: the swapchain.
pub struct App {
    sh: Arc<Shared>,
    core: Arc<Core>,
    side: u32,
    generation: u64,
    cur: usize,
    bufs: [Option<Buf>; 2],
}

fn u16b(v: u16) -> [u8; 2] { v.to_le_bytes() }
fn u32b(v: u32) -> [u8; 4] { v.to_le_bytes() }

fn query_extension(conn: &Conn, r: &mut Reader, name: &[u8]) -> Option<(u8, u8, u8)> {
    let mut b = Vec::new();
    b.extend_from_slice(&u16b(name.len() as u16)); b.extend_from_slice(&[0, 0]); b.extend_from_slice(name);
    let seq = conn.req(QUERY_EXTENSION, 0, &b);
    conn.flush();
    let rep = r.wait_reply(seq)?;
    if rep[8] == 0 { return None; }
    Some((rep[9], rep[10], rep[11]))
}
fn intern_atom(conn: &Conn, r: &mut Reader, name: &[u8]) -> u32 {
    let mut b = Vec::new();
    b.extend_from_slice(&u16b(name.len() as u16)); b.extend_from_slice(&[0, 0]); b.extend_from_slice(name);
    let seq = conn.req(INTERN_ATOM, 0, &b);
    conn.flush();
    r.wait_reply(seq).map(|p| rd32(&p, 8)).unwrap_or(0)
}
fn change_property(conn: &Conn, win: u32, prop: u32, ty: u32, format: u8, data: &[u8]) {
    let mut b = Vec::new();
    b.extend_from_slice(&u32b(win)); b.extend_from_slice(&u32b(prop)); b.extend_from_slice(&u32b(ty));
    b.push(format); b.extend_from_slice(&[0, 0, 0]);
    b.extend_from_slice(&u32b((data.len() / (format as usize / 8)) as u32));
    b.extend_from_slice(data);
    conn.req(CHANGE_PROPERTY, 0, &b);   // mode 0 = replace
}

impl App {
    fn make_buf(&mut self, i: usize) {
        let side = self.side as usize;
        let mem = match ShmMem::new(side * side * 4) { Some(m) => m, None => return };
        let sh = &self.sh;
        let seg = sh.conn.new_id();
        let pixmap = sh.conn.new_id();
        // ShmAttachFd(shmseg, fd, read_only) — the fd rides along in the same sendmsg.
        let mut b = Vec::new();
        b.extend_from_slice(&u32b(seg)); b.extend_from_slice(&[0, 0, 0, 0]);
        sh.conn.req_with_fd(sh.ext.shm, 6, &b, mem.fd);
        // ShmCreatePixmap(pid, drawable, w, h, depth, shmseg, offset)
        let mut b = Vec::new();
        b.extend_from_slice(&u32b(pixmap)); b.extend_from_slice(&u32b(sh.win));
        b.extend_from_slice(&u16b(side as u16)); b.extend_from_slice(&u16b(side as u16));
        b.push(sh.conn.setup.root_depth); b.extend_from_slice(&[0, 0, 0]);
        b.extend_from_slice(&u32b(seg)); b.extend_from_slice(&u32b(0));
        sh.conn.req(sh.ext.shm, 5, &b);
        sh.conn.flush();
        sh.pixmap[i].store(pixmap, Release);
        self.bufs[i] = Some(Buf { mem, seg, pixmap });
    }
    fn free_buf(&mut self, i: usize) {
        if let Some(b) = self.bufs[i].take() {
            let sh = &self.sh;
            sh.conn.req(FREE_PIXMAP, 0, &u32b(b.pixmap));
            sh.conn.req(sh.ext.shm, 2, &u32b(b.seg));   // ShmDetach
            sh.conn.flush();
            sh.pixmap[i].store(0, Release);
            drop(b.mem);
        }
    }
    /// Settle a pending resize (grow only), then hand out the free back buffer.
    pub fn get_framebuffer(&mut self) -> Option<(*mut u32, u32, u64)> {
        let w = self.core.win_w.load(Relaxed); let h = self.core.win_h.load(Relaxed);
        let want = next_pow2(w.max(h));
        if want > self.side || self.bufs[0].is_none() {
            self.free_buf(0); self.free_buf(1);
            self.side = want;
            self.generation += 1;
            self.cur = 0;
            self.core.busy[0].store(false, Relaxed); self.core.busy[1].store(false, Relaxed);
            self.make_buf(0); self.make_buf(1);
            self.core.full_redraw.store(true, Relaxed);
        }
        if self.core.busy[self.cur].load(Acquire) { return None; }
        let b = self.bufs[self.cur].as_ref()?;
        Some((b.mem.ptr as *mut u32, self.side, (self.generation << 1) | self.cur as u64))
    }
    pub fn submit(&mut self) {
        let sh = &self.sh;
        let Some(b) = self.bufs[self.cur].as_ref() else { return };
        let serial = sh.present_serial.fetch_add(1, Relaxed);
        // PresentPixmap: everything optional zeroed — no fences, no target msc, next vblank.
        let mut r = Vec::with_capacity(72);
        r.extend_from_slice(&u32b(sh.win)); r.extend_from_slice(&u32b(b.pixmap)); r.extend_from_slice(&u32b(serial));
        r.extend_from_slice(&[0u8; 8]);     // valid, update regions
        r.extend_from_slice(&[0u8; 4]);     // x_off, y_off
        r.extend_from_slice(&[0u8; 16]);    // target_crtc, wait_fence, idle_fence, options
        r.extend_from_slice(&[0u8; 4]);     // pad
        r.extend_from_slice(&[0u8; 24]);    // target_msc, divisor, remainder
        sh.conn.req(sh.ext.present, 1, &r);
        self.core.busy[self.cur].store(true, Release);
        self.core.in_flight.fetch_add(1, AcqRel);
        self.cur ^= 1;
        // _NET_WM_SYNC_REQUEST: the frame for the WM's last requested size is now
        // queued, so release its brake. Acking one frame early is only ever as bad
        // as not implementing the protocol at all.
        let want = sh.sync_want.load(Acquire);
        if sh.sync_counter != 0 && want != sh.sync_done.load(Relaxed) {
            let mut b = Vec::new();
            b.extend_from_slice(&u32b(sh.sync_counter));
            b.extend_from_slice(&u32b((want >> 32) as u32)); b.extend_from_slice(&u32b(want as u32));
            sh.conn.req(sh.ext.sync, 3, &b);
            sh.sync_done.store(want, Relaxed);
        }
        sh.conn.flush();
    }
}
impl Drop for App {
    fn drop(&mut self) { self.free_buf(0); self.free_buf(1); }
}

// ============================================================================
// Pump thread
// ============================================================================

struct ScrollDev { id: u16, v_num: u16, h_num: u16, v_inc: f64, h_inc: f64, v_last: Option<f64>, h_last: Option<f64> }

struct Pump {
    sh: Arc<Shared>,
    core: Arc<Core>,
    r: Reader,
    atoms: Atoms,
    keymap: xkb::Keymap,
    last_msc: u64,
    first_complete: bool,
    win_root_x: i32, win_root_y: i32,
    crtc_rect: (i32, i32, u32, u32),
    cursor_applied: bool,
    fs_applied: bool,
    scroll: Vec<ScrollDev>,
    pinch_scale: i32,
    quit_seen: bool,
    debug: bool,
}

impl Pump {
    fn round_trip(&mut self, seq: u64) -> Option<Vec<u8>> { self.sh.conn.flush(); self.r.wait_reply(seq) }

    // ---- XKB ---------------------------------------------------------------------
    fn load_keymap(&mut self) {
        let x = self.sh.ext.xkb;
        if x == 0 { return; }
        // XkbGetMap(deviceSpec=UseCoreKbd, full = KeyTypes|KeySyms)
        let mut b = vec![0u8; 24];
        b[0..2].copy_from_slice(&u16b(0x100)); b[2..4].copy_from_slice(&u16b(1 | 2));
        let seq = self.sh.conn.req(x, 8, &b);
        if let Some(rep) = self.round_trip(seq) {
            if let Some(km) = xkb::parse_x11_getmap(&rep) { self.keymap = km; }
        }
    }

    // ---- RandR: the exact refresh period of the CRTC under the window centre ----------
    fn query_period(&mut self) {
        let rr = self.sh.ext.randr;
        if rr == 0 { return; }
        let seq = self.sh.conn.req(rr, 25, &u32b(self.sh.conn.setup.root));   // GetScreenResourcesCurrent
        let Some(res) = self.round_trip(seq) else { return };
        let ncrtc = rd16(&res, 16) as usize; let nout = rd16(&res, 18) as usize; let nmode = rd16(&res, 20) as usize;
        let cfg_ts = rd32(&res, 12);
        let crtcs: Vec<u32> = (0..ncrtc).map(|i| rd32(&res, 32 + 4 * i)).collect();
        let modes_off = 32 + 4 * ncrtc + 4 * nout;
        let cx = self.win_root_x + self.core.win_w.load(Relaxed) as i32 / 2;
        let cy = self.win_root_y + self.core.win_h.load(Relaxed) as i32 / 2;
        let mut chosen: Option<(u32, (i32, i32, u32, u32))> = None;   // mode id, rect
        let mut first: Option<(u32, (i32, i32, u32, u32))> = None;
        for c in crtcs {
            let mut b = Vec::new(); b.extend_from_slice(&u32b(c)); b.extend_from_slice(&u32b(cfg_ts));
            let seq = self.sh.conn.req(rr, 20, &b);   // GetCrtcInfo
            let Some(ci) = self.round_trip(seq) else { continue };
            let x = rd16(&ci, 12) as i16 as i32; let y = rd16(&ci, 14) as i16 as i32;
            let w = rd16(&ci, 16) as u32; let h = rd16(&ci, 18) as u32; let mode = rd32(&ci, 20);
            if mode == 0 || w == 0 { continue; }
            if first.is_none() { first = Some((mode, (x, y, w, h))); }
            if cx >= x && cy >= y && cx < x + w as i32 && cy < y + h as i32 { chosen = Some((mode, (x, y, w, h))); break; }
        }
        let Some((mode, rect)) = chosen.or(first) else { return };
        for i in 0..nmode {
            let o = modes_off + 32 * i;
            if o + 32 > res.len() { break; }
            if rd32(&res, o) == mode {
                let dot = rd32(&res, o + 8) as u64; let ht = rd16(&res, o + 16) as u64; let vt = rd16(&res, o + 24) as u64;
                if dot != 0 && ht != 0 && vt != 0 {
                    // 1e15 * ht * vt / dot in two stages so nothing overflows 64 bits.
                    let period = (1_000_000_000_000_000u64 / dot) * ht * vt + ((1_000_000_000_000_000u64 % dot) * ht * vt) / dot;
                    self.core.set_period_fs(period);
                }
                break;
            }
        }
        self.crtc_rect = rect;
    }
    fn maybe_requery_period(&mut self) {
        let (x, y, w, h) = self.crtc_rect;
        let cx = self.win_root_x + self.core.win_w.load(Relaxed) as i32 / 2;
        let cy = self.win_root_y + self.core.win_h.load(Relaxed) as i32 / 2;
        if w == 0 || cx < x || cy < y || cx >= x + w as i32 || cy >= y + h as i32 { self.query_period(); }
    }

    // ---- XInput 2: which valuators are scroll axes, per source device --------------
    fn query_devices(&mut self) {
        let xi = self.sh.ext.xi;
        if xi == 0 { return; }
        let seq = self.sh.conn.req(xi, 48, &[0, 0, 0, 0]);   // XIQueryDevice(XIAllDevices)
        let Some(rep) = self.round_trip(seq) else { return };
        self.scroll.clear();
        let n = rd16(&rep, 8) as usize;
        let mut o = 32usize;
        for _ in 0..n {
            if o + 12 > rep.len() { break; }
            let id = rd16(&rep, o); let nclasses = rd16(&rep, o + 6) as usize; let name_len = rd16(&rep, o + 8) as usize;
            o += 12 + ((name_len + 3) & !3);
            let mut dev = ScrollDev { id, v_num: u16::MAX, h_num: u16::MAX, v_inc: 0.0, h_inc: 0.0, v_last: None, h_last: None };
            for _ in 0..nclasses {
                if o + 4 > rep.len() { break; }
                let ty = rd16(&rep, o); let len = rd16(&rep, o + 2) as usize * 4;
                if ty == 3 && o + 24 <= rep.len() {   // ScrollClass
                    let number = rd16(&rep, o + 6); let stype = rd16(&rep, o + 8);
                    let inc = rd32(&rep, o + 16) as i32 as f64 + rd32(&rep, o + 20) as f64 / 4294967296.0;
                    if stype == 1 { dev.v_num = number; dev.v_inc = inc; } else if stype == 2 { dev.h_num = number; dev.h_inc = inc; }
                }
                o += len;
            }
            if dev.v_num != u16::MAX || dev.h_num != u16::MAX { self.scroll.push(dev); }
        }
    }

    // ---- outward state the app sets -----------------------------------------------
    fn apply_cursor(&mut self) {
        let want = self.core.cursor_hidden.load(Relaxed);
        if want == self.cursor_applied || self.sh.ext.xfixes == 0 { return; }
        self.cursor_applied = want;
        self.sh.conn.req(self.sh.ext.xfixes, if want { 29 } else { 30 }, &u32b(self.sh.win));
        self.sh.conn.flush();
    }
    fn apply_fullscreen(&mut self) {
        let want = self.core.wants_fullscreen.load(Relaxed);
        if want == self.fs_applied { return; }
        self.fs_applied = want;
        // _NET_WM_STATE client message to the root: {action, atom, 0, source=1}
        let mut ev = vec![33u8, 32, 0, 0];
        ev.extend_from_slice(&u32b(self.sh.win)); ev.extend_from_slice(&u32b(self.atoms.net_wm_state));
        ev.extend_from_slice(&u32b(want as u32)); ev.extend_from_slice(&u32b(self.atoms.net_wm_fullscreen));
        ev.extend_from_slice(&u32b(0)); ev.extend_from_slice(&u32b(1)); ev.extend_from_slice(&u32b(0));
        let mut b = vec![0u8; 0];
        b.extend_from_slice(&u32b(self.sh.conn.setup.root)); b.extend_from_slice(&u32b((1 << 20) | (1 << 19)));
        b.extend_from_slice(&ev);
        self.sh.conn.req(SEND_EVENT, 0, &b);
        self.sh.conn.flush();
    }

    // ---- event handling ------------------------------------------------------------
    fn handle(&mut self, p: &[u8], now: u64) {
        let ty = p[0] & 0x7f;
        let synthetic = p[0] & 0x80 != 0;
        if self.debug {
            let ms = (sys::clock_monotonic_ns() / 1_000_000) % 100_000;
            if ty == 35 && p[1] == self.sh.ext.present {
                let et = rd16(p, 8);
                if et == 1 { eprintln!("[{ms}] present complete mode {} msc {} in_flight {}", p[11], rd64(p, 32), self.core.in_flight.load(Relaxed)); }
                else if et == 2 { eprintln!("[{ms}] present idle pixmap 0x{:x}", rd32(p, 24)); }
                else { eprintln!("[{ms}] present configure {}x{}", rd16(p, 24), rd16(p, 26)); }
            } else if ty != 35 { eprintln!("[{ms}] x11 event type {ty} detail {} {}", p[1], if synthetic { "(synthetic)" } else { "" }); }
        }
        let ext = &self.sh.ext;
        match ty {
            0 => eprintln!("softer_gui: X error code {} major {} minor {}", p[1], p[10], rd16(p, 8)),
            2 | 3 => {   // KeyPress / KeyRelease
                let keycode = p[1] as u32;
                let code = keycode.saturating_sub(8);
                if ty == 3 { self.core.key(code, false); return; }
                // With detectable autorepeat the server still sends a fresh KeyPress per
                // repeat tick; a press whose bit is already set IS that tick. We repeat ourselves.
                if self.core.key_down(code) { return; }
                let state = rd16(p, 28);
                let (sym, text) = self.keymap.text(keycode, (state & 0xff) as u8, ((state >> 13) & 3) as u32);
                self.core.key_press_sym(code, sym, &text, now);
            }
            4 | 5 => {   // core ButtonPress/Release: only when XI2 is absent
                if ext.xi != 0 { return; }
                let b = p[1] as u32;
                let code = match b { 1 => BTN_LEFT, 2 => BTN_MIDDLE, 3 => BTN_RIGHT, 8 => BTN_SIDE, 9 => BTN_EXTRA, _ => 0 };
                if ty == 4 && (4..=7).contains(&b) {
                    let d = match b { 4 => -SCROLL_STEP, 5 => SCROLL_STEP, 6 => -SCROLL_STEP, _ => SCROLL_STEP };
                    let axis = if b <= 5 { AXIS_SCROLL_V } else { AXIS_SCROLL_H };
                    self.core.push_axes(&[AxisDiff { axis, delta: d }]);
                }
                if code != 0 { self.core.key(code, ty == 4); }
            }
            6 => {   // MotionNotify (no XI2)
                if ext.xi != 0 { return; }
                let x = rd16(p, 24) as i16 as i64; let y = rd16(p, 26) as i16 as i64;
                self.core.pointer_abs(x << 32, y << 32);
            }
            9 => { self.core.focused.store(true, Relaxed); }
            10 => { self.core.focused.store(false, Relaxed); self.core.release_all_keys(true); }
            12 => {   // Expose: restart the frame chain if it is idle
                if self.core.in_flight.load(Acquire) == 0 { self.core.push_render(); }
            }
            22 => {   // ConfigureNotify
                if rd32(p, 8) != self.sh.win { return; }
                let w = rd16(p, 20) as u32; let h = rd16(p, 22) as u32;
                if synthetic {
                    // Only the synthetic one carries ROOT coordinates; the real one is relative to the WM frame.
                    self.win_root_x = rd16(p, 16) as i16 as i32; self.win_root_y = rd16(p, 18) as i16 as i32;
                    self.maybe_requery_period();
                }
                if w != 0 && h != 0 && (w != self.core.win_w.load(Relaxed) || h != self.core.win_h.load(Relaxed)) {
                    self.core.win_w.store(w, Relaxed); self.core.win_h.store(h, Relaxed);
                    self.core.full_redraw.store(true, Relaxed);
                    if self.core.in_flight.load(Acquire) == 0 { self.core.push_render(); }
                }
            }
            33 => {   // ClientMessage
                if rd32(p, 8) == self.atoms.wm_protocols {
                    let proto = rd32(p, 12);
                    if proto == self.atoms.wm_delete { self.core.push_close(); }
                    else if proto == self.atoms.net_wm_sync && self.atoms.net_wm_sync != 0 {
                        let v = rd32(p, 20) as u64 | ((rd32(p, 24) as u64) << 32);
                        self.sh.sync_want.store(v, Release);
                    }
                }
            }
            34 => { self.load_keymap(); }   // MappingNotify
            35 => self.handle_generic(p, now),
            t if ext.rr_event != 0 && t == ext.rr_event => { self.query_period(); }
            t if ext.xkb_event != 0 && t == ext.xkb_event => { if p[1] <= 1 { self.load_keymap(); } }
            _ => {}
        }
    }

    fn handle_generic(&mut self, p: &[u8], _now: u64) {
        let extension = p[1]; let evtype = rd16(p, 8);
        let ext = &self.sh.ext;
        if extension == ext.present {
            match evtype {
                0 => {   // PresentConfigureNotify
                    let w = rd16(p, 24) as u32; let h = rd16(p, 26) as u32;
                    if w != 0 && h != 0 { self.core.win_w.store(w, Relaxed); self.core.win_h.store(h, Relaxed); self.core.full_redraw.store(true, Relaxed); }
                }
                1 => {   // CompleteNotify
                    let mode = p[11]; let msc = rd64(p, 32);
                    let frames = if self.first_complete { 1 } else { msc.saturating_sub(self.last_msc) };
                    let first = self.first_complete;
                    self.first_complete = false;
                    self.last_msc = msc;
                    if self.core.in_flight.load(Acquire) > 0 { self.core.in_flight.fetch_sub(1, AcqRel); }
                    // Several presents can retire against ONE vblank: a delta of zero advances
                    // nothing and drives no tick, or the swapchain runs at 2x the monitor with
                    // two presents permanently in flight (measured under Xwayland: the second
                    // retires as a frames==0 COPY, not a SKIP).
                    self.core.display_tick(frames);
                    if mode != 2 && (frames != 0 || first) { self.core.push_render(); }
                }
                2 => {   // IdleNotify
                    let pix = rd32(p, 24);
                    for i in 0..2 { if self.sh.pixmap[i].load(Acquire) == pix { self.core.busy[i].store(false, Release); } }
                    // The app skipped a frame on backpressure and the chain went idle: restart it.
                    if self.core.in_flight.load(Acquire) == 0 { self.core.push_render(); }
                }
                _ => {}
            }
            return;
        }
        if extension == ext.xi && ext.xi != 0 {
            match evtype {
                1 | 11 => { for d in self.scroll.iter_mut() { d.v_last = None; d.h_last = None; } if evtype == 11 { self.query_devices(); } }
                4 | 5 | 6 | 7 | 8 => {
                    if p.len() < 80 { return; }
                    let ex = rd32(p, 40) as i32 as i64; let ey = rd32(p, 44) as i32 as i64;   // FP1616
                    let x_fx = ex << 16; let y_fx = ey << 16;
                    if evtype == 6 || evtype == 7 || evtype == 8 {
                        if x_fx != self.core.cursor_x.load(Relaxed) || y_fx != self.core.cursor_y.load(Relaxed) || evtype == 7 {
                            self.core.pointer_abs(x_fx, y_fx);
                        }
                    } else {
                        self.core.cursor_x.store(x_fx, Relaxed); self.core.cursor_y.store(y_fx, Relaxed);
                    }
                    if evtype == 8 { return; }
                    // Valuators: the scroll axes are absolute accumulators; deltas come from our last value.
                    let blen = rd16(p, 48) as usize; let vlen = rd16(p, 50) as usize; let sourceid = rd16(p, 52);
                    let flags = rd32(p, 56);
                    let mut o = 80 + blen * 4;
                    let mask_off = o; o += vlen * 4;
                    let mut diffs: Vec<AxisDiff> = Vec::new();
                    for bit in 0..(vlen * 32) {
                        if mask_off + bit / 8 >= p.len() || p[mask_off + bit / 8] & (1 << (bit % 8)) == 0 { continue; }
                        if o + 8 > p.len() { break; }
                        let v = rd32(p, o) as i32 as f64 + rd32(p, o + 4) as f64 / 4294967296.0;
                        o += 8;
                        if let Some(d) = self.scroll.iter_mut().find(|d| d.id == sourceid) {
                            if bit as u16 == d.v_num {
                                if let Some(last) = d.v_last { let px = (v - last) / d.v_inc * 15.0; diffs.push(AxisDiff { axis: AXIS_SCROLL_V, delta: (px * 256.0) as i32 }); }
                                d.v_last = Some(v);
                            } else if bit as u16 == d.h_num {
                                if let Some(last) = d.h_last { let px = (v - last) / d.h_inc * 15.0; diffs.push(AxisDiff { axis: AXIS_SCROLL_H, delta: (px * 256.0) as i32 }); }
                                d.h_last = Some(v);
                            }
                        }
                    }
                    if !diffs.is_empty() { self.core.push_axes(&diffs); }
                    if evtype == 4 || evtype == 5 {
                        let b = rd32(p, 16);
                        let code = match b { 1 => BTN_LEFT, 2 => BTN_MIDDLE, 3 => BTN_RIGHT, 8 => BTN_SIDE, 9 => BTN_EXTRA, _ => 0 };
                        if code != 0 { self.core.key(code, evtype == 4); }
                        else if evtype == 4 && (4..=7).contains(&b) && flags & 0x10000 == 0 {
                            // A wheel on a device without a ScrollClass: legacy buttons, one click each.
                            let known = self.scroll.iter().any(|d| d.id == sourceid);
                            if !known {
                                let d = match b { 4 | 6 => -SCROLL_STEP, _ => SCROLL_STEP };
                                self.core.push_axes(&[AxisDiff { axis: if b <= 5 { AXIS_SCROLL_V } else { AXIS_SCROLL_H }, delta: d }]);
                            }
                        }
                    }
                }
                27 => { self.pinch_scale = 1 << 16; }
                28 => {
                    if p.len() < 80 { return; }
                    let scale = rd32(p, 64) as i32; let angle = rd32(p, 68) as i32;
                    let dz = scale - self.pinch_scale; self.pinch_scale = scale;
                    let mut d = Vec::new();
                    if dz != 0 { d.push(AxisDiff { axis: AXIS_ZOOM, delta: dz }); }
                    if angle != 0 { d.push(AxisDiff { axis: AXIS_ROTATE, delta: angle }); }
                    if !d.is_empty() { self.core.push_axes(&d); }
                }
                _ => {}
            }
        }
    }

    fn run(mut self) {
        let fd = self.sh.conn.fd;
        loop {
            if self.core.quit.load(Relaxed) || self.r.dead { break; }
            self.core.service_resync();
            // Drain to empty BEFORE blocking: a packet already read into our buffer would
            // otherwise wait out the whole poll timeout (the mouse-stop hitch).
            while let Some(p) = self.r.next_packet(true) {
                let now = sys::clock_monotonic_ns();
                self.handle(&p, now);
                if self.r.dead { return; }
            }
            let now = sys::clock_monotonic_ns();
            let mut timeout = self.core.repeat_tick(now);
            self.apply_cursor();
            self.apply_fullscreen();
            if self.core.wants_frame.load(Relaxed) {
                if self.core.in_flight.load(Acquire) == 0 && !self.core.render_pending() { self.core.push_render(); }
                timeout = timeout.min(4);
            }
            self.sh.conn.flush();
            if self.r.has_buffered() { continue; }
            let mut pfd = [sys::PollFd { fd, events: sys::POLLIN, revents: 0 }];
            let r = sys::poll(&mut pfd, timeout);
            if r < 0 && r != sys::EINTR { break; }
            if pfd[0].revents & (sys::POLLERR | sys::POLLHUP) != 0 { break; }
        }
        if !self.quit_seen { self.core.push_close(); }
    }
}

// ============================================================================
// Open
// ============================================================================
pub fn open(core: Arc<Core>, title: &str, width: u32, height: u32) -> Option<App> {
    let conn = Conn::open()?;
    let mut r = Reader::new(conn.fd);

    let present = query_extension(&conn, &mut r, b"Present").map(|e| e.0);
    let shm = query_extension(&conn, &mut r, b"MIT-SHM").map(|e| e.0);
    let (Some(present), Some(shm)) = (present, shm) else { eprintln!("softer_gui: X server lacks Present or MIT-SHM"); return None; };
    let sync = query_extension(&conn, &mut r, b"SYNC").map(|e| e.0).unwrap_or(0);
    let xfixes = query_extension(&conn, &mut r, b"XFIXES").map(|e| e.0).unwrap_or(0);
    let (randr, rr_event) = query_extension(&conn, &mut r, b"RANDR").map(|e| (e.0, e.1)).unwrap_or((0, 0));
    let (xkb, xkb_event) = query_extension(&conn, &mut r, b"XKEYBOARD").map(|e| (e.0, e.1)).unwrap_or((0, 0));
    let xi = query_extension(&conn, &mut r, b"XInputExtension").map(|e| e.0).unwrap_or(0);

    // Version handshakes (the server requires them before other requests of the extension).
    { let mut b = Vec::new(); b.extend_from_slice(&u32b(1)); b.extend_from_slice(&u32b(0)); let s = conn.req(present, 0, &b); conn.flush(); r.wait_reply(s)?; }
    {
        let s = conn.req(shm, 0, &[]); conn.flush();
        let rep = r.wait_reply(s)?;
        let shared_pixmaps = rep[1] != 0; let major = rd16(&rep, 8); let minor = rd16(&rep, 10);
        if !shared_pixmaps || major < 1 || (major == 1 && minor < 2) { eprintln!("softer_gui: MIT-SHM {major}.{minor} lacks fd passing / shared pixmaps"); return None; }
    }
    if sync != 0 { let s = conn.req(sync, 0, &[3, 1, 0, 0]); conn.flush(); r.wait_reply(s); }
    if xfixes != 0 { let mut b = Vec::new(); b.extend_from_slice(&u32b(4)); b.extend_from_slice(&u32b(0)); let s = conn.req(xfixes, 0, &b); conn.flush(); r.wait_reply(s); }
    if randr != 0 { let mut b = Vec::new(); b.extend_from_slice(&u32b(1)); b.extend_from_slice(&u32b(5)); let s = conn.req(randr, 0, &b); conn.flush(); r.wait_reply(s); }
    let mut xi_minor = 0u16;
    let xi = if xi != 0 {
        let mut b = Vec::new(); b.extend_from_slice(&u16b(2)); b.extend_from_slice(&u16b(4));
        let s = conn.req(xi, 47, &b); conn.flush();
        match r.wait_reply(s) { Some(rep) if rd16(&rep, 8) >= 2 => { xi_minor = rd16(&rep, 10); xi } _ => 0 }
    } else { 0 };
    let xkb = if xkb != 0 {
        let s = conn.req(xkb, 0, &[1, 0, 0, 0]); conn.flush();   // XkbUseExtension(1.0)
        match r.wait_reply(s) { Some(rep) if rep[1] != 0 => xkb, _ => 0 }
    } else { 0 };

    let ext = Ext { present, shm, sync, xfixes, randr, rr_event, xkb, xkb_event, xi };

    // Detectable autorepeat + the user's repeat rate.
    let mut repeat: Option<(u64, u64)> = None;
    if xkb != 0 {
        let mut b = vec![0u8; 24];
        b[0..2].copy_from_slice(&u16b(0x100)); b[4..8].copy_from_slice(&u32b(1)); b[8..12].copy_from_slice(&u32b(1));
        let s = conn.req(xkb, 21, &b); conn.flush(); r.wait_reply(s);   // PerClientFlags(DetectableAutoRepeat)
        let mut b = vec![0u8; 4]; b[0..2].copy_from_slice(&u16b(0x100));
        let s = conn.req(xkb, 6, &b); conn.flush();                      // GetControls
        if let Some(rep) = r.wait_reply(s) {
            // xkbGetControlsReply: ...internalModsVmods@16, ignoreLockModsVmods@18, repeatDelay@20, repeatInterval@22.
            let delay = rd16(&rep, 20) as u64; let interval = rd16(&rep, 22) as u64;
            if std::env::var("SOFTER_GUI_DEBUG").is_ok() { eprintln!("softer_gui: xkb repeat delay {delay} ms interval {interval} ms"); }
            if interval > 0 { repeat = Some((delay, interval)); }
        }
        // XkbSelectEvents: NewKeyboardNotify (all details) + MapNotify (all map parts).
        let mut b = Vec::new();
        b.extend_from_slice(&u16b(0x100)); b.extend_from_slice(&u16b(3)); b.extend_from_slice(&u16b(0)); b.extend_from_slice(&u16b(1));
        b.extend_from_slice(&u16b(0x0fff)); b.extend_from_slice(&u16b(0x0fff));
        conn.req(xkb, 1, &b);
    }

    // Window.
    let win = conn.new_id();
    let root = conn.setup.root;
    {
        // background_pixmap = None and bit_gravity = NorthWest: the server must never paint
        // the window itself (that is the black flash on every step of a resize drag), and
        // the old contents stay anchored top-left until our next present lands.
        let mut b = Vec::new();
        b.extend_from_slice(&u32b(win)); b.extend_from_slice(&u32b(root));
        b.extend_from_slice(&u16b(0)); b.extend_from_slice(&u16b(0));
        b.extend_from_slice(&u16b(width as u16)); b.extend_from_slice(&u16b(height as u16));
        b.extend_from_slice(&u16b(0)); b.extend_from_slice(&u16b(1));   // border, class InputOutput
        b.extend_from_slice(&u32b(conn.setup.root_visual));
        let event_mask: u32 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 6) | (1 << 15) | (1 << 17) | (1 << 21);
        b.extend_from_slice(&u32b(1 | (1 << 4) | (1 << 11)));   // CWBackPixmap | CWBitGravity | CWEventMask
        b.extend_from_slice(&u32b(0)); b.extend_from_slice(&u32b(1)); b.extend_from_slice(&u32b(event_mask));
        conn.req(CREATE_WINDOW, conn.setup.root_depth, &b);
    }
    let atoms = Atoms {
        wm_protocols: intern_atom(&conn, &mut r, b"WM_PROTOCOLS"),
        wm_delete: intern_atom(&conn, &mut r, b"WM_DELETE_WINDOW"),
        net_wm_sync: intern_atom(&conn, &mut r, b"_NET_WM_SYNC_REQUEST"),
        net_wm_sync_counter: intern_atom(&conn, &mut r, b"_NET_WM_SYNC_REQUEST_COUNTER"),
        net_wm_state: intern_atom(&conn, &mut r, b"_NET_WM_STATE"),
        net_wm_fullscreen: intern_atom(&conn, &mut r, b"_NET_WM_STATE_FULLSCREEN"),
        net_wm_name: intern_atom(&conn, &mut r, b"_NET_WM_NAME"),
        utf8_string: intern_atom(&conn, &mut r, b"UTF8_STRING"),
        net_wm_pid: intern_atom(&conn, &mut r, b"_NET_WM_PID"),
    };
    change_property(&conn, win, 39, 31, 8, title.as_bytes());   // WM_NAME / STRING
    change_property(&conn, win, atoms.net_wm_name, atoms.utf8_string, 8, title.as_bytes());
    change_property(&conn, win, 67, 31, 8, title.as_bytes());   // WM_CLASS-ish hint: WM_ICON_NAME
    let _ = atoms.net_wm_pid;

    // _NET_WM_SYNC_REQUEST counter, best effort and independent of the close protocol.
    let mut sync_counter = 0u32;
    let mut sync_atom = 0u32;
    if sync != 0 && atoms.net_wm_sync != 0 && atoms.net_wm_sync_counter != 0 {
        sync_counter = conn.new_id();
        let mut b = Vec::new(); b.extend_from_slice(&u32b(sync_counter)); b.extend_from_slice(&[0u8; 8]);
        conn.req(sync, 2, &b);
        change_property(&conn, win, atoms.net_wm_sync_counter, 6, 32, &u32b(sync_counter));
        sync_atom = atoms.net_wm_sync;
    }
    let mut protos = Vec::new();
    protos.extend_from_slice(&u32b(atoms.wm_delete));
    if sync_atom != 0 { protos.extend_from_slice(&u32b(sync_atom)); }
    change_property(&conn, win, atoms.wm_protocols, 4, 32, &protos);

    // Extension event selection.
    if randr != 0 { let mut b = Vec::new(); b.extend_from_slice(&u32b(root)); b.extend_from_slice(&u16b(1)); b.extend_from_slice(&u16b(0)); conn.req(randr, 4, &b); }
    if xi != 0 {
        // Master pointer events on our window: Motion, ButtonPress/Release, Enter/Leave, and (2.4+) pinch gestures.
        let mut mask: u32 = (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8);
        if xi_minor >= 4 { mask |= (1 << 27) | (1 << 28) | (1 << 29); }
        let mut b = Vec::new();
        b.extend_from_slice(&u32b(win)); b.extend_from_slice(&u16b(1)); b.extend_from_slice(&u16b(0));
        b.extend_from_slice(&u16b(1)); b.extend_from_slice(&u16b(1)); b.extend_from_slice(&u32b(mask));   // XIAllMasterDevices
        conn.req(xi, 46, &b);
        // Device topology changes on the root, for every device.
        let mut b = Vec::new();
        b.extend_from_slice(&u32b(root)); b.extend_from_slice(&u16b(1)); b.extend_from_slice(&u16b(0));
        b.extend_from_slice(&u16b(0)); b.extend_from_slice(&u16b(1)); b.extend_from_slice(&u32b((1 << 1) | (1 << 11)));
        conn.req(xi, 46, &b);
    }
    conn.req(MAP_WINDOW, 0, &u32b(win));
    if std::env::var("SOFTER_GUI_DEBUG").is_ok() { eprintln!("softer_gui: x11 window 0x{win:x}"); }
    // PresentSelectInput(eid, window, ConfigureNotify|CompleteNotify|IdleNotify)
    let eid = conn.new_id();
    { let mut b = Vec::new(); b.extend_from_slice(&u32b(eid)); b.extend_from_slice(&u32b(win)); b.extend_from_slice(&u32b(1 | 2 | 4)); conn.req(present, 3, &b); }
    conn.flush();

    core.win_w.store(width, Relaxed); core.win_h.store(height, Relaxed);
    if let Some((d, i)) = repeat { core.set_repeat(d, i); }

    let sh = Arc::new(Shared {
        conn, win, ext, sync_counter,
        sync_want: AtomicU64::new(0), sync_done: AtomicU64::new(0),
        pixmap: [AtomicU32::new(0), AtomicU32::new(0)], present_serial: AtomicU32::new(1),
    });
    let mut pump = Pump {
        sh: sh.clone(), core: core.clone(), r, atoms, keymap: xkb::Keymap::default(),
        last_msc: 0, first_complete: true, win_root_x: 0, win_root_y: 0, crtc_rect: (0, 0, 0, 0),
        cursor_applied: false, fs_applied: false, scroll: Vec::new(), pinch_scale: 1 << 16, quit_seen: false,
        debug: std::env::var("SOFTER_GUI_DEBUG").is_ok(),
    };
    pump.load_keymap();
    pump.query_devices();
    pump.query_period();
    if pump.keymap.keys.is_empty() { eprintln!("softer_gui: no XKB keymap; text input will be empty"); }
    std::thread::Builder::new().name("softer_gui-x11-pump".into()).spawn(move || pump.run()).ok()?;

    Some(App { sh, core, side: 0, generation: 0, cur: 0, bufs: [None, None] })
}
