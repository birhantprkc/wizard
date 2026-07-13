//! Session-scoped image store under `~/.wizard/images/<session>/`.
//!
//! Every image that passes through a turn — returned by a tool, or produced by
//! the model itself — is written here as a real file before the surfaces are
//! told about it, so a renderer never has to re-derive a path or handle base64:
//!
//! ```text
//! ~/.wizard/images/<session-id>/<content-hash>.png
//! ```
//!
//! The name is the hash of the file's own bytes, so the path is stable: the
//! same image saved twice (a model that repeats itself, a session resumed and
//! replayed) lands on the same file instead of accumulating copies.
//!
//! The base64 stays in the model's [`ChatMessage`](crate::llm::ChatMessage) —
//! a vision model needs it in history — but the surfaces only ever see an
//! [`ImageRef`], which is a path. The TUI prints it when the terminal cannot
//! draw; the GUI links to it for "open full size".

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::llm::{Image, MAX_IMAGE_BYTES};

/// An image on disk, as announced to the surfaces
/// ([`AgentEvent::Images`](crate::agent::AgentEvent::Images)). Deliberately
/// carries no base64: a transcript frame references the image, it never embeds
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    /// Absolute path of the saved file.
    pub path: PathBuf,
    /// Media type of the file, e.g. `image/png`.
    pub mime: String,
    /// Size of the file on disk, in bytes.
    pub bytes: usize,
}

/// Where one session's images land. Cheap to share behind an `Arc`; all
/// methods take `&self`. The directory is created on the first save, so a
/// session that never sees an image leaves nothing behind.
#[derive(Debug)]
pub struct ImageStore {
    dir: PathBuf,
}

impl ImageStore {
    /// The store for session `id`: `~/.wizard/images/<id>/`.
    pub fn open(session_id: &str) -> Result<Self> {
        Ok(Self::in_dir(Config::images_dir()?.join(session_id)))
    }

