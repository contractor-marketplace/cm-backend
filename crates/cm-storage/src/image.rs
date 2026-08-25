//! Turning an uploaded file into something safe to publish.
//!
//! This module exists because of one fact: a photograph of a house usually
//! contains the coordinates of that house. Phones write GPS into EXIF by
//! default, and `jobs` has deliberately no address column and no precise point —
//! that absence is the entire privacy argument for the table. Storing an upload
//! byte-for-byte would hand back the address we refused to collect, in a place
//! nobody would think to look.
//!
//! So nothing is stored as uploaded. Every image is decoded to pixels and
//! re-encoded from scratch. Metadata is discarded by construction rather than by
//! remembering to strip particular tags — there is no code path here that copies
//! a tag across, so there is no code path that can be made to forget one.
//!
//! Three other things fall out of the same pass, which is why it is one pass:
//!
//! - A file that does not decode is not an image, whatever it claims to be. A
//!   polyglot that is both a valid JPEG and a valid HTML page stops being the
//!   second half of that when it is re-encoded.
//! - A decompression bomb — a few kilobytes that expand to gigabytes of pixels —
//!   is refused by a limit set before decoding, not discovered by running out of
//!   memory.
//! - EXIF orientation is applied and then dropped. Skipping this is how every
//!   photo taken in portrait ends up sideways.

use cm_core::AppError;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ImageDecoder, ImageReader, Limits};
use std::io::Cursor;

/// The long edge of a stored photo, in pixels.
///
/// Large enough that a contractor can see the state of a wall; small enough that
/// a job with eight photos is not a multi-megabyte page. Phone cameras are
/// several times this, so nearly every upload is scaled down.
pub const MAX_EDGE: u32 = 2000;

/// Quality for the re-encode. 82 is the usual point past which a photograph
/// gets bigger without looking better.
const JPEG_QUALITY: u8 = 82;

/// Ceiling on decoded pixels, enforced before any are allocated.
///
/// 50 megapixels is beyond any phone and roughly 200 MB decoded, which bounds
/// what one request can cost us. The point is that the limit is checked against
/// the header rather than reached by allocating until it hurts.
const MAX_PIXELS: u64 = 50_000_000;

/// A normalised image, ready to be stored. Always JPEG.
pub struct Normalised {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Normalised {
    pub const CONTENT_TYPE: &'static str = "image/jpeg";
}

/// Decode, upright, downscale and re-encode.
///
/// Every error is `AppError::invalid`, because every way this fails is the
/// caller's file being unusable rather than us being broken.
pub fn normalise(bytes: &[u8]) -> Result<Normalised, AppError> {
    // Format comes from the magic bytes, never from a client-supplied
    // Content-Type. A caller who mislabels a PNG as a JPEG gets their PNG
    // decoded; a caller who labels a zip as a JPEG gets rejected.
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| AppError::invalid("That file could not be read as an image."))?;

