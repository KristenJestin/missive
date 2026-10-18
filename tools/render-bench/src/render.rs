//! Draws HTML with Blitz on the CPU into a PNG: no window, no GPU, no network.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyrender::{PaintScene as _, render_to_buffer};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_dom::util::Color;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use peniko::Fill;
use peniko::kurbo::{Affine, Rect};

use crate::net::MessageOnly;

/// Relative URLs need a base to resolve against (Blitz panics without one); `.invalid` never
/// resolves, and nothing is fetched from it anyway.
const BASE_URL: &str = "https://message.invalid/";

/// A taller page is cut: the PNG of a very long newsletter stays a few tens of megabytes.
pub const MAX_HEIGHT: u32 = 12_000;

pub struct Drawn {
    pub height: u32,
    pub clipped: bool,
    pub blocked: usize,
    pub embedded: usize,
}

pub fn draw(
    html: &str,
    parts: HashMap<String, Vec<u8>>,
    width: u32,
    scheme: ColorScheme,
    png: &Path,
) -> Result<Drawn, String> {
    let net = Arc::new(MessageOnly::new(parts));
    let mut document = HtmlDocument::from_html(
        html,
        DocumentConfig {
            base_url: Some(BASE_URL.to_string()),
            net_provider: Some(Arc::clone(&net) as _),
            viewport: Some(Viewport::new(width, 600, 1.0, scheme)),
            ..Default::default()
        },
    );

    // Every resource is answered at once, but a stylesheet answered in one pass can ask for an
    // image or an import read in the next: three passes let those in.
    for _ in 0..3 {
        document.resolve(0.0);
    }

    let full_height = document.root_element().final_layout().size.height.ceil() as u32;
    let height = full_height.clamp(1, MAX_HEIGHT);
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            // What a mail view shows under a message that sets no background.
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                Color::WHITE,
                None,
                &Rect::new(0.0, 0.0, width as f64, height as f64),
            );
            paint_scene(scene, &mut document, 1.0, width, height, 0, 0);
        },
        width,
        height,
    );
    image::save_buffer_with_format(
        png,
        &buffer,
        width,
        height,
        image::ExtendedColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .map_err(|error| format!("cannot write the PNG: {error}"))?;

    Ok(Drawn {
        height,
        clipped: full_height > MAX_HEIGHT,
        blocked: net.blocked.load(Ordering::Relaxed),
        embedded: net.embedded.load(Ordering::Relaxed),
    })
}
