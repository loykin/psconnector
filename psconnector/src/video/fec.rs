//! Forward error correction for video frames.
//!
//! The console sends each frame as `k` source units followed by `m` FEC units. The FEC units are a
//! Cauchy Reed-Solomon code over GF(2^8), byte by byte, exactly as jerasure's
//! `cauchy_original_coding_matrix(k, m, 8)` (used by chiaki and pyremoteplay): parity row `i`, column
//! `j` is `1 / (i ^ (m + j))`, with the field polynomial x^8+x^4+x^3+x^2+1 (0x11D). Any `k` of the
//! `k + m` units are enough to rebuild the missing source units.

use std::sync::OnceLock;

struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

fn gf() -> &'static Gf {
    static TABLES: OnceLock<Gf> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for (i, e) in exp.iter_mut().take(255).enumerate() {
            *e = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11D;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        Gf { exp, log }
    })
}

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let t = gf();
    t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
}

fn inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    let t = gf();
    t.exp[255 - t.log[a as usize] as usize]
}

/// Row `r` of the generator: the identity for source units, Cauchy rows for FEC units.
fn generator_row(r: usize, k: usize, m: usize) -> Vec<u8> {
    if r < k {
        (0..k).map(|j| u8::from(j == r)).collect()
    } else {
        let i = r - k;
        (0..k).map(|j| inv((i ^ (m + j)) as u8)).collect()
    }
}

/// Builds the FEC units for `k` equally long source units (used by tests; the console makes them).
#[cfg(test)]
pub(crate) fn encode(sources: &[Vec<u8>], m: usize) -> Vec<Vec<u8>> {
    let k = sources.len();
    let len = sources.first().map_or(0, Vec::len);
    (0..m)
        .map(|i| {
            let row = generator_row(k + i, k, m);
            let mut out = vec![0u8; len];
            for (c, src) in row.iter().zip(sources) {
                for (o, s) in out.iter_mut().zip(src) {
                    *o ^= mul(*c, *s);
                }
            }
            out
        })
        .collect()
}

/// Inverts a k×k matrix over GF(2^8). None if it is singular.
fn invert(mut a: Vec<Vec<u8>>) -> Option<Vec<Vec<u8>>> {
    let n = a.len();
    let mut b: Vec<Vec<u8>> = (0..n).map(|r| (0..n).map(|c| u8::from(r == c)).collect()).collect();
    for col in 0..n {
        let pivot = (col..n).find(|&r| a[r][col] != 0)?;
        a.swap(col, pivot);
        b.swap(col, pivot);
        let p = inv(a[col][col]);
        for c in 0..n {
            a[col][c] = mul(a[col][c], p);
            b[col][c] = mul(b[col][c], p);
        }
        for r in 0..n {
            if r != col && a[r][col] != 0 {
                let f = a[r][col];
                for c in 0..n {
                    a[r][c] ^= mul(f, a[col][c]);
                    b[r][c] ^= mul(f, b[col][c]);
                }
            }
        }
    }
    Some(b)
}

