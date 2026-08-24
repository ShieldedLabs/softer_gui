//! Synthetic X11 input through the XTEST extension, for driving the demo without a
//! human: `xtest move X Y`, `xtest click N`, `xtest key CODE` (evdev code, press+release),
//! `xtest type TEXT` (qwerty a-z/space via evdev codes), `xtest wheel N` (N clicks, negative = up).
//! Several commands can be chained: `xtest move 300 300 click 1 type hello`.
#[cfg(target_os = "linux")]
use softer_gui::x11_conn::{Conn, Reader};
#[cfg(not(target_os = "linux"))]
fn main() {}
#[cfg(target_os = "linux")]
#[cfg(target_os = "linux")]
fn fake(conn: &Conn, xt: u8, ty: u8, detail: u8, x: i16, y: i16) {
    let mut b = vec![ty, detail, 0, 0];
    b.extend_from_slice(&0u32.to_le_bytes()); b.extend_from_slice(&0u32.to_le_bytes()); b.extend_from_slice(&[0u8; 8]);
    b.extend_from_slice(&x.to_le_bytes()); b.extend_from_slice(&y.to_le_bytes()); b.extend_from_slice(&[0u8; 8]);
    conn.req(xt, 2, &b);
    conn.flush();
    std::thread::sleep(std::time::Duration::from_millis(8));
}
#[cfg(target_os = "linux")]
fn main() {
    let conn = Conn::open().expect("X");
    let mut r = Reader::new(conn.fd);
    let name = b"XTEST";
    let mut b = Vec::new(); b.extend_from_slice(&(name.len() as u16).to_le_bytes()); b.extend_from_slice(&[0, 0]); b.extend_from_slice(name);
    let s = conn.req(98, 0, &b); conn.flush();
    let rep = r.wait_reply(s).expect("QueryExtension");
    assert!(rep[8] != 0, "no XTEST");
    let xt = rep[9];
    let s = conn.req(xt, 0, &[2, 0, 2, 0]); conn.flush(); r.wait_reply(s);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "move" => { let x: i16 = args[i + 1].parse().unwrap(); let y: i16 = args[i + 2].parse().unwrap(); fake(&conn, xt, 6, 0, x, y); i += 3; }
            "click" => { let n: u8 = args[i + 1].parse().unwrap(); fake(&conn, xt, 4, n, 0, 0); fake(&conn, xt, 5, n, 0, 0); i += 2; }
            "down" => { let n: u8 = args[i + 1].parse().unwrap(); fake(&conn, xt, 4, n, 0, 0); i += 2; }
            "up" => { let n: u8 = args[i + 1].parse().unwrap(); fake(&conn, xt, 5, n, 0, 0); i += 2; }
            "key" => { let c: u8 = args[i + 1].parse().unwrap(); fake(&conn, xt, 2, c + 8, 0, 0); fake(&conn, xt, 3, c + 8, 0, 0); i += 2; }
            "hold" => { let c: u8 = args[i + 1].parse().unwrap(); let ms: u64 = args[i + 2].parse().unwrap(); fake(&conn, xt, 2, c + 8, 0, 0); std::thread::sleep(std::time::Duration::from_millis(ms)); fake(&conn, xt, 3, c + 8, 0, 0); i += 3; }
            "wheel" => { let n: i32 = args[i + 1].parse().unwrap(); for _ in 0..n.abs() { let b = if n < 0 { 4 } else { 5 }; fake(&conn, xt, 4, b, 0, 0); fake(&conn, xt, 5, b, 0, 0); } i += 2; }
            "type" => {
                const ROW1: &[u8] = b"qwertyuiop"; const ROW2: &[u8] = b"asdfghjkl"; const ROW3: &[u8] = b"zxcvbnm";
                for ch in args[i + 1].bytes() {
                    let code = if let Some(p) = ROW1.iter().position(|c| *c == ch) { 16 + p } else if let Some(p) = ROW2.iter().position(|c| *c == ch) { 30 + p } else if let Some(p) = ROW3.iter().position(|c| *c == ch) { 44 + p } else if ch == b' ' { 57 } else if ch.is_ascii_digit() { if ch == b'0' { 11 } else { 1 + (ch - b'0') as usize } } else { continue };
                    fake(&conn, xt, 2, code as u8 + 8, 0, 0); fake(&conn, xt, 3, code as u8 + 8, 0, 0);
                }
                i += 2;
            }
            "shift" => { fake(&conn, xt, 2, 42 + 8, 0, 0); i += 1; }
            "unshift" => { fake(&conn, xt, 3, 42 + 8, 0, 0); i += 1; }
            "where" => { let s = conn.req(38, 0, &conn.setup.root.to_le_bytes()); conn.flush(); if let Some(p) = r.wait_reply(s) { println!("pointer at {},{}", i16::from_le_bytes([p[16], p[17]]), i16::from_le_bytes([p[18], p[19]])); } i += 1; }
            "geom" => {
                let id = u32::from_str_radix(args[i + 1].trim_start_matches("0x"), 16).unwrap();
                let s = conn.req(14, 0, &id.to_le_bytes()); conn.flush();
                let g = r.wait_reply(s).expect("GetGeometry");
                let mut b = Vec::new(); b.extend_from_slice(&id.to_le_bytes()); b.extend_from_slice(&conn.setup.root.to_le_bytes()); b.extend_from_slice(&[0, 0, 0, 0]);
                let s = conn.req(40, 0, &b); conn.flush();
                let t = r.wait_reply(s).expect("TranslateCoordinates");
                println!("geom {},{} {}x{}", i16::from_le_bytes([t[12], t[13]]), i16::from_le_bytes([t[14], t[15]]), u16::from_le_bytes([g[16], g[17]]), u16::from_le_bytes([g[18], g[19]]));
                i += 2;
            }
            "activate" => {
                let id = u32::from_str_radix(args[i + 1].trim_start_matches("0x"), 16).unwrap();
                let name = b"_NET_ACTIVE_WINDOW";
                let mut b = Vec::new(); b.extend_from_slice(&(name.len() as u16).to_le_bytes()); b.extend_from_slice(&[0, 0]); b.extend_from_slice(name);
                let s = conn.req(16, 0, &b); conn.flush();
                let atom = u32::from_le_bytes(r.wait_reply(s).expect("atom")[8..12].try_into().unwrap());
                let mut ev = vec![33u8, 32, 0, 0];
                ev.extend_from_slice(&id.to_le_bytes()); ev.extend_from_slice(&atom.to_le_bytes());
                ev.extend_from_slice(&2u32.to_le_bytes()); ev.extend_from_slice(&0u32.to_le_bytes()); ev.extend_from_slice(&0u32.to_le_bytes());
                ev.extend_from_slice(&[0u8; 8]);
                let mut b = Vec::new(); b.extend_from_slice(&conn.setup.root.to_le_bytes()); b.extend_from_slice(&((1u32 << 20) | (1 << 19)).to_le_bytes()); b.extend_from_slice(&ev);
                conn.req(25, 0, &b); conn.flush();
                std::thread::sleep(std::time::Duration::from_millis(200));
                i += 2;
            }
            "raise" => {
                let id = u32::from_str_radix(args[i + 1].trim_start_matches("0x"), 16).unwrap();
                let mut b = Vec::new(); b.extend_from_slice(&id.to_le_bytes()); b.extend_from_slice(&(1u16 << 6).to_le_bytes()); b.extend_from_slice(&[0, 0]);
                b.extend_from_slice(&0u32.to_le_bytes());   // stack_mode Above
                conn.req(12, 0, &b); conn.flush();
                i += 2;
            }
            "drag" => {   // drag X0 Y0 X1 Y1 STEPS MS: press at X0,Y0, move in STEPS to X1,Y1 every MS, release
                let v: Vec<i32> = args[i + 1..i + 7].iter().map(|a| a.parse().unwrap()).collect();
                let (x0, y0, x1, y1, steps, ms) = (v[0], v[1], v[2], v[3], v[4], v[5] as u64);
                fake(&conn, xt, 6, 0, x0 as i16, y0 as i16); fake(&conn, xt, 4, 1, 0, 0);
                for k in 1..=steps { fake(&conn, xt, 6, 0, (x0 + (x1 - x0) * k / steps) as i16, (y0 + (y1 - y0) * k / steps) as i16); std::thread::sleep(std::time::Duration::from_millis(ms)); }
                fake(&conn, xt, 5, 1, 0, 0);
                i += 7;
            }
            "focus" => {
                let id = u32::from_str_radix(args[i + 1].trim_start_matches("0x"), 16).unwrap();
                let mut b = Vec::new(); b.extend_from_slice(&id.to_le_bytes()); b.extend_from_slice(&[0, 0, 0, 0]);
                conn.req(42, 1, &b); conn.flush();   // SetInputFocus, revert_to PointerRoot
                i += 2;
            }
            "grabtest" => {
                let mut b = Vec::new(); b.extend_from_slice(&conn.setup.root.to_le_bytes()); b.extend_from_slice(&[0, 0, 0, 0, 1, 1, 0, 0]);
                let s = conn.req(31, 0, &b); conn.flush();
                let st = r.wait_reply(s).map(|p| p[1]).unwrap_or(255);
                println!("GrabKeyboard status {st} (0 ok, 1 already grabbed by another client, 4 frozen)");
                conn.req(32, 0, &0u32.to_le_bytes());
                let mut b = Vec::new(); b.extend_from_slice(&conn.setup.root.to_le_bytes()); b.extend_from_slice(&0u32.to_le_bytes()); b.extend_from_slice(&[0, 0, 1, 1]); b.extend_from_slice(&[0u8; 8]); b.extend_from_slice(&0u32.to_le_bytes());
                let s = conn.req(26, 0, &b); conn.flush();
                let st = r.wait_reply(s).map(|p| p[1]).unwrap_or(255);
                println!("GrabPointer status {st}");
                conn.req(27, 0, &0u32.to_le_bytes()); conn.flush();
                i += 1;
            }
            "resize" => {
                let id = u32::from_str_radix(args[i + 1].trim_start_matches("0x"), 16).unwrap();
                let w: u32 = args[i + 2].parse().unwrap(); let h: u32 = args[i + 3].parse().unwrap();
                let mut b = Vec::new(); b.extend_from_slice(&id.to_le_bytes()); b.extend_from_slice(&((1u16 << 2) | (1 << 3)).to_le_bytes()); b.extend_from_slice(&[0, 0]);
                b.extend_from_slice(&w.to_le_bytes()); b.extend_from_slice(&h.to_le_bytes());
                conn.req(12, 0, &b); conn.flush();
                i += 4;
            }
            "sleep" => { let ms: u64 = args[i + 1].parse().unwrap(); std::thread::sleep(std::time::Duration::from_millis(ms)); i += 2; }
            other => { eprintln!("unknown command {other}"); break; }
        }
    }
    // Sync and report any errors the fake-input requests produced.
    let s = conn.req(43, 0, &[]); conn.flush();
    r.wait_reply(s);
    while let Some(p) = r.queued.pop_front() { if p[0] == 0 { eprintln!("xtest: X error code {} major {} minor {}", p[1], p[10], u16::from_le_bytes([p[8], p[9]])); } }
}