    /// A store rooted at `dir` — for callers that own their own root (tests,
    /// and any surface that keeps images outside `~/.wizard`).
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Directory this store writes to (created lazily by [`Self::save`]).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write `image` to disk and describe where it landed. The file is named
    /// after the hash of its own bytes, so saving the same image twice is
    /// idempotent rather than duplicative.
    pub fn save(&self, image: &Image) -> Result<ImageRef> {
        let bytes = image.decode().context("decoding image payload")?;
        if bytes.len() > MAX_IMAGE_BYTES {
            anyhow::bail!(
                "image is {} bytes, over the {MAX_IMAGE_BYTES} byte cap",
                bytes.len()
            );
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let digest = Sha256::digest(&bytes);
        let name = format!("{:x}", digest);
        let path = self
            .dir
            .join(format!("{}.{}", &name[..16], image.extension()));
        // Content-addressed: an existing file with this name already holds
        // exactly these bytes, so rewriting it would only cost the I/O.
        if !path.is_file() {
            std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(ImageRef {
            path,
            mime: image.mime.clone(),
            bytes: bytes.len(),
        })
    }

    /// Take in a batch: each image is written, tagged with where it landed
    /// ([`Image::at_path`], so the session file records the path and no surface
    /// replaying a transcript has to re-derive it), and described by an
    /// [`ImageRef`] for the surfaces.
    ///
    /// Returns `(images, refs)`. Persistence is best-effort: an image that
    /// cannot be written is still returned (the model gets it either way, from
    /// the base64 in history) but carries no path and no ref — a full disk
    /// costs the surfaces their copy, never the turn.
    pub fn save_all(&self, images: Vec<Image>) -> (Vec<Image>, Vec<ImageRef>) {
        let mut kept = Vec::with_capacity(images.len());
        let mut refs = Vec::with_capacity(images.len());
        for image in images {
            match self.save(&image) {
                Ok(saved) => {
                    kept.push(image.at_path(saved.path.clone()));
                    refs.push(saved);
                }
                Err(err) => {
                    tracing::warn!("could not persist image: {err:#}");
                    kept.push(image);
                }
            }
        }
        (kept, refs)
    }
}

/// Split `images` into those within [`MAX_IMAGE_BYTES`] and those over it.
/// The oversized ones are dropped by the caller (with a notice): an absurd
/// image must not reach history, where it would melt the context window and
/// bloat the session file.
pub fn split_oversized(images: Vec<Image>) -> (Vec<Image>, Vec<Image>) {
    images
        .into_iter()
        .partition(|image| image.decoded_len() <= MAX_IMAGE_BYTES)
}

/// One-line notice naming the images dropped for being over the cap.
pub fn oversized_notice(dropped: &[Image]) -> String {
    let sizes: Vec<String> = dropped
        .iter()
        .map(|image| format!("{} MB", image.decoded_len() / (1024 * 1024)))
        .collect();
    format!(
        "dropped {} oversized image(s) ({}) — the cap is {} MB",
        dropped.len(),
        sizes.join(", "),
        MAX_IMAGE_BYTES / (1024 * 1024)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Image {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(b"pixels");
        Image::from_bytes(&bytes).expect("a PNG")
    }

    #[test]
    fn save_writes_the_decoded_file_and_reports_where() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::in_dir(dir.path().join("session-1"));
        let image = png();

        let saved = store.save(&image).expect("saved");
        assert_eq!(saved.mime, "image/png");
        assert_eq!(saved.bytes, image.decode().unwrap().len());
        assert!(saved.path.starts_with(store.dir()), "under the session dir");
        assert_eq!(saved.path.extension().unwrap(), "png");
        assert_eq!(
            std::fs::read(&saved.path).expect("file on disk"),
            image.decode().unwrap(),
            "the file holds the decoded image, not base64"
        );
    }

    #[test]
    fn the_path_is_stable_for_the_same_image() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::in_dir(dir.path());
        let first = store.save(&png()).expect("saved");
        let again = store.save(&png()).expect("saved");
        assert_eq!(first.path, again.path, "content-addressed: one file");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no duplicate copies accumulate"
        );

        // A different image gets a different name.
        let other = Image::from_bytes(&[0xff, 0xd8, 0xff, 0x01]).expect("a JPEG");
        let other = store.save(&other).expect("saved");
        assert_ne!(other.path, first.path);
        assert_eq!(other.path.extension().unwrap(), "jpg");
    }

    #[test]
    fn the_directory_is_created_lazily() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::in_dir(dir.path().join("deep").join("session-2"));
        assert!(
            !store.dir().exists(),
            "nothing on disk until an image lands"
        );
        store.save(&png()).expect("saved");
        assert!(store.dir().is_dir());
    }

    #[test]
    fn save_all_tags_each_image_with_where_it_landed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::in_dir(dir.path());
        let (images, refs) = store.save_all(vec![png()]);
        assert_eq!(images[0].path.as_ref(), Some(&refs[0].path));
        assert!(
            refs[0].path.is_file(),
            "the path on the message is the file that exists"
        );
    }

    #[test]
    fn save_all_keeps_what_it_cannot_write_but_leaves_it_unannounced() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::in_dir(dir.path());
        // The second image is not valid base64: it cannot be written, but the
        // model still gets it and the other two land.
        let (images, refs) = store.save_all(vec![png(), Image::new("!!!", "image/png"), png()]);
        assert_eq!(images.len(), 3, "a broken payload never fails the turn");
        assert!(images[1].path.is_none(), "and is announced to no one");
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn oversized_images_are_split_off_with_a_notice() {
        let huge = Image::new("A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 8), "image/png");
        let (kept, dropped) = split_oversized(vec![png(), huge, png()]);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped.len(), 1);
        let notice = oversized_notice(&dropped);
        assert!(notice.contains("dropped 1 oversized image"), "{notice}");
        assert!(notice.contains("10 MB"), "the cap is named: {notice}");

        // The store refuses one too, in case a caller hand-builds it.
        let dir = tempfile::tempdir().unwrap();
        assert!(
            ImageStore::in_dir(dir.path()).save(&dropped[0]).is_err(),
            "the store is a backstop for the cap"
        );
    }
}
