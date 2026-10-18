//! The only images the reader shows: the message's own, its `cid:` parts and its `data:` URLs.
//! Every other URL resolves to a refusal here, so that neither GPUI's image loader nor any HTTP
//! client is ever asked for one: a remote image, a tracking pixel, a `file:` path or a bare name
//! all draw nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gpui_kit::{Image, ImageCacheError, ImageFormat, ImageSource};
use missive_render::message::decode_data_url;

/// The images of one message.
pub struct MessageImages {
    /// By Content-ID, without its angle brackets.
    parts: HashMap<String, Arc<Image>>,
    /// `data:` URLs decoded once, by URL; `None` for one that is not an image shown here.
    data: Mutex<HashMap<String, Option<Arc<Image>>>>,
}

impl MessageImages {
    pub fn new(parts: &HashMap<String, Vec<u8>>) -> MessageImages {
        MessageImages {
            parts: parts
                .iter()
                .filter_map(|(id, bytes)| Some((id.clone(), image(bytes)?)))
                .collect(),
            data: Mutex::new(HashMap::new()),
        }
    }

    /// What the reader draws for an image URL: authoritative, nothing is ever fetched.
    pub fn source(&self, uri: &str) -> ImageSource {
        let found = if let Some(id) = strip_scheme(uri, "cid:") {
            self.parts.get(id).cloned()
        } else if let Some(rest) = strip_scheme(uri, "data:") {
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            data.entry(uri.to_owned())
                .or_insert_with(|| image(&decode_data_url(&format!("data:{rest}"))?))
                .clone()
        } else {
            None
        };
        match found {
            Some(image) => ImageSource::Image(image),
            None => refused(),
        }
    }
}

/// `uri` without its scheme, matched without regard to case as URL schemes are.
fn strip_scheme<'a>(uri: &'a str, scheme: &str) -> Option<&'a str> {
    let head = uri.get(..scheme.len())?;
    head.eq_ignore_ascii_case(scheme)
        .then(|| &uri[scheme.len()..])
}

/// An image GPUI can draw from bytes alone, by its signature: the raster formats mail uses. An
/// SVG is not one: it may name other files.
fn image(bytes: &[u8]) -> Option<Arc<Image>> {
    let format = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ImageFormat::Png
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        ImageFormat::Jpeg
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        ImageFormat::Gif
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        ImageFormat::Webp
    } else {
        return None;
    };
    Some(Arc::new(Image::from_bytes(format, bytes.to_vec())))
}

/// A source that loads nothing and draws nothing.
fn refused() -> ImageSource {
    ImageSource::Custom(Arc::new(|_, _| {
        Some(Err(ImageCacheError::Io(Arc::new(std::io::Error::other(
            "not an image of the message",
        )))))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1×1 PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    fn images() -> MessageImages {
        MessageImages::new(&HashMap::from([
            ("logo@example.org".to_owned(), PNG.to_vec()),
            ("notes@example.org".to_owned(), b"plain text".to_vec()),
        ]))
    }

    fn is_image(source: &ImageSource) -> bool {
        matches!(source, ImageSource::Image(_))
    }

    fn is_refused(source: &ImageSource) -> bool {
        matches!(source, ImageSource::Custom(_))
    }

    #[test]
    fn a_part_of_the_message_is_served_by_its_content_id() {
        assert!(is_image(&images().source("cid:logo@example.org")));
        assert!(is_image(&images().source("CID:logo@example.org")));
    }

    #[test]
    fn a_data_image_is_served() {
        let source = images().source(&format!("data:image/png;base64,{PNG_BASE64}"));
        assert!(is_image(&source));
    }

    #[test]
    fn the_image_source_refuses_every_url_but_the_message_parts_and_data_images() {
        let images = images();
        for uri in [
            "https://images.example.org/logo.png",
            "http://127.0.0.1:9/pixel.gif",
            "//images.example.org/logo.png",
            "file:///etc/hostname",
            "/etc/hostname",
            "logo.png",
            "ftp://example.org/logo.png",
            "about:blank",
            // A part the message does not have, one that is not an image, and data that is not.
            "cid:other@example.org",
            "cid:notes@example.org",
            "data:text/plain,hello",
            "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg'><image href='https://example.org/a.png'/></svg>",
            "data:image/png;base64,!!!",
            "",
        ] {
            assert!(is_refused(&images.source(uri)), "{uri}");
        }
    }
}
