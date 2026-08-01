//! A minimal PNG writer, for one job.
//!
//! Windows will put an icon beside the app name in a toast header, but only for
//! a *registered* AppUserModelID, and the registration points at an **image file
//! on disk**. An `HICON` cannot be handed over: the shell reads a path.
//!
//! So the gauge has to be written out as a file, and PNG is what that
//! registration documents. Rather than take an image crate for it, this writes
//! the format directly, which is about a hundred lines because none of the hard
//! parts are needed:
//!
//! - **No compression.** Deflate has a "stored" block type that is a length and
//!   the bytes. A 64 px icon is 16 KB uncompressed and this is written once at
//!   startup, so paying bytes to avoid an LZ77 implementation is the right trade
//!   by a wide margin.
//! - **No filtering.** Filter type 0 for every row.
//! - **One colour type**, 8-bit RGBA.
//!
//! What is *not* skipped is the two checksums. A PNG with a bad CRC or a bad
//! Adler is rejected silently by the shell, which would look exactly like the
//! missing-icon bug this exists to fix.

/// Encode straight (non-premultiplied) RGBA, top-down, into a PNG.
pub fn encode(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(rgba.len(), (width * height * 4) as usize, "pixel buffer is the wrong size");

    let mut out = Vec::with_capacity(rgba.len() + 1024);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // colour type: truecolour with alpha
    ihdr.push(0); // compression: deflate
    ihdr.push(0); // filter method
    ihdr.push(0); // no interlace
    chunk(&mut out, b"IHDR", &ihdr);

    // Each scanline is prefixed with its filter type. Zero means "none", which
    // is what makes the raw bytes the pixel bytes.
    let stride = (width * 4) as usize;
    let mut raw = Vec::with_capacity(rgba.len() + height as usize);
    for row in rgba.chunks_exact(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }

    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc::new();
    crc.push(kind);
    crc.push(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// A zlib stream whose deflate blocks are all stored.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut z = Vec::with_capacity(data.len() + 64);
    // CMF: deflate, 32 KB window. FLG chosen so the pair is a multiple of 31,
    // which is the header check every decoder applies first.
    z.push(0x78);
    z.push(0x01);
    debug_assert_eq!(
        (z[0] as u16 * 256 + z[1] as u16) % 31,
        0,
        "the zlib header pair must be a multiple of 31"
    );

    // A stored block carries a u16 length, so anything longer is split.
    const MAX: usize = 0xFFFF;
    let mut chunks = data.chunks(MAX).peekable();
    if data.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(part) = chunks.next() {
        let last = chunks.peek().is_none();
        z.push(if last { 1 } else { 0 }); // BFINAL, BTYPE=00, byte-aligned
        let n = part.len() as u16;
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(!n).to_le_bytes());
        z.extend_from_slice(part);
    }

    z.extend_from_slice(&adler32(data).to_be_bytes());
    z
}

/// CRC-32, as PNG specifies it: the reflected `0xEDB88320` form.
struct Crc(u32);

