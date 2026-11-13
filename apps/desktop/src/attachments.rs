// SPDX-License-Identifier: EUPL-1.2
//! What the composer sends along with its text. Mumble has no file transfer:
//! images go as stock Mumble sends them, an `<img>` in the message, one per
//! message; other files are offered for a peer-to-peer transfer.
use gpui_kit::RenderImage;
use std::{
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
};

pub enum Attachment {
    /// Re-encoded by the content worker once attached. One from a file goes
    /// as a file instead if the worker cannot encode it.
    Image {
        id: u64,
        name: String,
        file: Option<(PathBuf, u64)>,
        /// The `<img>` element to send, and its thumbnail, once prepared.
        prepared: Option<(String, Arc<RenderImage>)>,
    },
    File {
        id: u64,
        name: String,
        path: PathBuf,
        size: u64,
    },
}

impl Attachment {
    pub fn id(&self) -> u64 {
        match self {
            Self::Image { id, .. } | Self::File { id, .. } => *id,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Self::Image { name, .. } | Self::File { name, .. } => name,
        }
    }
    /// Images wait for the content worker; files are ready at once.
    pub fn ready(&self) -> bool {
        !matches!(self, Self::Image { prepared: None, .. })
    }
}

/// Where an attached image comes from: a file is read when its turn comes.
pub enum Source {
    Bytes(Vec<u8>),
    File(PathBuf),
}

/// Images one message may carry. Each keeps up to 256 KiB to send and a
/// thumbnail; a hundred take two minutes to leave at stock Murmur's rate limit.
pub const MAX_IMAGES: usize = 100;
/// An image's base64 must fit one content worker frame (`MAX_FRAME_BYTES`);
/// a larger one goes as a file.
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Room kept beside every image for the text, which goes with the first.
pub const CAPTION_ROOM: usize = 1024;
/// Between the text and the first image.
const BREAK: &str = "<br/>";

/// What a dropped or chosen path can be sent as.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    /// PNG, JPEG, GIF or BMP, small enough for the content worker.
    Image {
        size: u64,
    },
    File {
        size: u64,
    },
}

/// Why a path cannot be attached.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Folder,
    Unreadable(String),
}

/// Whether `path` can go as an image, judged from its first bytes (the
/// content worker decodes the rest), or else as a file.
pub fn check(path: &Path) -> Result<Kind, Refusal> {
    let unreadable = |error: std::io::Error| Refusal::Unreadable(error.to_string());
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let metadata = file.metadata().map_err(unreadable)?;
    if metadata.is_dir() {
        return Err(Refusal::Folder);
    }
    let size = metadata.len();
    let mut head = Vec::with_capacity(12);
    file.take(12).read_to_end(&mut head).map_err(unreadable)?;
    let webp = head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP");
    let image = webp
        || [
            &b"\x89PNG\r\n\x1a\n"[..],
            b"\xff\xd8\xff",
            b"GIF87a",
            b"GIF89a",
            b"BM",
        ]
        .iter()
        .any(|magic| head.starts_with(magic));
    Ok(if image && size <= MAX_IMAGE_BYTES {
        Kind::Image { size }
    } else {
        Kind::File { size }
    })
}

/// Read an attached image, refusing one that has grown past the limit.
pub fn read(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|error| format!("cannot read it: {error}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read it: {error}"))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err("it is larger than 8 MB".into());
    }
    Ok(bytes)
}

