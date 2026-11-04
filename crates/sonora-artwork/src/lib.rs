//! Artwork extraction, conversion and export.
//!
//! Two jobs, both testable without an audio device or a window:
//!
//! 1. Pull the embedded picture out of a file's tags.
//! 2. Re-encode it to the user's chosen format (JPEG or PNG), scaled down to a
//!    sane size, and write it beside the audio file as `cover.jpg`/`cover.png`
//!    (what Rockbox and most players expect) or into a content-addressed cache
//!    for the UI.
//!
//! Conversion is idempotent: an identical `cover.jpg` is left alone, a different
//! one is only replaced when the caller explicitly asks for it, and nothing is
//! written into a tag here. A batch over a large library therefore costs one
//! decode per file and does not churn the disk on repeat runs.

use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbImage};
use sonora_core::{ArtworkFormat, Error, Result};

/// Longest edge artwork is scaled to before it is stored or exported.
pub const DEFAULT_MAX_DIMENSION: u32 = 1400;

/// File stem written next to audio files.
pub const COVER_STEM: &str = "cover";

/// What to produce when converting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtworkOptions {
    pub format: ArtworkFormat,
    /// JPEG quality, 1–100; ignored for PNG.
    pub quality: u8,
    /// Longest edge in pixels; larger images are scaled down, never up.
    pub max_dimension: u32,
}

impl Default for ArtworkOptions {
    fn default() -> Self {
        Self {
            format: ArtworkFormat::Jpeg,
            quality: 90,
            max_dimension: DEFAULT_MAX_DIMENSION,
        }
    }
}

impl ArtworkOptions {
    pub fn normalized(mut self) -> Self {
        self.quality = self.quality.clamp(1, 100);
        if self.max_dimension == 0 {
            self.max_dimension = DEFAULT_MAX_DIMENSION;
        }
        self
    }
}

/// Encoded artwork ready to be written somewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertedArtwork {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Result of writing a cover beside an audio file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverWrite {
    /// The file did not exist and was created.
    Written(PathBuf),
    /// The same bytes were already there; nothing was touched.
    Unchanged(PathBuf),
    /// A different cover exists and overwriting was not requested.
    KeptExisting(PathBuf),
}

impl CoverWrite {
    pub fn path(&self) -> &Path {
        match self {
            Self::Written(path) | Self::Unchanged(path) | Self::KeptExisting(path) => path,
        }
    }
}

/// One entry in the content-addressed artwork cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtwork {
    /// blake3 of the *original* bytes, so re-tagging with a different encoder
    /// does not create a second cache entry.
    pub hash: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Read the first embedded picture from a file, if it has one.
pub fn extract_embedded(path: &Path) -> Result<Option<Vec<u8>>> {
    use lofty::prelude::TaggedFileExt;

    let tagged =
        lofty::read_from_path(path).map_err(|err| Error::metadata(path, err.to_string()))?;
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return Ok(None);
    };
    Ok(tag
        .pictures()
        .first()
        .map(|picture| picture.data().to_vec()))
}

/// Decode, scale and re-encode artwork.
///
/// JPEG output has its alpha channel flattened onto white, because JPEG cannot
/// store transparency and players render a dropped alpha channel as black.
pub fn convert(raw: &[u8], options: ArtworkOptions) -> Result<ConvertedArtwork> {
    let options = options.normalized();
    let image = image::load_from_memory(raw)
        .map_err(|err| Error::artwork(format!("unreadable image data: {err}")))?;

    let longest = image.width().max(image.height());
    let image = if longest > options.max_dimension {
        image.resize(
            options.max_dimension,
            options.max_dimension,
            FilterType::Lanczos3,
        )
    } else {
        image
    };

    let (width, height) = (image.width(), image.height());
    let mut bytes = Vec::new();
    let extension = match options.format {
        ArtworkFormat::Jpeg => {
            let rgb = flatten_onto_white(&image);
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, options.quality)
                .write_image(rgb.as_raw(), width, height, ExtendedColorType::Rgb8)
                .map_err(|err| Error::artwork(format!("could not encode JPEG: {err}")))?;
            "jpg"
        }
        ArtworkFormat::Png => {
            let rgba = image.to_rgba8();
            image::codecs::png::PngEncoder::new(&mut bytes)
                .write_image(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
                .map_err(|err| Error::artwork(format!("could not encode PNG: {err}")))?;
            "png"
        }
    };

    Ok(ConvertedArtwork {
        bytes,
        extension,
        width,
        height,
    })
}

