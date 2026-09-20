//! Where `clip`/`image` pixels come from. `FrameSource` is the seam:
//! tests stub it, production backs it with scene-media decode + a cache.
//!
//! Sequential decode only — a clip plays forward, so its source stream
//! advances monotonically. Random access (scrubbing a long GOP) is the
//! cache's problem, not the rasterizer's.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use scene_media::{Frame, FrameStream, MediaError, confine_under_root, probe};

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

/// `root.join(src)` under project-root confinement — an authored `src`
/// may not reach outside the scene file's directory (same contract as
/// `program` sources and `<render target>`). An escape warns once and
/// reads as a permanently failed source, like a missing file.
fn confined(root: &Path, src: &str, kind: &str, warnings: &Option<WarnSink>) -> Option<PathBuf> {
    match confine_under_root(root, Path::new(src)) {
        Ok(path) => Some(path),
        Err(_) => {
            warn(
                warnings,
                format!("{kind} `{src}` escapes the project root — refused"),
            );
            None
        }
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
    /// Boxed so tests can drive the advance loop with a fake stream.
    stream: Box<dyn Iterator<Item = Result<Frame, MediaError>>>,
    /// Rate ratio: source frames per program frame.
    ratio: f64,
    /// Source-frame index the decode started at — nonzero after a `-ss`
    /// seek, since ffmpeg numbers piped frames from 0 again.
    base: u64,
    /// The latest decoded frame and its source index.
    current: Option<Frame>,
    /// True once the stream hit EOF or failed — a dead stream is never
    /// polled again (a failed child's `wait` would error every call).
    dead: bool,
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
                stream: Box::new(stream),
                ratio: source_fps,
                base,
                current: None,
                dead: false,
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
            match confined(root, src, "clip", warnings)
                .map(|p| Self::spawn(&p, frame, program_fps))
                .transpose()
            {
                Ok(Some(s)) => {
                    streams.insert(src.to_string(), Some(s));
                }
                Ok(None) => {
                    streams.insert(src.to_string(), None);
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
            match confined(&self.root, src, "clip", &self.warnings)
                .map(|p| Self::spawn(&p, frame, fps))
                .transpose()
            {
                Ok(Some(s)) => *seq = s,
                Ok(None) => return None,
                Err(e) => {
                    warn(&self.warnings, format!("clip `{src}` unreadable: {e}"));
                    return None;
                }
            }
        }
        // Advance the decode until the current frame covers `target`.
        // `Some(Err)` is not EOF: a mid-file failure must reach the
        // warning sink instead of silently freezing on the last frame.
        loop {
            match &seq.current {
                Some(f) if f.index >= target => return Some(f.clone()),
                _ if seq.dead => return seq.current.clone(),
                _ => match seq.stream.next() {
                    Some(Ok(mut f)) => {
                        f.index += seq.base;
                        seq.current = Some(f);
                    }
                    Some(Err(e)) => {
                        warn(&self.warnings, format!("clip `{src}` decode failed: {e}"));
                        seq.dead = true;
                        return seq.current.clone();
                    }
                    None => {
                        seq.dead = true;
                        return seq.current.clone();
                    }
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
            match confined(&self.root, src, "image", &self.warnings)
                .map(|p| decode_still(&p))
                .transpose()
            {
                Ok(Some(f)) => {
                    self.cache.insert(src.to_string(), Some(f));
                }
                Ok(None) => {
                    self.cache.insert(src.to_string(), None);
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
    fn escaping_src_is_refused_and_warned() {
        let sink = WarnSink::default();
        let mut still = StillFrameSource::new(PathBuf::from(".")).with_warnings(sink.clone());
        // `..` and absolute escapes refuse before the decoder runs.
        assert!(still.sample("../outside.png", 0, 0.0).is_none());
        assert!(still.sample("/etc/passwd", 0, 0.0).is_none());
        // Cached failure — no second warning per src.
        assert!(still.sample("../outside.png", 1, 0.0).is_none());
        let w = sink.lock().unwrap();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(
            w.iter().all(|m| m.contains("escapes the project root")),
            "{w:?}"
        );
        drop(w);

        let mut seq = SeqFrameSource::new(PathBuf::from(".")).with_warnings(sink.clone());
        assert!(seq.sample("../clip.mp4", 0, 30.0).is_none());
        assert!(seq.sample("/abs/clip.mp4", 0, 30.0).is_none());
        let w = sink.lock().unwrap();
        assert_eq!(w.len(), 4, "{w:?}");
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

    /// Yields two frames, one error, then panics if polled again — the
    /// dead flag must stop the source from re-polling a failed stream.
    struct FailThenPanic {
        items: std::vec::IntoIter<Result<Frame, MediaError>>,
        err_once: bool,
    }
    impl Iterator for FailThenPanic {
        type Item = Result<Frame, MediaError>;
        fn next(&mut self) -> Option<Self::Item> {
            if let Some(item) = self.items.next() {
                return Some(item);
            }
            if self.err_once {
                self.err_once = false;
                return Some(Err(MediaError::Io(std::io::Error::other(
                    "decoder blew up",
                ))));
            }
            panic!("polled a dead stream");
        }
    }

    #[test]
    fn mid_stream_decode_error_warns_and_holds_the_last_frame() {
        let sink = WarnSink::default();
        let mut source = SeqFrameSource::new(PathBuf::from("/unused")).with_warnings(sink.clone());
        let frame = |index: u64| Frame {
            index,
            width: 1,
            height: 1,
            pixels: vec![index as u8; 4],
        };
        source.streams.insert(
            "c.mp4".to_string(),
            Some(SeqStream {
                stream: Box::new(FailThenPanic {
                    items: vec![Ok(frame(0)), Ok(frame(1))].into_iter(),
                    err_once: true,
                }),
                ratio: 30.0,
                base: 0,
                current: None,
                dead: false,
            }),
        );
        assert_eq!(source.sample("c.mp4", 0, 30.0).unwrap().index, 0);
        // Frame 2 targets past the failure — the error surfaces and the
        // last decoded frame holds.
        assert_eq!(source.sample("c.mp4", 2, 30.0).unwrap().index, 1);
        // Polling again must not touch the stream (would panic).
        assert_eq!(source.sample("c.mp4", 3, 30.0).unwrap().index, 1);
        let w = sink.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert!(w.iter().next().unwrap().contains("decode failed"), "{w:?}");
    }
}
