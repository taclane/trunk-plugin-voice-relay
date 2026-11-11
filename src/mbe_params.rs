//! A voice frame's MBE model parameters (fundamental, voicing, spectral
//! amplitudes) as the decoders dequantize them, frame by frame: the first
//! step of decoding, without synthesis. The IMBE → AMBE+2 conversion reads
//! IMBE frames with it, and keeps a second one fed with what it sends, to
//! predict as a radio's decoder does.
//!
//! The dequantizers are copied from trunk-core's vocoder (GPL-3.0-or-later),
//! itself derived in part from mbelib 1.3.0:
//!
//! Copyright (C) 2010 mbelib Author
//! GPG Key ID: 0xEA5EFE2C (9E7A 5527 9CDC EBF7 BF1B  D772 4F98 E863 EA5E FE2C)
//!
//! Permission to use, copy, modify, and/or distribute this software for any
//! purpose with or without fee is hereby granted, provided that the above
//! copyright notice and this permission notice appear in all copies.
//!
//! THE SOFTWARE IS PROVIDED "AS IS" AND ISC DISCLAIMS ALL WARRANTIES WITH
//! REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
//! AND FITNESS.  IN NO EVENT SHALL ISC BE LIABLE FOR ANY SPECIAL, DIRECT,
//! INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
//! LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE
//! OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
//! PERFORMANCE OF THIS SOFTWARE.

// (A port that keeps the original's 1-based loops and arithmetic, so it reads beside it.)
#![allow(clippy::needless_range_loop, clippy::manual_div_ceil)]

use std::f64::consts::PI;

use trunk_core::tables::{
    AMBE_DG, AMBE_HOCB5, AMBE_HOCB6, AMBE_HOCB7, AMBE_HOCB8, AMBE_LMPRBL, AMBE_LTABLE, AMBE_PRBA24, AMBE_PRBA58, AMBE_VUV, AMBE_W0TABLE, B2, BA, BO, HOBA,
    IMBE_JI, QUANTSTEP, STANDDEV,
};

/// One past the most harmonics: the prediction can read one beyond.
const NL: usize = 58;

/// A frame's model parameters: the fundamental (radians a sample), its
/// harmonics' voicing and log2 amplitudes (index 0 is harmonic 1), and
/// AMBE+2's gain.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelParams {
    pub w0: f64,
    pub voiced: Vec<bool>,
    pub log2ml: Vec<f64>,
    pub gamma: f64,
}

impl ModelParams {
    /// Harmonics.
    pub fn l(&self) -> usize {
        self.log2ml.len()
    }
}

/// The decoders' parameter step alone, with the state a decoder carries from
/// frame to frame. Both codecs predict amplitudes from the frame before, so
/// feed every frame of a call, in order. An invalid frame leaves the state as
/// it was (a decoder repeats the frame before).
pub struct ParamDecoder {
    cur: Parms,
    prev: Parms,
}

impl Default for ParamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ParamDecoder {
    pub fn new() -> Self {
        let (mut cur, mut prev, mut enh) = (Parms::default(), Parms::default(), Parms::default());
        init_parms(&mut cur, &mut prev, &mut enh);
        ParamDecoder { cur, prev }
    }

    fn params(p: &Parms) -> ModelParams {
        ModelParams { w0: p.w0, voiced: (1..=p.l).map(|l| p.vl[l] != 0).collect(), log2ml: (1..=p.l).map(|l| p.log2ml[l]).collect(), gamma: p.gamma }
    }

    /// The parameters the next frame is predicted from (the last valid frame's).
    pub fn last(&self) -> ModelParams {
        Self::params(&self.prev)
    }

    /// An IMBE frame's 88 information bits. None: invalid (pitch or L out of range).
    pub fn imbe(&mut self, d: &[u8; 88]) -> Option<ModelParams> {
        if decode_imbe4400_parms(d, &mut self.cur, &mut self.prev) {
            move_parms(&self.cur, &mut self.prev);
            Some(Self::params(&self.cur))
        } else {
            move_parms(&self.prev, &mut self.cur);
            None
        }
    }

    /// An AMBE+2 frame's 49 information bits. None: an erasure or tone frame.
    pub fn ambe(&mut self, d: &[u8; 49]) -> Option<ModelParams> {
        if !tone_frame(d) && decode_ambe2450_parms(d, &mut self.cur, &mut self.prev) == Ambe::Voice {
            move_parms(&self.cur, &mut self.prev);
            Some(Self::params(&self.cur))
        } else {
            move_parms(&self.prev, &mut self.cur);
            None
        }
    }
}

#[derive(Clone)]
struct Parms {
    w0: f64,
    l: usize,
    k: usize,
    vl: [i8; NL],
    ml: [f64; NL],
    log2ml: [f64; NL],
    phil: [f64; NL],
    psil: [f64; NL],
    gamma: f64,
    repeat: u32,
}

