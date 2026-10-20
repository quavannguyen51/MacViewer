//! HostedScreenMessage.swift + ZstdScreenEncoder.swift: screen payloads the
//! host sends to a modern (>= 6.0.0) Windows viewer. Returns LE32 multipart
//! bytes before relay routing.

use crate::wire::{encode_multipart, Result};

/// Captured RGB555 pixels: tightly packed bottom-up rows, u16 little endian,
/// blue bits 0..4, green 5..9, red 10..14. No row padding.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb555_bottom_up: Vec<u8>,
    pub monitors: Vec<Monitor>,
    /// Local capture identity; never sent to the viewer.
    pub layout_id: Option<String>,
}

impl Frame {
    pub fn same_layout(&self, other: &Frame) -> bool {
        self.width == other.width && self.height == other.height
            && self.monitors == other.monitors && self.layout_id == other.layout_id
    }
}

/// Monitor bounds in transmitted canvas pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// Level 1, standard zstd, no dictionary. Production default.
    Zstandard,
    /// Standard zstd RAW blocks, for fixtures.
    Raw,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    InvalidGeometry,
    InvalidPixelLength { expected: usize, received: usize },
    InvalidVersion,
    SizeLimitExceeded,
    InvalidOutputLimit,
    InputLimitExceeded,
    OutputLimitExceeded,
    CompressionFailed(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "screen encoding failed: {self:?}")
    }
}

impl std::error::Error for EncodeError {}

pub const FIRST_FRAME_NUMBER: i32 = -2_147_483_646;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
pub const MAX_DIMENSION: usize = 8192;
const MAX_BLOCK: usize = 128 * 1024;

fn long(v: i32) -> [u8; 4] {
    v.to_le_bytes()
}

fn checked_pixel_bytes(width: usize, height: usize) -> std::result::Result<usize, EncodeError> {
    if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
        return Err(EncodeError::InvalidGeometry);
    }
    let n = width * height * 2;
    if n > MAX_PAYLOAD {
        return Err(EncodeError::SizeLimitExceeded);
    }
    Ok(n)
}

/// Command 0. Every monitor rectangle has a trailing '|' because the native
/// InitMonitor iterates UBound-1 entries.
pub fn initialization(width: usize, height: usize, version: &str, virtual_left: i32, virtual_top: i32,
                      monitors: &[Monitor]) -> std::result::Result<Vec<u8>, EncodeError> {
    checked_pixel_bytes(width, height)?;
    let comps: Vec<&str> = version.split('.').collect();
    let valid = version.len() <= 64 && comps.len() == 3
        && comps.iter().all(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()) && c.parse::<u64>().is_ok())
        && comps[0].parse::<u64>().is_ok_and(|m| m >= 6);
    if !valid {
        return Err(EncodeError::InvalidVersion);
    }
    let right = virtual_left as i64 + width as i64;
    let bottom = virtual_top as i64 + height as i64;
    if right > i32::MAX as i64 || bottom > i32::MAX as i64 || monitors.len() > 64 {
        return Err(EncodeError::InvalidGeometry);
    }
    for m in monitors {
        if m.left < virtual_left || m.top < virtual_top || m.right <= m.left || m.bottom <= m.top
            || m.right as i64 > right || m.bottom as i64 > bottom {
            return Err(EncodeError::InvalidGeometry);
        }
    }
    let monitor = if monitors.is_empty() {
        format!("{virtual_left}:{virtual_top}:{right}:{bottom}|")
    } else {
        monitors.iter().map(|m| format!("{}:{}:{}:{}|", m.left, m.top, m.right, m.bottom)).collect()
    };
    Ok(encode_multipart(&[
        &[0], &long(width as i32), &long(height as i32), &[], &[], &[0], version.as_bytes(), &[1],
        monitor.as_bytes(), &long(virtual_left), &long(virtual_top),
    ]))
}

