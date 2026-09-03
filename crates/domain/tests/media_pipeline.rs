//! Tests for the image normalisation pipeline.
//!
//! These assert the security properties directly rather than trusting that
//! "we re-encode, so it must be fine". The EXIF test in particular builds a
//! JPEG carrying real GPS bytes and then asserts those bytes are gone from the
//! output — because "the metadata is stripped" is exactly the kind of claim
//! that is easy to write and easy to be wrong about.

use domain::{process_image, ImagePurpose, MediaError};
use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
use std::io::Cursor;

fn opaque_png(width: u32, height: u32) -> Vec<u8> {
    let mut buffer = Vec::new();
    DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, image::Rgb([10, 120, 200])))
        .write_to(&mut Cursor::new(&mut buffer), ImageFormat::Png)
        .expect("encoding a test png succeeds");
    buffer
}

fn transparent_png(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = RgbaImage::from_pixel(width, height, image::Rgba([10, 120, 200, 255]));
    // One genuinely transparent pixel is all it takes to make PNG the right
    // output format.
    pixels.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));

    let mut buffer = Vec::new();
    DynamicImage::ImageRgba8(pixels)
        .write_to(&mut Cursor::new(&mut buffer), ImageFormat::Png)
        .expect("encoding a test png succeeds");
    buffer
}

fn decode(bytes: &[u8]) -> DynamicImage {
    image::load_from_memory(bytes).expect("output is a decodable image")
}

#[test]
fn an_opaque_image_comes_out_as_jpeg() {
    let out = process_image(&opaque_png(64, 64), ImagePurpose::Avatar).expect("processes");

    assert_eq!(out.content_type, "image/jpeg");
    assert_eq!(out.extension, "jpg");
    assert!(!decode(&out.bytes).color().has_alpha());
}

#[test]
fn a_transparent_image_keeps_its_alpha_as_png() {
    let out = process_image(&transparent_png(64, 64), ImagePurpose::Avatar).expect("processes");

    assert_eq!(out.content_type, "image/png");
    assert!(decode(&out.bytes).color().has_alpha());
}

/// A PNG whose alpha channel exists but is fully opaque must NOT be treated as
/// transparent: that is the common screenshot case, and emitting PNG for it
/// stores several times the bytes for a visually identical image.
#[test]
fn an_alpha_channel_that_is_fully_opaque_still_becomes_jpeg() {
    let mut buffer = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(
        64,
        64,
        image::Rgba([10, 120, 200, 255]),
    ))
    .write_to(&mut Cursor::new(&mut buffer), ImageFormat::Png)
    .expect("encoding succeeds");

    let out = process_image(&buffer, ImagePurpose::Avatar).expect("processes");
    assert_eq!(out.content_type, "image/jpeg");
}

#[test]
fn an_oversized_image_is_scaled_into_the_box_keeping_its_aspect_ratio() {
    let out = process_image(&opaque_png(2000, 1000), ImagePurpose::Avatar).expect("processes");

    assert!(out.width <= 512 && out.height <= 512, "{out:?}");
    // 2:1 in, 2:1 out.
    assert_eq!(out.width, 512);
    assert_eq!(out.height, 256);
}

#[test]
fn a_small_image_is_not_enlarged() {
    let out = process_image(&opaque_png(48, 48), ImagePurpose::Avatar).expect("processes");
    assert_eq!((out.width, out.height), (48, 48));
}

#[test]
fn a_banner_gets_the_banner_box_not_the_avatar_one() {
    let out = process_image(&opaque_png(3000, 1000), ImagePurpose::Banner).expect("processes");
    assert_eq!(out.width, 1500);
    assert_eq!(out.height, 500);
}

/// The whole point of the module. A JPEG carrying an EXIF block with GPS
/// coordinates must come out the other side with no trace of them.
#[test]
fn exif_gps_metadata_does_not_survive() {
    let gps_marker = b"GPSLatitudeRef";

    let mut jpeg = Vec::new();
    DynamicImage::ImageRgb8(RgbImage::from_pixel(80, 80, image::Rgb([200, 30, 60])))
        .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
        .expect("encoding succeeds");

    // Splice a minimal APP1/Exif segment in right after the SOI marker, which
    // is where a camera would put it.
    let mut exif_payload = b"Exif\0\0MM\0*\0\0\0\x08".to_vec();
    exif_payload.extend_from_slice(gps_marker);
    exif_payload.extend_from_slice(b"\0\0GPS 40.7128 N 74.0060 W\0");

    let length = (exif_payload.len() + 2) as u16;
    let mut segment = vec![0xFF, 0xE1];
    segment.extend_from_slice(&length.to_be_bytes());
    segment.extend_from_slice(&exif_payload);

    let mut with_exif = Vec::new();
    with_exif.extend_from_slice(&jpeg[..2]); // SOI
    with_exif.extend_from_slice(&segment);
    with_exif.extend_from_slice(&jpeg[2..]);

    assert!(
        contains(&with_exif, gps_marker),
        "the fixture itself must carry the GPS bytes, or this test proves nothing"
    );

    let out = process_image(&with_exif, ImagePurpose::Avatar).expect("processes");

    assert!(
        !contains(&out.bytes, gps_marker),
        "GPS metadata survived re-encoding"
    );
    assert!(
        !contains(&out.bytes, b"Exif"),
        "an Exif block survived re-encoding"
    );
}

/// A polyglot: a valid image with a payload appended. It passes any magic-byte
/// check, because its header really is a PNG header. What must not happen is
/// the payload being stored and later served back.
#[test]
fn an_appended_payload_does_not_survive() {
    let payload = b"<?php system($_GET['c']); ?>";
    let mut polyglot = opaque_png(64, 64);
    polyglot.extend_from_slice(payload);

    let out = process_image(&polyglot, ImagePurpose::Avatar).expect("the image part still decodes");

    assert!(
        !contains(&out.bytes, payload),
        "an appended payload survived re-encoding"
    );
}

#[test]
fn a_file_that_is_not_an_image_is_rejected() {
    let result = process_image(b"just some text, definitely not an image", ImagePurpose::Avatar);
    assert!(
        matches!(result, Err(MediaError::UnknownFormat)),
        "got: {result:?}"
    );
}

/// A real image in a format we do not accept. It decodes fine — the point is
/// that the allowlist is what decides, not decodability.
#[test]
fn an_accepted_looking_but_unlisted_format_is_rejected() {
    let mut bmp = Vec::new();
    DynamicImage::ImageRgb8(RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3])))
        .write_to(&mut Cursor::new(&mut bmp), ImageFormat::Bmp)
        .expect("encoding succeeds");

    let result = process_image(&bmp, ImagePurpose::Avatar);
    assert!(
        matches!(result, Err(MediaError::UnsupportedFormat)),
        "got: {result:?}"
    );
}

#[test]
fn an_upload_over_the_byte_cap_is_rejected_before_decoding() {
    let too_big = vec![0u8; ImagePurpose::Avatar.max_upload_bytes() + 1];
    let result = process_image(&too_big, ImagePurpose::Avatar);
    assert!(matches!(result, Err(MediaError::TooLarge { .. })), "got: {result:?}");
}

#[test]
fn an_empty_upload_is_rejected() {
    assert!(matches!(
        process_image(&[], ImagePurpose::Avatar),
        Err(MediaError::Empty)
    ));
}

/// A banner may be larger than an avatar. If the caps were shared, this would
/// silently reject legitimate banners.
#[test]
fn the_banner_cap_is_larger_than_the_avatar_cap() {
    assert!(ImagePurpose::Banner.max_upload_bytes() > ImagePurpose::Avatar.max_upload_bytes());
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
