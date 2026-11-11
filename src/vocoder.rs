//! IMBE (P25 Phase 1, 4400 bps) → AMBE+2 (half rate, 2450 bps) by the
//! vocoder's model parameters, never through PCM.
//!
//! Each IMBE frame is dequantized to its fundamental, voicing and spectral
//! amplitudes ([`ParamDecoder`]), the amplitudes are resampled onto the
//! half-rate harmonics, and the result is quantized against the half-rate
//! decoder's own tables: b0 the nearest fundamental, b1 the voicing codeword
//! that fits best, b2 the differential gain, b3–b4 the PRBA vector, b5–b8
//! the higher-order DCT coefficients. The amplitudes are coded as a
//! prediction from the frame before, so a decoder runs in the loop (a second
//! [`ParamDecoder`], fed what we send) and the prediction is the one a radio
//! makes, not ours.

use std::f64::consts::PI;

use crate::mbe_params::{ModelParams, ParamDecoder};
use trunk_core::tables::{AMBE_DG, AMBE_HOCB5, AMBE_HOCB6, AMBE_HOCB7, AMBE_HOCB8, AMBE_LMPRBL, AMBE_LTABLE, AMBE_PRBA24, AMBE_PRBA58, AMBE_VUV, AMBE_W0TABLE};

/// The half-rate erasure frame (b0 = 120): the decoder repeats the frame before.
pub fn erasure_frame() -> [u8; 49] {
    let mut u = [0u8; 49];
    u[0..4].copy_from_slice(&[1, 1, 1, 1]);
    u
}

/// b0…b8 into the 49 information bits, MSB first, as the decoder reads them
/// (the order trunk-core's decoders take).
pub fn pack(b: &[usize; 9]) -> [u8; 49] {
    let mut d = [0u8; 49];
    let mut put = |v: usize, bits: &[(usize, usize)]| {
        for &(pos, bit) in bits {
            d[pos] = (v >> bit & 1) as u8;
        }
    };
    put(b[0], &[(0, 6), (1, 5), (2, 4), (3, 3), (37, 2), (38, 1), (39, 0)]);
    put(b[1], &[(4, 4), (5, 3), (6, 2), (7, 1), (35, 0)]);
    put(b[2], &[(8, 4), (9, 3), (10, 2), (11, 1), (36, 0)]);
    put(b[3], &[(12, 8), (13, 7), (14, 6), (15, 5), (16, 4), (17, 3), (18, 2), (19, 1), (40, 0)]);
    put(b[4], &[(20, 6), (21, 5), (22, 4), (23, 3), (41, 2), (42, 1), (43, 0)]);
    put(b[5], &[(24, 4), (25, 3), (26, 2), (27, 1), (44, 0)]);
    put(b[6], &[(28, 3), (29, 2), (30, 1), (45, 0)]);
    put(b[7], &[(31, 3), (32, 2), (33, 1), (46, 0)]);
    put(b[8], &[(34, 2), (47, 1), (48, 0)]);
    d
}

fn nearest(n: usize, dim: usize, table: &[f32], target: &[f64]) -> usize {
    let used = target.len().min(dim);
    let d = |i: usize| (0..used).map(|k| (table[i * dim + k] as f64 - target[k]).powi(2)).sum::<f64>();
    (0..n).min_by(|&a, &b| d(a).total_cmp(&d(b))).unwrap_or(0)
}

/// Linear interpolation of a 1-based harmonic series at `x` (clamped to its ends).
fn at(v: &[f64], x: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let x = x.clamp(1.0, v.len() as f64);
    let i = x.floor() as usize;
    let f = x - i as f64;
    let a = v[i - 1];
    let b = *v.get(i).unwrap_or(&a);
    a + f * (b - a)
}

/// The half-rate pitch index nearest `w0` (radians a sample).
pub fn pitch_index(w0: f64) -> usize {
    let d = |b: usize| (AMBE_W0TABLE[b] as f64 * 2.0 * PI - w0).abs();
    (0..120).min_by(|&a, &b| d(a).total_cmp(&d(b))).unwrap()
}

