// SPDX-License-Identifier: EUPL-1.2
use nuschl_content_wasm::{ContentProcessor, MAX_IMAGE_INPUT, MAX_REPLY_BYTES};
use nuschl_core::{ContentBlock, MAX_CONTENT_BYTES, MessageDocument};

const PNG: &[u8] = include_bytes!("fixtures/rgba-2x2.png");

#[test]
fn actual_legacy_images_and_code_become_inert_typed_content() {
    let codec = ContentProcessor::new();
    let encoded: serde_json::Value =
        serde_json::from_str(&codec.encode_image(PNG, 10000).unwrap()).unwrap();
    let html = encoded["html"].as_str().unwrap();
    assert!(html.starts_with("<img"));
    let source = format!(
        "<p>first<br>second <code>β&lt;x&gt;</code></p>{html}\
         <script>window.bad=1</script><img src='https://invalid.example/canary.png'>"
    );
    let rendered: serde_json::Value =
        serde_json::from_str(&codec.render_message(&source, "legacy").unwrap()).unwrap();
    let document: MessageDocument = serde_json::from_value(rendered["document"].clone()).unwrap();
    assert!(document.blocks.iter().any(|block| matches!(
        block,
        ContentBlock::Image {
            width: 2,
            height: 2,
            source: None,
            ..
        }
    )));
    assert!(document.blocks.iter().any(|block| match block {
        ContentBlock::Text { text, spans, .. } =>
            text.contains("second") && spans.iter().any(|span| span.style.code),
        _ => false,
    }));
    let plain = rendered["plain_text"].as_str().unwrap();
    assert!(plain.contains("first\nsecond β<x>"));
    assert!(!plain.contains("window.bad"));
    assert!(!plain.contains("canary.png"));
    assert!(!rendered["document"].to_string().contains("data:image"));
}

#[test]
fn plain_source_newlines_and_actual_rust_highlighting_survive() {
    let codec = ContentProcessor::new();
    let source = "first\n\nsecond 🦀\n```rust\nlet answer = 42;\n```\nlast";
    let result = codec.render_message(source, "plain").unwrap();
    assert!(result.len() <= MAX_REPLY_BYTES);
    let rendered: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(rendered["marked_text"], source);
    assert_eq!(rendered["plain_text"], source);
    let document: MessageDocument = serde_json::from_value(rendered["document"].clone()).unwrap();
    assert!(document.blocks.iter().any(|block| matches!(
        block, ContentBlock::Text { text, .. } if text.contains("first\n\nsecond 🦀")
    )));
    let runs = document
        .blocks
        .iter()
        .find_map(|block| match block {
            ContentBlock::Code { language, runs } if language.as_deref() == Some("rust") => {
                Some(runs)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        runs.iter().map(|run| run.text.as_str()).collect::<String>(),
        "let answer = 42;\n"
    );
    assert!(runs.iter().any(|run| run.foreground != runs[0].foreground));
}

#[test]
fn invalid_or_unbounded_work_does_not_poison_the_next_job() {
    let codec = ContentProcessor::new();
    assert!(
        codec
            .render_message(&"x".repeat(MAX_CONTENT_BYTES + 1), "plain")
            .is_err()
    );
    assert!(codec.render_message("x", "url").is_err());
    assert!(codec.decode_image(b"not an image").is_err());
    assert!(
        codec
            .encode_image(&vec![0; MAX_IMAGE_INPUT + 1], 10000)
            .is_err()
    );
    assert!(codec.encode_image(PNG, MAX_CONTENT_BYTES + 1).is_err());
    assert!(codec.encode_image(PNG, 63).is_err());
    let decoded: ContentBlock = serde_json::from_str(&codec.decode_image(PNG).unwrap()).unwrap();
    assert!(matches!(
        decoded,
        ContentBlock::Image {
            width: 2,
            height: 2,
            source: None,
            ..
        }
    ));
    assert!(codec.render_message("still\nworking", "plain").is_ok());
}