fn flatten_onto_white(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let pixel = rgba.get_pixel(x, y);
        let alpha = f32::from(pixel[3]) / 255.0;
        let blend = |channel: u8| {
            (f32::from(channel) * alpha + 255.0 * (1.0 - alpha))
                .round()
                .clamp(0.0, 255.0) as u8
        };
        image::Rgb([blend(pixel[0]), blend(pixel[1]), blend(pixel[2])])
    })
}

/// Write `cover.jpg`/`cover.png` next to `audio_path`.
pub fn write_cover(
    audio_path: &Path,
    cover: &ConvertedArtwork,
    overwrite: bool,
) -> Result<CoverWrite> {
    let directory = audio_path.parent().ok_or_else(|| {
        Error::artwork(format!(
            "{} has no parent directory to write a cover into",
            audio_path.display()
        ))
    })?;
    let target = directory.join(format!("{COVER_STEM}.{}", cover.extension));

    if let Ok(existing) = std::fs::read(&target) {
        if existing == cover.bytes {
            return Ok(CoverWrite::Unchanged(target));
        }
        if !overwrite {
            return Ok(CoverWrite::KeptExisting(target));
        }
    }

    std::fs::write(&target, &cover.bytes).map_err(|err| Error::io(&target, err))?;
    Ok(CoverWrite::Written(target))
}

/// Content-addressed artwork cache, used by the UI for thumbnails.
#[derive(Debug, Clone)]
pub struct ArtworkStore {
    root: PathBuf,
}

impl ArtworkStore {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Cache `raw` under the hash of the original bytes.
    pub fn store(&self, raw: &[u8], options: ArtworkOptions) -> Result<StoredArtwork> {
        let hash = blake3::hash(raw).to_hex().to_string();
        let converted = convert(raw, options)?;

        let shard = self.root.join(&hash[..2]);
        std::fs::create_dir_all(&shard).map_err(|err| Error::io(&shard, err))?;
        let path = shard.join(format!("{hash}.{}", converted.extension));
        if !path.exists() {
            std::fs::write(&path, &converted.bytes).map_err(|err| Error::io(&path, err))?;
        }

        Ok(StoredArtwork {
            hash,
            path,
            bytes: converted.bytes.len() as u64,
        })
    }
}

/// Outcome of a batch run, for the CLI and the status bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtworkBatchReport {
    pub tracks: u64,
    pub written: u64,
    pub unchanged: u64,
    pub kept_existing: u64,
    pub without_artwork: u64,
    pub failed: u64,
}

impl ArtworkBatchReport {
    pub fn exported(&self) -> u64 {
        self.written + self.unchanged
    }
}