/// Command 1 with one full-screen main rectangle, no JPEG/scroll/cache. Rows
/// are sent top-down; the viewer writes its own bottom-up DIB.
pub fn full_frame(frame: &Frame, sequence: i32, compression: Compression) -> std::result::Result<Vec<u8>, EncodeError> {
    full_frame_with(frame, sequence, compression, None)
}

/// The full frame, compressed with the trained screen dictionary when one is
/// given.
pub fn full_frame_with(frame: &Frame, sequence: i32, compression: Compression, dictionary: Option<&[u8]>) -> std::result::Result<Vec<u8>, EncodeError> {
    let n = checked_pixel_bytes(frame.width, frame.height)?;
    if frame.rgb555_bottom_up.len() != n {
        return Err(EncodeError::InvalidPixelLength { expected: n, received: frame.rgb555_bottom_up.len() });
    }
    // 11 length words + command + rectangle + sequence.
    let max_compressed = MAX_PAYLOAD - (44 + 1 + 8 + 4);
    if compression == Compression::Raw {
        let blocks = n.div_ceil(MAX_BLOCK);
        if n > max_compressed - 9 - blocks * 3 {
            return Err(EncodeError::SizeLimitExceeded);
        }
    }
    let stride = frame.width * 2;
    let mut top_down = Vec::with_capacity(n);
    for row in frame.rgb555_bottom_up.chunks_exact(stride).rev() {
        top_down.extend_from_slice(row);
    }
    let mut rect = Vec::with_capacity(8);
    for v in [0u16, 0, frame.width as u16, frame.height as u16] {
        rect.extend_from_slice(&v.to_le_bytes());
    }
    let compressed = match compression {
        Compression::Zstandard => compress(&top_down, max_compressed, dictionary).map_err(|e| match e {
            EncodeError::OutputLimitExceeded => EncodeError::SizeLimitExceeded,
            e => e,
        })?,
        Compression::Raw => raw_zstd_frame(&top_down),
    };
    Ok(encode_multipart(&[&[1], &rect, &compressed, &[], &long(sequence), &[], &[], &[], &[], &[], &[]]))
}

/// The tile size used to find the changed parts of the screen.
pub const TILE: usize = 64;

/// A rectangle of the screen, in top-left coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// The changed parts of the screen, as rectangles that cover every changed
/// tile. The tiles of one row that follow each other are merged.
pub fn changed_rectangles(current: &Frame, previous: &Frame) -> Vec<Rect> {
    let (w, h) = (current.width, current.height);
    if w != previous.width || h != previous.height {
        return Vec::new();
    }
    let stride = w * 2;
    let tiles_x = w.div_ceil(TILE);
    let tiles_y = h.div_ceil(TILE);
    let mut rects = Vec::new();
    for ty in 0..tiles_y {
        let y = ty * TILE;
        let th = TILE.min(h - y);
        let mut tx = 0;
        while tx < tiles_x {
            let x = tx * TILE;
            let tw = TILE.min(w - x);
            if !tile_changed(current, previous, stride, x, y, tw, th) {
                tx += 1;
                continue;
            }
            // Merge the tiles that follow in the same row.
            let mut end = tx + 1;
            while end < tiles_x {
                let nx = end * TILE;
                let ntw = TILE.min(w - nx);
                if !tile_changed(current, previous, stride, nx, y, ntw, th) {
                    break;
                }
                end += 1;
            }
            let right = (end * TILE).min(w);
            rects.push(Rect { x: x as u16, y: y as u16, w: (right - x) as u16, h: th as u16 });
            tx = end;
        }
    }
    rects
}

/// True when the pixels of the tile differ between the two frames.
fn tile_changed(current: &Frame, previous: &Frame, stride: usize, x: usize, y: usize, w: usize, h: usize) -> bool {
    let first = current.height - y - h;
    let off = x * 2;
    let len = w * 2;
    for row in 0..h {
        let at = (first + row) * stride + off;
        if current.rgb555_bottom_up[at..at + len] != previous.rgb555_bottom_up[at..at + len] {
            return true;
        }
    }
    false
}

