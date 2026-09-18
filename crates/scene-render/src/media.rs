//! Where `clip`/`image` pixels come from. `FrameSource` is the seam:
//! tests stub it, production backs it with scene-media decode + a cache.
//!
//! Sequential decode only — a clip plays forward, so its source stream
//! advances monotonically. Random access (scrubbing a long GOP) is the
//! cache's problem, not the rasterizer's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use scene_media::{Frame, FrameStream, MediaError};

/// Pixels for one authored asset at one program instant.
pub trait FrameSource {
    /// Sample `src` at program frame `frame` (the element's local frame).
    /// `fps` is the program frame rate — the source maps it onto its own
    /// stream rate. `None` = no pixels (draw the placeholder).
    fn sample(&mut self, src: &str, frame: u64, fps: f64) -> Option<Frame>;
}

/// Decodes each `src` on first use via scene-media, then walks forward.
/// A source frame `f` is produced for every program frame that maps onto
/// it — the mapping is nearest-source-frame, so a 10fps clip on a 30fps
/// program holds each source frame for three program frames.
pub struct SeqFrameSource {
    root: PathBuf,
    streams: HashMap<String, Option<SeqStream>>,
}

struct SeqStream {
    stream: FrameStream,
    /// Rate ratio: source frames per program frame.
    ratio: f64,
    /// The latest decoded frame and its source index.
    current: Option<Frame>,
}

impl SeqFrameSource {
    /// `root` is the project directory `src` paths resolve against.
    pub fn new(root: PathBuf) -> Self {
        SeqFrameSource {
            root,
            streams: HashMap::new(),
        }
    }

    fn open(&mut self, src: &str) -> Option<&mut SeqStream> {
        if !self.streams.contains_key(src) {
            let path = self.root.join(src);
            let opened = FrameStream::open(&path).ok().map(|stream| {
                let source_fps = stream
                    .info()
                    .video
                    .as_ref()
                    .and_then(|v| v.frame_rate)
                    .map(|r| r.to_f64())
                    .unwrap_or(30.0);
                SeqStream {
                    stream,
                    ratio: source_fps,
                    current: None,
                }
            });
            self.streams.insert(src.to_string(), opened);
        }
        self.streams.get_mut(src)?.as_mut()
    }
}

impl FrameSource for SeqFrameSource {
    fn sample(&mut self, src: &str, frame: u64, fps: f64) -> Option<Frame> {
        let seq = self.open(src)?;
        let target = ((frame as f64) * seq.ratio / fps).floor() as u64;
        // Advance the decode until the current frame covers `target`.
        loop {
            match &seq.current {
                Some(f) if f.index >= target => return Some(f.clone()),
                _ => match seq.stream.next() {
                    Some(Ok(f)) => seq.current = Some(f),
                    _ => return seq.current.clone(),
                },
            }
        }
    }
}

/// Raster output for a `sample` that produced nothing: a dark slate so a
/// missing clip reads as intentional, not a black hole.
pub fn placeholder_frame(width: u32, height: u32) -> Frame {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.copy_from_slice(&[0x18, 0x1a, 0x20, 0xff]);
    }
    Frame {
        index: 0,
        width,
        height,
        pixels,
    }
}

/// Decode a still image file (png/jpeg) to an RGBA frame via `image`.
pub fn decode_still(path: &Path) -> Result<Frame, MediaError> {
    let img = image::ImageReader::open(path)
        .map_err(|e| MediaError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, e)))?
        .decode()
        .map_err(|e| MediaError::Io(std::io::Error::other(e.to_string())))?;
    let rgba = img.to_rgba8();
    Ok(Frame {
        index: 0,
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

/// Still images: decode once, serve every sample.
pub struct StillFrameSource {
    root: PathBuf,
    cache: HashMap<String, Option<Frame>>,
}

impl StillFrameSource {
    pub fn new(root: PathBuf) -> Self {
        StillFrameSource {
            root,
            cache: HashMap::new(),
        }
    }
}

impl FrameSource for StillFrameSource {
    fn sample(&mut self, src: &str, _frame: u64, _fps: f64) -> Option<Frame> {
        if !self.cache.contains_key(src) {
            let frame = decode_still(&self.root.join(src)).ok();
            self.cache.insert(src.to_string(), frame);
        }
        self.cache.get(src)?.clone()
    }
}
