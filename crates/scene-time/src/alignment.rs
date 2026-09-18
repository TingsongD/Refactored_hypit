//! Alignment input: what a transcription/alignment connector hands the
//! realize pass. Per script line, the spoken words with their measured
//! times in seconds.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One spoken word with measured bounds in seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start_s: f64,
    pub end_s: f64,
}

/// A script line's measured performance: the words alignment assigned to
/// this cue, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignedLine {
    pub cue: String,
    pub words: Vec<Word>,
}

impl AlignedLine {
    pub fn start_s(&self) -> Option<f64> {
        self.words.first().map(|w| w.start_s)
    }

    pub fn end_s(&self) -> Option<f64> {
        self.words.last().map(|w| w.end_s)
    }
}

/// The measured timing of one timing source (e.g. the `voice` track).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TimingSource {
    pub lines: Vec<AlignedLine>,
}

impl TimingSource {
    pub fn line(&self, cue: &str) -> Option<&AlignedLine> {
        self.lines.iter().find(|l| l.cue == cue)
    }

    /// End of the last measured word across all lines.
    pub fn end_s(&self) -> f64 {
        self.lines
            .iter()
            .filter_map(AlignedLine::end_s)
            .fold(0.0, f64::max)
    }

    /// Sorted word-start positions plus the final word end — the lattice
    /// that `±Nw` offsets move along. Crossing cue boundaries is intended:
    /// `hook+2w` two words past a one-word line lands in the next line.
    pub fn word_boundaries(&self) -> Vec<f64> {
        let mut boundaries: Vec<f64> = Vec::new();
        for line in &self.lines {
            for word in &line.words {
                boundaries.push(word.start_s);
            }
        }
        if let Some(end) = self.lines.iter().filter_map(AlignedLine::end_s).next_back() {
            boundaries.push(end);
        }
        boundaries.sort_by(f64::total_cmp);
        boundaries.dedup();
        boundaries
    }
}

/// Timing sources keyed by source name — `voice`, a music track, whatever
/// a connector measured. Cue anchors always resolve against the scene's
/// script timing source. Serialized form doubles as the alignment
/// connector's output document (`engine render --timings file.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimingMap {
    pub sources: BTreeMap<String, TimingSource>,
}

impl TimingMap {
    pub fn insert(&mut self, source: impl Into<String>, timing: TimingSource) {
        self.sources.insert(source.into(), timing);
    }

    pub fn get(&self, source: &str) -> Option<&TimingSource> {
        self.sources.get(source)
    }
}