/// The pixels of the rectangles, top-down and back to back, as the viewer
/// expects them.
fn rectangle_pixels(frame: &Frame, rects: &[Rect]) -> Vec<u8> {
    let stride = frame.width * 2;
    let mut out = Vec::new();
    for r in rects {
        let (x, y, w, h) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
        let first = frame.height - y - h;
        for row in (0..h).rev() {
            let at = (first + row) * stride + x * 2;
            out.extend_from_slice(&frame.rgb555_bottom_up[at..at + w * 2]);
        }
    }
    out
}

/// Command 1 with only the parts of the screen that changed. `current` and
/// `previous` must have the same layout. Falls back to the full frame when
/// almost everything changed: that is smaller and simpler.
pub fn delta_frame(current: &Frame, previous: &Frame, sequence: i32, dictionary: Option<&[u8]>) -> std::result::Result<Vec<u8>, EncodeError> {
    let rects = changed_rectangles(current, previous);
    if rects.is_empty() {
        return full_frame_with(current, sequence, Compression::Zstandard, dictionary);
    }
    let changed: usize = rects.iter().map(|r| r.w as usize * r.h as usize).sum();
    let whole = current.width * current.height;
    if changed * 2 >= whole {
        // Most of the screen changed: a full frame is not bigger.
        return full_frame_with(current, sequence, Compression::Zstandard, dictionary);
    }
    let pixels = rectangle_pixels(current, &rects);
    let max_compressed = MAX_PAYLOAD - (44 + 1 + 8 + 4);
    let compressed = compress(&pixels, max_compressed, dictionary)?;
    let mut words = Vec::with_capacity(rects.len() * 8);
    for r in &rects {
        for v in [r.x, r.y, r.w, r.h] {
            words.extend_from_slice(&v.to_le_bytes());
        }
    }
    Ok(encode_multipart(&[&[1], &words, &compressed, &[], &long(sequence), &[], &[], &[], &[], &[], &[]]))
}

/// Reply to the viewer's ASCII "20" keepalive.
pub fn keepalive_reply() -> Vec<u8> {
    encode_multipart(&[&[20]])
}

/// Standard zstd frame: single segment, LE32 content size, raw blocks <= 128 KiB.
fn raw_zstd_frame(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd, 0xa0];
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    let mut chunks = bytes.chunks(MAX_BLOCK).peekable();
    while let Some(chunk) = chunks.next() {
        let header = ((chunk.len() as u32) << 3) | chunks.peek().is_none() as u32;
        out.extend_from_slice(&header.to_le_bytes()[..3]);
        out.extend_from_slice(chunk);
    }
    out
}

/// Compress with the trained screen dictionary when one is given: the
/// Windows desktop loads the same dictionary to decode the frames.
fn compress(bytes: &[u8], max_output: usize, dictionary: Option<&[u8]>) -> std::result::Result<Vec<u8>, EncodeError> {
    let Some(dict) = dictionary else { return zstd_encode(bytes, max_output) };
    if bytes.len() > ZSTD_MAX_INPUT {
        return Err(EncodeError::InputLimitExceeded);
    }
    let mut out = vec![0u8; zstd_safe::compress_bound(bytes.len()).min(max_output)];
    let mut cctx = zstd_safe::CCtx::create();
    if let Err(code) = cctx.load_dictionary(dict) {
        let name = zstd_safe::get_error_name(code);
        return Err(EncodeError::CompressionFailed(name.into()));
    }
    match cctx.compress(&mut out[..], bytes, 1) {
        Ok(n) => {
            out.truncate(n);
            Ok(out)
        }
        Err(code) => {
            let name = zstd_safe::get_error_name(code);
            if name.contains("too small") {
                Err(EncodeError::OutputLimitExceeded)
            } else {
                Err(EncodeError::CompressionFailed(name.into()))
            }
        }
    }
}