/// Quantize one frame's model parameters (`w0`, voicing and log2 amplitudes
/// per harmonic) given the decoder state `prev` (its last frame). Returns
/// b0…b8.
pub fn quantize(w0: f64, voiced: &[bool], log2m: &[f64], prev: &ModelParams) -> [usize; 9] {
    // b0: the nearest fundamental (the table holds ω0 / 2π).
    let b0 = pitch_index(w0);
    let f0 = AMBE_W0TABLE[b0] as f64;
    let w0a = f0 * 2.0 * PI;
    let l = AMBE_LTABLE[b0] as usize;
    // The source's harmonics resampled onto ours: harmonic h sits at h·w0a,
    // the source's harmonic h·w0a / w0.
    let src_l = log2m.len().max(1);
    let x = |h: usize| h as f64 * w0a / w0;
    let m: Vec<f64> = (1..=l).map(|h| at(log2m, x(h))).collect();
    let v_src: Vec<bool> = (1..=l).map(|h| voiced.get((x(h).round() as usize).clamp(1, src_l) - 1).copied().unwrap_or(false)).collect();
    // b1: of the first 17 voicing vectors, the one nearest, weighted by |M|².
    let jl = |h: usize| ((h as f64 * 16.0 * f0) as usize).min(7);
    let weight: Vec<f64> = m.iter().map(|lm| 2f64.powf(2.0 * lm)).collect();
    let e = |n: usize| (1..=l).filter(|&h| (AMBE_VUV[n * 8 + jl(h)] != 0) != v_src[h - 1]).map(|h| weight[h - 1]).sum::<f64>();
    let b1 = (0..17).min_by(|&a, &b| e(a).total_cmp(&e(b))).unwrap();
    let voiced_a: Vec<bool> = (1..=l).map(|h| AMBE_VUV[b1 * 8 + jl(h)] != 0).collect();
    // Λ: voiced harmonics carry 0.5·log2 L, unvoiced ones the decoder's
    // unvoiced scale too, so a decoder gives back the same amplitudes.
    let lf = l as f64;
    let lam: Vec<f64> = (0..l).map(|i| if voiced_a[i] { m[i] + 0.5 * lf.log2() } else { m[i] + 0.5 * (w0a * lf).log2() + 2.289 }).collect();
    // b2: the differential gain.
    let gamma = lam.iter().sum::<f64>() / lf;
    let dg = gamma - 0.5 * prev.gamma;
    let b2 = (0..32).min_by(|&a, &b| (AMBE_DG[a] as f64 - dg).abs().total_cmp(&(AMBE_DG[b] as f64 - dg).abs())).unwrap();
    // Prediction residuals, from the decoder's last amplitudes
    // (it keeps Λ − 0.5·log2 L: a constant, which the mean removes).
    let pl = prev.l().max(1);
    let pv = |k: usize| if k == 0 { prev.log2ml.first().copied().unwrap_or(0.0) } else { prev.log2ml.get(k.min(pl) - 1).copied().unwrap_or(0.0) };
    let pred: Vec<f64> = (1..=l)
        .map(|h| {
            let k = pl as f64 / lf * h as f64;
            let (ki, d) = (k.floor() as usize, k - k.floor());
            0.65 * ((1.0 - d) * pv(ki) + d * pv(ki + 1))
        })
        .collect();
    let pm = pred.iter().sum::<f64>() / lf;
    let t: Vec<f64> = (0..l).map(|i| lam[i] - pred[i] + pm).collect();
    // Four blocks (the decoder's block lengths for L), a DCT each.
    let ji: [usize; 4] = std::array::from_fn(|i| AMBE_LMPRBL[l * 4 + i] as usize);
    let mut c: Vec<Vec<f64>> = Vec::new();
    let mut from = 0;
    for &j in &ji {
        let block = &t[from.min(l)..(from + j).min(l)];
        from += j;
        let n = block.len().max(1) as f64;
        c.push((1..=j).map(|k| block.iter().enumerate().map(|(jj, v)| v * (PI * (k - 1) as f64 * (jj as f64 + 0.5) / n).cos()).sum::<f64>() / n).collect());
    }
    // The PRBA vector and its DCT.
    let s2 = std::f64::consts::SQRT_2;
    let mut r = [0.0f64; 8];
    for i in 0..4 {
        let (c1, c2) = (c[i].first().copied().unwrap_or(0.0), c[i].get(1).copied().unwrap_or(0.0));
        r[2 * i] = c1 + s2 * c2;
        r[2 * i + 1] = c1 - s2 * c2;
    }
    let g: Vec<f64> = (1..=8).map(|mm| (0..8).map(|i| r[i] * (PI * (mm - 1) as f64 * (i as f64 + 0.5) / 8.0).cos()).sum::<f64>() / 8.0).collect();
    let b3 = nearest(512, 3, &AMBE_PRBA24, &g[1..4]);
    let b4 = nearest(128, 4, &AMBE_PRBA58, &g[4..8]);
    // Higher-order coefficients: each block's C(i,3…), its first four at most.
    let hoc: [(&[f32], usize); 4] = [(&AMBE_HOCB5, 32), (&AMBE_HOCB6, 16), (&AMBE_HOCB7, 16), (&AMBE_HOCB8, 8)];
    let mut b = [b0, b1, b2, b3, b4, 0, 0, 0, 0];
    for i in 0..4 {
        if ji[i] > 2 {
            let h: Vec<f64> = c[i][2..].iter().take(4).copied().collect();
            b[5 + i] = nearest(hoc[i].1, 4, hoc[i].0, &h);
        }
    }
    b
}