impl Crc {
    fn new() -> Crc {
        Crc(0xFFFF_FFFF)
    }
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u32;
            for _ in 0..8 {
                // Branchless would be a table; a table would be 1 KB of static
                // for something that runs once per startup.
                self.0 = (self.0 >> 1) ^ (0xEDB8_8320 & (!(self.0 & 1)).wrapping_add(1));
            }
        }
    }
    fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published check value. If this is wrong every chunk is wrong and the
    /// shell rejects the file without saying anything.
    #[test]
    fn crc32_matches_the_known_vector() {
        let mut c = Crc::new();
        c.push(b"123456789");
        assert_eq!(c.finish(), 0xCBF4_3926);
    }

    #[test]
    fn adler32_matches_the_known_vector() {
        assert_eq!(adler32(b"123456789"), 0x091E_01DE);
        assert_eq!(adler32(b""), 1);
    }

    /// A CRC accumulated across two pushes must equal one over the join, since
    /// that is exactly how `chunk` uses it: kind then data.
    #[test]
    fn crc32_is_the_same_split_or_whole() {
        let mut split = Crc::new();
        split.push(b"IHDR");
        split.push(&[1, 2, 3, 4]);
        let mut whole = Crc::new();
        whole.push(b"IHDR\x01\x02\x03\x04");
        assert_eq!(split.finish(), whole.finish());
    }

    /// Parse the stored blocks back out. Not a formality: a wrong NLEN or a
    /// missing BFINAL produces a stream that some decoders accept and the shell
    /// does not, which is the worst possible way for this to fail.
    fn inflate_stored(z: &[u8]) -> Vec<u8> {
        assert_eq!(z[0], 0x78, "bad CMF");
        assert_eq!((z[0] as u16 * 256 + z[1] as u16) % 31, 0, "bad zlib header check");
        let mut out = Vec::new();
        let mut i = 2;
        loop {
            let header = z[i];
            assert_eq!(header & 0x06, 0, "not a stored block");
            let final_block = header & 1 == 1;
            let len = u16::from_le_bytes([z[i + 1], z[i + 2]]);
            let nlen = u16::from_le_bytes([z[i + 3], z[i + 4]]);
            assert_eq!(nlen, !len, "NLEN is not the complement of LEN");
            out.extend_from_slice(&z[i + 5..i + 5 + len as usize]);
            i += 5 + len as usize;
            if final_block {
                break;
            }
        }
        let want = u32::from_be_bytes([z[i], z[i + 1], z[i + 2], z[i + 3]]);
        assert_eq!(adler32(&out), want, "adler does not match the payload");
        assert_eq!(i + 4, z.len(), "trailing bytes after the stream");
        out
    }

    #[test]
    fn the_zlib_stream_round_trips() {
        for n in [0usize, 1, 100, 0xFFFF, 0x1_0000, 0x2_0003] {
            let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
            assert_eq!(inflate_stored(&zlib_stored(&data)), data, "n = {n}");
        }
    }

    /// Walk the whole file the way a decoder does, and get the pixels back.
    #[test]
    fn a_png_round_trips_to_its_pixels() {
        let (w, h) = (7u32, 5u32);
        let rgba: Vec<u8> = (0..(w * h * 4)).map(|i| (i % 256) as u8).collect();
        let png = encode(w, h, &rgba);

        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

        let mut i = 8;
        let mut kinds = Vec::new();
        let mut idat = Vec::new();
        let mut ihdr = Vec::new();
        while i < png.len() {
            let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
            let kind = &png[i + 4..i + 8];
            let data = &png[i + 8..i + 8 + len];

            let mut crc = Crc::new();
            crc.push(kind);
            crc.push(data);
            let stored = u32::from_be_bytes(png[i + 8 + len..i + 12 + len].try_into().unwrap());
            assert_eq!(crc.finish(), stored, "bad CRC on {:?}", std::str::from_utf8(kind));

            match kind {
                b"IHDR" => ihdr = data.to_vec(),
                b"IDAT" => idat.extend_from_slice(data),
                _ => {}
            }
            kinds.push(String::from_utf8(kind.to_vec()).unwrap());
            i += 12 + len;
        }

        assert_eq!(kinds, vec!["IHDR", "IDAT", "IEND"], "chunk order");
        assert_eq!(u32::from_be_bytes(ihdr[0..4].try_into().unwrap()), w);
        assert_eq!(u32::from_be_bytes(ihdr[4..8].try_into().unwrap()), h);
        assert_eq!(&ihdr[8..], &[8, 6, 0, 0, 0], "depth/colour/compression/filter/interlace");

        // Strip the per-row filter byte and the pixels must be what went in.
        let raw = inflate_stored(&idat);
        let stride = (w * 4) as usize;
        let mut got = Vec::new();
        for row in raw.chunks_exact(stride + 1) {
            assert_eq!(row[0], 0, "filter type is not None");
            got.extend_from_slice(&row[1..]);
        }
        assert_eq!(got, rgba);
    }
}
