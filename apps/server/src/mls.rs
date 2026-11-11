// SPDX-License-Identifier: EUPL-1.2
//! The MLS delivery service (`mls/1`, docs/e2ee.md §6.3). Work in progress.
use base64::Engine as _;
use nuschel_protocol::nuschel::is_mls_format;

/// An MLS row keeps its message as base64 in the stored `content` (the history
/// table has no binary column; a later schema change is mechanical). Returns the
/// wire `content` and `mls`.
pub fn row_content(format: u8, content: String) -> (String, Vec<u8>) {
    if is_mls_format(i32::from(format)) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content.as_bytes())
            .unwrap_or_default();
        (String::new(), bytes)
    } else {
        (content, Vec::new())
    }
}

/// Whether `mls/1` is offered (with `kt/1`).
pub const OFFERED: bool = false;
