// SPDX-License-Identifier: EUPL-1.2
//! The image viewer: a click on an inline image shows it large over a
//! gradient backdrop (decoded again, larger, by the sandboxed content worker),
//! and a right click offers copying the displayed image or saving the original file.
use crate::{state::Desktop, theme::*};
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
    /// Copy only pixels already decoded and validated by the content worker.
    /// PNG encoding runs off the UI thread; the platform clipboard never receives
    /// the message's original, untrusted encoded image or data URI.
    pub fn copy_image(&mut self, image: Arc<RenderImage>, cx: &mut Context<Self>) {
        self.menu = None;
        let encoded = cx
            .background_executor()
            .spawn(async move { clipboard_image(&image) });
        cx.spawn(async move |this, cx| {
            let result = encoded.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(image) => cx.write_to_clipboard(ClipboardItem::new_image(&image)),
                    Err(error) => this.error = Some(format!("Could not copy image: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn open_viewer(&mut self, image: ViewerImage) {
        self.menu = None;
        self.viewer_requests += 1;
        let request = self.viewer_requests;
        let loading = match &image.source {
            Some(source) => {
                self.services.decode_view(request, source.to_string());
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
        if self.client.servers.is_empty() {
            return;
        }
        self.menu_image = Some(image);
        self.make_menu(server, crate::menus::MenuTarget::Image, position);
    }

    /// Save the original file the image was sent as.
    pub fn save_image(&mut self, image: &ViewerImage, cx: &mut Context<Self>) {
        self.menu = None;
        let Some((bytes, extension)) = image
            .source
            .as_deref()
            .and_then(nuschl_core::data_uri_image)
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

/// The open viewer, over everything else: the image on a dark glow. A click
/// anywhere but the image closes it (nothing behind receives it); a right
/// click on the image offers Copy image and Save image as.
pub fn overlay(this: &Desktop, window: &Window, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let viewer = this.viewer.as_ref()?;
    let shown = viewer.shown.clone();
    let viewport = window.viewport_size();
    // Fit inside the window; enlarge small images up to twice their size.
    let room_width = (f32::from(viewport.width) - 96.).max(64.);
    let room_height = (f32::from(viewport.height) - 96.).max(64.);
    let scale = (room_width / shown.width.max(1) as f32)
        .min(room_height / shown.height.max(1) as f32)
        .min(2.);
    let (width, height) = (shown.width as f32 * scale, shown.height as f32 * scale);
    let menu = shown.clone();
    Some(
        div()
            .id("image-viewer")
            .role(Role::Dialog)
            .aria_label("Image")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(tint(BACKDROP, 0xe0))
            .map(|backdrop| crate::chrome::round(backdrop, this.window_corners))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
                    // A dark glow around the picture.
                    .shadow(vec![BoxShadow {
                        color: tint(BLACK, 0xd0).into(),
                        offset: point(px(0.), px(0.)),
                        blur_radius: px(140.),
                        spread_radius: px(36.),
                        inset: false,
                    }])
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
            .into_any_element(),
    )
}

/// GPUI stores BGRA; PNG and the native clipboard exchange straight RGBA.
/// Take dimensions from the validated frame, not its message's metadata.
fn clipboard_image(image: &RenderImage) -> Result<Image, String> {
    use image::ImageEncoder as _;

    let size = image.size(0);
    let width = u32::try_from(size.width.0).map_err(|_| "invalid image width")?;
    let height = u32::try_from(size.height.0).map_err(|_| "invalid image height")?;
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err("the displayed image has invalid dimensions".into());
    }
    let mut pixels = image
        .as_bytes(0)
        .ok_or("the displayed image has no pixels")?
        .to_vec();
    if pixels.len() != width as usize * height as usize * 4 {
        return Err("the displayed image has incomplete pixels".into());
    }
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(&pixels, width, height, image::ExtendedColorType::Rgba8)
    .map_err(|error| error.to_string())?;
    Ok(Image::from_bytes(ImageFormat::Png, png))
}

#[cfg(test)]
mod tests {
    use super::clipboard_image;
    use gpui_kit::{ImageFormat, RenderImage};

    #[test]
    fn copied_image_preserves_displayed_colours_and_transparency_without_a_source() {
        // The displayed BGRA pixels can come from a message without a data URI.
        let pixels = vec![3, 2, 1, 64, 90, 80, 70, 255];
        let frame = image::RgbaImage::from_raw(2, 1, pixels).unwrap();
        let displayed = RenderImage::new(vec![image::Frame::new(frame)]);
        let copied = clipboard_image(&displayed).unwrap();
        assert_eq!(copied.format, ImageFormat::Png);
        let decoded = image::load_from_memory_with_format(&copied.bytes, image::ImageFormat::Png)
            .unwrap()
            .into_rgba8();
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.into_raw(), [1, 2, 3, 64, 70, 80, 90, 255]);
    }
}
