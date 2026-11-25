//! Pure-Rust image-to-PDF export.
//!
//! One image per A4 page, scaled to fit with a 12 mm margin and centred. The
//! crate writes the PDF structure itself: a catalogue, a page tree, one page
//! and content stream per image, and a compressed RGB image XObject. That keeps
//! the dependency surface to an image decoder and a zlib encoder. The platform
//! shell chooses the images and the destination; this crate only produces PDF
//! bytes.

#![forbid(unsafe_code)]

use std::fmt;
use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use antirustpaste_imaging::decode_rgb;

/// One selected image. PNG, TIFF and JPEG are decoded by `antirustpaste-imaging`.
#[derive(Clone, Debug)]
pub struct PdfImage {
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NoImages,
    Decode,
    Encode,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoImages => f.write_str("no images selected"),
            Self::Decode => f.write_str("an image could not be decoded"),
            Self::Encode => f.write_str("the PDF could not be encoded"),
        }
    }
}

impl std::error::Error for Error {}

/// A4 in PostScript points (210 × 297 mm).
const A4_WIDTH_PT: f64 = 595.276;
const A4_HEIGHT_PT: f64 = 841.89;
/// 12 mm page margin in points.
const MARGIN_PT: f64 = 34.0;
const MEDIA_BOX: &str = "[0 0 595.276 841.89]";
const COMPRESSION: u32 = 6;

/// Build a PDF with one image per A4 page, preserving order and aspect ratio.
pub fn write_pdf(images: &[PdfImage]) -> Result<Vec<u8>, Error> {
    if images.is_empty() {
        return Err(Error::NoImages);
    }
    let page_count = u32::try_from(images.len()).map_err(|_| Error::Encode)?;
    let last_object = page_count
        .checked_mul(3)
        .and_then(|count| count.checked_add(2))
        .ok_or(Error::Encode)?;

    let mut writer = Writer::new();

    // 1: catalogue, 2: page tree. Page objects start at 3.
    writer.begin_object(1);
    writer.push("<< /Type /Catalog /Pages 2 0 R >>");
    writer.end_object();

    writer.begin_object(2);
    writer.push("<< /Type /Pages /Kids [");
    for index in 0..page_count {
        let page_object = 3 + index.saturating_mul(3);
        writer.push(&format!("{page_object} 0 R "));
    }
    writer.push(&format!("] /Count {page_count} >>"));
    writer.end_object();

    for (index, image) in images.iter().enumerate() {
        let rgb = decode_rgb(&image.bytes).map_err(|_| Error::Decode)?;
        let (draw_width, draw_height, offset_x, offset_y) = placement(rgb.width, rgb.height)?;
        let pixels = deflate(&rgb.samples)?;

        let index = u32::try_from(index).map_err(|_| Error::Encode)?;
        let page_object = 3 + index.saturating_mul(3);
        let content_object = page_object.saturating_add(1);
        let image_object = page_object.saturating_add(2);

        writer.begin_object(page_object);
        writer.push(&format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox {MEDIA_BOX} /Resources << /XObject << /Im0 {image_object} 0 R >> >> /Contents {content_object} 0 R >>"
        ));
        writer.end_object();

        // Image samples start at the top row while PDF user space grows
        // upwards, so the height is negated and the origin moved to the top
        // edge of the destination rectangle.
        let content = format!(
            "q\n{draw_width:.3} 0 0 -{draw_height:.3} {offset_x:.3} {:.3} cm\n/Im0 Do\nQ\n",
            offset_y + draw_height
        );
        writer.stream_object(content_object, "", content.as_bytes());

        writer.stream_object(
            image_object,
            &format!(
                "/Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode",
                rgb.width, rgb.height
            ),
            &pixels,
        );
    }

    if u32::try_from(writer.object_count()).ok() != Some(last_object) {
        return Err(Error::Encode);
    }
    writer.finish()
}

fn placement(width: u32, height: u32) -> Result<(f64, f64, f64, f64), Error> {
    if width == 0 || height == 0 {
        return Err(Error::Decode);
    }
    let content_width = A4_WIDTH_PT - 2.0 * MARGIN_PT;
    let content_height = A4_HEIGHT_PT - 2.0 * MARGIN_PT;
    let image_width = width as f64;
    let image_height = height as f64;
    // Scale down only when the image would not fit the printable area. Smaller
    // images keep their native size (1 pixel = 1 point at 72 dpi).
    let fit = (content_width / image_width).min(content_height / image_height);
    let scale = fit.min(1.0);
    let draw_width = image_width * scale;
    let draw_height = image_height * scale;
    let offset_x = MARGIN_PT + (content_width - draw_width) / 2.0;
    let offset_y = MARGIN_PT + (content_height - draw_height) / 2.0;
    Ok((draw_width, draw_height, offset_x, offset_y))
}

fn deflate(data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(COMPRESSION));
    encoder.write_all(data).map_err(|_| Error::Encode)?;
    encoder.finish().map_err(|_| Error::Encode)
}

struct Writer {
    bytes: Vec<u8>,
    offsets: Vec<usize>,
}

