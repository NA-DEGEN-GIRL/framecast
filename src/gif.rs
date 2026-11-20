//! Animated GIF encoder, written from scratch (no image crates).
//!
//! Screen recordings are mostly static, so each frame only stores the
//! rectangle that changed, with unchanged pixels inside it marked transparent.
//! Each frame gets its own 255-color palette (exact when the region has few
//! colors, median-cut otherwise) with a fixed ordered dither, which keeps
//! gradients smooth without the shimmering error-diffusion dithering causes.

use std::io::{BufWriter, Write};
use std::path::Path;

use crate::util::{Ctx, Res};

const TRANSPARENT: u8 = 255;
/// Sum of |ΔR|+|ΔG|+|ΔB| above which a pixel counts as changed. Absorbs video noise.
const CHANGE_THRESHOLD: i32 = 18;

pub struct GifWriter {
    f: BufWriter<std::fs::File>,
    w: usize,
    h: usize,
    /// Source pixels as of the last time each pixel was emitted.
    shown: Vec<[u8; 3]>,
    first: bool,
    /// Last encoded frame, held back so identical following frames can extend its delay.
    pending: Option<(Vec<u8>, u32)>,
}

impl GifWriter {
    pub fn create(path: &Path, w: u32, h: u32) -> Res<GifWriter> {
        let mut f = BufWriter::new(std::fs::File::create(path).ctx("create gif")?);
        let mut hdr = Vec::new();
        hdr.extend_from_slice(b"GIF89a");
        hdr.extend_from_slice(&(w as u16).to_le_bytes());
        hdr.extend_from_slice(&(h as u16).to_le_bytes());
        hdr.extend_from_slice(&[0x70, 0, 0]); // no global palette, 8-bit color resolution
        // Loop forever (NETSCAPE2.0 application extension).
        hdr.extend_from_slice(&[0x21, 0xFF, 0x0B]);
        hdr.extend_from_slice(b"NETSCAPE2.0");
        hdr.extend_from_slice(&[0x03, 0x01, 0x00, 0x00, 0x00]);
        f.write_all(&hdr).ctx("gif header")?;
        Ok(GifWriter {
            f,
            w: w as usize,
            h: h as usize,
            shown: vec![[0; 3]; (w * h) as usize],
            first: true,
            pending: None,
        })
    }

