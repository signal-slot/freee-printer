//! Converts PWG Raster and Apple Raster (URF) documents to PDF.
//!
//! Clients that cannot produce PDF themselves (Windows, iOS, Android) send
//! one of these raster formats. Each raster page becomes one image page.

use std::io::Write;

use anyhow::{Result, bail};
use flate2::Compression;
use flate2::write::ZlibEncoder;

const PWG_HEADER_LEN: usize = 1796;
const MAX_DIMENSION: u32 = 100_000;

#[derive(Clone, Copy, PartialEq)]
enum ColorSpace {
    Gray,
    Rgb,
    Cmyk,
}

struct Page {
    width: u32,
    height: u32,
    xdpi: u32,
    ydpi: u32,
    color: ColorSpace,
    /// 0 is white (PWG black_8), so samples must be inverted for DeviceGray.
    inverted: bool,
}

impl Page {
    fn bytes_per_pixel(&self) -> usize {
        match self.color {
            ColorSpace::Gray => 1,
            ColorSpace::Rgb => 3,
            ColorSpace::Cmyk => 4,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_DIMENSION
            || self.height > MAX_DIMENSION
        {
            bail!(
                "ラスターのページサイズが不正です: {}x{}",
                self.width,
                self.height
            );
        }
        if self.xdpi == 0 || self.ydpi == 0 {
            bail!("ラスターの解像度が不正です");
        }
        Ok(())
    }
}

pub fn is_pwg(data: &[u8]) -> bool {
    data.starts_with(b"RaS2")
}

pub fn is_urf(data: &[u8]) -> bool {
    data.starts_with(b"UNIRAST\0")
}

/// Returns the PDF and its page count.
pub fn to_pdf(data: &[u8]) -> Result<(Vec<u8>, u32)> {
    let mut pdf = PdfWriter::new();
    if is_pwg(data) {
        let mut rest = &data[4..];
        while !rest.is_empty() {
            let header = take(&mut rest, PWG_HEADER_LEN)?;
            let page = pwg_page(header)?;
            pdf.add_page(&page, &mut rest)?;
        }
    } else if is_urf(data) {
        let mut rest = &data[8..];
        let count = be_u32(take(&mut rest, 4)?, 0);
        // A count of zero means "unknown"; read until the data runs out.
        let mut remaining = if count == 0 { u32::MAX } else { count };
        while remaining > 0 && !rest.is_empty() {
            let header = take(&mut rest, 32)?;
            let page = urf_page(header)?;
            pdf.add_page(&page, &mut rest)?;
            remaining -= 1;
        }
    } else {
        bail!("ラスター形式ではありません");
    }
    if pdf.pages.is_empty() {
        bail!("ラスターにページがありません");
    }
    let pages = pdf.pages.len() as u32;
    Ok((pdf.finish(), pages))
}

fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if rest.len() < n {
        bail!("ラスターデータが途中で切れています");
    }
    let (head, tail) = rest.split_at(n);
    *rest = tail;
    Ok(head)
}

fn be_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

/// Field offsets follow cups_page_header2_t (PWG 5102.4 section 4.3).
fn pwg_page(header: &[u8]) -> Result<Page> {
    let bits_per_color = be_u32(header, 384);
    let bits_per_pixel = be_u32(header, 388);
    let color_space = be_u32(header, 400);
    if bits_per_color != 8 {
        bail!("{bits_per_color} bit のラスターには対応していません");
    }
    let (color, inverted) = match (color_space, bits_per_pixel) {
        (0 | 18, 8) => (ColorSpace::Gray, false),
        (3, 8) => (ColorSpace::Gray, true),
        (1 | 19 | 20, 24) => (ColorSpace::Rgb, false),
        (6, 32) => (ColorSpace::Cmyk, false),
        _ => bail!(
            "対応していないラスター色空間です: cupsColorSpace={color_space}, {bits_per_pixel}bpp"
        ),
    };
    let page = Page {
        width: be_u32(header, 372),
        height: be_u32(header, 376),
        xdpi: be_u32(header, 276),
        ydpi: be_u32(header, 280),
        color,
        inverted,
    };
    page.validate()?;
    Ok(page)
}