impl Writer {
    fn new() -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");
        Self {
            bytes,
            offsets: Vec::new(),
        }
    }

    fn begin_object(&mut self, number: u32) {
        self.offsets.push(self.bytes.len());
        self.push(&format!("{number} 0 obj\n"));
    }

    fn object_count(&self) -> usize {
        self.offsets.len()
    }

    fn end_object(&mut self) {
        self.push("\nendobj\n");
    }

    fn push(&mut self, text: &str) {
        self.bytes.extend_from_slice(text.as_bytes());
    }

    fn stream_object(&mut self, number: u32, dictionary: &str, data: &[u8]) {
        self.begin_object(number);
        if dictionary.is_empty() {
            self.push(&format!("<< /Length {} >>\nstream\n", data.len()));
        } else {
            self.push(&format!(
                "<< /Length {} {dictionary} >>\nstream\n",
                data.len()
            ));
        }
        self.bytes.extend_from_slice(data);
        self.push("\nendstream");
        self.end_object();
    }

    fn finish(mut self) -> Result<Vec<u8>, Error> {
        let xref_offset = self.bytes.len();
        let size = self.offsets.len().checked_add(1).ok_or(Error::Encode)?;
        self.push("xref\n");
        self.push(&format!("0 {size}\n"));
        self.push("0000000000 65535 f \n");
        let offsets = std::mem::take(&mut self.offsets);
        for offset in offsets {
            self.push(&format!("{offset:010} 00000 n \n"));
        }
        self.push("trailer\n");
        self.push(&format!("<< /Size {size} /Root 1 0 R >>\n"));
        self.push("startxref\n");
        self.push(&format!("{xref_offset}\n"));
        self.push("%%EOF\n");
        Ok(self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2 opaque red PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd,
        0xd4, 0x9a, 0x73, 0x00, 0x00, 0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x44, 0x0c, 0x10, 0x0a, 0x00, 0x1f, 0xee, 0x03, 0xfd, 0x8b, 0x5f, 0x14,
        0xd4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn image() -> PdfImage {
        PdfImage {
            bytes: PNG.to_vec(),
        }
    }

    #[test]
    fn empty_selection_is_rejected() {
        assert_eq!(write_pdf(&[]), Err(Error::NoImages));
    }

    #[test]
    fn invalid_image_is_rejected() {
        let images = vec![PdfImage {
            bytes: b"not an image".to_vec(),
        }];
        assert_eq!(write_pdf(&images), Err(Error::Decode));
    }

    #[test]
    fn writes_one_page_per_image() {
        let pdf = write_pdf(&[image(), image()]).expect("pdf");
        assert!(pdf.starts_with(b"%PDF-1.7"));
        assert!(pdf.ends_with(b"%%EOF\n"));
        assert_eq!(count(&pdf, b"/Type /Page "), 2);
        assert_eq!(count(&pdf, b"/Subtype /Image"), 2);
        assert_eq!(count(&pdf, b"/Count 2"), 1);
        assert_eq!(count(&pdf, b"/Width 2 /Height 2"), 2);
        assert_eq!(count(&pdf, b"/Filter /FlateDecode"), 2);
    }

    #[test]
    fn image_content_is_flipped_upright() {
        // Samples arrive top row first; the content stream negates the height
        // and moves the origin to the top edge so the page is not upside down.
        // The 2×2 fixture is smaller than the printable area, so it keeps its
        // native size.
        let pdf = write_pdf(&[image()]).expect("pdf");
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("2.000 0 0 -2.000 296.638 421.945 cm"));
        assert!(!text.contains("0 0 2.000 296.638 419.945 cm"));
    }

    #[test]
    fn large_images_are_scaled_to_fit_and_small_ones_keep_their_size() {
        let (width, height, _, y) = placement(2000, 1000).expect("landscape");
        assert!((width - 527.276).abs() < 0.001);
        assert!((height - 263.638).abs() < 0.001);
        assert!((y - 289.126).abs() < 0.01);

        let (width, height, x, y) = placement(100, 50).expect("small");
        assert!((width - 100.0).abs() < 0.001);
        assert!((height - 50.0).abs() < 0.001);
        assert!((x - 247.638).abs() < 0.001);
        assert!((y - 395.945).abs() < 0.001);

        let (width, _, _, _) = placement(400, 4000).expect("portrait");
        assert!((width - 77.389).abs() < 0.01);
    }

    #[test]
    fn cross_reference_offsets_point_at_objects() {
        let pdf = write_pdf(&[image()]).expect("pdf");
        let start = start_xref(&pdf).expect("startxref");
        assert_eq!(pdf.get(start..start + 4), Some(&b"xref"[..]));

        let entries = pdf.get(start..).expect("xref section");
        let text = String::from_utf8_lossy(entries);
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("xref"));
        let header = lines.next().unwrap_or_default();
        let size = header
            .split_whitespace()
            .next_back()
            .and_then(|value| value.parse::<usize>().ok())
            .expect("xref size");
        assert_eq!(size, 6);
        assert_eq!(lines.next(), Some("0000000000 65535 f "));

        for number in 1..size {
            let entry = lines.next().unwrap_or_default();
            let offset = entry
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<usize>().ok())
                .expect("object offset");
            let expected = format!("{number} 0 obj");
            assert_eq!(
                pdf.get(offset..offset + expected.len()),
                Some(expected.as_bytes()),
                "object {number} offset"
            );
        }
    }

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    fn start_xref(pdf: &[u8]) -> Option<usize> {
        let marker = b"startxref\n";
        let position = pdf
            .windows(marker.len())
            .position(|window| window == marker)?;
        let rest = pdf.get(position + marker.len()..)?;
        let end = rest.iter().position(|byte| *byte == b'\n')?;
        std::str::from_utf8(rest.get(..end)?).ok()?.parse().ok()
    }
}
