// SPDX-License-Identifier: EUPL-1.2
//! The image viewer: a click on an inline image shows it large over a
//! gradient backdrop (decoded again, larger, by the sandboxed content worker),
//! and a right click or the viewer's button saves the original file.
use crate::{
    assets::icon,
    dialogs::{Tone, styled_button},
    state::Desktop,
    theme::*,
};
use gpui_kit::{prelude::*, *};
use std::sync::Arc;

/// An image as shown in a message, with the `data:` URI it came from.
#[derive(Clone)]
pub struct ViewerImage {
    pub image: Arc<RenderImage>,
    pub width: u32,
    pub height: u32,
    pub source: Option<Arc<str>>,
}

pub struct Viewer {
    pub shown: ViewerImage,
    /// Identifies the worker request for the larger version.
    pub request: u64,
    pub loading: bool,
}

impl Desktop {
    pub fn open_viewer(&mut self, image: ViewerImage) {
        self.menu = None;
        self.viewer_requests += 1;
        let request = self.viewer_requests;
        let loading = match &image.source {
            Some(source) => {
                self.backend.decode_view(request, source.to_string());
                true
            }
            None => false,
        };
        self.viewer = Some(Viewer {
            shown: image,
            request,
            loading,
        });
    }

    /// The worker's larger decode for `request`, if the viewer still shows it.
    pub fn viewed(&mut self, request: u64, image: Option<(Arc<RenderImage>, u32, u32)>) {
        if let Some(viewer) = self
            .viewer
            .as_mut()
            .filter(|viewer| viewer.request == request)
        {
            viewer.loading = false;
            if let Some((image, width, height)) = image {
                viewer.shown.image = image;
                viewer.shown.width = width;
                viewer.shown.height = height;
            }
        }
    }

    pub fn open_image_menu(&mut self, image: ViewerImage, position: Point<Pixels>) {
        let server = self.selected_indices().map_or(0, |(server, _)| server);
        if self.servers.is_empty() {
            return;
        }
        self.menu_image = Some(image);
        self.menu = Some(crate::menus::Menu {
            server,
            target: crate::menus::MenuTarget::Image,
            position,
        });
    }

    /// Save the original file the image was sent as.
    pub fn save_image(&mut self, image: &ViewerImage, cx: &mut Context<Self>) {
        self.menu = None;
        let Some((bytes, extension)) = image
            .source
            .as_deref()
            .and_then(crate::document::data_uri_file)
        else {
            self.error = Some("This image cannot be saved.".into());
            return;
        };
        let directory = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .map(|home| {
                let downloads = home.join("Downloads");
                if downloads.is_dir() { downloads } else { home }
            })
            .unwrap_or_else(std::env::temp_dir);
        let target = cx.prompt_for_new_path(&directory, Some(&format!("image.{extension}")));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = target.await else {
                return;
            };
            let result = std::fs::write(&path, &bytes);
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error = Some(format!("Could not save {}: {error}", path.display()));
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// The open viewer, over everything else.
pub fn overlay(this: &Desktop, window: &Window, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let viewer = this.viewer.as_ref()?;
    let shown = viewer.shown.clone();
    let viewport = window.viewport_size();
    // Fit inside the window with room for the toolbar; enlarge small images
    // up to twice their size.
    let room_width = (f32::from(viewport.width) - 96.).max(64.);
    let room_height = (f32::from(viewport.height) - 160.).max(64.);
    let scale = (room_width / shown.width.max(1) as f32)
        .min(room_height / shown.height.max(1) as f32)
        .min(2.);
    let (width, height) = (shown.width as f32 * scale, shown.height as f32 * scale);
    let saving = shown.clone();
    let menu = shown.clone();
    Some(
        div()
            .id("image-viewer")
            .role(Role::Dialog)
            .aria_label("Image")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(linear_gradient(
                160.,
                linear_color_stop(tint(0x0b0e14, 0xf2), 0.),
                linear_color_stop(tint(0x1d2433, 0xe6), 1.),
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.viewer = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("image-viewer-picture")
                    .w(px(width))
                    .h(px(height))
                    .rounded(px(10.))
                    .overflow_hidden()
                    .shadow_lg()
                    // Clicks on the picture itself do not close the viewer.
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.open_image_menu(menu.clone(), event.position);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(img(shown.image.clone()).size_full()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        text!(if viewer.loading {
                            "Loading the full image…".to_string()
                        } else {
                            format!("{} × {}", shown.width, shown.height)
                        })
                        .with_id("image-viewer-size"),
                    )
                    .when(shown.source.is_some(), |row| {
                        row.child(styled_button(
                            "image-viewer-save",
                            "Save as…",
                            Tone::Secondary,
                            move |this, _, cx| {
                                this.save_image(&saving, cx);
                                cx.notify();
                            },
                            cx,
                        ))
                    })
                    .child(styled_button(
                        "image-viewer-close",
                        "Close",
                        Tone::Ghost,
                        |this, _, cx| {
                            this.viewer = None;
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(icon("info", 14., color(TEXT_FAINT)))
                    .child(div().text_xs().text_color(color(TEXT_FAINT)).child(
                        text!("Esc closes · right-click to save").with_id("image-viewer-hint"),
                    )),
            )
            .into_any_element(),
    )
}