pub const ZSTD_MAX_INPUT: usize = 16 * 1024 * 1024;
pub const ZSTD_MAX_OUTPUT: usize = 16 * 1024 * 1024;

/// ZstdScreenEncoder.encode: an independent level-1 frame without dictionary.
/// The output buffer never exceeds `max_output` or compressBound.
pub fn zstd_encode(bytes: &[u8], max_output: usize) -> std::result::Result<Vec<u8>, EncodeError> {
    if !(1..=ZSTD_MAX_OUTPUT).contains(&max_output) {
        return Err(EncodeError::InvalidOutputLimit);
    }
    if bytes.len() > ZSTD_MAX_INPUT {
        return Err(EncodeError::InputLimitExceeded);
    }
    let mut out = vec![0u8; zstd_safe::compress_bound(bytes.len()).min(max_output)];
    let mut cctx = zstd_safe::CCtx::create();
    match cctx.compress(&mut out[..], bytes, 1) {
        Ok(n) => {
            out.truncate(n);
            Ok(out)
        }
        Err(code) => {
            let name = zstd_safe::get_error_name(code);
            if name.contains("too small") {
                Err(EncodeError::OutputLimitExceeded)
            } else {
                Err(EncodeError::CompressionFailed(name.into()))
            }
        }
    }
}

