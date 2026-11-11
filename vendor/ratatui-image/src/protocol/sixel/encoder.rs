// SPDX-License-Identifier: MIT
// Sixel band framing adapted from icy_sixel 0.5.1 (Mike Krüger, 2025).
// See ../../../NOTICE and ../../../LICENSE-icy_sixel-MIT for provenance.
// Quantization, input bounds and safe formatting are Nuschl modifications.
#![forbid(unsafe_code)]

use color_quant::NeuQuant;
use std::{collections::BTreeMap, fmt::Write as _};

const MAX_DIMENSION: usize = 4096;
const MAX_PIXELS: usize = 4 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = 32 * 1024 * 1024;

pub(super) fn dimensions(width: usize, height: usize) -> Result<usize, &'static str> {
    let pixels = width
        .checked_mul(height)
        .ok_or("image dimensions overflow")?;
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || pixels > MAX_PIXELS
    {
        return Err("image dimensions exceed the renderer limit");
    }
    pixels.checked_mul(4).ok_or("RGBA size overflows")
}

#[derive(Clone, Copy)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

/// Encode only validated RGBA, never an untrusted source image format.
pub(super) fn encode(rgba: &[u8], width: usize, height: usize) -> Result<String, &'static str> {
    let expected = dimensions(width, height)?;
    if rgba.len() != expected {
        return Err("RGBA buffer length does not match dimensions");
    }
    let pixels = expected / 4;
    let mut opacity_mask = BitMask::zeros(pixels);
    let mut colors = BTreeMap::<[u8; 3], u8>::new();
    let mut palette = Vec::new();
    let mut too_many = false;
    for (index, pixel) in rgba.chunks_exact(4).enumerate() {
        if pixel[3] < 128 {
            continue;
        }
        opacity_mask.set(index);
        if too_many {
            continue;
        }
        let rgb = [pixel[0], pixel[1], pixel[2]];
        if !colors.contains_key(&rgb) {
            if palette.len() == 256 {
                too_many = true;
                continue;
            }
            colors.insert(rgb, palette.len() as u8);
            palette.push(Rgb {
                r: rgb[0],
                g: rgb[1],
                b: rgb[2],
            });
        }
    }
    let indices = if too_many {
        // Transparent RGB cannot dominate the palette; SIXEL alpha is binary.
        // The caller can precompose partial alpha against its chosen background.
        let mut training = Vec::with_capacity(expected);
        for pixel in rgba.chunks_exact(4).filter(|pixel| pixel[3] >= 128) {
            training.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
        let quantizer = NeuQuant::new(10, 256, &training);
        palette = quantizer
            .color_map_rgb()
            .chunks_exact(3)
            .map(|rgb| Rgb {
                r: rgb[0],
                g: rgb[1],
                b: rgb[2],
            })
            .collect();
        let mut indices = Vec::with_capacity(pixels);
        for pixel in rgba.chunks_exact(4) {
            let index = if pixel[3] >= 128 {
                quantizer.index_of(&[pixel[0], pixel[1], pixel[2], 255])
            } else {
                0
            };
            indices.push(
                u8::try_from(index).map_err(|_| "quantizer returned an invalid palette index")?,
            );
        }
        indices
    } else {
        // Exact small palettes retain sharp colors and antialiased grayscale;
        // unlike a fixed RGB cube they do not collapse math into a few shades.
        rgba.chunks_exact(4)
            .map(|pixel| *colors.get(&[pixel[0], pixel[1], pixel[2]]).unwrap_or(&0))
            .collect()
    };
    if palette.is_empty() {
        palette.push(Rgb { r: 0, g: 0, b: 0 });
    }
    encode_indexed_to_sixel(&palette, &indices, &opacity_mask, width, height)
}

/// A compact 1-bit-per-element mask backed by `u64` words.
///
/// Uses 1/8 the memory of a `Vec<bool>`, which improves cache behavior in the
/// per-color band-encoding loop that re-reads the opacity mask many times.
struct BitMask {
    words: Vec<u64>,
}

impl BitMask {
    /// Create a mask with `len` bits, all cleared.
    fn zeros(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
        }
    }

    /// Set the bit at `index` to 1.
    #[inline]
    fn set(&mut self, index: usize) {
        self.words[index >> 6] |= 1u64 << (index & 63);
    }

    /// Return whether the bit at `index` is set.
    #[inline]
    fn get(&self, index: usize) -> bool {
        (self.words[index >> 6] >> (index & 63)) & 1 != 0
    }
}

