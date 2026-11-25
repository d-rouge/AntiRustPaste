//! Pure-Rust image metadata and thumbnail generation.
//!
//! This crate is deliberately free of AppKit, filesystem and environment
//! dependencies so it can be reused outside macOS. The application decides
//! where thumbnails are cached; this crate only turns encoded image bytes into
//! a bounded PNG thumbnail.

#![forbid(unsafe_code)]

use std::fmt;
use std::io::Cursor;

use image::{ImageFormat, ImageReader};

/// Maximum decoded pixel count accepted from untrusted pasteboard data.
///
/// 32 megapixels is enough for screenshots and ordinary camera images while
/// keeping the worst-case RGBA buffer around 128 MiB.
pub const MAX_SOURCE_PIXELS: u64 = 32_000_000;

/// Largest thumbnail edge this crate will produce.
pub const MAX_THUMBNAIL_EDGE: u32 = 512;

/// Encoded image formats RustClip accepts for thumbnails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Tiff,
    Jpeg,
}

/// Dimensions and format read from an encoded image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub format: Format,
}

/// A PNG thumbnail produced from an encoded image.
#[derive(Clone, Debug)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// Encoded PNG bytes, safe to write to the app's cache.
    pub png: Vec<u8>,
}

/// Failure modes for metadata and thumbnail generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Empty,
    UnsupportedFormat,
    ZeroDimension,
    TooLarge,
    InvalidEdge,
    Decode,
    Encode,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Empty => "image data is empty",
            Self::UnsupportedFormat => "image format is not supported",
            Self::ZeroDimension => "image has a zero dimension",
            Self::TooLarge => "image dimensions exceed the safety limit",
            Self::InvalidEdge => "thumbnail edge must be between 1 and 512",
            Self::Decode => "image could not be decoded",
            Self::Encode => "thumbnail could not be encoded",
        };
        f.write_str(message)
    }
}

impl std::error::Error for Error {}

/// Read the dimensions and format without decoding the full image.
pub fn info(bytes: &[u8]) -> Result<ImageInfo, Error> {
    if bytes.is_empty() {
        return Err(Error::Empty);
    }

    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Error::UnsupportedFormat)?;
    let format = match reader.format() {
        Some(ImageFormat::Png) => Format::Png,
        Some(ImageFormat::Tiff) => Format::Tiff,
        Some(ImageFormat::Jpeg) => Format::Jpeg,
        _ => return Err(Error::UnsupportedFormat),
    };
    let (width, height) = reader.into_dimensions().map_err(|_| Error::Decode)?;
    validate_dimensions(width, height)?;

    Ok(ImageInfo {
        width,
        height,
        format,
    })
}

/// Decode an image and return a PNG thumbnail bounded by `max_edge`.
///
/// The aspect ratio is preserved and small images are not upscaled. `max_edge`
/// is clamped to the supported range: 0 is an error, values above 512 are
/// reduced to 512.
pub fn thumbnail_png(bytes: &[u8], max_edge: u32) -> Result<Thumbnail, Error> {
    if max_edge == 0 {
        return Err(Error::InvalidEdge);
    }
    let max_edge = max_edge.min(MAX_THUMBNAIL_EDGE);

    let info = info(bytes)?;
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Error::UnsupportedFormat)?;
    let decoded = reader.decode().map_err(|_| Error::Decode)?;

    let (width, height) = thumbnail_size(info.width, info.height, max_edge);
    let resized = image::imageops::thumbnail(&decoded, width, height);

    let mut png = Vec::new();
    resized
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|_| Error::Encode)?;

    Ok(Thumbnail {
        width: resized.width(),
        height: resized.height(),
        png,
    })
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), Error> {
    if width == 0 || height == 0 {
        return Err(Error::ZeroDimension);
    }
    if (width as u64)
        .saturating_mul(height as u64)
        .gt(&MAX_SOURCE_PIXELS)
    {
        return Err(Error::TooLarge);
    }
    Ok(())
}

