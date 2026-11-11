//! DMR bursts as a repeater sends them (ETSI TS 102 361-1), and the
//! MMDVM network `DMRD` packet that carries one burst over the network.
//!
//! A burst is 264 bits: 108 of payload, 48 of sync or EMB + embedded
//! signalling, 108 of payload. Voice bursts carry three AMBE+2 codewords
//! (72 bits each, 49 information bits + FEC); data bursts (voice LC header,
//! terminator) carry 196 BPTC bits split around a slot type and the sync.

use trunk_core::ambe::encode_vcw;
use trunk_core::dmr::fec::{bptc196_encode, embedded_lc_encode, golay20_encode, qr16_encode, rs129_parity, unpack, MASK_TERMINATOR_LC, MASK_VOICE_LC_HEADER};

pub const DT_VOICE_LC_HEADER: u8 = 1;
pub const DT_TERMINATOR_LC: u8 = 2;

/// BS-sourced sync patterns (a repeater's). MMDVM firmware puts its own back
/// on the air, so these only have to be right enough to be recognised.
const BS_VOICE_SYNC: u64 = 0x755F_D7DF_75F7;
const BS_DATA_SYNC: u64 = 0xDFF5_7D75_DF5D;

/// Talker alias FLCOs (TS 102 361-2 7.2.18).
const FLCO_GROUP: u8 = 0;
const FLCO_TA_HEADER: u8 = 4;

/// The AMBE+2 silence frame radios send (codeword B9E881526173002A6B),
/// as its 49 information bits.
pub fn silence_frame() -> [u8; 49] {
    const S: [u8; 49] =
        [1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 1, 0, 1, 0, 0, 1, 1, 0, 0, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 1, 0, 0, 1, 1, 1, 0, 0, 0, 0, 0, 1];
    S
}

/// A group voice call's full link control: FLCO, FID, service options, target, source.
pub fn voice_lc(dst: u32, src: u32, emergency: bool) -> [u8; 9] {
    let (d, s) = (dst.to_be_bytes(), src.to_be_bytes());
    [FLCO_GROUP, 0, if emergency { 0x80 } else { 0 }, d[1], d[2], d[3], s[1], s[2], s[3]]
}

/// The talker alias as embedded LCs: a header, then up to three blocks
/// (8-bit ISO 8859-1 format, at most 27 characters). Characters outside
/// Latin-1 become `?`.
pub fn talker_alias_lcs(alias: &str) -> Vec<[u8; 9]> {
    let text: Vec<u8> = alias.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).take(27).collect();
    if text.is_empty() {
        return Vec::new();
    }
    // In 8-bit format the characters are whole octets: six after the
    // header's format and length (its last bit unused), seven in each block.
    let mut h = [0u8; 9];
    h[0] = FLCO_TA_HEADER;
    h[2] = 1 << 6 | (text.len() as u8) << 1;
    for (i, &c) in text.iter().take(6).enumerate() {
        h[3 + i] = c;
    }
    let mut out = vec![h];
    for (k, chunk) in text.get(6..).unwrap_or(&[]).chunks(7).enumerate() {
        let mut l = [0u8; 9];
        l[0] = FLCO_TA_HEADER + 1 + k as u8;
        l[2..2 + chunk.len()].copy_from_slice(chunk);
        out.push(l);
    }
    out
}

fn put(bits: &mut [u8; 264], at: usize, v: u64, n: usize) {
    for i in 0..n {
        bits[at + i] = (v >> (n - 1 - i) & 1) as u8;
    }
}

fn pack33(bits: &[u8; 264]) -> [u8; 33] {
    std::array::from_fn(|i| bits[i * 8..i * 8 + 8].iter().fold(0u8, |v, &b| v << 1 | b))
}

/// A voice LC header or terminator: the LC with its RS(12,9) parity under
/// the data type's mask, BPTC(196,96), slot type and data sync.
pub fn lc_burst(cc: u8, dt: u8, lc: &[u8; 9]) -> [u8; 33] {
    let mask = if dt == DT_VOICE_LC_HEADER { MASK_VOICE_LC_HEADER } else { MASK_TERMINATOR_LC };
    let p = rs129_parity(lc);
    let mut bytes = lc.to_vec();
    bytes.extend([p[0] ^ (mask >> 16) as u8, p[1] ^ (mask >> 8) as u8, p[2] ^ mask as u8]);
    let info = bptc196_encode(&unpack(&bytes).try_into().unwrap());
    let st = golay20_encode((cc as u32 & 15) << 4 | dt as u32) as u64;
    let mut bits = [0u8; 264];
    bits[..98].copy_from_slice(&info[..98]);
    put(&mut bits, 98, st >> 10, 10);
    put(&mut bits, 108, BS_DATA_SYNC, 48);
    put(&mut bits, 156, st & 0x3FF, 10);
    bits[166..].copy_from_slice(&info[98..]);
    pack33(&bits)
}