impl Default for Parms {
    fn default() -> Self {
        Parms { w0: 0.0, l: 0, k: 0, vl: [0; NL], ml: [0.0; NL], log2ml: [0.0; NL], phil: [0.0; NL], psil: [0.0; NL], gamma: 0.0, repeat: 0 }
    }
}

fn move_parms(cur: &Parms, prev: &mut Parms) {
    prev.w0 = cur.w0;
    prev.l = cur.l;
    prev.k = cur.k;
    prev.ml[0] = 0.0;
    prev.gamma = cur.gamma;
    prev.repeat = cur.repeat;
    for l in 0..=56 {
        prev.ml[l] = cur.ml[l];
        prev.vl[l] = cur.vl[l];
        prev.log2ml[l] = cur.log2ml[l];
        prev.phil[l] = cur.phil[l];
        prev.psil[l] = cur.psil[l];
    }
}

fn init_parms(cur: &mut Parms, prev: &mut Parms, enh: &mut Parms) {
    prev.w0 = 0.09378;
    prev.l = 30;
    prev.k = 10;
    prev.gamma = 0.0;
    for l in 0..=56 {
        prev.ml[l] = 0.0;
        prev.vl[l] = 0;
        prev.log2ml[l] = 0.0;
        prev.phil[l] = 0.0;
        prev.psil[l] = PI / 2.0;
    }
    prev.repeat = 0;
    move_parms(prev, cur);
    move_parms(prev, enh);
}

/// An IMBE frame's parameters into `cur` (`prev`: the frame before). False: invalid.
fn decode_imbe4400_parms(d: &[u8; 88], cur: &mut Parms, prev: &mut Parms) -> bool {
    let mut bb = [[0u8; 12]; 58];
    let mut cik = [[0.0f64; 11]; 7];
    let (mut gm, mut ri) = ([0.0f64; 7], [0.0f64; 7]);
    let mut tl = [0.0f64; NL];
    let mut flokl = [0.0f64; NL];
    let mut deltal = [0.0f64; NL];
    let mut intkl = [0usize; NL];

    cur.repeat = prev.repeat;
    let b0 = ((d[0] as u32) << 7)
        | ((d[1] as u32) << 6)
        | ((d[2] as u32) << 5)
        | ((d[3] as u32) << 4)
        | ((d[4] as u32) << 3)
        | ((d[5] as u32) << 2)
        | ((d[85] as u32) << 1)
        | d[86] as u32;
    if b0 > 207 {
        return false;
    }
    cur.w0 = 4.0 * PI / (b0 as f64 + 39.5);
    let l = (0.9254 * ((PI / cur.w0 + 0.25) as i64) as f64) as usize;
    if !(9..=56).contains(&l) {
        return false;
    }
    cur.l = l;
    let l9 = l - 9;
    let k = if l < 37 { (l + 2) / 3 } else { 12 };
    cur.k = k;

    let bo_base = l9 * 79 * 2;
    for (p, i) in (6..85).enumerate() {
        bb[BO[bo_base + 2 * p] as usize][BO[bo_base + 2 * p + 1] as usize] = d[i];
    }
    let (mut j, mut kk) = (1, k - 1);
    for i in 1..=l {
        cur.vl[i] = bb[1][kk] as i8;
        if j == 3 {
            j = 1;
            kk = kk.saturating_sub(1);
        } else {
            j += 1;
        }
    }
    let b2 = ((bb[2][5] as usize) << 5)
        | ((bb[2][4] as usize) << 4)
        | ((bb[2][3] as usize) << 3)
        | ((bb[2][2] as usize) << 2)
        | ((bb[2][1] as usize) << 1)
        | bb[2][0] as usize;
    gm[1] = B2[b2] as f64;
    let ba_base = l9 * 5 * 2;
    for i in 2..7 {
        let ba1 = BA[ba_base + (i - 2) * 2] as i32;
        let ba2 = BA[ba_base + (i - 2) * 2 + 1] as f64;
        let mut bm = 0i32;
        for jj in (0..ba1).rev() {
            bm = (bm << 1) | bb[i + 1][jj as usize] as i32;
        }
        gm[i] = ba2 * (bm as f64 - 2f64.powi(ba1 - 1) + 0.5);
    }
    for i in 1..=6 {
        ri[i] = (1..=6).map(|m| (if m == 1 { 1.0 } else { 2.0 }) * gm[m] * (PI * (m - 1) as f64 * (i as f64 - 0.5) / 6.0).cos()).sum();
    }
    let mut m = 8usize;
    for i in 1..=6 {
        cik[i][1] = ri[i];
        let ji = IMBE_JI[l9 * 6 + i - 1] as usize;
        for kk in 2..=ji {
            let bm_bits = HOBA[l9 * 50 + m - 8] as i32;
            cik[i][kk] = if bm_bits == 0 {
                0.0
            } else {
                let mut bm = 0i32;
                for b in 0..bm_bits {
                    bm = (bm << 1) | bb[m][(bm_bits - b - 1) as usize] as i32;
                }
                QUANTSTEP[(bm_bits - 1) as usize] as f64 * STANDDEV[kk - 2] as f64 * (bm as f64 - 2f64.powi(bm_bits - 1) + 0.5)
            };
            m += 1;
        }
    }
    let mut li = 1;
    for i in 1..=6 {
        let ji = IMBE_JI[l9 * 6 + i - 1] as usize;
        for jj in 1..=ji {
            tl[li] = (1..=ji).map(|kk| (if kk == 1 { 1.0 } else { 2.0 }) * cik[i][kk] * (PI * (kk - 1) as f64 * (jj as f64 - 0.5) / ji as f64).cos()).sum();
            li += 1;
        }
    }
    let rho = if cur.l <= 15 {
        0.4
    } else if cur.l <= 24 {
        0.03 * cur.l as f64 - 0.05
    } else {
        0.7
    };
    if cur.l > prev.l {
        for l in prev.l + 1..=cur.l {
            prev.ml[l] = prev.ml[prev.l];
            prev.log2ml[l] = prev.log2ml[prev.l];
        }
    }
    let mut sum77 = 0.0;
    for l in 1..=cur.l {
        flokl[l] = (prev.l as f64 / cur.l as f64) * l as f64;
        intkl[l] = flokl[l] as usize;
        deltal[l] = flokl[l] - intkl[l] as f64;
        sum77 += (1.0 - deltal[l]) * prev.log2ml[intkl[l]] + deltal[l] * prev.log2ml[intkl[l] + 1];
    }
    sum77 *= rho / cur.l as f64;
    for l in 1..=cur.l {
        let c1 = rho * (1.0 - deltal[l]) * prev.log2ml[intkl[l]];
        let c2 = rho * deltal[l] * prev.log2ml[intkl[l] + 1];
        cur.log2ml[l] = tl[l] + c1 + c2 - sum77;
        cur.ml[l] = 2f64.powf(cur.log2ml[l]);
    }
    true
}

