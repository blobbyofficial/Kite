//! An indexed all-intra frame container.
//!
//! Long-GOP H.264 is hostile to random access: a single seek can require decoding hundreds of
//! frames. On import we transcode once into this format, where every frame is independent and
//! its byte range is a direct index lookup. Seeking to any frame is then O(1) plus one JPEG
//! decode, which is what makes scrubbing feel instant regardless of the source codec.
//!
//! Layout:
//!   [0..8)    magic "KITEFS01"
//!   [8..12)   width          u32 le
//!   [12..16)  height         u32 le
//!   [16..20)  fps            u32 le
//!   [20..24)  frame_count    u32 le
//!   [24..32)  index_offset   u64 le
//!   [32..64)  reserved
//!   [64..index_offset)       JPEG payloads, back to back
//!   [index_offset..)         frame_count entries of (u64 offset, u32 len)

use anyhow::{bail, Result};
use memmap2::Mmap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"KITEFS01";
const HEADER: u64 = 64;
const ENTRY: usize = 12;

pub struct FrameStoreWriter {
    out: BufWriter<File>,
    index: Vec<(u64, u32)>,
    cursor: u64,
    width: u32,
    height: u32,
    fps: u32,
}

impl FrameStoreWriter {
    pub fn create(path: &Path, fps: u32) -> Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let f = File::create(path)?;
        let mut out = BufWriter::with_capacity(1 << 20, f);
        out.write_all(&[0u8; HEADER as usize])?;
        Ok(Self { out, index: Vec::new(), cursor: HEADER, width: 0, height: 0, fps })
    }

    pub fn push(&mut self, jpeg: &[u8]) -> Result<()> {
        if self.width == 0 {
            if let Some((w, h)) = jpeg_dimensions(jpeg) {
                self.width = w;
                self.height = h;
            }
        }
        self.out.write_all(jpeg)?;
        self.index.push((self.cursor, jpeg.len() as u32));
        self.cursor += jpeg.len() as u64;
        Ok(())
    }

    pub fn frame_count(&self) -> usize {
        self.index.len()
    }

    pub fn finish(mut self) -> Result<()> {
        let index_offset = self.cursor;
        let mut buf = Vec::with_capacity(self.index.len() * ENTRY);
        for (off, len) in &self.index {
            buf.extend_from_slice(&off.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
        }
        self.out.write_all(&buf)?;
        self.out.flush()?;

        let mut file = self.out.into_inner()?;
        let mut header = [0u8; HEADER as usize];
        header[0..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&self.width.to_le_bytes());
        header[12..16].copy_from_slice(&self.height.to_le_bytes());
        header[16..20].copy_from_slice(&self.fps.to_le_bytes());
        header[20..24].copy_from_slice(&(self.index.len() as u32).to_le_bytes());
        header[24..32].copy_from_slice(&index_offset.to_le_bytes());
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.sync_all()?;
        Ok(())
    }
}

/// A memory-mapped, read-only view. The OS page cache does the caching for us, so repeated
/// scrubbing over the same region costs nothing after the first pass.
pub struct FrameStore {
    map: Mmap,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: usize,
    index_offset: usize,
}

impl FrameStore {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER as usize || &map[0..8] != MAGIC {
            bail!("not a kite frame store: {}", path.display());
        }
        let u32at = |o: usize| u32::from_le_bytes(map[o..o + 4].try_into().unwrap());
        let width = u32at(8);
        let height = u32at(12);
        let fps = u32at(16);
        let frames = u32at(20) as usize;
        let index_offset = u64::from_le_bytes(map[24..32].try_into().unwrap()) as usize;
        if index_offset + frames * ENTRY > map.len() {
            bail!("frame store index is truncated");
        }
        Ok(Self { map, width, height, fps, frames, index_offset })
    }

    /// Raw JPEG bytes for a frame, or `None` if out of range.
    pub fn jpeg(&self, i: usize) -> Option<&[u8]> {
        if i >= self.frames {
            return None;
        }
        let e = self.index_offset + i * ENTRY;
        let off = u64::from_le_bytes(self.map[e..e + 8].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(self.map[e + 8..e + 12].try_into().unwrap()) as usize;
        self.map.get(off..off + len)
    }
}

