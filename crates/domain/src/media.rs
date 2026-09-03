//! Image normalisation for profile pictures and banners.
//!
//! The invariant this module exists to uphold: **the server never stores or
//! serves the bytes the user uploaded.** Everything below follows from that.
//!
//! A presigned direct-to-storage upload cannot make that promise. `Content-Type`
//! is client-supplied and trivially forged; a magic-byte check is necessary but
//! not sufficient, because a polyglot file carries a valid image header with a
//! payload appended; EXIF survives untouched, and a phone photo carries GPS
//! accurate to a few metres, so a profile picture can publish its owner's home
//! address. So avatars take the one media path that passes through the backend,
//! and come out the other side rebuilt from decoded pixels.

use std::io::{BufReader, Cursor};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use thiserror::Error;

/// Formats accepted on the way IN. Not the same set as what goes out.
const ACCEPTED_INPUT: [ImageFormat; 3] = [ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::WebP];

/// Ceiling on the *decoded* dimensions, which is the real decompression-bomb
/// guard — the byte cap alone is not one, since a 100 KiB PNG can describe a
/// 20000x20000 canvas. 8000x8000 still accepts a 48-megapixel phone photo.
const MAX_DECODED_DIMENSION: u32 = 8000;

/// Ceiling on what a single decode may allocate. 8000x8000 at 4 bytes per
/// pixel is 256 MiB, so this is the arithmetic above, expressed as the limit
/// the decoder actually enforces.
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// Quality for the JPEG we emit. 82 is the usual "no visible loss at normal
/// viewing size" point; above ~90 the file grows fast for nothing.
const JPEG_QUALITY: u8 = 82;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("image is larger than the {limit} byte limit")]
    TooLarge { limit: usize },
    #[error("empty upload")]
    Empty,
    #[error("unrecognised image format")]
    UnknownFormat,
    #[error("unsupported image format: only JPEG, PNG and WebP are accepted")]
    UnsupportedFormat,
    #[error("image could not be decoded: {0}")]
    Decode(String),
    #[error("image could not be re-encoded: {0}")]
    Encode(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePurpose {
    Avatar,
    Banner,
}

impl ImagePurpose {
    /// Cap on the uploaded body. Enforced by the caller BEFORE reading the
    /// whole thing into memory — checking after buffering means a hostile
    /// client already spent the memory.
    pub const fn max_upload_bytes(self) -> usize {
        match self {
            ImagePurpose::Avatar => 2 * 1024 * 1024,
            ImagePurpose::Banner => 5 * 1024 * 1024,
        }
    }

    /// The box the stored image is fitted into, preserving aspect ratio.
    pub const fn max_stored_dimensions(self) -> (u32, u32) {
        match self {
            ImagePurpose::Avatar => (512, 512),
            ImagePurpose::Banner => (1500, 500),
        }
    }
}

/// What comes out: bytes nobody uploaded.
#[derive(Debug, Clone)]
pub struct ProcessedImage {
    pub bytes: Vec<u8>,
    /// Derived from what was actually encoded, never echoed from the request.
    pub content_type: &'static str,
    pub extension: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Decodes `input`, normalises it, and re-encodes it.
///
/// Pure: no storage, no database, no authorization. That makes the whole
/// security-relevant path testable with a byte slice and no fixtures.
pub fn process_image(input: &[u8], purpose: ImagePurpose) -> Result<ProcessedImage, MediaError> {
    if input.is_empty() {
        return Err(MediaError::Empty);
    }

    let limit = purpose.max_upload_bytes();
    if input.len() > limit {
        return Err(MediaError::TooLarge { limit });
    }

    let mut reader = ImageReader::new(BufReader::new(Cursor::new(input)))
        .with_guessed_format()
        .map_err(|e| MediaError::Decode(e.to_string()))?;

    // The format comes from sniffing the actual bytes. The request's
    // Content-Type is never consulted anywhere in this module — that header is
    // a claim by the uploader, and this is the code that decides whether the
    // claim was true.
    let format = reader.format().ok_or(MediaError::UnknownFormat)?;
    if !ACCEPTED_INPUT.contains(&format) {
        return Err(MediaError::UnsupportedFormat);
    }

    reader.limits(decode_limits());

    let mut decoder = reader
        .into_decoder()
        .map_err(|e| MediaError::Decode(e.to_string()))?;

    // Read the orientation BEFORE decoding, because it is EXIF — the very
    // thing this pipeline destroys. Strip it without applying it and every
    // phone photo comes out sideways, which looks like a bug in the resize.
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);

    let mut image =
        DynamicImage::from_decoder(decoder).map_err(|e| MediaError::Decode(e.to_string()))?;
    image.apply_orientation(orientation);

    let (max_w, max_h) = purpose.max_stored_dimensions();
    // `resize` only ever shrinks here: enlarging a small avatar would spend
    // bytes to add no detail.
    if image.width() > max_w || image.height() > max_h {
        image = image.resize(max_w, max_h, FilterType::Lanczos3);
    }

    if has_visible_transparency(&image) {
        encode_png(&image)
    } else {
        encode_jpeg(&image)
    }
}

fn decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODED_DIMENSION);
    limits.max_image_height = Some(MAX_DECODED_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

/// Whether the image *uses* its alpha channel, as opposed to merely having
/// one. Screenshots and many exported PNGs carry a fully-opaque alpha channel;
/// treating those as transparent would emit PNG and store several times the
/// bytes JPEG needs, for an image that looks identical. At these dimensions
/// the scan is a fraction of a millisecond.
fn has_visible_transparency(image: &DynamicImage) -> bool {
    if !image.has_alpha() {
        return false;
    }
    image.to_rgba8().pixels().any(|pixel| pixel.0[3] < u8::MAX)
}

fn encode_jpeg(image: &DynamicImage) -> Result<ProcessedImage, MediaError> {
    // JPEG has no alpha channel. Flattening explicitly is clearer than relying
    // on the encoder's implicit conversion, and makes the discard deliberate.
    let rgb = DynamicImage::ImageRgb8(image.to_rgb8());
    let mut bytes = Vec::new();
    rgb.write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY))
        .map_err(|e| MediaError::Encode(e.to_string()))?;

    Ok(ProcessedImage {
        bytes,
        content_type: "image/jpeg",
        extension: "jpg",
        width: rgb.width(),
        height: rgb.height(),
    })
}

fn encode_png(image: &DynamicImage) -> Result<ProcessedImage, MediaError> {
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|e| MediaError::Encode(e.to_string()))?;

    Ok(ProcessedImage {
        bytes,
        content_type: "image/png",
        extension: "png",
        width: image.width(),
        height: image.height(),
    })
}