/// What an AMBE+2 frame held.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ambe {
    Voice,
    Erasure,
    Tone,
}

/// A tone frame: its first six bits all ones.
fn tone_frame(d: &[u8; 49]) -> bool {
    !d[..6].contains(&0)
}

/// An AMBE+2 frame's parameters into `cur` (`prev`: the frame before).
fn decode_ambe2450_parms(d: &[u8; 49], cur: &mut Parms, prev: &mut Parms) -> Ambe {
    let bit = |i: usize| d[i] as usize;
    let mut cik = [[0.0f64; 18]; 5];
    let mut gm = [0.0f64; 9];
    let mut ri = [0.0f64; 9];
    let mut tl = [0.0f64; NL];
    let mut flokl = [0.0f64; NL];
    let mut deltal = [0.0f64; NL];
    let mut intkl = [0usize; NL];
    let mut silence = false;
    let mut f0 = 0.0;
    let mut l_count = 0usize;

    cur.repeat = prev.repeat;
    let b0 = (bit(0) << 6) | (bit(1) << 5) | (bit(2) << 4) | (bit(3) << 3) | (bit(37) << 2) | (bit(38) << 1) | bit(39);
    match b0 {
        120..=123 => return Ambe::Erasure,
        124 | 125 => {
            silence = true;
            cur.w0 = 2.0 * PI / 32.0;
            f0 = 1.0 / 32.0;
            l_count = 14;
            cur.l = 14;
            for l in 1..=l_count {
                cur.vl[l] = 0;
            }
        }
        126 | 127 => return Ambe::Tone,
        _ => {}
    }
    if !silence {
        f0 = AMBE_W0TABLE[b0] as f64;
        cur.w0 = f0 * 2.0 * PI;
    }
    let unvc = 0.2046 / cur.w0.sqrt();
    if !silence {
        l_count = AMBE_LTABLE[b0] as usize;
        cur.l = l_count;
    }

    // V/UV
    let b1 = (bit(4) << 4) | (bit(5) << 3) | (bit(6) << 2) | (bit(7) << 1) | bit(35);
    for l in 1..=l_count {
        let jl = (l as f64 * 16.0 * f0) as usize;
        if !silence {
            cur.vl[l] = AMBE_VUV[b1 * 8 + jl] as i8;
        }
    }
    // gain
    let b2 = (bit(8) << 4) | (bit(9) << 3) | (bit(10) << 2) | (bit(11) << 1) | bit(36);
    cur.gamma = AMBE_DG[b2] as f64 + 0.5 * prev.gamma;

    // PRBA vectors
    let b3 = (bit(12) << 8) | (bit(13) << 7) | (bit(14) << 6) | (bit(15) << 5) | (bit(16) << 4) | (bit(17) << 3) | (bit(18) << 2) | (bit(19) << 1) | bit(40);
    for k in 0..3 {
        gm[2 + k] = AMBE_PRBA24[b3 * 3 + k] as f64;
    }
    let b4 = (bit(20) << 6) | (bit(21) << 5) | (bit(22) << 4) | (bit(23) << 3) | (bit(41) << 2) | (bit(42) << 1) | bit(43);
    for k in 0..4 {
        gm[5 + k] = AMBE_PRBA58[b4 * 4 + k] as f64;
    }
    for i in 1..=8 {
        ri[i] = (1..=8).map(|m| (if m == 1 { 1.0 } else { 2.0 }) * gm[m] * (PI * (m - 1) as f64 * (i as f64 - 0.5) / 8.0).cos()).sum();
    }
    let rconst = 1.0 / (2.0 * std::f64::consts::SQRT_2);
    for i in 1..=4 {
        cik[i][1] = 0.5 * (ri[2 * i - 1] + ri[2 * i]);
        cik[i][2] = rconst * (ri[2 * i - 1] - ri[2 * i]);
    }

    // HOC
    let b5 = (bit(24) << 4) | (bit(25) << 3) | (bit(26) << 2) | (bit(27) << 1) | bit(44);
    let b6 = (bit(28) << 3) | (bit(29) << 2) | (bit(30) << 1) | bit(45);
    let b7 = (bit(31) << 3) | (bit(32) << 2) | (bit(33) << 1) | bit(46);
    let b8 = (bit(34) << 2) | (bit(47) << 1) | bit(48);
    let ji: [usize; 5] = [
        0,
        AMBE_LMPRBL[l_count * 4] as usize,
        AMBE_LMPRBL[l_count * 4 + 1] as usize,
        AMBE_LMPRBL[l_count * 4 + 2] as usize,
        AMBE_LMPRBL[l_count * 4 + 3] as usize,
    ];
    let hoc: [(&[f32], usize); 4] = [(&AMBE_HOCB5, b5), (&AMBE_HOCB6, b6), (&AMBE_HOCB7, b7), (&AMBE_HOCB8, b8)];
    for i in 1..=4 {
        let (tbl, bi) = hoc[i - 1];
        for k in 3..=ji[i] {
            cik[i][k] = if k > 6 { 0.0 } else { tbl[bi * 4 + k - 3] as f64 };
        }
    }
    // inverse DCT of each C(i,k) → T(l)
    let mut l = 1;
    for i in 1..=4 {
        for j in 1..=ji[i] {
            tl[l] = (1..=ji[i]).map(|k| (if k == 1 { 1.0 } else { 2.0 }) * cik[i][k] * (PI * (k - 1) as f64 * (j as f64 - 0.5) / ji[i] as f64).cos()).sum();
            l += 1;
        }
    }

    if cur.l > prev.l {
        for l in prev.l + 1..=cur.l {
            prev.ml[l] = prev.ml[prev.l];
            prev.log2ml[l] = prev.log2ml[prev.l];
        }
    }
    prev.log2ml[0] = prev.log2ml[1];
    prev.ml[0] = prev.ml[1];

    let mut sum43 = 0.0;
    for l in 1..=cur.l {
        flokl[l] = (prev.l as f64 / cur.l as f64) * l as f64;
        intkl[l] = flokl[l] as usize;
        deltal[l] = flokl[l] - intkl[l] as f64;
        sum43 += (1.0 - deltal[l]) * prev.log2ml[intkl[l]] + deltal[l] * prev.log2ml[intkl[l] + 1];
    }
    sum43 *= 0.65 / cur.l as f64;
    let sum42 = (1..=cur.l).map(|l| tl[l]).sum::<f64>() / cur.l as f64;
    let big_gamma = cur.gamma - 0.5 * ((cur.l as f64).ln() / 2f64.ln()) - sum42;
    for l in 1..=cur.l {
        let c1 = 0.65 * (1.0 - deltal[l]) * prev.log2ml[intkl[l]];
        let c2 = 0.65 * deltal[l] * prev.log2ml[intkl[l] + 1];
        cur.log2ml[l] = tl[l] + c1 + c2 - sum43 + big_gamma;
        cur.ml[l] = if cur.vl[l] == 1 { (0.693 * cur.log2ml[l]).exp() } else { unvc * (0.693 * cur.log2ml[l]).exp() };
    }
    Ambe::Voice
}
