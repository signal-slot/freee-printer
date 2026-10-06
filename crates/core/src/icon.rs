//! The printer icon served as `printer-icons`, drawn at runtime to avoid
//! shipping a binary asset.

use flate2::Crc;

const SIZE: usize = 128;

pub fn png() -> Vec<u8> {
    let mut raw = Vec::with_capacity(SIZE * (SIZE * 4 + 1));
    for y in 0..SIZE {
        raw.push(0); // filter: none
        for x in 0..SIZE {
            raw.extend_from_slice(&pixel(x as i32, y as i32));
        }
    }

    let mut header = Vec::new();
    header.extend_from_slice(&(SIZE as u32).to_be_bytes());
    header.extend_from_slice(&(SIZE as u32).to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bit RGBA

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Wraps `data` in a zlib stream of stored (uncompressed) blocks. A real
/// compressor needs more stack than the firmware's threads have.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = data.chunks(0xffff).peekable();
    while let Some(block) = blocks.next() {
        out.push(blocks.peek().is_none() as u8);
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// A sheet of paper dropping into a box.
fn pixel(x: i32, y: i32) -> [u8; 4] {
    const BOX: [u8; 4] = [0x2b, 0x6c, 0xd9, 0xff];
    const BOX_DARK: [u8; 4] = [0x1f, 0x4f, 0xa3, 0xff];
    const PAPER: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
    const EDGE: [u8; 4] = [0x8a, 0x94, 0xa6, 0xff];
    const LINE: [u8; 4] = [0xb8, 0xc0, 0xcc, 0xff];

    // The front of the box covers the lower part of the sheet.
    if (16..112).contains(&x) && (72..116).contains(&y) {
        return if y < 80 { BOX_DARK } else { BOX };
    }
    if (32..96).contains(&x) && (12..80).contains(&y) {
        if x == 32 || x == 95 || y == 12 {
            return EDGE;
        }
        if (42..86).contains(&x) && matches!(y, 26..=29 | 38..=41 | 50..=53 | 62..=65) {
            return LINE;
        }
        return PAPER;
    }
    [0, 0, 0, 0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn stored_blocks_inflate_back() {
        let data: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        let mut back = Vec::new();
        flate2::read::ZlibDecoder::new(&zlib_stored(&data)[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn png_has_the_expected_size() {
        let png = png();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&png[16..24], &[0, 0, 0, 128, 0, 0, 0, 128]);
    }
}
