// SPDX-License-Identifier: EUPL-1.2
//! Hostile image bytes through the content worker's decoders, as peers and
//! websites send them: avatars, website icons (also ICO, BMP and WebP), inline
//! `data:` images at the viewer's size, and pasted images re-encoded to send
//! (which receivers, Nuschel included, must decode again). The first byte picks
//! the path. Nothing may panic, every result fits its limits and has exactly
//! its pixels. Images whose header claims more than 256 pixels a side are
//! skipped: the decoders' own limits allow 4096, and decoding that much only
//! slows the fuzzer down.
#![no_main]

use base64::Engine as _;
use libfuzzer_sys::fuzz_target;
use nuschel_content::{
    VIEW_HEIGHT, VIEW_WIDTH, decode_icon, decode_image, decode_view, encode_image_message,
};
use nuschel_core::ContentBlock;

const SIDE: u32 = 256;

/// Small enough to decode, or not even a readable header.
fn small(bytes: &[u8]) -> bool {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
        .is_none_or(|(width, height)| width <= SIDE && height <= SIDE)
}

/// An image block's size, checked against its pixels and `max`.
fn image(block: &ContentBlock, max: (u32, u32)) -> (u32, u32) {
    let ContentBlock::Image {
        width,
        height,
        rgba,
        source,
    } = block
    else {
        panic!("not an image block");
    };
    assert!(source.is_none());
    assert!((1..=max.0).contains(width) && (1..=max.1).contains(height));
    let pixels = base64::engine::general_purpose::STANDARD
        .decode(rgba)
        .expect("base64 pixels");
    assert_eq!(
        pixels.len() as u64,
        u64::from(*width) * u64::from(*height) * 4
    );
    (*width, *height)
}

/// `data:image/…;base64,…`, percent-encoded and in lines as stock Mumble writes it.
fn data_uri(kind: &str, bytes: &[u8], mumble: bool) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let payload = if mumble {
        encoded
            .as_bytes()
            .chunks(72)
            .map(|line| {
                String::from_utf8_lossy(line)
                    .replace('+', "%2B")
                    .replace('/', "%2F")
                    .replace('=', "%3D")
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        encoded
    };
    format!("data:image/{kind};base64,{payload}")
}

fuzz_target!(|data: &[u8]| {
    let Some((&selector, bytes)) = data.split_first() else {
        return;
    };
    if !small(bytes) {
        return;
    }
    match selector % 4 {
        // An avatar (the worker's size) and an inline image (a message's thumbnail).
        0 => {
            if let Some(block) = decode_image(bytes, 96, 96) {
                image(&block, (96, 96));
            }
        }
        1 => {
            if let Some(block) = decode_icon(bytes) {
                image(&block, (64, 64));
            }
        }
        2 => {
            let kind = ["png", "JPEG", "jpg", "gif", "webp"][usize::from(selector / 4) % 5];
            if let Some(block) = decode_view(&data_uri(kind, bytes, selector & 0x80 != 0)) {
                image(&block, (VIEW_WIDTH, VIEW_HEIGHT));
            }
        }
        _ => {
            // Limits from far too small to the server's usual 128 KiB.
            let limit = 64 << (selector / 4 % 12);
            let Ok((html, thumbnail)) = encode_image_message(bytes, limit) else {
                return;
            };
            assert!(html.len() <= limit, "{} > {limit}", html.len());
            image(&thumbnail, (128, 128));
            // What receivers do with it: the element's data URI, at the viewer's size.
            let uri = html
                .strip_prefix("<img src=\"")
                .and_then(|rest| rest.strip_suffix("\" />"))
                .expect("an img element");
            let viewed = decode_view(uri).expect("receivers decode what was sent");
            image(&viewed, (VIEW_WIDTH, VIEW_HEIGHT));
        }
    }
});