/// Rebuilds missing source units. `units` holds all `k + m` units in order (`None` = lost), each
/// zero-padded to the same length by the caller. Returns the `k` source units, or None if fewer than
/// `k` units arrived.
pub(crate) fn recover(units: &[Option<Vec<u8>>], k: usize, len: usize) -> Option<Vec<Vec<u8>>> {
    let m = units.len().checked_sub(k)?;
    let present: Vec<usize> = (0..units.len()).filter(|&i| units[i].is_some()).take(k).collect();
    if present.len() < k {
        return None;
    }
    if present.iter().all(|&i| i < k) {
        return Some(units[..k].iter().map(|u| u.clone().unwrap_or_default()).collect());
    }
    let rows: Vec<Vec<u8>> = present.iter().map(|&r| generator_row(r, k, m)).collect();
    let decode = invert(rows)?;
    let shard = |i: usize| -> &[u8] { units[i].as_deref().unwrap_or(&[]) };
    let mut out = Vec::with_capacity(k);
    for (j, row) in decode.iter().enumerate() {
        if let Some(Some(u)) = units.get(j).filter(|u| u.is_some()) {
            out.push(u.clone());
            continue;
        }
        let mut data = vec![0u8; len];
        for (c, &src) in row.iter().zip(&present) {
            if *c == 0 {
                continue;
            }
            for (d, s) in data.iter_mut().zip(shard(src)) {
                *d ^= mul(*c, *s);
            }
        }
        out.push(data);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_inverse_and_matrix_entries() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1);
        }
        // jerasure's cauchy_original_coding_matrix(k=2, m=1, w=8): 1/(0^1), 1/(0^2)
        assert_eq!(generator_row(2, 2, 1), vec![1, inv(2)]);
    }

    #[test]
    fn rebuilds_any_lost_units_up_to_m() {
        let k = 6;
        let m = 3;
        let src: Vec<Vec<u8>> = (0..k).map(|i| (0..97).map(|b| (i * 31 + b * 7) as u8).collect()).collect();
        let fec = encode(&src, m);
        let all: Vec<Vec<u8>> = src.iter().chain(&fec).cloned().collect();
        for lost in [vec![0], vec![5], vec![1, 4], vec![0, 2, 5], vec![3, 6, 8]] {
            let units: Vec<Option<Vec<u8>>> =
                all.iter().enumerate().map(|(i, u)| (!lost.contains(&i)).then(|| u.clone())).collect();
            assert_eq!(recover(&units, k, 97).unwrap(), src, "lost {lost:?}");
        }
        let too_many: Vec<Option<Vec<u8>>> = all.iter().enumerate().map(|(i, u)| (i >= 4).then(|| u.clone())).collect();
        assert!(recover(&too_many, k, 97).is_none(), "5 of 9 units cannot rebuild k = 6");
    }

    /// Checks recovery against units captured from a real console (see PSCONNECTOR_DUMP_VIDEO in stream.rs):
    /// `PSCONNECTOR_FEC_DUMP=/path cargo test -p psconnector --release real_capture -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_capture() {
        let path = std::env::var("PSCONNECTOR_FEC_DUMP").expect("PSCONNECTOR_FEC_DUMP");
        let text = std::fs::read_to_string(path).unwrap();
        type Captured = (usize, usize, Vec<Option<Vec<u8>>>);
        let mut frames: std::collections::BTreeMap<u16, Captured> = Default::default();
        for line in text.lines() {
            let f: Vec<&str> = line.split(' ').collect();
            let (frame, unit, src, total): (u16, usize, usize, usize) =
                (f[0].parse().unwrap(), f[1].parse().unwrap(), f[2].parse().unwrap(), f[3].parse().unwrap());
            let data: Vec<u8> =
                (0..f[4].len()).step_by(2).map(|i| u8::from_str_radix(&f[4][i..i + 2], 16).unwrap()).collect();
            let e = frames.entry(frame).or_insert_with(|| (src, total, vec![None; total]));
            e.2[unit] = Some(data);
        }
        let (mut tried, mut ok) = (0, 0);
        for (frame, (k, total, units)) in &frames {
            if units.iter().any(Option::is_none) || total == k {
                continue;
            }
            let lens: Vec<usize> = units.iter().flatten().map(Vec::len).collect();
            let len = *lens.iter().max().unwrap();
            for lost in 0..*k {
                let padded: Vec<Option<Vec<u8>>> = units
                    .iter()
                    .enumerate()
                    .map(|(i, u)| {
                        (i != lost).then(|| {
                            let mut v = u.clone().unwrap();
                            v.resize(len, 0);
                            v
                        })
                    })
                    .collect();
                let got = recover(&padded, *k, len).unwrap();
                let want = units[lost].as_ref().unwrap();
                tried += 1;
                if &got[lost][..want.len()] == want.as_slice() {
                    ok += 1;
                } else if tried - ok <= 3 {
                    println!("frame {frame}: k={k} m={} lens={lens:?} unit {lost} differs", total - k);
                }
            }
        }
        println!("real capture: {ok} of {tried} lost units rebuilt exactly");
        assert!(tried > 0 && ok == tried);
    }
}