fn urf_page(header: &[u8]) -> Result<Page> {
    let color = match (header[1], header[0]) {
        (0 | 4, 8) => ColorSpace::Gray,
        (1 | 3 | 5, 24) => ColorSpace::Rgb,
        (6, 32) => ColorSpace::Cmyk,
        (space, bpp) => bail!("対応していない URF 色空間です: {space}, {bpp}bpp"),
    };
    let dpi = be_u32(header, 20);
    let page = Page {
        width: be_u32(header, 12),
        height: be_u32(header, 16),
        xdpi: dpi,
        ydpi: dpi,
        color,
        inverted: false,
    };
    page.validate()?;
    Ok(page)
}

/// Decodes one page of the PackBits-like encoding shared by PWG and Apple
/// raster, handing each scan line to `sink`.
fn decode_page(
    page: &Page,
    rest: &mut &[u8],
    mut sink: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let bpp = page.bytes_per_pixel();
    let mut line = vec![0u8; page.width as usize * bpp];
    // CMYK "white" is no ink; everything else is full intensity.
    let white = if page.color == ColorSpace::Cmyk {
        0x00
    } else {
        0xff
    };
    let mut y = 0u32;
    while y < page.height {
        let repeat = take(rest, 1)?[0] as u32 + 1;
        let mut x = 0usize;
        while x < line.len() {
            let code = take(rest, 1)?[0];
            if code == 0x80 {
                // Apple raster: clear to end of line.
                line[x..].fill(white);
                x = line.len();
            } else if code > 0x80 {
                let n = ((257 - code as usize) * bpp).min(line.len() - x);
                line[x..x + n].copy_from_slice(take(rest, n)?);
                x += n;
            } else {
                let pixel = take(rest, bpp)?;
                let n = ((code as usize + 1) * bpp).min(line.len() - x);
                for chunk in line[x..x + n].chunks_mut(bpp) {
                    chunk.copy_from_slice(&pixel[..chunk.len()]);
                }
                x += n;
            }
        }
        if page.inverted {
            let inverted: Vec<u8> = line.iter().map(|b| !b).collect();
            for _ in 0..repeat.min(page.height - y) {
                sink(&inverted)?;
            }
        } else {
            for _ in 0..repeat.min(page.height - y) {
                sink(&line)?;
            }
        }
        y += repeat;
    }
    Ok(())
}

struct PdfWriter {
    out: Vec<u8>,
    /// Byte offset of each object; index 0 is object 1.
    offsets: Vec<usize>,
    pages: Vec<usize>,
}

const CATALOG: usize = 1;
const PAGES: usize = 2;

impl PdfWriter {
    fn new() -> Self {
        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n");
        // Objects 1 and 2 are written last, once the page list is known.
        PdfWriter {
            out,
            offsets: vec![0, 0],
            pages: Vec::new(),
        }
    }

    fn begin_object(&mut self) -> usize {
        self.offsets.push(self.out.len());
        let id = self.offsets.len();
        writeln!(self.out, "{id} 0 obj").unwrap();
        id
    }

    fn add_page(&mut self, page: &Page, rest: &mut &[u8]) -> Result<()> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        decode_page(page, rest, |line| Ok(encoder.write_all(line)?))?;
        let compressed = encoder.finish()?;

        let color = match page.color {
            ColorSpace::Gray => "/DeviceGray",
            ColorSpace::Rgb => "/DeviceRGB",
            ColorSpace::Cmyk => "/DeviceCMYK",
        };
        let image = self.begin_object();
        write!(
            self.out,
            "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace {} /BitsPerComponent 8 /Filter /FlateDecode /Length {} >>\nstream\n",
            page.width,
            page.height,
            color,
            compressed.len()
        )?;
        self.out.extend_from_slice(&compressed);
        self.out.extend_from_slice(b"\nendstream\nendobj\n");

        let width = page.width as f64 * 72.0 / page.xdpi as f64;
        let height = page.height as f64 * 72.0 / page.ydpi as f64;
        let content = format!("q {width:.2} 0 0 {height:.2} 0 0 cm /Im0 Do Q\n");
        let contents = self.begin_object();
        write!(
            self.out,
            "<< /Length {} >>\nstream\n{}endstream\nendobj\n",
            content.len(),
            content
        )?;

