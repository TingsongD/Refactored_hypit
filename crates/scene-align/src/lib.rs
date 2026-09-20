//! scene-align — alignment connectors: measured speech → `TimingSource`.
//!
//! Every connector answers the same question — "when was each word of
//! each script cue actually spoken?" — and produces the same output
//! shape, so `scene-time::realize` never cares which connector ran:
//!
//! - [`parse_markers`] + [`markers_to_timing`] — the markers file, a
//!   human-authorable `cue start_s end_s` format (hermetic, testable)
//! - [`whisperx_to_timing`] — WhisperX service JSON → real word times
//! - [`timing_map_for`] — wrap a source in the `--timings` JSON shape

mod markers;
mod whisperx;

pub use markers::{Marker, markers_to_timing, parse_markers};
pub use whisperx::{timing_map_for, whisperx_to_timing};

#[cfg(test)]
mod tests {
    use scene_ir::{Script, ScriptLine, Span};
    use scene_time::realize;

    use super::*;

    fn script() -> Script {
        Script {
            track: "voice".into(),
            voice: None,
            lines: vec![
                ScriptLine {
                    id: "hook".into(),
                    text: "Nobody talks about the third rule.".into(),
                    span: Span::new(0, 0),
                },
                ScriptLine {
                    id: "payoff".into(),
                    text: "Compound interest is a treadmill.".into(),
                    span: Span::new(0, 0),
                },
            ],
            span: Span::new(0, 0),
        }
    }