fn thumbnail_size(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let long_edge = width.max(height);
    let edge = max_edge.min(long_edge).max(1);
    let width_ratio = (width as u64).saturating_mul(edge as u64) / long_edge as u64;
    let height_ratio = (height as u64).saturating_mul(edge as u64) / long_edge as u64;
    (width_ratio.max(1) as u32, height_ratio.max(1) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

    fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255])
        });
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .expect("encode fixture");
        bytes
    }

    #[test]
    fn reads_png_dimensions_and_format() {
        let bytes = encoded(7, 3, ImageFormat::Png);
        let info = info(&bytes).expect("png info");
        assert_eq!(info.width, 7);
        assert_eq!(info.height, 3);
        assert_eq!(info.format, Format::Png);
    }

    #[test]
    fn reads_tiff_dimensions_and_format() {
        let bytes = encoded(5, 4, ImageFormat::Tiff);
        let info = info(&bytes).expect("tiff info");
        assert_eq!((info.width, info.height), (5, 4));
        assert_eq!(info.format, Format::Tiff);
    }

    fn encoded_rgb(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .expect("encode rgb fixture");
        bytes
    }

    #[test]
    fn reads_jpeg_dimensions_and_formats() {
        let bytes = encoded_rgb(9, 4, ImageFormat::Jpeg);
        let info = info(&bytes).expect("jpeg info");
        assert_eq!((info.width, info.height), (9, 4));
        assert_eq!(info.format, Format::Jpeg);
    }

    #[test]
    fn thumbnail_preserves_aspect_ratio_and_bounds() {
        let bytes = encoded(200, 100, ImageFormat::Png);
        let thumb = thumbnail_png(&bytes, 64).expect("thumbnail");
        assert_eq!(thumb.width, 64);
        assert_eq!(thumb.height, 32);
        assert!(!thumb.png.is_empty());
        let round_trip = info(&thumb.png).expect("encoded thumbnail is a png");
        assert_eq!((round_trip.width, round_trip.height), (64, 32));
    }

    #[test]
    fn thumbnail_does_not_upscale_small_images() {
        let bytes = encoded(11, 8, ImageFormat::Png);
        let thumb = thumbnail_png(&bytes, 128).expect("thumbnail");
        assert_eq!((thumb.width, thumb.height), (11, 8));
    }

    #[test]
    fn truncated_data_is_rejected() {
        let bytes = encoded(16, 16, ImageFormat::Png);
        let truncated = &bytes[..20];
        assert!(info(truncated).is_err());
    }

    #[test]
    fn non_image_data_is_rejected() {
        assert_eq!(
            info(b"definitely not an image"),
            Err(Error::UnsupportedFormat)
        );
        assert_eq!(info(b""), Err(Error::Empty));
    }

    #[test]
    fn unsafe_dimensions_are_rejected_before_decode() {
        assert_eq!(validate_dimensions(0, 4), Err(Error::ZeroDimension));
        assert_eq!(validate_dimensions(4, 0), Err(Error::ZeroDimension));
        assert_eq!(validate_dimensions(100_000, 100_000), Err(Error::TooLarge));
        assert!(validate_dimensions(8000, 4000).is_ok());
    }

    #[test]
    fn invalid_edge_is_rejected() {
        let bytes = encoded(4, 4, ImageFormat::Png);
        assert!(matches!(thumbnail_png(&bytes, 0), Err(Error::InvalidEdge)));
        assert!(thumbnail_png(&bytes, 9999).is_ok());
    }

    #[test]
    fn thumbnail_size_keeps_long_edge() {
        assert_eq!(thumbnail_size(400, 200, 100), (100, 50));
        assert_eq!(thumbnail_size(200, 400, 100), (50, 100));
        assert_eq!(thumbnail_size(1, 1000, 100), (1, 100));
        assert_eq!(thumbnail_size(10, 10, 100), (10, 10));
    }
}