/// One line about the paths that could not be attached.
pub fn refusal(refused: &[(String, Refusal)]) -> Option<String> {
    let folders: Vec<&str> = refused
        .iter()
        .filter(|(_, refusal)| *refusal == Refusal::Folder)
        .map(|(name, _)| name.as_str())
        .collect();
    let mut parts = Vec::new();
    if !folders.is_empty() {
        parts.push(format!(
            "Folders cannot be attached ({}).",
            folders.join(", ")
        ));
    }
    for (name, refusal) in refused {
        if let Refusal::Unreadable(error) = refusal {
            parts.push(format!("Could not read {name}: {error}."));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// A name for a pasted image that no other attachment in the tray has.
pub fn pasted_name(tray: &[Attachment]) -> String {
    (1..)
        .map(|number| format!("Pasted image {number}"))
        .find(|name| tray.iter().all(|attachment| attachment.name() != name))
        .unwrap_or_default()
}

/// The messages that carry `caption` (Mumble HTML) and `images`. Stock Mumble
/// shows any number of images in a message, but the whole message must fit
/// the server's image limit: one image each keeps every image as large as
/// that allows. The caption goes with the first image when both fit the
/// server's limits, or on its own before them.
pub fn messages(
    caption: Option<String>,
    images: Vec<String>,
    image_limit: usize,
    message_limit: usize,
) -> Vec<String> {
    let mut images = images.into_iter();
    let mut messages = Vec::new();
    if let Some(caption) = caption {
        match images.next() {
            // Murmur also measures an image message without the images'
            // sources against its text limit (with some markup of its own).
            Some(first)
                if caption.len() + BREAK.len() + first.len() <= image_limit
                    && caption.len() + 64 <= message_limit =>
            {
                messages.push(format!("{caption}{BREAK}{first}"));
            }
            Some(first) => messages.extend([caption, first]),
            None => messages.push(caption),
        }
    }
    messages.extend(images);
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(size: usize) -> String {
        format!(
            "<img src=\"data:image/png;base64,{}\" />",
            "A".repeat(size - 36)
        )
    }

    #[test]
    fn images_go_one_per_message_with_the_text_in_the_first() {
        let images = vec![image(100_000), image(120_000), image(50)];
        let sent = messages(Some("Look &amp; see".into()), images.clone(), 131_072, 5000);
        assert_eq!(
            sent,
            [
                format!("Look &amp; see<br/>{}", images[0]),
                images[1].clone(),
                images[2].clone()
            ]
        );
        // Without text, just the images; without images, just the text.
        assert_eq!(messages(None, images.clone(), 131_072, 5000), images);
        assert_eq!(
            messages(Some("hi".into()), Vec::new(), 131_072, 5000),
            ["hi"]
        );
        assert!(messages(None, Vec::new(), 131_072, 5000).is_empty());
    }

    #[test]
    fn text_that_does_not_fit_beside_the_first_image_goes_before_it() {
        let images = vec![image(130_000), image(1000)];
        // Over the image limit together.
        let caption = "x".repeat(2000);
        let sent = messages(Some(caption.clone()), images.clone(), 131_072, 5000);
        assert_eq!(sent, [caption, images[0].clone(), images[1].clone()]);
        // Within the image limit, but too long for the text limit beside an image.
        let caption = "y".repeat(4990);
        let sent = messages(Some(caption.clone()), vec![image(1000)], 131_072, 5000);
        assert_eq!(sent, [caption, image(1000)]);
        // The caption room kept beside every image is enough for ordinary text.
        let first = image(131_072 - CAPTION_ROOM);
        let caption = "z".repeat(CAPTION_ROOM - BREAK.len());
        let sent = messages(Some(caption.clone()), vec![first.clone()], 131_072, 5000);
        assert_eq!(sent, [format!("{caption}{BREAK}{first}")]);
    }

    #[test]
    fn paths_go_as_images_or_files_judged_by_their_first_bytes() {
        let directory = std::env::temp_dir().join(format!("nuschl-attach-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = directory.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let png = write("shot", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        assert_eq!(check(&png), Ok(Kind::Image { size: 16 }));
        let jpeg = write("photo.jpg", b"\xff\xd8\xff\xe0");
        assert_eq!(check(&jpeg), Ok(Kind::Image { size: 4 }));
        let pdf = write("report.pdf", b"%PDF-1.7");
        assert_eq!(check(&pdf), Ok(Kind::File { size: 8 }));
        assert_eq!(check(&write("empty.txt", b"")), Ok(Kind::File { size: 0 }));
        let webp = write("photo.webp", b"RIFF\0\0\0\0WEBP");
        assert_eq!(check(&webp), Ok(Kind::Image { size: 12 }));
        assert_eq!(check(&directory), Err(Refusal::Folder));
        assert!(matches!(
            check(&directory.join("missing.png")),
            Err(Refusal::Unreadable(_))
        ));
        // An image too large for the content worker goes as a file.
        let large = write("large.png", b"\x89PNG\r\n\x1a\n");
        std::fs::File::options()
            .write(true)
            .open(&large)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .unwrap();
        assert_eq!(
            check(&large),
            Ok(Kind::File {
                size: MAX_IMAGE_BYTES + 1
            })
        );
        assert!(read(&large).is_err());
        assert_eq!(read(&png).unwrap().len(), 16);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn refusals_name_the_paths_in_one_line() {
        let refused = [
            ("Photos".to_owned(), Refusal::Folder),
            (
                "b.png".to_owned(),
                Refusal::Unreadable("Permission denied".into()),
            ),
            ("Music".to_owned(), Refusal::Folder),
        ];
        assert_eq!(
            refusal(&refused).unwrap(),
            "Folders cannot be attached (Photos, Music). \
             Could not read b.png: Permission denied."
        );
        assert_eq!(refusal(&[]), None);
    }

    #[test]
    fn pasted_images_get_names_of_their_own() {
        let image = |name: &str| Attachment::Image {
            id: 0,
            name: name.into(),
            file: None,
            prepared: None,
        };
        assert_eq!(pasted_name(&[]), "Pasted image 1");
        let tray = [image("Pasted image 2"), image("Pasted image 1")];
        assert_eq!(pasted_name(&tray), "Pasted image 3");
        assert_eq!(pasted_name(&tray[..1]), "Pasted image 1");
    }
}
