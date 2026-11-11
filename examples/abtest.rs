//! A/B a Phase 1 call: its IMBE as TRP plays it, against the same frames
//! converted to AMBE+2 (as a DMR radio would get them) and played by TRP's
//! AMBE+2 decoder. Writes both WAVs and says how far apart their spectra are.
//!
//! ```text
//! cargo run --release --example abtest -- <call.sdr> <out-prefix>
//! ```

#[path = "../src/dmr.rs"]
#[allow(dead_code)]
mod dmr;
#[path = "../src/mbe_params.rs"]
#[allow(dead_code)]
mod mbe_params;
#[path = "../src/vocoder.rs"]
#[allow(dead_code)]
mod vocoder;

use trunk_core::mbe::{lcg, to_limited, Decoder, Profile, FRAME_SAMPLES};
use trunk_core::trunk::frames::Codec;

/// Log energies in 20 bands from 100 Hz to 3.9 kHz (a plain DFT of a
/// Hann-windowed frame).
fn bands(x: &[f32]) -> [f64; 20] {
    let n = x.len();
    let mut out = [0f64; 20];
    for (b, o) in out.iter_mut().enumerate() {
        let (f1, f2) = (100.0 + b as f64 * 190.0, 290.0 + b as f64 * 190.0);
        let mut e = 0.0;
        let mut f = f1;
        while f < f2 {
            let w = 2.0 * std::f64::consts::PI * f / 8000.0;
            let (mut re, mut im) = (0.0, 0.0);
            for (i, &s) in x.iter().enumerate() {
                let h = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
                re += s as f64 * h * (w * i as f64).cos();
                im -= s as f64 * h * (w * i as f64).sin();
            }
            e += re * re + im * im;
            f += 31.25;
        }
        *o = 10.0 * (e + 1e-9).log10();
    }
    out
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (path, prefix) = (&a[1], &a[2]);
    let frames = trunk_core::trunk::sdr::sdr_frames(&std::fs::read(path).unwrap());
    let mut ref_dec = Decoder::new(lcg(1), Profile::Enhanced);
    let mut amb_dec = Decoder::new(lcg(1), Profile::Enhanced);
    // The same IMBE again with other noise: how far apart two plays of one signal are.
    let mut alt_dec = Decoder::new(lcg(7), Profile::Enhanced);
    let mut xa = Vec::new();
    let mut conv = vocoder::ImbeToAmbe::new();
    let (mut ra, mut aa) = (Vec::new(), Vec::new());
    let mut buf = [0f32; FRAME_SAMPLES];
    let mut n_imbe = 0;
    for f in &frames {
        if f.codec != Codec::Imbe {
            continue;
        }
        n_imbe += 1;
        let d: [u8; 88] = std::array::from_fn(|i| f.bits[i]);
        ref_dec.imbe(&d, f.e0, if f.erased { 0 } else { f.errs }, f.erased, &mut buf);
        to_limited(&mut buf);
        ra.extend_from_slice(&buf);
        alt_dec.imbe(&d, f.e0, if f.erased { 0 } else { f.errs }, f.erased, &mut buf);
        to_limited(&mut buf);
        xa.extend_from_slice(&buf);
        let am = if f.bad() { conv.lost() } else { conv.convert(&d) };
        amb_dec.ambe(&am, 0, &mut buf);
        to_limited(&mut buf);
        aa.extend_from_slice(&buf);
    }
    std::fs::write(format!("{prefix}.imbe.wav"), trunk_core::wav::encode(&ra, 8000)).unwrap();
    std::fs::write(format!("{prefix}.ambe.wav"), trunk_core::wav::encode(&aa, 8000)).unwrap();
    // Log-spectral distance over frames with speech in them; and the same
    // with the second signal a frame late, to show what misalignment costs.
    let rms = |v: &[f32]| (v.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
    let peak = ra.chunks(FRAME_SAMPLES).map(rms).fold(0.0, f64::max);
    let lsd_of = |other: &[f32]| -> Vec<f64> {
        let mut v = Vec::new();
        for (r, c) in ra.chunks(FRAME_SAMPLES).zip(other.chunks(FRAME_SAMPLES)) {
            if rms(r) < peak * 0.1 {
                continue;
            }
            let (br, bc) = (bands(r), bands(c));
            v.push((br.iter().zip(&bc).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / 20.0).sqrt());
        }
        v
    };
    let mut lsd = lsd_of(&aa);
    let floor = lsd_of(&xa);
    println!("noise floor (same IMBE, other noise): mean {:.1} dB", floor.iter().sum::<f64>() / floor.len().max(1) as f64);
    lsd.sort_by(|a, b| a.total_cmp(b));
    let mean = lsd.iter().sum::<f64>() / lsd.len().max(1) as f64;
    println!(
        "{n_imbe} IMBE frames; {} with speech: log-spectral distance mean {mean:.1} dB, median {:.1}, 90th {:.1}; level IMBE {:.1} dBFS, AMBE {:.1} dBFS",
        lsd.len(),
        lsd.get(lsd.len() / 2).unwrap_or(&0.0),
        lsd.get(lsd.len() * 9 / 10).unwrap_or(&0.0),
        20.0 * rms(&ra).log10(),
        20.0 * rms(&aa).log10()
    );
}
