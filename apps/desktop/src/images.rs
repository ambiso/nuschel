// SPDX-License-Identifier: EUPL-1.2
//! GPUI keeps every `RenderImage` it has drawn in the window's texture atlas
//! until `Window::drop_image`. Images Nuschel decodes itself (message images,
//! avatars, the viewer's large version) are tracked here and released once
//! nothing but the tracker holds them.
use gpui_kit::{RenderImage, Window};
use std::{cell::RefCell, sync::Arc};

thread_local! {
    static TRACKED: RefCell<Vec<Arc<RenderImage>>> = const { RefCell::new(Vec::new()) };
}

pub fn track(image: Arc<RenderImage>) -> Arc<RenderImage> {
    TRACKED.with_borrow_mut(|tracked| tracked.push(image.clone()));
    image
}

/// Release the atlas space of images no message, avatar or viewer uses any more.
pub fn collect(window: &mut Window) {
    TRACKED.with_borrow_mut(|tracked| {
        tracked.retain(|image| {
            let used = Arc::strong_count(image) > 1;
            if !used {
                let _ = window.drop_image(image.clone());
            }
            used
        })
    });
}