/// Voice burst `n` (0 = A … 5 = F) of a superframe. `emb`: the embedded LC
/// being sent this superframe (B–E carry its four 32-bit fragments; F
/// carries none).
pub fn voice_burst(cc: u8, n: usize, frames: &[[u8; 49]; 3], emb: Option<&[u8; 9]>) -> [u8; 33] {
    let mut bits = [0u8; 264];
    let cw: Vec<u8> = frames.iter().flat_map(|f| encode_vcw(f).into_iter().flat_map(|d| [d >> 1 & 1, d & 1])).collect();
    // 72 + 36 | 48 | 36 + 72
    bits[..108].copy_from_slice(&cw[..108]);
    bits[156..].copy_from_slice(&cw[108..]);
    if n == 0 {
        put(&mut bits, 108, BS_VOICE_SYNC, 48);
        return pack33(&bits);
    }
    let fragments = (1..=4).contains(&n) && emb.is_some();
    // LCSS: first, continuation, continuation, last; F: a single fragment (null here).
    let lcss = if fragments { [0, 1, 3, 3, 2, 0][n] } else { 0 };
    let e = qr16_encode((cc as u32 & 15) << 3 | lcss) as u64;
    put(&mut bits, 108, e >> 8, 8);
    put(&mut bits, 148, e & 0xFF, 8);
    if let (true, Some(lc)) = (fragments, emb) {
        let full = embedded_lc_encode(&unpack(lc).try_into().unwrap());
        bits[116..148].copy_from_slice(&full[(n - 1) * 32..n * 32]);
    }
    pack33(&bits)
}

/// What a `DMRD` packet's flags byte says the burst is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Voice burst A (voice sync).
    VoiceSync,
    /// Voice bursts B–F: 1..=5.
    Voice(u8),
    /// A data burst of this data type (1 header, 2 terminator, …).
    Data(u8),
}

/// An MMDVM network `DMRD` packet (53 octets: no BER / RSSI).
#[derive(Clone, Debug, PartialEq)]
pub struct Dmrd {
    pub seq: u8,
    pub src: u32,
    pub dst: u32,
    pub peer: u32,
    /// 1 or 2.
    pub slot: u8,
    pub private: bool,
    pub kind: Kind,
    pub stream: u32,
    pub data: [u8; 33],
}

