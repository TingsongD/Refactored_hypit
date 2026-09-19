//! AudioGraph → ffmpeg `-filter_complex` + argv. Pure string building —
//! every decision (delay ms, trim window, compressor wiring, split
//! labels) is unit-testable without running ffmpeg.
//!
//! Graph shape per clip `i`:
//!   [i:a]atrim → asetpts → aresample → aformat → volume → afade(s)
//!         → adelay → [a{i}]  (+ asplit if it keys someone's duck)
//! Ducking uses real sidechain compression: the duck target's clips are
//! submixed into a key signal that drives `sidechaincompress` on the
//! ducked clip — no volume-expression approximation.
//! All chains merge in `amix=normalize=0` → `[aout]`.

use std::path::Path;

use crate::graph::{AudioGraph, DUCK_ATTACK_MS, DUCK_RATIO, DUCK_RELEASE_MS, DUCK_THRESHOLD};

/// 48 kHz program rate — the one clock the whole engine agrees on.
pub const PROGRAM_RATE: u64 = 48_000;

/// dB → linear gain (`-14 dB` → `0.199526…`). Volume filters take the
/// linear form so the emitted graph is unambiguous about semantics.
pub fn gain_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Program samples → adelay milliseconds (integer, floor — a clip lands
/// at-or-before its authored sample, never after).
pub fn samples_to_ms(samples: u64) -> u64 {
    samples * 1000 / PROGRAM_RATE
}

fn fmt(v: f64) -> String {
    format!("{v:.6}")
}

/// The `-filter_complex` value for the whole graph.
pub fn filter_complex(graph: &AudioGraph) -> String {
    let mut out = String::new();
    let mut chains: Vec<String> = Vec::with_capacity(graph.clips.len());

    // Which clips serve as duck keys — they need a split: one branch to
    // the main mix, one to the key submix.
    let mut is_key = vec![false; graph.clips.len()];
    for link in &graph.duck {
        for &k in &link.key {
            is_key[k] = true;
        }
    }

    for (i, clip) in graph.clips.iter().enumerate() {
        let len_s = (clip.target.end - clip.target.start) as f64 / PROGRAM_RATE as f64;
        let mut chain = format!(
            "[{i}:a]atrim=start={}:end={},asetpts=PTS-STARTPTS,aresample={PROGRAM_RATE},aformat=channel_layouts=stereo,volume={}",
            fmt(clip.src_start_s),
            fmt(clip.src_start_s + len_s),
            fmt(gain_linear(clip.gain_db)),
        );
        if clip.fade.in_s > 0.0 {
            chain += &format!(",afade=t=in:st=0:d={}", fmt(clip.fade.in_s));
        }
        if clip.fade.out_s > 0.0 {
            chain += &format!(
                ",afade=t=out:st={}:d={}",
                fmt((len_s - clip.fade.out_s).max(0.0)),
                fmt(clip.fade.out_s)
            );
        }
        let ms = samples_to_ms(clip.target.start);
        chain += &format!(",adelay={ms}|{ms}");
        if is_key[i] {
            chain += &format!(",asplit=2[a{i}m][a{i}k]");
        } else {
            chain += &format!("[a{i}]");
        }
        chains.push(chain);
    }

    // Duck links: key submix → sidechaincompress the ducked clip.
    let mut mix_label: Vec<String> = (0..graph.clips.len())
        .map(|i| {
            if is_key[i] {
                format!("a{i}m")
            } else {
                format!("a{i}")
            }
        })
        .collect();
    for (l, link) in graph.duck.iter().enumerate() {
        let key_label = format!("key{l}");
        let keys: String = link.key.iter().map(|k| format!("[a{k}k]")).collect();
        out += &format!(
            "{keys}amix=inputs={}:normalize=0[{key_label}];",
            link.key.len()
        );
        out += &format!(
            "[{}][{key_label}]sidechaincompress=threshold={}:ratio={}:attack={}:release={}[duck{l}];",
            mix_label[link.clip],
            fmt(DUCK_THRESHOLD),
            fmt(DUCK_RATIO),
            fmt(DUCK_ATTACK_MS),
            fmt(DUCK_RELEASE_MS),
        );
        mix_label[link.clip] = format!("duck{l}");
    }

    for chain in &chains {
        out += chain;
        out.push(';');
    }
    let inputs: String = mix_label.iter().map(|l| format!("[{l}]")).collect();
    // amix ends at its longest chain — a 0.5s sting in a 3s program
    // mixes 0.5s. `apad` extends with silence to the program length so
    // the muxer's `-shortest` can't truncate the video track; `-t` in
    // mix_args still cuts any overshoot to the program exactly.
    let program_s = graph.program_samples as f64 / PROGRAM_RATE as f64;
    out += &format!(
        "{inputs}amix=inputs={}:normalize=0,apad=whole_dur={}[aout]",
        mix_label.len(),
        fmt(program_s)
    );
    out
}

/// Full argv for `ffmpeg` rendering the graph to a 48 kHz stereo WAV.
/// Output length is cut to the program exactly.
pub fn mix_args(graph: &AudioGraph, out_wav: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec!["-y".into(), "-v".into(), "error".into()];
    for clip in &graph.clips {
        args.push("-i".into());
        args.push(clip.src.display().to_string());
    }
    args.push("-filter_complex".into());
    args.push(filter_complex(graph));
    let program_s = graph.program_samples as f64 / PROGRAM_RATE as f64;
    for a in [
        "-map",
        "[aout]",
        "-t",
        &fmt(program_s),
        "-ar",
        &PROGRAM_RATE.to_string(),
        "-ac",
        "2",
        "-c:a",
        "pcm_s16le",
        &out_wav.display().to_string(),
    ] {
        args.push(a.to_string());
    }
    args
}
