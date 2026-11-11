// SPDX-License-Identifier: EUPL-1.2
use crate::state::Desktop;
use gpui_kit::Context;

impl Desktop {
    /// Offer files in the open conversation: stub; the transfer agent provides the real one.
    pub fn offer_files(&mut self, files: Vec<std::path::PathBuf>, _cx: &mut Context<Self>) {
        let _ = files;
    }
}