/// Converts a top-down 32-bit XRGB (little endian B,G,R,X) image to bottom-up RGB555.
pub fn rgb555_from_bgrx(src: &[u8], width: usize, height: usize, bytes_per_row: usize) -> Result<Vec<u8>> {
    if bytes_per_row < width * 4 || src.len() < bytes_per_row * height.saturating_sub(1) + width * 4 {
        return Err("captured image is shorter than its geometry".into());
    }
    let mut out = Vec::with_capacity(width * height * 2);
    for y in (0..height).rev() {
        let row = &src[y * bytes_per_row..y * bytes_per_row + width * 4];
        for px in row.chunks_exact(4) {
            let w = ((px[2] as u16 >> 3) << 10) | ((px[1] as u16 >> 3) << 5) | (px[0] as u16 >> 3);
            out.extend_from_slice(&w.to_le_bytes());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::decode_multipart;

    fn decompress(b: &[u8], cap: usize) -> Vec<u8> {
        let mut out = vec![0u8; cap.max(1)];
        let n = zstd_safe::decompress(&mut out[..], b).unwrap();
        out.truncate(n);
        out
    }

    /// A screen of `w` x `h` pixels with a pattern that changes at `spot`.
    fn pattern(w: usize, h: usize, spot: Option<(usize, usize)>) -> Frame {
        let mut pixels = vec![0u8; w * h * 2];
        for y in 0..h {
            for x in 0..w {
                let at = (y * w + x) * 2;
                let mut v = ((x * 7 + y * 13) % 0x7fff) as u16;
                if let Some((sx, sy)) = spot {
                    if x == sx && y == sy {
                        v = 0x1234;
                    }
                }
                pixels[at..at + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        // The frame keeps the rows bottom-up.
        let stride = w * 2;
        let mut bottom_up = vec![0u8; pixels.len()];
        for y in 0..h {
            let src = (h - 1 - y) * stride;
            bottom_up[y * stride..(y + 1) * stride].copy_from_slice(&pixels[src..src + stride]);
        }
        Frame { width: w, height: h, rgb555_bottom_up: bottom_up, ..Default::default() }
    }

    /// The delta frame must paint exactly the same picture as a full frame.
    #[test]
    fn delta_frame_paints_the_same_picture_as_a_full_frame() {
        use crate::screen::{Limits, Renderer, ScreenInit};
        let (w, h) = (256usize, 128usize);
        let before = pattern(w, h, None);
        let after = pattern(w, h, Some((100, 60)));

        let first = full_frame(&before, 1, Compression::Zstandard).unwrap();
        let full = full_frame(&after, 2, Compression::Zstandard).unwrap();
        let delta = delta_frame(&after, &before, 2, None).unwrap();
        // The delta carries only the changed part.
        assert!(delta.len() < full.len(), "delta {} full {}", delta.len(), full.len());

        let paint = |payloads: &[&[u8]]| {
            let mut r = Renderer::with_options(&ScreenInit { width: w as u32, height: h as u32, tile_size: 16, ..Default::default() },
                                                0, 0, Limits::default()).unwrap();
            for payload in payloads {
                let parts = decode_multipart(payload).unwrap();
                let refs: Vec<&[u8]> = parts.iter().copied().collect();
                r.apply(&refs).unwrap();
            }
            r.pixels().to_vec()
        };
        // Painting the first frame and then the delta must give the same
        // picture as painting both full frames.
        assert_eq!(paint(&[&first, &full]), paint(&[&first, &delta]));

        // The same with the trained dictionary, which the viewer loads too.
        let first_dict = full_frame_with(&before, 1, Compression::Zstandard, Some(crate::screen::SCREEN_DICTIONARY)).unwrap();
        let delta_dict = delta_frame(&after, &before, 2, Some(crate::screen::SCREEN_DICTIONARY)).unwrap();
        assert_eq!(paint(&[&first_dict, &full]), paint(&[&first_dict, &delta_dict]));
    }

    /// The changed rectangles must cover every changed pixel.
    #[test]
    fn changed_rectangles_cover_the_change() {
        let (w, h) = (256usize, 128usize);
        let before = pattern(w, h, None);
        let after = pattern(w, h, Some((100, 60)));
        let rects = changed_rectangles(&after, &before);
        assert!(!rects.is_empty());
        assert!(rects.iter().any(|r| r.x <= 100 && 100 < r.x as usize + r.w as usize
            && r.y <= 60 && 60 < r.y as usize + r.h as usize));
        // An unchanged screen has no rectangles.
        assert!(changed_rectangles(&before, &before).is_empty());
    }

    // HostedScreenMessageTests.swift
    #[test]
    fn multiple_monitor_metadata_and_outside_canvas() {
        let mons = [Monitor { left: 0, top: 120, right: 1920, bottom: 1200 },
                    Monitor { left: 1920, top: 0, right: 3840, bottom: 1080 }];
        let p = initialization(3840, 1200, "6.6.133", 0, 0, &mons).unwrap();
        let parts = decode_multipart(&p).unwrap();
        assert_eq!(parts[8], b"0:120:1920:1200|1920:0:3840:1080|");
        for bad in [Monitor { left: -1, top: 0, right: 1, bottom: 1 }, Monitor { left: 0, top: 0, right: 3841, bottom: 1 },
                    Monitor { left: 0, top: 0, right: 1, bottom: 1201 }, Monitor { left: 1, top: 0, right: 1, bottom: 1 }] {
            assert_eq!(initialization(3840, 1200, "6.6.133", 0, 0, &[bad]), Err(EncodeError::InvalidGeometry));
        }
        assert_eq!(initialization(3840, 1200, "6.6.133", 0, 0, &[mons[0]; 65]), Err(EncodeError::InvalidGeometry));
    }

    #[test]
    fn display_switch_requires_reinit() {
        let first = Frame { width: 2, height: 2, rgb555_bottom_up: vec![0; 8], layout_id: Some("built-in".into()), ..Default::default() };
        let second = Frame { layout_id: Some("external".into()), ..first.clone() };
        assert!(!first.same_layout(&second));
        let changed = Frame { monitors: vec![Monitor { left: 0, top: 0, right: 1, bottom: 2 }, Monitor { left: 1, top: 0, right: 2, bottom: 2 }], ..first.clone() };
        assert!(!first.same_layout(&changed));
        assert!(first.same_layout(&first));
    }

    #[test]
    fn initialization_fields() {
        let p = initialization(1920, 1080, "6.6.125", -1920, 100, &[]).unwrap();
        let parts = decode_multipart(&p).unwrap();
        assert_eq!(parts.len(), 11);
        assert_eq!(parts[0], [0]);
        assert_eq!(parts[1], [0x80, 7, 0, 0]);
        assert_eq!(parts[2], [0x38, 4, 0, 0]);
        assert!(parts[3].is_empty() && parts[4].is_empty());
        assert!(parts[5] == [0] && parts[7] == [1]);
        assert_eq!(parts[6], b"6.6.125");
        assert_eq!(parts[8], b"-1920:100:0:1180|");
        assert_eq!(parts[9], [0x80, 0xf8, 0xff, 0xff]);
        assert_eq!(parts[10], [100, 0, 0, 0]);
        assert_eq!(keepalive_reply(), [1, 0, 0, 0, 20]);
    }

    #[test]
    fn full_frame_raw_fixture_and_pixel_order() {
        let frame = Frame { width: 2, height: 2, rgb555_bottom_up: vec![0x1f, 0, 0xff, 0x7f, 0, 0x7c, 0xe0, 3], ..Default::default() };
        let p = full_frame(&frame, FIRST_FRAME_NUMBER, Compression::Raw).unwrap();
        let parts = decode_multipart(&p).unwrap();
        assert_eq!(parts.len(), 11);
        assert_eq!(parts[0], [1]);
        assert_eq!(parts[1], [0, 0, 0, 0, 2, 0, 2, 0]);
        assert_eq!(parts[2], [0x28, 0xb5, 0x2f, 0xfd, 0xa0, 8, 0, 0, 0, 0x41, 0, 0, 0, 0x7c, 0xe0, 3, 0x1f, 0, 0xff, 0x7f]);
        assert_eq!(parts[4], [2, 0, 0, 0x80]);
        assert!([3, 5, 6, 7, 8, 9, 10].iter().all(|&i| parts[i].is_empty()));
        assert_eq!(decompress(parts[2], 8), [0, 0x7c, 0xe0, 3, 0x1f, 0, 0xff, 0x7f]);
    }

    #[test]
    fn odd_width_rows_unpadded() {
        let frame = Frame { width: 3, height: 2, rgb555_bottom_up: vec![0x1f, 0, 0x1f, 0, 0x1f, 0, 0, 0x7c, 0, 0x7c, 0, 0x7c], ..Default::default() };
        let parts_owned = full_frame(&frame, 1, Compression::Zstandard).unwrap();
        let parts = decode_multipart(&parts_owned).unwrap();
        assert_eq!(decompress(parts[2], 12), [0, 0x7c, 0, 0x7c, 0, 0x7c, 0x1f, 0, 0x1f, 0, 0x1f, 0]);
    }

    #[test]
    fn raw_splits_at_128k() {
        let frame = Frame { width: 512, height: 256, rgb555_bottom_up: vec![0x55; 262_144], ..Default::default() };
        let owned = full_frame(&frame, 9, Compression::Raw).unwrap();
        let parts = decode_multipart(&owned).unwrap();
        let b = parts[2];
        assert_eq!(b[..9], [0x28, 0xb5, 0x2f, 0xfd, 0xa0, 0, 0, 4, 0]);
        assert_eq!(b[9..12], [0, 0, 0x10]);
        assert_eq!(b[12 + 131_072..15 + 131_072], [1, 0, 0x10]);
        assert_eq!(b.len(), 9 + 6 + 262_144);
        assert_eq!(decompress(b, 262_144), vec![0x55; 262_144]);
    }

    #[test]
    fn production_frame_is_compressed() {
        let (w, h) = (1920usize, 1080usize);
        let mut bu = vec![0u8; w * h * 2];
        for y in 0..h {
            for x in 0..w {
                let c = (((x / 240) << 10) | ((y / 80) << 5) | ((x / 16) & 15)) as u16;
                bu[(y * w + x) * 2..][..2].copy_from_slice(&c.to_le_bytes());
            }
        }
        let frame = Frame { width: w, height: h, rgb555_bottom_up: bu.clone(), ..Default::default() };
        let p = full_frame(&frame, 42, Compression::Zstandard).unwrap();
        let raw = full_frame(&frame, 42, Compression::Raw).unwrap();
        assert!(p.len() < 64 * 1024 && p.len() < raw.len() / 50);
        let parts = decode_multipart(&p).unwrap();
        assert_eq!(parts[4], [42, 0, 0, 0]);
        let expected: Vec<u8> = bu.chunks_exact(w * 2).rev().flatten().copied().collect();
        assert_eq!(decompress(parts[2], bu.len()), expected);
    }

    #[test]
    fn rejects_bad_geometry_length_version() {
        let frame = Frame { width: 4096, height: 2048, rgb555_bottom_up: vec![0; MAX_PAYLOAD], ..Default::default() };
        assert_eq!(full_frame(&frame, 1, Compression::Raw), Err(EncodeError::SizeLimitExceeded));
        assert_eq!(initialization(0, 2, "6.6.125", 0, 0, &[]), Err(EncodeError::InvalidGeometry));
        assert_eq!(initialization(2, 2, "5.9.0", 0, 0, &[]), Err(EncodeError::InvalidVersion));
        assert_eq!(initialization(2, 2, "6.6.125:extra", 0, 0, &[]), Err(EncodeError::InvalidVersion));
        let short = Frame { width: 2, height: 2, rgb555_bottom_up: vec![0; 7], ..Default::default() };
        assert_eq!(full_frame(&short, 1, Compression::Zstandard), Err(EncodeError::InvalidPixelLength { expected: 8, received: 7 }));
        assert_eq!(initialization(8192, 8192, "6.6.125", 0, 0, &[]), Err(EncodeError::SizeLimitExceeded));
    }

    // ZstdScreenEncoderTests.swift
    #[test]
    fn zstd_within_actual_budget() {
        let px = vec![0x55u8; 256 * 1024];
        let e = zstd_encode(&px, 128).unwrap();
        assert!(e.len() <= 128 && e[..4] == [0x28, 0xb5, 0x2f, 0xfd]);
        assert_eq!(decompress(&e, px.len()), px);
    }

    #[test]
    fn zstd_incompressible_honors_budget() {
        let mut s: u32 = 0x6d2b79f5;
        let px: Vec<u8> = (0..64 * 1024).map(|_| { s ^= s << 13; s ^= s >> 17; s ^= s << 5; s as u8 }).collect();
        assert_eq!(zstd_encode(&px, 32 * 1024), Err(EncodeError::OutputLimitExceeded));
        let e = zstd_encode(&px, px.len() + 1024).unwrap();
        assert!(e.len() <= px.len() + 1024);
        assert_eq!(decompress(&e, px.len()), px);
    }

    #[test]
    fn zstd_rejects_invalid_limits() {
        for lim in [0, ZSTD_MAX_OUTPUT + 1] {
            assert_eq!(zstd_encode(&[1], lim), Err(EncodeError::InvalidOutputLimit));
        }
        assert_eq!(zstd_encode(&vec![0; ZSTD_MAX_INPUT + 1], ZSTD_MAX_OUTPUT), Err(EncodeError::InputLimitExceeded));
        assert_eq!(zstd_encode(&[1], 1), Err(EncodeError::OutputLimitExceeded));
        assert!(decompress(&zstd_encode(&[], 32).unwrap(), 1).is_empty());
    }

    #[test]
    fn bgrx_converts_bottom_up() {
        // top row red, bottom row blue (B,G,R,X)
        let src = [0, 0, 255, 0, 255, 0, 0, 0];
        assert_eq!(rgb555_from_bgrx(&src, 1, 2, 4).unwrap(), [0x1f, 0, 0, 0x7c]);
    }
}