fn encode_indexed_to_sixel(
    palette: &[Rgb],
    indices: &[u8],
    opacity_mask: &BitMask,
    width: usize,
    height: usize,
) -> Result<String, &'static str> {
    let mut out = String::new();

    // DCS introducer for SIXEL: ESC P p1 ; p2 ; p3 q
    // p1=aspect ratio, p2=background mode, p3=0 (grid size default)
    out.push_str("\x1bP");
    write_number(&mut out, 9);
    out.push(';');
    write_number(&mut out, 1);
    out.push_str(";0q");

    // Set raster attributes: " Pan ; Pad ; Ph ; Pv
    // Pan:Pad is vertical:horizontal, so it mirrors the P1 macro parameter.
    // Emitting this is required for terminals and multiplexers (e.g. tmux) that
    // drop or rewrite P1 but forward the raster attributes.
    out.push('"');
    write_number(&mut out, 1);
    out.push(';');
    write_number(&mut out, 1);
    out.push(';');
    write_number(&mut out, width);
    out.push(';');
    write_number(&mut out, height);

    // Define palette in RGB percent (0-100)
    for (i, c) in palette.iter().enumerate() {
        let r = (c.r as u32 * 100) / 255;
        let g = (c.g as u32 * 100) / 255;
        let b = (c.b as u32 * 100) / 255;
        out.push('#');
        write_number(&mut out, i);
        out.push(';');
        out.push('2');
        out.push(';');
        write_number(&mut out, r as usize);
        out.push(';');
        write_number(&mut out, g as usize);
        out.push(';');
        write_number(&mut out, b as usize);
    }

    let bands = height.div_ceil(6);
    let palette_len = palette.len();

    // Scratch buffer holding the 6-bit sixel value for every (color, column)
    // pair in the current band. Reused across bands; only the rows of colors
    // actually used in a band are cleared, so this stays cheap.
    let mut sixels = vec![0u8; palette_len * width];
    let mut colors_used = vec![false; palette_len];

    for band in 0..bands {
        let y0 = band * 6;
        let y_max = usize::min(y0 + 6, height);

        // Reset only the rows touched by the previous band.
        for (color_index, used) in colors_used.iter_mut().enumerate() {
            if *used {
                sixels[color_index * width..(color_index + 1) * width].fill(0);
                *used = false;
            }
        }

        // Single pass over the band: scatter each opaque pixel's bit into the
        // scratch buffer keyed by its color. This replaces the previous
        // O(colors x pixels) re-scan with a single O(pixels) pass.
        for y in y0..y_max {
            let bit = 1u8 << (y - y0);
            let row = y * width;
            for x in 0..width {
                let pixel_idx = row + x;
                if opacity_mask.get(pixel_idx) {
                    let color_index = indices[pixel_idx] as usize;
                    sixels[color_index * width + x] |= bit;
                    colors_used[color_index] = true;
                }
            }
        }

        // Emit each used color, run-length encoding consecutive identical sixels.
        for color_index in 0..palette_len {
            if !colors_used[color_index] {
                continue; // Skip colors not used in this band
            }

            // Select color map register
            out.push('#');
            write_number(&mut out, color_index);

            let row = &sixels[color_index * width..(color_index + 1) * width];
            let mut x = 0;
            while x < width {
                let bits = row[x];

                // Run-length encode consecutive identical sixel values
                let mut run_len = 1usize;
                while x + run_len < width && row[x + run_len] == bits {
                    run_len += 1;
                }

                // Write RLE or raw sixels
                if run_len > 3 {
                    out.push('!');
                    write_number(&mut out, run_len);
                    out.push((63 + bits) as char);
                } else {
                    let ch = (63 + bits) as char;
                    for _ in 0..run_len {
                        out.push(ch);
                    }
                }
                x += run_len;
            }

            // Carriage return to start of band for next color overlay
            out.push('$');
        }

        // Move to next band
        out.push('-');
        if out.len() > MAX_ENCODED_BYTES {
            return Err("sixel output exceeds the renderer limit");
        }
    }

    // String terminator: ESC \
    out.push('\x1b');
    out.push('\\');

    Ok(out)
}

fn write_number(out: &mut String, n: usize) {
    write!(out, "{n}").expect("formatting into a String cannot fail");
}
