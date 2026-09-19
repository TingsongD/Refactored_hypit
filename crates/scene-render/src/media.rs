//! Where `clip`/`image` pixels come from. `FrameSource` is the seam:
//! tests stub it, production backs it with scene-media decode + a cache.
//!
//! Sequential decode only — a clip plays forward, so its source stream
//! advances monotonically. Random access (scrubbing a long GOP) is the
//! cache's problem, not the rasterizer's.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use scene_media::{Frame, FrameStream, MediaError, probe};

/// Shared per-render failure log. Worker-local sources record
/// `src → reason` here; the caller drains it into diagnostics after
/// `render_frames`. Set-dedup means a broken asset reports once, not
/// once per frame per worker.
pub type WarnSink = Arc<Mutex<BTreeSet<String>>>;

fn warn(sink: &Option<WarnSink>, msg: String) {
    if let Some(s) = sink
        && let Ok(mut w) = s.lock()
    {
        w.insert(msg);
    }
}

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
    warnings: Option<WarnSink>,
}

struct SeqStream {
    stream: FrameStream,
    /// Rate ratio: source frames per program frame.
    ratio: f64,
    /// Source-frame index the decode started at — nonzero after a `-ss`
    /// seek, since ffmpeg numbers piped frames from 0 again.
    base: u64,
    /// The latest decoded frame and its source index.
    current: Option<Frame>,
}

/// Prefix length past which an initial `-ss` seek beats decoding and
/// discarding. ~2s at typical rates — trivial prefixes just decode.
const SEEK_THRESHOLD: u64 = 60;

impl SeqFrameSource {
    /// `root` is the project directory `src` paths resolve against.
    pub fn new(root: PathBuf) -> Self {
        SeqFrameSource {
            root,
            streams: HashMap::new(),
            warnings: None,
        }
    }

    /// Record decode failures into `sink` so a missing/unreadable clip
    /// reaches the diagnostic report instead of silently placeholdering.
    pub fn with_warnings(mut self, sink: WarnSink) -> Self {
        self.warnings = Some(sink);
        self
    }

    /// Decode `path` starting near `frame` (a program frame): a deep
    /// first target opens with `-ss` instead of decoding the whole
    /// prefix — each pool worker otherwise re-decodes frames
    /// 0..shard_start for every clip. Returns the stream plus enough
    /// bookkeeping to translate pipe-relative indices back to source
    /// frames.
    fn spawn(path: &Path, frame: u64, program_fps: f64) -> Result<SeqStream, String> {
        let info = probe(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let source_fps = info
            .video
            .as_ref()
            .and_then(|v| v.frame_rate)
            .map(|r| r.to_f64())
            .unwrap_or(30.0);
        let target = ((frame as f64) * source_fps / program_fps).floor() as u64;
        // Seek to half a frame before the target: ffmpeg's accurate
        // seek then delivers `target` as the first decoded frame
        // (exact on CFR sources; ±1 on VFR). Decoded indices restart
        // at 0, so `base` carries the source offset.
        let (base, stream) = if target >= SEEK_THRESHOLD {
            let secs = format!("{:.6}", (target as f64 - 0.5) / source_fps);
            (target, FrameStream::open_with(path, &info, &["-ss", &secs]))
        } else {
            (0, FrameStream::open_with(path, &info, &[]))
        };
        stream
            .map(|stream| SeqStream {
                stream,
                ratio: source_fps,
                base,
                current: None,
            })
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Open (lazily) the stream for `src`. Free-standing over the two
    /// fields so `sample` can hold the map borrow while respawning.
    fn open<'a>(
        streams: &'a mut HashMap<String, Option<SeqStream>>,
        root: &Path,
        src: &str,
        frame: u64,
        program_fps: f64,
        warnings: &Option<WarnSink>,
    ) -> Option<&'a mut SeqStream> {
        if !streams.contains_key(src) {
            match Self::spawn(&root.join(src), frame, program_fps) {
                Ok(s) => {
                    streams.insert(src.to_string(), Some(s));
                }
                Err(e) => {
                    warn(warnings, format!("clip `{src}` unreadable: {e}"));
                    streams.insert(src.to_string(), None);
                }
            }
        }
        streams.get_mut(src)?.as_mut()
    }
}

impl FrameSource for SeqFrameSource {
    fn sample(&mut self, src: &str, frame: u64, fps: f64) -> Option<Frame> {
        let seq = Self::open(
            &mut self.streams,
            &self.root,
            src,
            frame,
            fps,
            &self.warnings,
        )?;
        let target = ((frame as f64) * seq.ratio / fps).floor() as u64;
        // A backward target means a later element restarted the source —
        // the pipe can't rewind, so serving `current` would freeze the
        // clip on the previous element's last frame. Reopen instead.
        if seq.current.as_ref().is_some_and(|f| f.index > target) {
            match Self::spawn(&self.root.join(src), frame, fps) {
                Ok(s) => *seq = s,
                Err(e) => {
                    warn(&self.warnings, format!("clip `{src}` unreadable: {e}"));
                    return None;
                }
            }
        }
        // Advance the decode until the current frame covers `target`.
        loop {
            match &seq.current {
                Some(f) if f.index >= target => return Some(f.clone()),
                _ => match seq.stream.next() {
                    Some(Ok(mut f)) => {
                        f.index += seq.base;
                        seq.current = Some(f);
                    }
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
    warnings: Option<WarnSink>,
}

impl StillFrameSource {
    pub fn new(root: PathBuf) -> Self {
        StillFrameSource {
            root,
            cache: HashMap::new(),
            warnings: None,
        }
    }

    /// Record decode failures into `sink` — see `SeqFrameSource`.
    pub fn with_warnings(mut self, sink: WarnSink) -> Self {
        self.warnings = Some(sink);
        self
    }
}

impl FrameSource for StillFrameSource {
    fn sample(&mut self, src: &str, _frame: u64, _fps: f64) -> Option<Frame> {
        if !self.cache.contains_key(src) {
            match decode_still(&self.root.join(src)) {
                Ok(f) => {
                    self.cache.insert(src.to_string(), Some(f));
                }
                Err(e) => {
                    warn(&self.warnings, format!("image `{src}` unreadable: {e}"));
                    self.cache.insert(src.to_string(), None);
                }
            }
        }
        self.cache.get(src)?.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_still_reports_into_the_sink() {
        let sink = WarnSink::default();
        let mut source =
            StillFrameSource::new(PathBuf::from("/nonexistent-dir")).with_warnings(sink.clone());
        assert!(source.sample("missing.png", 0, 0.0).is_none());
        // Second sample hits the cache — the warning must not repeat.
        assert!(source.sample("missing.png", 0, 0.0).is_none());
        let w = sink.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert!(w.iter().next().unwrap().contains("missing.png"), "{w:?}");
    }

    #[test]
    fn missing_clip_reports_into_the_sink() {
        let sink = WarnSink::default();
        let mut source =
            SeqFrameSource::new(PathBuf::from("/nonexistent-dir")).with_warnings(sink.clone());
        assert!(source.sample("gone.mp4", 0, 30.0).is_none());
        assert!(!sink.lock().unwrap().is_empty());
    }

    #[test]
    fn no_sink_is_silent_but_still_none() {
        let mut source = StillFrameSource::new(PathBuf::from("/nonexistent-dir"));
        assert!(source.sample("missing.png", 0, 0.0).is_none());
    }
}