    if reader.format().is_none() {
        return Err(AppError::invalid(
            "That file is not an image we recognise. JPEG, PNG and WebP work.",
        ));
    }

    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_PIXELS as u32);
    limits.max_image_height = Some(MAX_PIXELS as u32);
    limits.max_alloc = Some(MAX_PIXELS * 4);
    reader.limits(limits);

    let mut decoder = reader
        .into_decoder()
        .map_err(|_| AppError::invalid("That image could not be decoded."))?;

    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(AppError::invalid("That image is too large to process."));
    }

    // Read the orientation BEFORE decoding consumes the decoder. This is the
    // only thing carried across from the metadata, and it is applied to pixels
    // rather than preserved as a tag.
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);

    let mut decoded = image::DynamicImage::from_decoder(decoder)
        .map_err(|_| AppError::invalid("That image could not be decoded."))?;
    decoded.apply_orientation(orientation);

    // Only ever downwards: enlarging a small photo makes a blurrier file, not a
    // better one.
    if decoded.width() > MAX_EDGE || decoded.height() > MAX_EDGE {
        decoded = decoded.resize(MAX_EDGE, MAX_EDGE, FilterType::Lanczos3);
    }

    // Flatten to RGB8. JPEG has no alpha channel, and going through RGB8
    // explicitly beats letting the encoder pick for an RGBA or 16-bit input.
    let rgb = decoded.to_rgb8();
    let (out_width, out_height) = (rgb.width(), rgb.height());

    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode(
            rgb.as_raw(),
            out_width,
            out_height,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|error| AppError::internal(format!("re-encoding an image failed: {error}")))?;

    Ok(Normalised {
        bytes: out,
        width: out_width,
        height: out_height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG carrying an EXIF APP1 segment with a GPS IFD, built by hand so the
    /// test does not depend on a fixture file somebody might replace.
    ///
    /// The GPS block encodes 34.0781 N, 118.2606 W — Silver Lake — as three
    /// rationals each, which is exactly what a phone writes.
    fn jpeg_with_gps_exif() -> Vec<u8> {
        // A minimal 8x8 red JPEG to graft the metadata onto.
        let base = {
            let image = image::RgbImage::from_pixel(8, 8, image::Rgb([220, 40, 40]));
            let mut bytes = Vec::new();
            JpegEncoder::new_with_quality(&mut bytes, 90)
                .encode(image.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
                .expect("encode");
            bytes
        };

        // TIFF header, little endian, one IFD entry pointing at a GPS IFD.
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"II\x2a\x00"); // little endian, magic 42
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8

        tiff.extend_from_slice(&1u16.to_le_bytes()); // one entry
        tiff.extend_from_slice(&0x8825u16.to_le_bytes()); // GPSInfoIFDPointer
        tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
        tiff.extend_from_slice(&1u32.to_le_bytes()); // count
        tiff.extend_from_slice(&26u32.to_le_bytes()); // offset of the GPS IFD
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        // GPS IFD at offset 26: latitude ref, latitude, longitude ref, longitude.
        let gps_values_at = 26u32 + 2 + (4 * 12) + 4;
        tiff.extend_from_slice(&4u16.to_le_bytes());
        for (tag, kind, count, value) in [
            (0x0001u16, 2u16, 2u32, u32::from_le_bytes(*b"N\0\0\0")), // N
            (0x0002u16, 5u16, 3u32, gps_values_at),                   // lat, 3 rationals
            (0x0003u16, 2u16, 2u32, u32::from_le_bytes(*b"W\0\0\0")), // W
            (0x0004u16, 5u16, 3u32, gps_values_at + 24),              // lon, 3 rationals
        ] {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&kind.to_le_bytes());
            tiff.extend_from_slice(&count.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
        }
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        // 34/1 4/1 4116/100  and  118/1 15/1 3816/100
        for (num, den) in [
            (34u32, 1u32),
            (4, 1),
            (4116, 100),
            (118, 1),
            (15, 1),
            (3816, 100),
        ] {
            tiff.extend_from_slice(&num.to_le_bytes());
            tiff.extend_from_slice(&den.to_le_bytes());
        }

        let mut app1: Vec<u8> = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);

        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&base[..2]); // SOI
        out.extend_from_slice(&[0xFF, 0xE1]); // APP1
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&base[2..]);
        out
    }

    /// The assertion this whole module exists for.
    #[test]
    fn the_stored_image_carries_no_metadata() {
        let uploaded = jpeg_with_gps_exif();

        // The fixture is only meaningful if it really does carry the GPS. If a
        // future edit breaks the builder, this catches it rather than letting
        // the real assertion pass vacuously.
        assert!(
            uploaded.windows(6).any(|w| w == b"Exif\0\0"),
            "the fixture must actually contain an EXIF segment"
        );

        let stored = normalise(&uploaded).expect("a valid JPEG");

        assert!(
            !stored.bytes.windows(6).any(|w| w == b"Exif\0\0"),
            "an EXIF segment survived the re-encode"
        );
        assert!(
            !stored.bytes.windows(2).any(|w| w == [0xFF, 0xE1]),
            "an APP1 marker survived the re-encode"
        );
        // The coordinates themselves, in the exact byte form the fixture wrote
        // them. Belt and braces: this fails even if some future encoder invents
        // a metadata container that is not APP1.
        assert!(
            !stored.bytes.windows(8).any(|w| w
                == [34u32.to_le_bytes(), 1u32.to_le_bytes()]
                    .concat()
                    .as_slice()
                || w == [118u32.to_le_bytes(), 1u32.to_le_bytes()]
                    .concat()
                    .as_slice()),
            "GPS rationals survived the re-encode"
        );
    }

    /// Orientation is the one thing read from the metadata, and it must be
    /// applied to the pixels before the metadata is dropped — otherwise every
    /// photo taken in portrait is stored sideways.
    #[test]
    fn a_sideways_photo_is_uprighted() {
        // A 4x2 landscape image tagged "rotate 90°", which should come out 2x4.
        let base = {
            let image = image::RgbImage::from_pixel(4, 2, image::Rgb([10, 200, 90]));
            let mut bytes = Vec::new();
            JpegEncoder::new_with_quality(&mut bytes, 90)
                .encode(image.as_raw(), 4, 2, image::ExtendedColorType::Rgb8)
                .expect("encode");
            bytes
        };

        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"II\x2a\x00");
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&6u16.to_le_bytes()); // 6 = rotate 90 CW
        tiff.extend_from_slice(&0u16.to_le_bytes()); // pad the 4-byte value
        tiff.extend_from_slice(&0u32.to_le_bytes());

        let mut app1: Vec<u8> = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);

        let mut uploaded: Vec<u8> = Vec::new();
        uploaded.extend_from_slice(&base[..2]);
        uploaded.extend_from_slice(&[0xFF, 0xE1]);
        uploaded.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        uploaded.extend_from_slice(&app1);
        uploaded.extend_from_slice(&base[2..]);

        let stored = normalise(&uploaded).expect("a valid JPEG");
        assert_eq!(
            (stored.width, stored.height),
            (2, 4),
            "a 4x2 image tagged rotate-90 should be stored 2x4"
        );
    }

    #[test]
    fn a_large_photo_is_scaled_to_the_long_edge() {
        let image =
            image::RgbImage::from_pixel(MAX_EDGE + 600, MAX_EDGE / 2, image::Rgb([1, 2, 3]));
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 80)
            .encode(
                image.as_raw(),
                MAX_EDGE + 600,
                MAX_EDGE / 2,
                image::ExtendedColorType::Rgb8,
            )
            .expect("encode");

        let stored = normalise(&bytes).expect("a valid JPEG");
        assert_eq!(stored.width, MAX_EDGE);
        assert!(stored.height < MAX_EDGE, "the aspect ratio is preserved");
    }

    #[test]
    fn a_small_photo_is_not_enlarged() {
        let image = image::RgbImage::from_pixel(120, 90, image::Rgb([9, 9, 9]));
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 80)
            .encode(image.as_raw(), 120, 90, image::ExtendedColorType::Rgb8)
            .expect("encode");

        let stored = normalise(&bytes).expect("a valid JPEG");
        assert_eq!((stored.width, stored.height), (120, 90));
    }

    /// A PNG is accepted and comes back as a JPEG: one stored format means one
    /// content type to serve and one thing to reason about.
    #[test]
    fn a_png_is_accepted_and_stored_as_jpeg() {
        let image = image::RgbaImage::from_pixel(30, 30, image::Rgba([5, 5, 5, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode");

        let stored = normalise(&bytes).expect("a valid PNG");
        assert_eq!(&stored.bytes[..2], &[0xFF, 0xD8], "stored as JPEG");
    }

    #[test]
    fn a_non_image_is_refused() {
        for junk in [
            &b"not an image at all"[..],
            &b"PK\x03\x04zip file"[..],
            &b"<!doctype html><script>alert(1)</script>"[..],
            &[][..],
        ] {
            assert!(
                normalise(junk).is_err(),
                "junk was accepted as an image: {junk:?}"
            );
        }
    }
}
