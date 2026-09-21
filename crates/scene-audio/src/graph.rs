//! The audio graph: which clips play, where in the program, at what
//! level, and who ducks whom. Pure data — building it touches no files.

use std::path::{Path, PathBuf};

use scene_ir::{Diagnostic, ElementKind, Span};
use scene_time::{ResolvedElement, ResolvedScene, SampleRange};

/// 48 kHz program rate, float form — window math shifts source read
/// points by sample deltas over this.
const PROGRAM_RATE_F64: f64 = 48_000.0;

/// Sidechain compressor constants for `duck` — fixed for v1 so the
/// audible behavior is a property of the engine, not per-scene tuning.
pub const DUCK_THRESHOLD: f64 = 0.02;
pub const DUCK_RATIO: f64 = 8.0;
pub const DUCK_ATTACK_MS: f64 = 20.0;
pub const DUCK_RELEASE_MS: f64 = 300.0;

/// Linear fade edges. Zero means "no fade". (Scene attributes for fades
/// are a later module; the graph already carries them.)
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Fade {
    pub in_s: f64,
    pub out_s: f64,
}

/// One audio source placed on the program timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioClip {
    /// Absolute path — `from_scene` joins `src` against the project root.
    pub src: PathBuf,
    /// Where the clip lands in 48 kHz program samples.
    pub target: SampleRange,
    /// How far into the source playback starts, seconds. v1: always 0.
    pub src_start_s: f64,
    pub gain_db: f64,
    pub fade: Fade,
    /// Track the element sat on — duck keys resolve through this.
    pub track_id: String,
    /// Index of the element's source span, for diagnostics.
    pub span: Span,
}

/// `clip`'s level is compressed by the mix of `key` clips.
#[derive(Debug, Clone, PartialEq)]
pub struct DuckLink {
    pub clip: usize,
    pub key: Vec<usize>,
}

/// The whole program mix.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioGraph {
    pub clips: Vec<AudioClip>,
    pub duck: Vec<DuckLink>,
    /// Program length in 48 kHz samples — the mix is cut to exactly this.
    pub program_samples: u64,
}

impl AudioGraph {
    /// Collect `music`/`sound` elements (at any depth) into a graph.
    /// `duck="T"` resolves to the indices of clips on track `T`; a duck
    /// target with no audio produces one warning per element.
    pub fn from_scene(scene: &ResolvedScene, root: &Path) -> (Self, Vec<Diagnostic>) {
        let mut clips = Vec::new();
        let mut ducks: Vec<(usize, String, Span)> = Vec::new();
        let mut diags = Vec::new();
        for track in &scene.tracks {
            collect(
                &track.elements,
                &track.id,
                root,
                &mut clips,
                &mut ducks,
                &mut diags,
            );
        }
        let mut links = Vec::new();
        for (clip, target_track, span) in ducks {
            let key: Vec<usize> = clips
                .iter()
                .enumerate()
                .filter(|(i, c)| *i != clip && c.track_id == target_track)
                .map(|(i, _)| i)
                .collect();
            if key.is_empty() {
                diags.push(Diagnostic::warning(
                    format!("duck target `{target_track}` has no audio to key on"),
                    Some(span),
                ));
            } else {
                links.push(DuckLink { clip, key });
            }
        }
        (
            AudioGraph {
                clips,
                duck: links,
                program_samples: scene.program.samples.end,
            },
            diags,
        )
    }

    /// Rebase the mix onto `[start, end)` program samples — the window a
    /// `--frames` partial render actually covers. Clips are clipped to
    /// the window: the source read point shifts by however much of the
    /// front was cut, delays rebase to the window start, and
    /// non-overlapping clips (and duck links that lose their keys) drop
    /// out. Without this a partial render would play the opening audio
    /// under later frames.
    pub fn window(&self, start: u64, end: u64) -> AudioGraph {
        let end = end.min(self.program_samples);
        if end <= start {
            return AudioGraph::default();
        }
        let mut remap = vec![None; self.clips.len()];
        let mut clips = Vec::with_capacity(self.clips.len());
        for (i, clip) in self.clips.iter().enumerate() {
            let s = clip.target.start.max(start);
            let e = clip.target.end.min(end);
            if e <= s {
                continue;
            }
            remap[i] = Some(clips.len());
            clips.push(AudioClip {
                src_start_s: clip.src_start_s + (s - clip.target.start) as f64 / PROGRAM_RATE_F64,
                target: SampleRange {
                    start: s - start,
                    end: e - start,
                },
                ..clip.clone()
            });
        }
        let duck = self
            .duck
            .iter()
            .filter_map(|link| {
                let clip = remap[link.clip]?;
                let key: Vec<usize> = link.key.iter().filter_map(|&k| remap[k]).collect();
                (!key.is_empty()).then_some(DuckLink { clip, key })
            })
            .collect();
        AudioGraph {
            clips,
            duck,
            program_samples: end - start,
        }
    }
}

fn collect(
    elements: &[ResolvedElement],
    track_id: &str,
    root: &Path,
    clips: &mut Vec<AudioClip>,
    ducks: &mut Vec<(usize, String, Span)>,
    diags: &mut Vec<Diagnostic>,
) {
    for el in elements {
        let (src, gain_db, duck, from_s) = match &el.kind {
            ElementKind::Music {
                src,
                gain_db,
                duck,
                from_s,
            } => (src.as_str(), *gain_db, duck.as_deref(), *from_s),
            ElementKind::Sound {
                src,
                gain_db,
                from_s,
            } => (src.as_str(), *gain_db, None, *from_s),
            _ => {
                collect(&el.children, track_id, root, clips, ducks, diags);
                continue;
            }
        };
        // Authored src stays inside the project root — same contract as
        // visual sources and `<render target>`. An escape drops the clip
        // with a warning rather than failing the whole mix.
        let src = match scene_media::confine_under_root(root, Path::new(src)) {
            Ok(path) => path,
            Err(_) => {
                diags.push(Diagnostic::warning(
                    format!("audio src `{src}` escapes the project root — clip skipped"),
                    Some(el.span),
                ));
                continue;
            }
        };
        clips.push(AudioClip {
            src,
            target: el.timing.samples,
            src_start_s: from_s,
            gain_db,
            fade: Fade::default(),
            track_id: track_id.to_string(),
            span: el.span,
        });
        if let Some(d) = duck {
            ducks.push((clips.len() - 1, d.to_string(), el.span));
        }
    }
}