        let page_id = self.begin_object();
        write!(
            self.out,
            "<< /Type /Page /Parent {PAGES} 0 R /MediaBox [0 0 {width:.2} {height:.2}] /Resources << /XObject << /Im0 {image} 0 R >> >> /Contents {contents} 0 R >>\nendobj\n"
        )?;
        self.pages.push(page_id);
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        self.offsets[CATALOG - 1] = self.out.len();
        write!(
            self.out,
            "{CATALOG} 0 obj\n<< /Type /Catalog /Pages {PAGES} 0 R >>\nendobj\n"
        )
        .unwrap();

        self.offsets[PAGES - 1] = self.out.len();
        let kids: Vec<String> = self.pages.iter().map(|id| format!("{id} 0 R")).collect();
        write!(
            self.out,
            "{PAGES} 0 obj\n<< /Type /Pages /Count {} /Kids [{}] >>\nendobj\n",
            self.pages.len(),
            kids.join(" ")
        )
        .unwrap();

        let xref = self.out.len();
        write!(
            self.out,
            "xref\n0 {}\n0000000000 65535 f \n",
            self.offsets.len() + 1
        )
        .unwrap();
        for offset in &self.offsets {
            writeln!(self.out, "{offset:010} 00000 n ").unwrap();
        }
        write!(
            self.out,
            "trailer\n<< /Size {} /Root {CATALOG} 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            self.offsets.len() + 1
        )
        .unwrap();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pwg_header(width: u32, height: u32, color_space: u32, bits_per_pixel: u32) -> Vec<u8> {
        let mut header = vec![0u8; PWG_HEADER_LEN];
        let mut put = |offset: usize, value: u32| {
            header[offset..offset + 4].copy_from_slice(&value.to_be_bytes())
        };
        put(276, 300);
        put(280, 300);
        put(372, width);
        put(376, height);
        put(384, 8);
        put(388, bits_per_pixel);
        put(392, width * bits_per_pixel / 8);
        put(400, color_space);
        header
    }

    fn decode_all(page: &Page, mut data: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        decode_page(page, &mut data, |line| {
            lines.push(line.to_vec());
            Ok(())
        })
        .unwrap();
        assert!(data.is_empty());
        lines
    }

    #[test]
    fn decodes_repeats_and_literals() {
        let page = pwg_page(&pwg_header(4, 3, 19, 24)).unwrap();
        let data = [
            1, // this line appears twice
            1, 0xff, 0x00, 0x00, // two red pixels
            0xff, 1, 2, 3, 4, 5, 6, // two literal pixels
            0, // one line
            3, 9, 9, 9, // four identical pixels
        ];
        let lines = decode_all(&page, &data);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], [0xff, 0, 0, 0xff, 0, 0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(lines[0], lines[1]);
        assert_eq!(lines[2], [9; 12]);
    }

    #[test]
    fn urf_clear_to_end_of_line() {
        let mut header = [0u8; 32];
        header[0] = 8;
        header[12..16].copy_from_slice(&3u32.to_be_bytes());
        header[16..20].copy_from_slice(&1u32.to_be_bytes());
        header[20..24].copy_from_slice(&300u32.to_be_bytes());
        let page = urf_page(&header).unwrap();
        let lines = decode_all(&page, &[0, 0, 0x10, 0x80]);
        assert_eq!(lines, [[0x10, 0xff, 0xff]]);
    }

    #[test]
    fn black_pages_are_inverted() {
        let page = pwg_page(&pwg_header(2, 1, 3, 8)).unwrap();
        let lines = decode_all(&page, &[0, 1, 0x00]);
        assert_eq!(lines, [[0xff, 0xff]]);
    }

    #[test]
    fn builds_a_pdf_per_raster_page() {
        let mut data = b"RaS2".to_vec();
        for _ in 0..2 {
            data.extend_from_slice(&pwg_header(2, 2, 18, 8));
            data.extend_from_slice(&[1, 1, 0x80]);
        }
        let (pdf, pages) = to_pdf(&data).unwrap();
        assert_eq!(pages, 2);
        assert!(pdf.starts_with(b"%PDF-1.4"));
        assert!(pdf.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&pdf);
        assert_eq!(text.matches("/Type /Page ").count(), 2);
        assert!(text.contains("/MediaBox [0 0 0.48 0.48]"));
    }

    #[test]
    fn truncated_raster_is_an_error() {
        let mut data = b"RaS2".to_vec();
        data.extend_from_slice(&pwg_header(2, 2, 18, 8));
        data.extend_from_slice(&[1, 1]);
        assert!(to_pdf(&data).is_err());
    }
}
