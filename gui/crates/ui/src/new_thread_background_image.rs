//! One decoding contract for background installation and rendering. Attachment
//! formats are broader (notably SVG), so attachment staging is not validation.

pub(crate) fn decode(bytes: &[u8]) -> image::ImageResult<image::DynamicImage> {
    // Inspect the exact bytes that will be saved, not the source extension or
    // a second read of a file that could change between validation and copy.
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()?
        .decode()
}

/// Bundled Wizard artwork: Starship stacked on the pad at Starbase. It is the
/// new-chat background whenever Wizard is the selected harness and the user
/// has not chosen an image of their own.
pub(crate) static WIZARD_BACKGROUND: &[u8] =
    include_bytes!("../assets/backgrounds/wizard-starship.jpg");
