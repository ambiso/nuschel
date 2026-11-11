// SPDX-License-Identifier: EUPL-1.2
//! The real content worker in its real sandbox, as the desktop drives it.
#![cfg(target_os = "linux")]
use std::{path::Path, time::Instant};

use nuschel_core::ContentBlock;

fn worker() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_nuschel-content-worker"))
}

/// A history page: plain chat, with a code block in every tenth message.
fn page(code: bool) -> Vec<String> {
    (0..40)
        .map(|index| {
            if code && index % 10 == 3 {
                format!("The fix:\n```rust\nfn answer() -> u32 {{\n    {index}\n}}\n```\nWorks?")
            } else {
                format!("Message {index}: ordinary chat text with <html> & such.\nA second line.")
            }
        })
        .collect()
}

#[tokio::test]
async fn a_history_page_renders_in_order_in_one_worker() {
    let texts = page(true);
    let documents = nuschel_ipc::host::render_many(worker(), &texts).await;
    assert_eq!(documents.len(), texts.len());
    for (index, document) in documents.iter().enumerate() {
        let blocks = &document.as_ref().expect("rendered").blocks;
        if index % 10 == 3 {
            assert!(matches!(blocks.get(1), Some(ContentBlock::Code { .. })));
        } else {
            assert!(
                matches!(blocks.as_slice(), [ContentBlock::Text { text, .. }] if *text == texts[index])
            );
        }
    }
}

/// A burst of received messages, stock Mumble's rich text among them, is
/// prepared in one worker exactly as each would be on its own.
#[tokio::test]
async fn a_burst_of_received_messages_prepares_in_order_in_one_worker() {
    let messages: Vec<(String, bool)> = (0..40)
        .map(|index| match index % 3 {
            0 => (format!("<b>Bold</b> rich text {index} &amp; a <a href=\"https://example.org/{index}\">link</a>"), true),
            1 => (format!("Plain text {index}\n```rust\nfn answer() -> u32 {{ {index} }}\n```"), false),
            _ => (format!("Message {index}: <html> & such"), false),
        })
        .collect();
    let burst = nuschel_ipc::host::prepare_contents(worker(), &messages).await;
    assert_eq!(burst.len(), messages.len());
    for ((text, legacy_html), prepared) in messages.iter().zip(burst) {
        let prepared = prepared.expect("prepared");
        let alone = nuschel_ipc::host::prepare_content(worker(), text, *legacy_html)
            .await
            .unwrap();
        assert_eq!(
            (prepared.source, prepared.document),
            (alone.source, alone.document),
            "{text}"
        );
    }
}

#[tokio::test]
async fn a_pasted_image_becomes_a_stock_mumble_image_message() {
    // A 2×2 RGB PNG.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x02\x08\x02\0\0\0\xfd\xd4\x9as\0\0\0\x12IDATx\x9cc\xf8\xcf\xc0\xc0\0\xc2\x0c\xff\x81\0\0\x1f\xee\x05\xfb\x0b\xd9h\x8b\0\0\0\0IEND\xaeB`\x82";
    let encoded = nuschel_ipc::host::encode_image(worker(), PNG, 131_072)
        .await
        .unwrap();
    let html = encoded.html;
    assert!(html.starts_with("<img src=\"data:image/"), "{html}");
    assert!(matches!(
        encoded.thumbnail,
        ContentBlock::Image {
            width: 2,
            height: 2,
            ..
        }
    ));
    let prepared = nuschel_ipc::host::prepare_content(worker(), &html, true)
        .await
        .unwrap();
    assert!(matches!(
        prepared.document.blocks.as_slice(),
        [ContentBlock::Image {
            width: 2,
            height: 2,
            ..
        }]
    ));
    assert!(matches!(
        nuschel_ipc::host::encode_image(worker(), b"GIF89a, but not really", 131_072).await,
        Err(nuschel_ipc::host::Error::Rejected(_))
    ));
}

/// `cargo test -p nuschel-content-worker --test worker -- --ignored --nocapture`
#[tokio::test]
#[ignore = "measurement, not a check"]
async fn history_page_cost() {
    for code in [false, true] {
        let texts = page(code);
        let label = if code { "4 with code" } else { "text only" };
        let started = Instant::now();
        for text in &texts {
            nuschel_ipc::host::render_content(worker(), text)
                .await
                .unwrap();
        }
        let each = started.elapsed();
        let started = Instant::now();
        let documents = nuschel_ipc::host::render_many(worker(), &texts).await;
        let batch = started.elapsed();
        assert!(documents.iter().all(Option::is_some));
        println!(
            "40 messages ({label}): one worker each {:.0} ms, one worker for the page {:.0} ms",
            each.as_secs_f64() * 1e3,
            batch.as_secs_f64() * 1e3
        );
    }
}