/// One call's conversion: the IMBE decoder's state, and the state of the
/// AMBE+2 decoder our frames go to.
pub struct ImbeToAmbe {
    imbe: ParamDecoder,
    ambe: ParamDecoder,
}

impl ImbeToAmbe {
    pub fn new() -> Self {
        ImbeToAmbe { imbe: ParamDecoder::new(), ambe: ParamDecoder::new() }
    }

    pub fn convert(&mut self, imbe: &[u8; 88]) -> [u8; 49] {
        let Some(p) = self.imbe.imbe(imbe) else { return self.lost() };
        let b = quantize(p.w0, &p.voiced, &p.log2ml, &self.ambe.last());
        let d = pack(&b);
        // What a radio makes of it is what the next frame is predicted from.
        self.ambe.ambe(&d);
        d
    }

    /// A frame lost on the air: an erasure (the radio repeats the frame before).
    pub fn lost(&mut self) -> [u8; 49] {
        erasure_frame()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame quantized from a decoder's own parameters comes back close.
    #[test]
    fn quantized_parameters_decode_close_to_the_source() {
        let mut dec = ParamDecoder::new();
        let mut errs = Vec::new();
        for frame in 0..40 {
            let w0 = 2.0 * PI * (0.02 + 0.002 * (frame % 7) as f64);
            let l = AMBE_LTABLE[pitch_index(w0)] as usize;
            // A falling spectrum, voiced low, unvoiced high.
            let log2m: Vec<f64> = (1..=l).map(|h| 9.0 - 6.0 * h as f64 / l as f64 + 0.5 * ((frame + h) as f64 * 0.7).sin()).collect();
            let voiced: Vec<bool> = (1..=l).map(|h| h * 2 < l).collect();
            let b = quantize(w0, &voiced, &log2m, &dec.last());
            let got = dec.ambe(&pack(&b)).expect("a voice frame");
            assert_eq!(got.l(), l);
            assert!((got.w0 - w0).abs() / w0 < 0.03, "w0 {} vs {}", got.w0, w0);
            let agree = got.voiced.iter().zip(&voiced).filter(|(a, b)| a == b).count();
            assert!(agree * 10 >= l * 7, "voicing {agree}/{l}");
            // Amplitudes, as the decoder plays them (unvoiced scaled), against the source's.
            let unvc = (0.2046 / got.w0.sqrt()).log2();
            let e: f64 = (0..l).map(|i| (got.log2ml[i] + if got.voiced[i] { 0.0 } else { unvc } - log2m[i]).powi(2)).sum::<f64>() / l as f64;
            if frame > 3 {
                errs.push(e.sqrt());
            }
        }
        let mean = errs.iter().sum::<f64>() / errs.len() as f64;
        eprintln!("mean amplitude error {mean:.3} log2 ({:.1} dB)", mean * 6.02);
        // log2 units: 1.0 is 6 dB.
        assert!(mean < 1.0, "mean amplitude error {mean:.2} (log2)");
    }

    #[test]
    fn packing_matches_the_decoders_reading() {
        let mut d = ParamDecoder::new();
        assert!(d.ambe(&pack(&[124, 0, 0, 0, 0, 0, 0, 0, 0])).is_some(), "silence decodes as a frame");
        assert!(d.ambe(&erasure_frame()).is_none());
        // Every field lands where the decoder reads it: b0 alone moves the pitch.
        let a = d.ambe(&pack(&[10, 0, 16, 0, 0, 0, 0, 0, 0])).unwrap();
        let b = d.ambe(&pack(&[90, 0, 16, 0, 0, 0, 0, 0, 0])).unwrap();
        assert!((a.w0 - AMBE_W0TABLE[10] as f64 * 2.0 * PI).abs() < 1e-6 && (b.w0 - AMBE_W0TABLE[90] as f64 * 2.0 * PI).abs() < 1e-6);
    }
}