/// Walks JPEG markers to find the end of the first complete image in `buf`.
///
/// Scanning naively for `FFD9` is wrong: the byte pair can occur inside entropy-coded data. This
/// walks the segment structure properly, and inside scan data relies on JPEG's `FF00` byte
/// stuffing, which guarantees any other `FFxx` is a real marker.
pub fn jpeg_end(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    loop {
        // Markers may be preceded by fill bytes.
        while i < buf.len() && buf[i] != 0xFF {
            i += 1;
        }
        while i < buf.len() && buf[i] == 0xFF {
            i += 1;
        }
        if i >= buf.len() {
            return None;
        }
        let marker = buf[i];
        i += 1;
        match marker {
            0xD9 => return Some(i), // EOI
            // Standalone markers carry no payload.
            0x01 | 0xD0..=0xD7 => continue,
            0xDA => {
                // Start of scan: skip the header, then run to the next real marker.
                if i + 2 > buf.len() {
                    return None;
                }
                let len = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
                i += len;
                while i + 1 < buf.len() {
                    if buf[i] == 0xFF {
                        let n = buf[i + 1];
                        if n != 0x00 && !(0xD0..=0xD7).contains(&n) && n != 0xFF {
                            break;
                        }
                    }
                    i += 1;
                }
                if i + 1 >= buf.len() {
                    return None;
                }
            }
            _ => {
                if i + 2 > buf.len() {
                    return None;
                }
                let len = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
                if len < 2 {
                    return None;
                }
                i += len;
            }
        }
        if i > buf.len() {
            return None;
        }
    }
}

/// Reads width/height out of the first SOF segment.
pub fn jpeg_dimensions(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 4 <= buf.len() {
        while i < buf.len() && buf[i] != 0xFF {
            i += 1;
        }
        while i < buf.len() && buf[i] == 0xFF {
            i += 1;
        }
        if i >= buf.len() {
            return None;
        }
        let marker = buf[i];
        i += 1;
        match marker {
            0x01 | 0xD0..=0xD7 | 0xD8 => continue,
            0xD9 | 0xDA => return None,
            // SOF0..SOF15, excluding DHT(C4), JPG(C8) and DAC(CC).
            0xC0..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
                if i + 7 > buf.len() {
                    return None;
                }
                let h = u16::from_be_bytes([buf[i + 3], buf[i + 4]]) as u32;
                let w = u16::from_be_bytes([buf[i + 5], buf[i + 6]]) as u32;
                return Some((w, h));
            }
            _ => {
                if i + 2 > buf.len() {
                    return None;
                }
                let len = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
                if len < 2 {
                    return None;
                }
                i += len;
            }
        }
    }
    None
}

/// Scans a raw MJPEG byte stream, returning the byte range of every complete image plus how many
/// bytes of `buf` were consumed. Returning ranges rather than slices lets the caller keep the
/// buffer mutable, which matters when it is being refilled from a pipe.
pub fn scan_frames(buf: &[u8]) -> (Vec<std::ops::Range<usize>>, usize) {
    let mut out = Vec::new();
    let mut consumed = 0usize;
    loop {
        let soi = match find_soi(&buf[consumed..]) {
            Some(p) => consumed + p,
            None => return (out, buf.len().saturating_sub(1).max(consumed)),
        };
        match jpeg_end(&buf[soi..]) {
            Some(end) => {
                out.push(soi..soi + end);
                consumed = soi + end;
            }
            None => return (out, soi),
        }
    }
}

fn find_soi(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w[0] == 0xFF && w[1] == 0xD8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg() -> Vec<u8> {
        // SOI, SOF0 (2x2), SOS with a byte-stuffed FFD9 in the scan data, EOI.
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x02, 0x00, 0x02, 0x01, 0x01, 0x11, 0x00]);
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        v.extend_from_slice(&[0x12, 0xFF, 0x00, 0xD9, 0x34, 0xFF, 0xD0, 0x56]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn finds_real_end_not_stuffed_bytes() {
        let j = tiny_jpeg();
        assert_eq!(jpeg_end(&j), Some(j.len()));
    }

    #[test]
    fn reads_dimensions() {
        assert_eq!(jpeg_dimensions(&tiny_jpeg()), Some((2, 2)));
    }

    #[test]
    fn splits_a_stream_and_leaves_partial_tail() {
        let j = tiny_jpeg();
        let mut stream = j.clone();
        stream.extend_from_slice(&j);
        stream.extend_from_slice(&j[..5]); // partial third frame
        let (ranges, used) = scan_frames(&stream);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&stream[ranges[0].clone()], &j[..]);
        assert_eq!(&stream[ranges[1].clone()], &j[..]);
        assert_eq!(used, j.len() * 2);
    }
}
