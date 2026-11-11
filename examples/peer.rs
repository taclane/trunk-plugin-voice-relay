//! A test peer: logs into an MMDVM network master as a hotspot would, takes the
//! talkgroups given, and decodes what it's sent: each stream's LC, its
//! talker alias, and its voice into a WAV (TRP's own AMBE+2 decoder).
//!
//! ```text
//! cargo run --example peer -- <master host:port> <peer id> <password> "<TS1=…;TS2=…>" <out.wav> [seconds]
//! ```

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use trunk_core::ambe::decode_vcw;
use trunk_core::dmr::fec::{bptc196_decode, embedded_lc_decode, pack, rs129_decode, unpack};

fn msg(tag: &[u8], id: u32, rest: &[u8]) -> Vec<u8> {
    let mut b = tag.to_vec();
    b.extend_from_slice(&id.to_be_bytes());
    b.extend_from_slice(rest);
    b
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        eprintln!("peer <master> <id> <password> <options> <out.wav> [seconds]");
        std::process::exit(2);
    }
    let (master, id, pw, opts, out) = (&a[1], a[2].parse::<u32>().unwrap(), &a[3], &a[4], &a[5]);
    let secs: u64 = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(60);
    let s = UdpSocket::bind("0.0.0.0:0").unwrap();
    s.connect(master).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut buf = [0u8; 1024];
    let mut recv = |s: &UdpSocket| s.recv(&mut buf).map(|n| buf[..n].to_vec());
    s.send(&msg(b"RPTL", id, &[])).unwrap();
    let r = recv(&s).expect("no answer to RPTL");
    assert_eq!(&r[..6], b"RPTACK", "{r:?}");
    let salt = &r[6..10];
    let mut h = Sha256::new();
    h.update(salt);
    h.update(pw.as_bytes());
    s.send(&msg(b"RPTK", id, &h.finalize())).unwrap();
    let r = recv(&s).unwrap();
    assert_eq!(&r[..6], b"RPTACK", "password refused");
    let cfg = format!(
        "{:<8}{:09}{:09}{:02}{:02}{:<8}{:<9}{:03}{:<20}{:<19}{:1}{:<124}{:<40}{:<40}",
        "TEST", 433450000u64, 433450000u64, 1, 1, "0.0", "0.0", 0, "Bench", "test peer", "3", "", "voice-relay peer", "example"
    );
    s.send(&msg(b"RPTC", id, &cfg.as_bytes()[..294])).unwrap();
    assert_eq!(&recv(&s).unwrap()[..6], b"RPTACK");
    if !opts.is_empty() {
        s.send(&msg(b"RPTO", id, opts.as_bytes())).unwrap();
        assert_eq!(&recv(&s).unwrap()[..6], b"RPTACK");
    }
    eprintln!("logged in as {id}");
    let mut dec = trunk_core::mbe::Decoder::new(trunk_core::mbe::lcg(1), trunk_core::mbe::Profile::Enhanced);
    let mut audio: Vec<f32> = Vec::new();
    let mut frame = [0f32; trunk_core::mbe::FRAME_SAMPLES];
    let (start, mut ping) = (Instant::now(), Instant::now());
    let mut emb: Vec<u8> = Vec::new();
    let mut ta: Vec<[u8; 9]> = Vec::new();
    let (mut bursts, mut fec_errs) = (0u64, 0u64);
    // Per slot: when its stream began, its voice bursts, and those all silence.
    let mut per: [(Option<Instant>, u32, u32); 3] = [(None, 0, 0); 3];
    // The bridge's silence frame (dmr::silence_frame).
    let silence: [u8; 49] =
        [1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 1, 0, 1, 0, 0, 1, 1, 0, 0, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 1, 0, 0, 1, 1, 1, 0, 0, 0, 0, 0, 1];
    while start.elapsed() < Duration::from_secs(secs) {
        if ping.elapsed() > Duration::from_secs(5) {
            ping = Instant::now();
            s.send(&msg(b"RPTPING", id, &[])).unwrap();
        }
        let Ok(b) = recv(&s) else { continue };
        if !b.starts_with(b"DMRD") || b.len() < 53 {
            continue;
        }
        let u24 = |i: usize| (b[i] as u32) << 16 | (b[i + 1] as u32) << 8 | b[i + 2] as u32;
        let (src, dst, flags, stream) = (u24(5), u24(8), b[15], u32::from_be_bytes(b[16..20].try_into().unwrap()));
        let slot = if flags & 0x80 != 0 { 2 } else { 1 };
        let bits = unpack(&b[20..53]);
        bursts += 1;
        match flags >> 4 & 3 {
            2 => {
                let dt = flags & 15;
                let raw: [u8; 196] = bits[..98].iter().chain(&bits[166..]).copied().collect::<Vec<_>>().try_into().unwrap();
                let got = bptc196_decode(&raw);
                let mut cw: [u8; 12] = pack(&got.bits).try_into().unwrap();
                let mask: u32 = if dt == 1 { 0x969696 } else { 0x999999 };
                cw[9] ^= (mask >> 16) as u8;
                cw[10] ^= (mask >> 8) as u8;
                cw[11] ^= mask as u8;
                let ok = rs129_decode(&mut cw).is_some();
                let lc_dst = (cw[3] as u32) << 16 | (cw[4] as u32) << 8 | cw[5] as u32;
                let lc_src = (cw[6] as u32) << 16 | (cw[7] as u32) << 8 | cw[8] as u32;
                let what = if dt == 1 { "header" } else { "terminator" };
                eprintln!(
                    "TS{slot} {what} stream {stream:08x}: LC {} {lc_src} → {lc_dst}{} (packet {src} → {dst})",
                    if ok { "ok" } else { "BAD" },
                    if cw[2] & 0x80 != 0 { " EMERGENCY" } else { "" }
                );
                let ps = &mut per[slot];
                if dt == 1 && ps.0.is_none() {
                    *ps = (Some(Instant::now()), 0, 0);
                }
                if dt == 2 {
                    if let Some(t0) = ps.0.take() {
                        let secs = t0.elapsed().as_secs_f64();
                        eprintln!("  {} voice bursts ({:.2} s of voice, {} all silence) in {secs:.2} s", ps.1, ps.1 as f64 * 0.06, ps.2);
                    }
                }
                if dt == 2 && !ta.is_empty() {
                    eprintln!("  talker alias: {:?}", alias(&ta));
                    ta.clear();
                }
                // A gap between streams, as a radio would hear it.
                audio.extend(std::iter::repeat_n(0.0, 1600));
            }
            sync => {
                let n = if sync == 1 { 0 } else { (flags & 15) as usize };
                let cw: Vec<u8> = bits[..108].iter().chain(&bits[156..]).copied().collect();
                let dib: Vec<u8> = cw.chunks(2).map(|c| c[0] << 1 | c[1]).collect();
                per[slot].1 += 1;
                let mut quiet = 0;
                for k in 0..3 {
                    let f = decode_vcw(&dib[k * 36..k * 36 + 36], None);
                    if f.bits == silence {
                        quiet += 1;
                    }
                    fec_errs += f.errs as u64;
                    dec.ambe(&f.bits, f.errs, &mut frame);
                    trunk_core::mbe::to_limited(&mut frame);
                    audio.extend_from_slice(&frame);
                }
                if quiet == 3 {
                    per[slot].2 += 1;
                }
                if n == 1 {
                    emb.clear();
                }
                if (1..=4).contains(&n) {
                    emb.extend_from_slice(&bits[116..148]);
                }
                if n == 4 && emb.len() == 128 {
                    if let Some((lc, _, true)) = embedded_lc_decode(&emb.clone().try_into().unwrap()) {
                        let lc: [u8; 9] = pack(&lc).try_into().unwrap();
                        if (4..=7).contains(&(lc[0] & 0x3F)) && !ta.iter().any(|t| t[0] == lc[0]) {
                            ta.push(lc);
                        }
                    }
                }
            }
        }
    }
    s.send(&msg(b"RPTCL", id, &[])).ok();
    std::fs::write(out, trunk_core::wav::encode(&audio, 8000)).unwrap();
    eprintln!("{bursts} bursts, {fec_errs} FEC errors, {:.1} s of audio → {out}", audio.len() as f64 / 8000.0);
}

/// A talker alias from its header and blocks (8-bit or 7-bit).
fn alias(lcs: &[[u8; 9]]) -> String {
    let mut l = lcs.to_vec();
    l.sort_by_key(|x| x[0] & 0x3F);
    let Some(h) = l.iter().find(|x| x[0] & 0x3F == 4) else { return String::new() };
    let (format, len) = (h[2] >> 6, (h[2] >> 1 & 31) as usize);
    if format == 0 {
        // 7-bit: packed straight on from the header's last bit.
        let mut bits = vec![h[2] & 1];
        bits.extend(unpack(&h[3..9]));
        for b in l.iter().filter(|x| x[0] & 0x3F > 4) {
            bits.extend(unpack(&b[2..9]));
        }
        return bits.chunks(7).take(len).map(|c| c.iter().fold(0u8, |v, &b| v << 1 | b) as char).collect();
    }
    // 8-bit: whole octets, from the header's fourth (as hotspots read them).
    let mut text: Vec<u8> = h[3..9].to_vec();
    for b in l.iter().filter(|x| x[0] & 0x3F > 4) {
        text.extend_from_slice(&b[2..9]);
    }
    text.iter().take(len).map(|&c| c as char).collect()
}