/// Extract, convert and export covers for `paths`.
///
/// Per-file failures are counted and logged rather than aborting the batch: one
/// corrupt file should not stop a 20 000-track run.
pub fn process_paths(
    paths: impl IntoIterator<Item = PathBuf>,
    options: ArtworkOptions,
    overwrite: bool,
) -> ArtworkBatchReport {
    let options = options.normalized();
    let mut report = ArtworkBatchReport::default();

    for path in paths {
        report.tracks += 1;
        let embedded = match extract_embedded(&path) {
            Ok(embedded) => embedded,
            Err(err) => {
                log::warn!("artwork: {err}");
                report.failed += 1;
                continue;
            }
        };

        let Some(raw) = embedded else {
            report.without_artwork += 1;
            continue;
        };

        let cover = match convert(&raw, options) {
            Ok(cover) => cover,
            Err(err) => {
                log::warn!("artwork: {}: {err}", path.display());
                report.failed += 1;
                continue;
            }
        };

        match write_cover(&path, &cover, overwrite) {
            Ok(CoverWrite::Written(_)) => report.written += 1,
            Ok(CoverWrite::Unchanged(_)) => report.unchanged += 1,
            Ok(CoverWrite::KeptExisting(_)) => report.kept_existing += 1,
            Err(err) => {
                log::warn!("artwork: {err}");
                report.failed += 1;
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use std::io::Cursor;

    fn png_with_alpha(width: u32, height: u32) -> Vec<u8> {
        let mut image = RgbaImage::from_pixel(width, height, Rgba([255, 0, 0, 255]));
        image.put_pixel(0, 0, Rgba([0, 0, 255, 0]));
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode fixture");
        bytes
    }

    fn silence_wav(path: &Path) {
        let samples: Vec<u8> = (0..64u16).flat_map(|_| 0i16.to_le_bytes()).collect();
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
        file.extend_from_slice(b"WAVEfmt ");
        file.extend_from_slice(&16u32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&44_100u32.to_le_bytes());
        file.extend_from_slice(&88_200u32.to_le_bytes());
        file.extend_from_slice(&2u16.to_le_bytes());
        file.extend_from_slice(&16u16.to_le_bytes());
        file.extend_from_slice(b"data");
        file.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        file.extend_from_slice(&samples);
        std::fs::write(path, file).expect("write wav fixture");
    }

    #[test]
    fn jpeg_output_drops_alpha_onto_white() {
        let cover = convert(&png_with_alpha(40, 40), ArtworkOptions::default()).unwrap();

        assert_eq!(cover.extension, "jpg");
        assert_eq!((cover.width, cover.height), (40, 40));
        assert_eq!(&cover.bytes[..2], &[0xff, 0xd8]);

        let decoded = image::load_from_memory(&cover.bytes).unwrap();
        assert_eq!(decoded.color(), image::ColorType::Rgb8);
        let corner = decoded.to_rgb8().get_pixel(0, 0).0;
        assert!(
            corner.iter().all(|channel| *channel > 200),
            "transparent pixels should become white, got {corner:?}"
        );
    }

    #[test]
    fn png_output_keeps_transparency() {
        let options = ArtworkOptions {
            format: ArtworkFormat::Png,
            ..ArtworkOptions::default()
        };
        let cover = convert(&png_with_alpha(40, 40), options).unwrap();

        assert_eq!(cover.extension, "png");
        let decoded = image::load_from_memory(&cover.bytes).unwrap();
        assert_eq!(decoded.color(), image::ColorType::Rgba8);
        assert_eq!(decoded.to_rgba8().get_pixel(0, 0).0[3], 0);
    }

    #[test]
    fn large_artwork_is_scaled_to_the_longest_edge() {
        let source = png_with_alpha(3000, 1500);
        let cover = convert(&source, ArtworkOptions::default()).unwrap();
        assert_eq!((cover.width, cover.height), (DEFAULT_MAX_DIMENSION, 700));
    }

    #[test]
    fn small_artwork_is_never_upscaled() {
        let source = png_with_alpha(200, 120);
        let cover = convert(&source, ArtworkOptions::default()).unwrap();
        assert_eq!((cover.width, cover.height), (200, 120));
    }

    #[test]
    fn quality_and_dimension_are_clamped() {
        let options = ArtworkOptions {
            quality: 0,
            max_dimension: 0,
            ..ArtworkOptions::default()
        }
        .normalized();
        assert_eq!(options.quality, 1);
        assert_eq!(options.max_dimension, DEFAULT_MAX_DIMENSION);
    }

    #[test]
    fn junk_data_is_an_error_not_a_panic() {
        let err = convert(b"definitely not an image", ArtworkOptions::default()).unwrap_err();
        assert!(err.to_string().contains("unreadable image data"));
    }

    #[test]
    fn the_cache_is_content_addressed_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtworkStore::at(directory.path().join("artwork"));
        let raw = png_with_alpha(32, 32);

        let first = store.store(&raw, ArtworkOptions::default()).unwrap();
        let second = store.store(&raw, ArtworkOptions::default()).unwrap();

        assert_eq!(first.path, second.path);
        assert_eq!(first.hash, blake3::hash(&raw).to_hex().to_string());
        assert!(first.path.exists());
        assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 1);
        let expected = convert(&raw, ArtworkOptions::default()).unwrap().bytes;
        assert_eq!(std::fs::read(&first.path).unwrap(), expected);
    }

    #[test]
    fn cover_is_written_beside_the_audio_file_then_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let audio = directory.path().join("01 Song.flac");
        silence_wav(&audio);
        let cover = convert(&png_with_alpha(32, 32), ArtworkOptions::default()).unwrap();

        let first = write_cover(&audio, &cover, false).unwrap();
        assert!(matches!(first, CoverWrite::Written(_)));
        assert_eq!(first.path(), &directory.path().join("cover.jpg"));
        assert_eq!(std::fs::read(first.path()).unwrap(), cover.bytes);

        let second = write_cover(&audio, &cover, false).unwrap();
        assert!(matches!(second, CoverWrite::Unchanged(_)));
    }

    #[test]
    fn a_different_cover_is_not_replaced_by_default() {
        let directory = tempfile::tempdir().unwrap();
        let audio = directory.path().join("track.mp3");
        let existing = directory.path().join("cover.jpg");
        std::fs::write(&existing, b"someone else's cover").unwrap();

        let cover = convert(&png_with_alpha(16, 16), ArtworkOptions::default()).unwrap();
        let outcome = write_cover(&audio, &cover, false).unwrap();
        assert!(matches!(outcome, CoverWrite::KeptExisting(_)));
        assert_eq!(std::fs::read(&existing).unwrap(), b"someone else's cover");

        let outcome = write_cover(&audio, &cover, true).unwrap();
        assert!(matches!(outcome, CoverWrite::Written(_)));
        assert_eq!(std::fs::read(&existing).unwrap(), cover.bytes);
    }

    /// Write a real ID3v2 tag with an embedded PNG into a WAV file, using
    /// `lofty` itself so the fixture is a genuinely tagged file.
    fn tag_wav_with_artwork(path: &Path) -> Vec<u8> {
        use lofty::config::WriteOptions;
        use lofty::picture::{MimeType, Picture, PictureType};
        use lofty::prelude::TaggedFileExt;
        use lofty::tag::{Tag, TagExt, TagType};

        let artwork = png_with_alpha(64, 64);
        silence_wav(path);

        let mut tagged = lofty::read_from_path(path).expect("read wav fixture");
        if tagged.primary_tag().is_none() {
            tagged.insert_tag(Tag::new(TagType::Id3v2));
        }
        let tag = tagged.primary_tag_mut().expect("tag after insert");
        let picture: Picture = Picture::unchecked(artwork.clone())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .description("front cover")
            .into();
        tag.push_picture(picture);
        tag.save_to_path(path, WriteOptions::default())
            .expect("write tag");
        artwork
    }

    #[test]
    fn embedded_artwork_is_extracted_and_exported() {
        let directory = tempfile::tempdir().unwrap();
        let audio = directory.path().join("tagged.wav");
        let artwork = tag_wav_with_artwork(&audio);

        let extracted = extract_embedded(&audio).unwrap().expect("embedded picture");
        assert_eq!(extracted, artwork);

        let report = process_paths([audio.clone()], ArtworkOptions::default(), false);
        assert_eq!(report.written, 1, "{report:?}");
        assert_eq!(report.failed, 0, "{report:?}");
        assert!(directory.path().join("cover.jpg").exists());
    }

    #[test]
    fn untagged_audio_counts_as_having_no_artwork() {
        let directory = tempfile::tempdir().unwrap();
        let audio = directory.path().join("plain.wav");
        silence_wav(&audio);

        let report = process_paths([audio], ArtworkOptions::default(), false);
        assert_eq!(report.tracks, 1);
        assert_eq!(report.without_artwork, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.written, 0);
    }
}