    /// Adds a BGRA frame (`pitch` bytes per row) shown for `delay_cs` hundredths of a second.
    pub fn add_frame(&mut self, bgra: &[u8], pitch: usize, delay_cs: u32) -> Res<()> {
        let (w, h) = (self.w, self.h);
        let px = |x: usize, y: usize| -> [u8; 3] {
            let o = y * pitch + x * 4;
            [bgra[o + 2], bgra[o + 1], bgra[o]]
        };

        // Which pixels changed, and their bounding box.
        let mut changed = vec![false; w * h];
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        for y in 0..h {
            for x in 0..w {
                let c = px(x, y);
                let s = self.shown[y * w + x];
                let d = (c[0] as i32 - s[0] as i32).abs() + (c[1] as i32 - s[1] as i32).abs() + (c[2] as i32 - s[2] as i32).abs();
                if self.first || d > CHANGE_THRESHOLD {
                    changed[y * w + x] = true;
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        if x1 < x0 {
            // Nothing changed: just show the previous frame longer.
            if let Some(p) = self.pending.as_mut() {
                p.1 += delay_cs;
            }
            return Ok(());
        }
        let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);

        // Palette for the changed pixels.
        let mut colors = Vec::with_capacity(bw * bh);
        for y in y0..=y1 {
            for x in x0..=x1 {
                if changed[y * w + x] {
                    colors.push(px(x, y));
                }
            }
        }
        let pal = Palette::build(&colors);

        // Indices (transparent where unchanged) and update what the viewer sees.
        let mut idx = Vec::with_capacity(bw * bh);
        let mut cache = vec![u16::MAX; 1 << 18];
        for y in y0..=y1 {
            for x in x0..=x1 {
                let i = y * w + x;
                if !changed[i] {
                    idx.push(TRANSPARENT);
                    continue;
                }
                let c = px(x, y);
                idx.push(pal.index(c, x, y, &mut cache));
                self.shown[i] = c;
            }
        }

        // Image descriptor + local palette + LZW data.
        let mut frame = Vec::with_capacity(idx.len() / 2 + 1024);
        frame.push(0x2C);
        for v in [x0, y0, bw, bh] {
            frame.extend_from_slice(&(v as u16).to_le_bytes());
        }
        frame.push(0x87); // local color table, 256 entries
        for i in 0..256 {
            let c = pal.colors.get(i).copied().unwrap_or([0, 0, 0]);
            frame.extend_from_slice(&c);
        }
        frame.push(8); // LZW minimum code size
        let data = lzw_encode(&idx);
        for chunk in data.chunks(255) {
            frame.push(chunk.len() as u8);
            frame.extend_from_slice(chunk);
        }
        frame.push(0);

        self.flush_pending()?;
        self.pending = Some((frame, delay_cs));
        self.first = false;
        Ok(())
    }

    fn flush_pending(&mut self) -> Res<()> {
        if let Some((frame, delay)) = self.pending.take() {
            // Graphic control: keep previous pixels (disposal 1), index 255 transparent.
            // GIF delays are 16-bit; split very long holds into several frames.
            let mut left = delay.max(2);
            let mut first = true;
            while left > 0 {
                let d = left.min(60000);
                left -= d;
                self.f.write_all(&[0x21, 0xF9, 0x04, 0x05]).ctx("gce")?;
                self.f.write_all(&(d as u16).to_le_bytes()).ctx("gce")?;
                self.f.write_all(&[TRANSPARENT, 0x00]).ctx("gce")?;
                if first {
                    self.f.write_all(&frame).ctx("frame")?;
                    first = false;
                } else {
                    // A 1×1 fully transparent frame just to carry the remaining delay.
                    self.f.write_all(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0x00, 8, 2, 0x4C, 0x01, 0]).ctx("hold")?;
                }
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Res<()> {
        self.flush_pending()?;
        self.f.write_all(&[0x3B]).ctx("trailer")?;
        self.f.flush().ctx("flush gif")
    }
}

struct Palette {
    colors: Vec<[u8; 3]>,
    /// Exact palette (≤255 distinct colors): no quantization, no dithering.
    exact: Option<std::collections::HashMap<[u8; 3], u8>>,
}

/// 4×4 Bayer matrix, centered around zero.
const BAYER: [[i32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

impl Palette {
    fn build(colors: &[[u8; 3]]) -> Palette {
        let mut exact = std::collections::HashMap::new();
        for c in colors {
            if exact.len() > 255 {
                break;
            }
            let n = exact.len() as u8;
            exact.entry(*c).or_insert(n);
        }
        if exact.len() <= 255 {
            let mut list = vec![[0u8; 3]; exact.len()];
            for (c, i) in &exact {
                list[*i as usize] = *c;
            }
            return Palette { colors: list, exact: Some(exact) };
        }
        Palette { colors: median_cut(colors, 255), exact: None }
    }

    fn index(&self, c: [u8; 3], x: usize, y: usize, cache: &mut [u16]) -> u8 {
        if let Some(m) = &self.exact {
            return m[&c];
        }
        // Small ordered dither breaks up banding in gradients and shadows.
        let d = (BAYER[y & 3][x & 3] * 2 - 15) / 2; // about -8..7
        let q = |v: u8| ((v as i32 + d).clamp(0, 255) >> 2) as usize; // 6 bits per channel
        let key = (q(c[0]) << 12) | (q(c[1]) << 6) | q(c[2]);
        if cache[key] != u16::MAX {
            return cache[key] as u8;
        }
        let (r, g, b) = ((q(c[0]) << 2) as i32 + 2, (q(c[1]) << 2) as i32 + 2, (q(c[2]) << 2) as i32 + 2);
        let mut best = (i32::MAX, 0usize);
        for (i, p) in self.colors.iter().enumerate() {
            // Weighted distance (eyes are most sensitive to green).
            let dr = r - p[0] as i32;
            let dg = g - p[1] as i32;
            let db = b - p[2] as i32;
            let dist = 2 * dr * dr + 4 * dg * dg + 3 * db * db;
            if dist < best.0 {
                best = (dist, i);
            }
        }
        cache[key] = best.1 as u16;
        best.1 as u8
    }
}

/// Median-cut quantization over a 5-bit-per-channel histogram.
fn median_cut(colors: &[[u8; 3]], max: usize) -> Vec<[u8; 3]> {
    // Histogram bins: count and channel sums (for accurate averages).
    let mut hist = vec![(0u32, [0u64; 3]); 1 << 15];
    for c in colors {
        let k = ((c[0] as usize >> 3) << 10) | ((c[1] as usize >> 3) << 5) | (c[2] as usize >> 3);
        let e = &mut hist[k];
        e.0 += 1;
        e.1[0] += c[0] as u64;
        e.1[1] += c[1] as u64;
        e.1[2] += c[2] as u64;
    }
    // Each bin: (avg color, count).
    let bins: Vec<([u8; 3], u32)> = hist
        .iter()
        .filter(|e| e.0 > 0)
        .map(|e| {
            let n = e.0 as u64;
            ([(e.1[0] / n) as u8, (e.1[1] / n) as u8, (e.1[2] / n) as u8], e.0)
        })
        .collect();
    let mut boxes: Vec<Vec<([u8; 3], u32)>> = vec![bins];
    while boxes.len() < max {
        // Split the box with the largest (range × population) score.
        let mut best = None;
        for (i, b) in boxes.iter().enumerate() {
            if b.len() < 2 {
                continue;
            }
            let (ch, range) = widest_channel(b);
            let pop: u64 = b.iter().map(|e| e.1 as u64).sum();
            let score = range as u64 * (pop as f64).sqrt() as u64;
            if best.is_none_or(|(_, _, s)| score > s) {
                best = Some((i, ch, score));
            }
        }
        let Some((i, ch, _)) = best else { break };
        let mut b = boxes.swap_remove(i);
        b.sort_unstable_by_key(|e| e.0[ch]);
        let total: u64 = b.iter().map(|e| e.1 as u64).sum();
        let mut acc = 0u64;
        let mut cut = 1;
        for (j, e) in b.iter().enumerate() {
            acc += e.1 as u64;
            if acc * 2 >= total {
                cut = (j + 1).clamp(1, b.len() - 1);
                break;
            }
        }
        let rest = b.split_off(cut);
        boxes.push(b);
        boxes.push(rest);
    }
    boxes
        .iter()
        .map(|b| {
            let n: u64 = b.iter().map(|e| e.1 as u64).sum::<u64>().max(1);
            let mut s = [0u64; 3];
            for e in b {
                for k in 0..3 {
                    s[k] += e.0[k] as u64 * e.1 as u64;
                }
            }
            [(s[0] / n) as u8, (s[1] / n) as u8, (s[2] / n) as u8]
        })
        .collect()
}

fn widest_channel(b: &[([u8; 3], u32)]) -> (usize, u8) {
    let mut lo = [255u8; 3];
    let mut hi = [0u8; 3];
    for e in b {
        for k in 0..3 {
            lo[k] = lo[k].min(e.0[k]);
            hi[k] = hi[k].max(e.0[k]);
        }
    }
    let r = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
    let ch = (0..3).max_by_key(|&k| r[k]).unwrap();
    (ch, r[ch])
}

/// GIF-flavored LZW (8-bit symbols, variable code width up to 12 bits).
fn lzw_encode(data: &[u8]) -> Vec<u8> {
    const CLEAR: u32 = 256;
    const EOI: u32 = 257;
    const TABLE: usize = 1 << 13;
    let mut out = Vec::with_capacity(data.len() / 2);
    let (mut bitbuf, mut nbits) = (0u32, 0u32);
    let mut emit = |code: u32, width: u32, out: &mut Vec<u8>| {
        bitbuf |= code << nbits;
        nbits += width;
        while nbits >= 8 {
            out.push(bitbuf as u8);
            bitbuf >>= 8;
            nbits -= 8;
        }
    };
    // Open-addressing table: key = prefix << 8 | byte (+1 so 0 means empty).
    let mut keys = vec![0u32; TABLE];
    let mut vals = vec![0u16; TABLE];
    let mut next = 258u32;
    let mut width = 9u32;
    emit(CLEAR, width, &mut out);
    let Some((&first, rest)) = data.split_first() else {
        emit(EOI, width, &mut out);
        if nbits > 0 {
            out.push(bitbuf as u8);
        }
        return out;
    };
    let mut prefix = first as u32;
    for &b in rest {
        let key = (prefix << 8 | b as u32) + 1;
        let mut h = (key.wrapping_mul(2654435761) >> 19) as usize & (TABLE - 1);
        let mut found = None;
        while keys[h] != 0 {
            if keys[h] == key {
                found = Some(vals[h] as u32);
                break;
            }
            h = (h + 1) & (TABLE - 1);
        }
        if let Some(code) = found {
            prefix = code;
            continue;
        }
        emit(prefix, width, &mut out);
        if next < 4096 {
            keys[h] = key;
            vals[h] = next as u16;
            if next == 1 << width {
                width += 1;
            }
            next += 1;
        } else {
            // Dictionary full: start over.
            emit(CLEAR, width, &mut out);
            keys.iter_mut().for_each(|k| *k = 0);
            next = 258;
            width = 9;
        }
        prefix = b as u32;
    }
    emit(prefix, width, &mut out);
    emit(EOI, width, &mut out);
    if nbits > 0 {
        out.push(bitbuf as u8);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference GIF LZW decoder used to check the encoder round-trips.
    fn lzw_decode(data: &[u8]) -> Vec<u8> {
        let mut dict: Vec<Vec<u8>> = (0..256).map(|i| vec![i as u8]).collect();
        dict.push(vec![]);
        dict.push(vec![]);
        let (mut pos, mut width) = (0usize, 9u32);
        let read = |pos: &mut usize, width: u32| -> Option<u32> {
            let mut v = 0u32;
            for i in 0..width {
                let byte = *data.get((*pos + i as usize) / 8)?;
                v |= ((byte >> ((*pos + i as usize) % 8)) as u32 & 1) << i;
            }
            *pos += width as usize;
            Some(v)
        };
        let mut out = Vec::new();
        let mut prev: Option<Vec<u8>> = None;
        while let Some(code) = read(&mut pos, width) {
            if code == 256 {
                dict.truncate(258);
                width = 9;
                prev = None;
                continue;
            }
            if code == 257 {
                break;
            }
            let entry = if (code as usize) < dict.len() {
                dict[code as usize].clone()
            } else {
                let p = prev.clone().unwrap();
                let mut e = p.clone();
                e.push(p[0]);
                e
            };
            out.extend_from_slice(&entry);
            if let Some(p) = prev {
                let mut n = p.clone();
                n.push(entry[0]);
                dict.push(n);
                // Decoder grows one code later than the encoder emits.
                if dict.len() == (1 << width) as usize && width < 12 {
                    width += 1;
                }
            }
            prev = Some(entry);
        }
        out
    }

    #[test]
    fn lzw_roundtrip() {
        let mut data = Vec::new();
        let mut x = 12345u32;
        for i in 0..200_000 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            // Mix of runs and noise, enough to fill and reset the dictionary.
            data.push(if i % 7 < 4 { (i / 50) as u8 } else { (x >> 16) as u8 });
        }
        assert_eq!(lzw_decode(&lzw_encode(&data)), data);
        assert_eq!(lzw_decode(&lzw_encode(&[42])), vec![42]);
    }

    #[test]
    fn median_cut_limits_colors() {
        let colors: Vec<[u8; 3]> = (0..100_000).map(|i| [(i % 256) as u8, (i / 256 % 256) as u8, (i * 7 % 256) as u8]).collect();
        let p = median_cut(&colors, 255);
        assert!(p.len() <= 255 && p.len() > 200);
    }
}