impl Dmrd {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(55);
        b.extend_from_slice(b"DMRD");
        b.push(self.seq);
        b.extend_from_slice(&self.src.to_be_bytes()[1..]);
        b.extend_from_slice(&self.dst.to_be_bytes()[1..]);
        b.extend_from_slice(&self.peer.to_be_bytes());
        let mut f = if self.slot == 2 { 0x80 } else { 0 };
        if self.private {
            f |= 0x40;
        }
        f |= match self.kind {
            Kind::VoiceSync => 0x10,
            Kind::Voice(n) => n & 0x0F,
            Kind::Data(dt) => 0x20 | (dt & 0x0F),
        };
        b.push(f);
        b.extend_from_slice(&self.stream.to_be_bytes());
        b.extend_from_slice(&self.data);
        b
    }

    pub fn decode(b: &[u8]) -> Option<Dmrd> {
        if b.len() < 53 || &b[..4] != b"DMRD" {
            return None;
        }
        let u24 = |i: usize| (b[i] as u32) << 16 | (b[i + 1] as u32) << 8 | b[i + 2] as u32;
        let f = b[15];
        let kind = match f >> 4 & 3 {
            1 => Kind::VoiceSync,
            2 => Kind::Data(f & 0x0F),
            _ => Kind::Voice(f & 0x0F),
        };
        Some(Dmrd {
            seq: b[4],
            src: u24(5),
            dst: u24(8),
            peer: u32::from_be_bytes([b[11], b[12], b[13], b[14]]),
            slot: if f & 0x80 != 0 { 2 } else { 1 },
            private: f & 0x40 != 0,
            kind,
            stream: u32::from_be_bytes([b[16], b[17], b[18], b[19]]),
            data: b[20..53].try_into().unwrap(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunk_core::ambe::decode_vcw;
    use trunk_core::dmr::fec::{bptc196_decode, embedded_lc_decode, golay20_decode, pack, qr16_decode, rs129_decode};

    fn bits(d: &[u8; 33]) -> Vec<u8> {
        unpack(d)
    }

    fn frames_of(d: &[u8; 33]) -> [[u8; 49]; 3] {
        let b = bits(d);
        let cw: Vec<u8> = b[..108].iter().chain(&b[156..]).copied().collect();
        let dib: Vec<u8> = cw.chunks(2).map(|c| c[0] << 1 | c[1]).collect();
        std::array::from_fn(|k| decode_vcw(&dib[k * 36..k * 36 + 36], None).bits)
    }

    fn rand_frame(x: &mut u32) -> [u8; 49] {
        std::array::from_fn(|_| {
            *x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            (*x >> 31) as u8
        })
    }

    #[test]
    fn a_header_decodes_to_its_lc() {
        let lc = voice_lc(108703, 1234567, true);
        for dt in [DT_VOICE_LC_HEADER, DT_TERMINATOR_LC] {
            let b = bits(&lc_burst(1, dt, &lc));
            let raw: [u8; 196] = b[..98].iter().chain(&b[166..]).copied().collect::<Vec<_>>().try_into().unwrap();
            let got = bptc196_decode(&raw);
            assert_eq!(got.errs, Some(0));
            let mut cw: [u8; 12] = pack(&got.bits).try_into().unwrap();
            let mask = if dt == DT_VOICE_LC_HEADER { MASK_VOICE_LC_HEADER } else { MASK_TERMINATOR_LC };
            cw[9] ^= (mask >> 16) as u8;
            cw[10] ^= (mask >> 8) as u8;
            cw[11] ^= mask as u8;
            assert_eq!(rs129_decode(&mut cw), Some(0));
            assert_eq!(&cw[..9], &lc);
            // Slot type: colour code 1, this data type.
            let st = b[98..108].iter().chain(&b[156..166]).fold(0u32, |v, &x| v << 1 | x as u32);
            assert_eq!(golay20_decode(st, None).0, 1 << 4 | dt as u32);
        }
    }

    #[test]
    fn voice_bursts_carry_frames_and_embedded_lc() {
        let lc = voice_lc(108703, 1234567, false);
        let mut x = 7;
        let mut frag = Vec::new();
        for n in 0..6 {
            let f: [[u8; 49]; 3] = std::array::from_fn(|_| rand_frame(&mut x));
            let d = voice_burst(3, n, &f, Some(&lc));
            assert_eq!(frames_of(&d), f, "burst {n}");
            let b = bits(&d);
            if n == 0 {
                assert_eq!(b[108..156].iter().fold(0u64, |v, &x| v << 1 | x as u64), BS_VOICE_SYNC);
                continue;
            }
            let emb = b[108..116].iter().chain(&b[148..156]).fold(0u32, |v, &x| v << 1 | x as u32);
            let (e, _) = qr16_decode(emb, None);
            assert_eq!(e >> 3, 3, "colour code");
            if (1..=4).contains(&n) {
                frag.extend_from_slice(&b[116..148]);
            }
        }
        let (got, _, ok) = embedded_lc_decode(&frag.try_into().unwrap()).unwrap();
        assert!(ok);
        assert_eq!(pack(&got), lc);
    }

    #[test]
    fn talker_alias_spells_the_name() {
        let lcs = talker_alias_lcs("ENGINE 12 / STATION 4 NORTH");
        assert_eq!(lcs.len(), 4);
        assert_eq!(lcs.iter().map(|l| l[0]).collect::<Vec<_>>(), [4, 5, 6, 7]);
        assert_eq!(lcs[0][2] >> 6, 1);
        assert_eq!(lcs[0][2] >> 1 & 31, 27);
        // Read it back as a hotspot does: each LC's last seven octets end
        // to end, the text from the second octet on.
        let buf: Vec<u8> = lcs.iter().flat_map(|l| l[2..9].to_vec()).collect();
        assert_eq!(String::from_utf8(buf[1..28].to_vec()).unwrap(), "ENGINE 12 / STATION 4 NORTH");
        let bob = talker_alias_lcs("Bob");
        assert_eq!((bob.len(), &bob[0][2..6]), (1, &[1 << 6 | 3 << 1, b'B', b'o', b'b'][..]));
        assert_eq!(talker_alias_lcs("Seven c").len(), 2);
    }

    #[test]
    fn dmrd_round_trips() {
        let p = Dmrd { seq: 9, src: 1234567, dst: 108703, peer: 3112345, slot: 2, private: false, kind: Kind::Voice(4), stream: 0xDEADBEEF, data: [0x5A; 33] };
        assert_eq!(Dmrd::decode(&p.encode()), Some(p.clone()));
        let h = Dmrd { kind: Kind::Data(DT_VOICE_LC_HEADER), slot: 1, ..p.clone() };
        assert_eq!(h.encode()[15], 0x21);
        assert_eq!(Dmrd { kind: Kind::VoiceSync, ..p }.encode()[15], 0x90);
    }

    #[test]
    fn silence_is_the_standard_codeword() {
        let mut d = [0u8; 264];
        d[..72].copy_from_slice(&unpack(&[0xB9, 0xE8, 0x81, 0x52, 0x61, 0x73, 0x00, 0x2A, 0x6B]));
        let f = voice_burst(1, 0, &[silence_frame(); 3], None);
        assert_eq!(&unpack(&f)[..72], &d[..72]);
    }
}