    #[test]
    fn markers_parse_clean_file() {
        let (markers, diags) = parse_markers("# a comment\n\nhook 0.0 1.5\n  payoff   1.6\t3.0 \n");
        assert!(diags.is_empty());
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].cue, "hook");
        assert_eq!((markers[0].start_s, markers[0].end_s), (0.0, 1.5));
        assert_eq!((markers[1].start_s, markers[1].end_s), (1.6, 3.0));
    }

    #[test]
    fn markers_report_bad_lines_without_dying() {
        let (markers, diags) =
            parse_markers("hook 0.0 1.5\nbad line\nhook2 abc def\nneg 2.0 1.0\nextra 0 1 z\n");
        assert_eq!(markers.len(), 1, "only the good line survives");
        assert_eq!(diags.len(), 4);
        assert!(diags.iter().all(|d| d.is_error()));
    }

    #[test]
    fn markers_reject_nonfinite_times() {
        // `NaN`/`inf` parse as f64 but serialize to `null` — a timings
        // file the render side can't read back. Reject at parse time.
        let (markers, diags) = parse_markers("hook 0 NaN\npayoff 0 inf\nok 0.0 1.0\n");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].cue, "ok");
        assert_eq!(diags.len(), 2);
        assert!(diags.iter().all(|d| d.is_error()));
    }

    #[test]
    fn markers_spread_words_evenly() {
        let (markers, _) = parse_markers("hook 0.0 1.2\npayoff 1.2 2.4\n");
        let (source, diags) = markers_to_timing(&markers, &script());
        assert!(diags.is_empty());
        let hook = source.line("hook").unwrap();
        assert_eq!(hook.words.len(), 6);
        // 1.2s / 6 words = 0.2s each
        assert!((hook.words[0].start_s - 0.0).abs() < 1e-9);
        assert!((hook.words[1].start_s - 0.2).abs() < 1e-9);
        assert!((hook.words[5].end_s - 1.2).abs() < 1e-9);
    }

    #[test]
    fn markers_unknown_and_missing_cues_warn() {
        let (markers, _) = parse_markers("hook 0.0 1.0\nghost 2.0 3.0\n");
        let (source, diags) = markers_to_timing(&markers, &script());
        assert_eq!(diags.len(), 2); // ghost unknown + payoff unmarked
        assert!(diags.iter().all(|d| !d.is_error()));
        assert_eq!(source.lines.len(), 1);
    }

    #[test]
    fn whisperx_uses_real_word_times() {
        let json = r#"{"segments":[
            {"words":[
                {"word":"Nobody","start":0.01,"end":0.31},
                {"word":"talks","start":0.32,"end":0.55},
                {"word":"about","start":0.56,"end":0.80},
                {"word":"the","start":0.81,"end":0.90},
                {"word":"third","start":0.91,"end":1.10},
                {"word":"rule.","start":1.11,"end":1.45}
            ]},
            {"words":[
                {"word":"Compound","start":1.60,"end":1.90},
                {"word":"interest","start":1.91,"end":2.30},
                {"word":"is","start":2.31,"end":2.40},
                {"word":"a","start":2.41,"end":2.45},
                {"word":"treadmill.","start":2.46,"end":2.95}
            ]}
        ]}"#;
        let (source, diags) = whisperx_to_timing(json, &script()).unwrap();
        assert!(diags.is_empty());
        let payoff = source.line("payoff").unwrap();
        assert_eq!(payoff.words.len(), 5);
        assert!((payoff.words[0].start_s - 1.60).abs() < 1e-9);
        assert!((source.end_s() - 2.95).abs() < 1e-9);
    }

    #[test]
    fn whisperx_segment_mismatch_and_untimed_words_warn() {
        let json = r#"{"segments":[
            {"words":[{"word":"hi","start":0.0,"end":0.2}]},
            {"words":[{"word":"dangling"}]}
        ]}"#;
        let (source, diags) = whisperx_to_timing(json, &script()).unwrap();
        // payoff got a segment but zero timed words → warning
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("payoff"));
        assert_eq!(source.line("hook").unwrap().words.len(), 1);
        assert!(source.line("payoff").unwrap().words.is_empty());
    }

    #[test]
    fn whisperx_invalid_word_times_warn_but_keep_the_words() {
        // Negative and reversed spans parse fine from JSON — they must
        // surface as warnings, not ride silently into the lattice.
        let json = r#"{"segments":[
            {"words":[
                {"word":"ok","start":0.0,"end":0.2},
                {"word":"neg","start":-0.5,"end":0.1},
                {"word":"rev","start":0.9,"end":0.4}
            ]},
            {"words":[{"word":"clean","start":1.0,"end":1.5}]}
        ]}"#;
        let (source, diags) = whisperx_to_timing(json, &script()).unwrap();
        assert_eq!(source.line("hook").unwrap().words.len(), 3);
        assert!(
            diags.iter().any(|d| !d.is_error()
                && d.message.contains("hook")
                && d.message.contains("invalid times")),
            "{diags:?}"
        );
    }

    /// The M7 gate's whole point: connector output drives `realize`
    /// exactly like authored timing data — same lattice, same frames.
    #[test]
    fn connector_output_realizes_anchors() {
        let scene_src = r#"
<scene canvas="320x180" fps="30">
  <script track="voice">
    <line id="hook">Nobody talks about the third rule.</line>
    <line id="payoff">Compound interest is a treadmill.</line>
  </script>
  <track id="voice" kind="audio"></track>
  <track kind="visual">
    <board at="center" during="hook..payoff"><text>card</text></board>
    <board at="center" during="payoff+2w"><text>shifted</text></board>
  </track>
</scene>"#;
        let outcome = scene_markup::compile(scene_src);
        assert!(outcome.diagnostics.iter().all(|d| !d.is_error()));
        let scene = outcome.scene.expect("compiles");
        let script = scene.script.as_ref().unwrap();

        let (markers, m_diags) = parse_markers("hook 0.0 1.5\npayoff 1.6 3.0\n");
        assert!(m_diags.is_empty());
        let (source, t_diags) = markers_to_timing(&markers, script);
        assert!(t_diags.is_empty());
        let timings = timing_map_for(script, source);

        let (resolved, r_diags) = realize(&scene, &timings);
        assert!(r_diags.iter().all(|d| !d.is_error()));
        let resolved = resolved.unwrap();

        let boards = &resolved.tracks[1].elements; // tracks[0] is `voice`
        // hook..payoff covers 0.0..3.0s → frames 0..90 @30fps
        assert_eq!(
            (boards[0].timing.frames.start, boards[0].timing.frames.end),
            (0, 90)
        );
        // payoff+2w: payoff's 5 words spread 1.6..3.0 → step 0.28s.
        // +2w on start = 1.6+0.56 = 2.16s → frame 64 (floor). The end
        // edge shifts +2w too but 3.0s is the last lattice boundary —
        // word offsets clamp at the measured stream's end → stays 90.
        assert_eq!(boards[1].timing.frames.start, 64);
        assert_eq!(boards[1].timing.frames.end, 90);
    }

    #[test]
    fn timing_map_stamps_the_script_fingerprint() {
        // The render side detects "aligned to different words" via this
        // hash — it must survive a serialize/deserialize round trip.
        let s = script();
        let (markers, _) = parse_markers("hook 0.0 1.5\npayoff 1.6 3.0\n");
        let (source, _) = markers_to_timing(&markers, &s);
        let map = timing_map_for(&s, source);
        assert_eq!(
            map.script_hash.as_deref(),
            Some(scene_ir::script_fingerprint(&s).as_str())
        );
        let json = serde_json::to_string(&map).unwrap();
        let back: scene_time::TimingMap = serde_json::from_str(&json).unwrap();
        assert_eq!(back.script_hash, map.script_hash);
        // …and a timing doc without the field still loads (legacy).
        let legacy: scene_time::TimingMap = serde_json::from_str(r#"{"sources":{}}"#).unwrap();
        assert!(legacy.script_hash.is_none());
    }
}
