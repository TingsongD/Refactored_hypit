//! Emit a draft `.scene` from an `Analysis`. The output is a starting
//! point for an author, not a finished edit: the clip covers the whole
//! program and each detected shot gets a labeled board to fill in.

use crate::Analysis;

/// Seconds-typed anchor literal, rounded to milliseconds — the same
/// `1.5s` grammar `AnchorRange::parse` accepts.
fn t(s: f64) -> String {
    format!("{:.3}s", (s * 1000.0).round() / 1000.0)
}

/// Minimal XML escaping for attribute values (`src` paths).
fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Build the draft markup. `src` is the path as it should appear in the
/// scene (typically relative to the project root).
pub fn emit_scene(src: &str, a: &Analysis) -> String {
    let (w, h) = (a.width.max(2), a.height.max(2));
    let dur = a.duration_s.max(0.1);
    let src = esc_attr(src);

    // Carry the source rate through so playback speed matches what was
    // analyzed; NTSC rates stay exact rationals. Unknown → 30.
    let fps = match a.fps {
        Some((n, d)) if n > 0 && d > 0 => format!("{n}/{d}"),
        _ => "30".to_string(),
    };

    let mut out = String::new();
    out.push_str(&format!(
        "<scene canvas=\"{w}x{h}\" fps=\"{fps}\" clear=\"#000\">\n"
    ));
    out.push_str("  <track kind=\"visual\">\n");
    out.push_str(&format!(
        "    <clip src=\"{src}\" during=\"0s..{}\"/>\n",
        t(dur)
    ));

    // One board per detected shot boundary — placeholders an author
    // replaces with real captions or cards.
    let mut bounds: Vec<f64> = a.cuts.clone();
    bounds.push(dur);
    let mut start = 0.0;
    for (i, &end) in bounds.iter().enumerate() {
        if end - start < 0.05 {
            start = end;
            continue;
        }
        out.push_str(&format!(
            "    <board during=\"{}..{}\" at=\"bottom\" anim=\"rise\">\n      <text>Shot {}</text>\n    </board>\n",
            t(start),
            t(end),
            i + 1
        ));
        start = end;
    }
    out.push_str("  </track>\n");

    if a.has_audio {
        out.push_str("  <track kind=\"audio\">\n");
        out.push_str(&format!(
            "    <music src=\"{src}\" during=\"0s..{}\"/>\n",
            t(dur)
        ));
        out.push_str("  </track>\n");
    }

    out.push_str("  <render target=\"out/adapted.mp4\"/>\n");
    out.push_str("</scene>\n");
    out
}
