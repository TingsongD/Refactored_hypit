//! The locked brief — loaded once at run start, hashed into every
//! downstream cache key. Thresholds live here, never inside Jev prompts
//! or code constants (flash-cut taste is subjective; tune on clips).

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::MemeError;

/// Default zero-shot tag vocabulary — candidate frames get labeled
/// against this when an `embed` connector is present. Fixed so the
/// fact-sheet tags stay comparable run to run.
pub const DEFAULT_TAG_VOCAB: &[&str] = &[
    "close-up",
    "wide",
    "face",
    "people",
    "text-on-screen",
    "dark",
    "bright",
    "action",
    "idle",
    "crowd",
    "product",
    "meme-format",
    "blurry",
    "logo",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Brief {
    /// Job discriminator — the pipeline only does one thing, but the
    /// field rides into Jev state so a shared endpoint sees intent.
    pub job: String,
    /// One-line routing hint Jev prepends to every question set.
    pub description: String,

    // --- perception grid -------------------------------------------------
    /// Analysis fps — frames and audio hops align on this grid. 30 is
    /// the target; degrade to 15 if the decode can't hold it.
    pub fps: f64,
    /// Analysis resolution (square) — embeddings and metrics run on
    /// frames scaled to this.
    pub perceive_size: u32,

    // --- peak pick -------------------------------------------------------
    /// Minimum frames between candidates. 2 (not 4) — flash beats can
    /// land 2–6 frames apart and a larger gap starves them.
    pub min_gap_frames: u32,
    /// `change` at or above this is a candidate spike.
    pub change_keep: f64,
    /// `change` below this is a same-shot hint on the fact sheet.
    pub change_skip: f64,
    /// A candidate whose best similarity to an already-kept frame
    /// exceeds this is a duplicate. Cosine under an encoder, hamming/64
    /// under dHash.
    pub dup_cosine: f64,
    /// Sharpness floor — below it a frame is transition smear.
    pub min_sharpness: f64,
    /// Mean-luma bounds — outside these a frame is near-black/white.
    pub brightness_min: f64,
    pub brightness_max: f64,
    /// Audio-onset contribution to `importance` — raises beats that
    /// cut on sound even when the visual change is modest.
    pub onset_weight: f64,
    /// Snap radius (frames) when aligning a keep to an onset or word
    /// boundary. ±3 ≈ 100 ms at 30 fps.
    pub snap_radius_frames: u32,

    // --- Jev -------------------------------------------------------------
    /// Cap on fact-sheet rows; extras are dropped by `importance`.
    pub max_candidates_to_jev: usize,
    /// Below this confidence the run surfaces to a human instead of
    /// auto-exporting.
    pub jev_min_confidence: f64,

    // --- Gemini ----------------------------------------------------------
    /// Capability name — resolves in scene.toml, not a hardcoded model.
    pub gemini_cap: String,
    /// Half-window around each keep for `windows` mode.
    pub gemini_window_sec: f64,
    /// Sampling rate for window transcodes — default Gemini sampling
    /// (1 fps) misses 2–6-frame cuts entirely.
    pub gemini_fps: f64,

    // --- encoder ---------------------------------------------------------
    /// `"dhash"` (built-in, offline) or an `embed` capability name whose
    /// connector returns frame embeddings (e.g. MobileCLIP).
    pub encoder: String,
    /// Zero-shot vocabulary for candidate tagging (embed mode only).
    pub tag_vocab: Vec<String>,
}

impl Default for Brief {
    fn default() -> Self {
        Brief {
            job: "flash_cut_meme".into(),
            description: "flash-cut meme: keep punchy unique frames; \
                          drop shake, blur, duplicates, dead air."
                .into(),
            fps: 30.0,
            perceive_size: 224,
            min_gap_frames: 2,
            change_keep: 0.35,
            change_skip: 0.15,
            dup_cosine: 0.92,
            min_sharpness: 0.35,
            brightness_min: 0.05,
            brightness_max: 0.95,
            onset_weight: 0.4,
            snap_radius_frames: 3,
            max_candidates_to_jev: 60,
            jev_min_confidence: 0.55,
            gemini_cap: "gemini".into(),
            gemini_window_sec: 0.6,
            gemini_fps: 10.0,
            encoder: "dhash".into(),
            tag_vocab: DEFAULT_TAG_VOCAB.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl Brief {
    /// Load `.toml`, `.yaml` or `.yml` by extension — unknown extensions
    /// are tried as TOML first, then YAML.
    pub fn load(path: &Path) -> Result<Brief, MemeError> {
        let text = fs::read_to_string(path).map_err(MemeError::io(path))?;
        let parsed: Result<Brief, String> = match path.extension().and_then(|e| e.to_str()) {
            Some("yaml") | Some("yml") => serde_yaml::from_str(&text).map_err(|e| e.to_string()),
            Some("toml") => toml::from_str(&text).map_err(|e| e.to_string()),
            _ => toml::from_str(&text)
                .map_err(|e| e.to_string())
                .or_else(|_| serde_yaml::from_str(&text).map_err(|e| e.to_string())),
        };
        let brief = parsed.map_err(|e| MemeError::Brief(format!("{}: {e}", path.display())))?;
        brief.validate()?;
        Ok(brief)
    }

    /// Reject nonsense before it poisons a cache — a brief that can't
    /// work should fail loudly at load, not produce empty keeps at run.
    pub fn validate(&self) -> Result<(), MemeError> {
        let bad = |m: &str| Err(MemeError::Brief(m.to_string()));
        if !(1.0..=120.0).contains(&self.fps) {
            return bad("fps must be in 1..=120");
        }
        if self.perceive_size < 32 {
            return bad("perceive_size must be >= 32");
        }
        if self.min_gap_frames < 2 {
            return bad("min_gap_frames < 2 starves 2–6-frame flash beats");
        }
        for (name, v) in [
            ("change_keep", self.change_keep),
            ("change_skip", self.change_skip),
            ("dup_cosine", self.dup_cosine),
            ("min_sharpness", self.min_sharpness),
            ("brightness_min", self.brightness_min),
            ("brightness_max", self.brightness_max),
            ("onset_weight", self.onset_weight),
            ("jev_min_confidence", self.jev_min_confidence),
        ] {
            if !(0.0..=1.0).contains(&v) {
                return bad(&format!("{name} must be in 0..=1, got {v}"));
            }
        }
        if self.change_skip >= self.change_keep {
            return bad("change_skip must be below change_keep");
        }
        if self.brightness_min >= self.brightness_max {
            return bad("brightness_min must be below brightness_max");
        }
        if self.max_candidates_to_jev == 0 {
            return bad("max_candidates_to_jev must be >= 1");
        }
        if self.gemini_window_sec <= 0.0 || self.gemini_fps <= 0.0 {
            return bad("gemini_window_sec and gemini_fps must be positive");
        }
        if self.encoder.is_empty() {
            return bad("encoder must be non-empty (`dhash` or a capability name)");
        }
        Ok(())
    }

    /// sha256 over the canonical serialization — struct field order is
    /// fixed, so the hash is stable across runs and machines.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(serde_json::to_vec(self).expect("brief serializes"));
        format!("{:x}", h.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(text.as_bytes()).unwrap();
        p
    }

    #[test]
    fn defaults_validate() {
        Brief::default().validate().unwrap();
    }

    #[test]
    fn loads_both_extensions() {
        let dir = std::env::temp_dir().join(format!("meme-brief-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let t = write(&dir, "b.toml", "fps = 24\nmin_gap_frames = 3\n");
        let y = write(&dir, "b.yaml", "fps: 24\nmin_gap_frames: 3\n");
        let a = Brief::load(&t).unwrap();
        let b = Brief::load(&y).unwrap();
        assert_eq!(a.fps, 24.0);
        assert_eq!(a.min_gap_frames, 3);
        assert_eq!(a.hash(), b.hash());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_nonsense() {
        for (brief, want) in [
            (
                Brief {
                    fps: 0.0,
                    ..Default::default()
                },
                "fps",
            ),
            (
                Brief {
                    min_gap_frames: 1,
                    ..Default::default()
                },
                "min_gap_frames",
            ),
            (
                Brief {
                    change_keep: 1.5,
                    ..Default::default()
                },
                "change_keep",
            ),
            (
                Brief {
                    change_skip: 0.5,
                    change_keep: 0.4,
                    ..Default::default()
                },
                "change_skip",
            ),
            (
                Brief {
                    brightness_min: 0.9,
                    brightness_max: 0.5,
                    ..Default::default()
                },
                "brightness_min",
            ),
            (
                Brief {
                    jev_min_confidence: -0.1,
                    ..Default::default()
                },
                "jev_min_confidence",
            ),
        ] {
            let e = brief.validate().unwrap_err().to_string();
            assert!(e.contains(want), "{e} should mention {want}");
        }
    }

    #[test]
    fn hash_is_order_stable() {
        // The same brief written two ways hashes identically — cache
        // keys depend on it.
        let a = Brief::default();
        let b: Brief = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(a.hash(), b.hash());
        let c = Brief {
            fps: 15.0,
            ..Default::default()
        };
        assert_ne!(a.hash(), c.hash());
    }
}
