//! The canonical scene model. One `Scene` is the complete, validated
//! content of a `.scene` document after lowering.

use serde::Serialize;

use crate::{AnchorRange, AnchorRef, BindPath, Span};

/// Wire format identifier emitted alongside serialized scenes.
pub const IR_FORMAT: &str = "scene.ir@1";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Scene {
    pub canvas: Canvas,
    pub frame_rate: Rational,
    pub clear: Color,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<Script>,
    pub tracks: Vec<Track>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render: Option<RenderTarget>,
}

impl Scene {
    /// All input files the scene pulls in (`src` attributes), paired with
    /// the element span for diagnostics. The render target is an output
    /// and deliberately excluded.
    pub fn asset_refs(&self) -> Vec<(&str, Span)> {
        let mut refs = Vec::new();
        fn walk<'a>(elements: &'a [Element], refs: &mut Vec<(&'a str, Span)>) {
            for element in elements {
                match &element.kind {
                    ElementKind::Clip { src, .. }
                    | ElementKind::Image { src }
                    | ElementKind::Music { src, .. }
                    | ElementKind::Sound { src, .. }
                    | ElementKind::Program { src, .. } => refs.push((src.as_str(), element.span)),
                    _ => {}
                }
                walk(&element.children, refs);
            }
        }
        for track in &self.tracks {
            walk(&track.elements, &mut refs);
        }
        refs
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}

impl Canvas {
    /// `"1080x1920"`.
    pub fn parse(s: &str) -> Result<Canvas, String> {
        let (w, h) = s
            .split_once('x')
            .ok_or_else(|| format!("expected `WIDTHxHEIGHT`, found `{s}`"))?;
        let width: u32 = w
            .parse()
            .map_err(|_| format!("invalid canvas width `{w}`"))?;
        let height: u32 = h
            .parse()
            .map_err(|_| format!("invalid canvas height `{h}`"))?;
        if width == 0 || height == 0 {
            return Err("canvas dimensions must be positive".to_string());
        }
        Ok(Canvas { width, height })
    }
}

/// Frame rate as a rational so NTSC rates stay exact (`30000/1001`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rational {
    pub numerator: u32,
    pub denominator: u32,
}

impl Rational {
    /// `"30"` or `"30000/1001"`. Decimal rates are rejected in favor of
    /// the exact rational.
    pub fn parse(s: &str) -> Result<Rational, String> {
        if let Some((n, d)) = s.split_once('/') {
            let numerator: u32 = n
                .parse()
                .map_err(|_| format!("invalid rate numerator `{n}`"))?;
            let denominator: u32 = d
                .parse()
                .map_err(|_| format!("invalid rate denominator `{d}`"))?;
            if numerator == 0 || denominator == 0 {
                return Err("frame rate must be positive".to_string());
            }
            return Ok(Rational {
                numerator,
                denominator,
            });
        }
        if s.contains('.') {
            return Err(format!(
                "write `{s}` as a rational (e.g. `30000/1001` for 29.97)"
            ));
        }
        let numerator: u32 = s
            .parse()
            .map_err(|_| format!("invalid frame rate `{s}` (try `30` or `30000/1001`)"))?;
        if numerator == 0 {
            return Err("frame rate must be positive".to_string());
        }
        Ok(Rational {
            numerator,
            denominator: 1,
        })
    }

    pub fn to_f64(self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    pub const WHITE: Color = Color {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    pub const BLACK: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };

    /// `#rgb`, `#rrggbb` or `#rrggbbaa`.
    pub fn parse(s: &str) -> Result<Color, String> {
        let hex = s
            .strip_prefix('#')
            .ok_or_else(|| format!("expected `#rrggbb`, found `{s}`"))?;
        // Byte-slicing below is only safe on ASCII — reject anything
        // else up front (`#中` is 3 *bytes*, lands in the 3-arm, and
        // would slice mid-char).
        if !hex.is_ascii() {
            return Err(format!("invalid color `{s}` (hex digits only)"));
        }
        let byte = |pair: &str| -> Result<u8, String> {
            u8::from_str_radix(pair, 16).map_err(|_| format!("invalid color `{s}`"))
        };
        match hex.len() {
            3 => Ok(Color {
                r: byte(&hex[0..1].repeat(2))?,
                g: byte(&hex[1..2].repeat(2))?,
                b: byte(&hex[2..3].repeat(2))?,
                a: 255,
            }),
            6 => Ok(Color {
                r: byte(&hex[0..2])?,
                g: byte(&hex[2..4])?,
                b: byte(&hex[4..6])?,
                a: 255,
            }),
            8 => Ok(Color {
                r: byte(&hex[0..2])?,
                g: byte(&hex[2..4])?,
                b: byte(&hex[4..6])?,
                a: byte(&hex[6..8])?,
            }),
            _ => Err(format!(
                "invalid color `{s}` (use #rgb, #rrggbb or #rrggbbaa)"
            )),
        }
    }
}

/// The spoken content every anchor can resolve against. `track` names
/// the timing source (an audio track id) this script's voice lives on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Script {
    pub track: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    pub lines: Vec<ScriptLine>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScriptLine {
    pub id: String,
    pub text: String,
    pub span: Span,
}

/// Stable content hash for staleness checks — FNV-1a over the track name
/// plus every line's cue id and text. Timing documents written by
/// `engine align` carry it; a render compares it against the current
/// script, so editing words without re-aligning produces a diagnostic
/// instead of silently captioned stale words. Delimiters inside the hash
/// keep `["ab","c"]` and `["a","bc"]` distinct.
pub fn script_fingerprint(script: &Script) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    feed(script.track.as_bytes());
    for line in &script.lines {
        feed(b"\x00");
        feed(line.id.as_bytes());
        feed(b"\x01");
        feed(line.text.as_bytes());
    }
    format!("{h:016x}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TrackKind {
    Visual,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track {
    pub id: String,
    pub kind: TrackKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<AnchorRef>,
    pub elements: Vec<Element>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Element {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: ElementKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<AnchorRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anim: Option<AnimKind>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Element>,
    #[serde(skip)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ElementKind {
    /// Frame-sampled video footage. `from_s` is a source-time offset —
    /// the clip plays `src[from .. from + during]` rather than starting
    /// at the source's first frame.
    Clip {
        src: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        from_s: f64,
    },
    Image {
        src: String,
    },
    Text {
        content: TextContent,
    },
    /// A positioned group with its own children.
    Board,
    Captions {
        style: CaptionStyle,
        source: AnchorRef,
    },
    Music {
        src: String,
        gain_db: f64,
        duck: Option<String>,
        /// Source-time offset — plays `src[from .. from + during]`.
        #[serde(default, skip_serializing_if = "is_zero")]
        from_s: f64,
    },
    Sound {
        src: String,
        gain_db: f64,
        #[serde(default, skip_serializing_if = "is_zero")]
        from_s: f64,
    },
    /// A sandboxed authored program — JS that emits a DrawList per
    /// frame. `with` is an optional JSON payload passed to `setup`/`render`.
    Program {
        src: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        with: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TextContent {
    Literal(String),
    Bind(BindPath),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptionStyle {
    Karaoke,
    Block,
}

/// `at="center"` or `at="540,960"` in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Placement {
    Named(NamedPlacement),
    Point { x: f64, y: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NamedPlacement {
    Center,
    Top,
    Bottom,
    Left,
    Right,
}

impl Placement {
    pub fn parse(s: &str) -> Result<Placement, String> {
        match s {
            "center" => return Ok(Placement::Named(NamedPlacement::Center)),
            "top" => return Ok(Placement::Named(NamedPlacement::Top)),
            "bottom" => return Ok(Placement::Named(NamedPlacement::Bottom)),
            "left" => return Ok(Placement::Named(NamedPlacement::Left)),
            "right" => return Ok(Placement::Named(NamedPlacement::Right)),
            _ => {}
        }
        if let Some((x, y)) = s.split_once(',') {
            let x: f64 = x.trim().parse().map_err(|_| format!("invalid x `{x}`"))?;
            let y: f64 = y.trim().parse().map_err(|_| format!("invalid y `{y}`"))?;
            return Ok(Placement::Point { x, y });
        }
        Err(format!(
            "invalid placement `{s}` (use center, top, bottom, left, right or `x,y`)"
        ))
    }
}

/// Named entrance/presentation animations; timings derive from the
/// element's resolved span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnimKind {
    Rise,
    Fade,
    Pop,
}

impl AnimKind {
    pub fn parse(s: &str) -> Result<AnimKind, String> {
        match s {
            "rise" => Ok(AnimKind::Rise),
            "fade" => Ok(AnimKind::Fade),
            "pop" => Ok(AnimKind::Pop),
            other => Err(format!(
                "unknown animation `{other}` (known: rise, fade, pop)"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RenderTarget {
    pub target: String,
    #[serde(skip)]
    pub span: Span,
}

/// Serde `skip_serializing_if` helper: `from_s: 0.0` is the default and
/// stays out of the printed IR.
fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// `from="12.37s"` or `from="250ms"` — a source-time offset. Seconds
/// and milliseconds only: frame counts (`Nf`) are rejected because the
/// source's own frame rate isn't known at parse time.
pub fn parse_secs(s: &str) -> Result<f64, String> {
    let digits = s
        .strip_suffix("ms")
        .map(|d| (d, 1000.0))
        .or_else(|| s.strip_suffix('s').map(|d| (d, 1.0)));
    let Some((digits, per_sec)) = digits else {
        return Err(format!("invalid time `{s}` (use `12.37s` or `250ms`)"));
    };
    let v: f64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid time `{s}`"))?;
    let secs = v / per_sec;
    // Same guard as parse_gain_db — `nan`/`inf` must not reach seeks
    // or the audio graph, and a negative offset would play before the
    // source starts.
    if !secs.is_finite() || secs < 0.0 {
        return Err(format!("invalid time `{s}` (need a finite value >= 0)"));
    }
    Ok(secs)
}

/// `gain="-14dB"` or `gain="-14"`.
pub fn parse_gain_db(s: &str) -> Result<f64, String> {
    let digits = s.strip_suffix("dB").unwrap_or(s);
    let v: f64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid gain `{s}` (try `-14dB`)"))?;
    // `f64::parse` accepts `nan`/`inf`/`1e999` — they must not reach the
    // filtergraph as `volume=nan`.
    if !v.is_finite() {
        return Err(format!("invalid gain `{s}` (must be a finite number)"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_parse() {
        assert_eq!(
            Canvas::parse("1080x1920").unwrap(),
            Canvas {
                width: 1080,
                height: 1920
            }
        );
        assert!(Canvas::parse("1080").is_err());
        assert!(Canvas::parse("0x1920").is_err());
    }

    #[test]
    fn rational_parse() {
        assert_eq!(
            Rational::parse("30").unwrap(),
            Rational {
                numerator: 30,
                denominator: 1
            }
        );
        assert_eq!(
            Rational::parse("30000/1001").unwrap(),
            Rational {
                numerator: 30000,
                denominator: 1001
            }
        );
        assert!(Rational::parse("29.97").is_err());
        assert!(Rational::parse("0").is_err());
    }

    #[test]
    fn color_parse() {
        assert_eq!(
            Color::parse("#0e0e12").unwrap(),
            Color {
                r: 0x0e,
                g: 0x0e,
                b: 0x12,
                a: 255
            }
        );
        assert_eq!(Color::parse("#abc").unwrap().a, 255);
        assert_eq!(Color::parse("#11223344").unwrap().a, 0x44);
        assert!(Color::parse("red").is_err());
        // Non-ASCII hex is an error, not a byte-slice panic — `#中` is
        // 3 bytes and would land in the #rgb arm mid-char.
        assert!(Color::parse("#中").is_err());
        assert!(Color::parse("#ab中").is_err());
    }

    #[test]
    fn placement_parse() {
        assert_eq!(
            Placement::parse("center").unwrap(),
            Placement::Named(NamedPlacement::Center)
        );
        assert_eq!(
            Placement::parse("540,960").unwrap(),
            Placement::Point { x: 540.0, y: 960.0 }
        );
        assert!(Placement::parse("middle").is_err());
    }

    #[test]
    fn secs_parse() {
        assert_eq!(parse_secs("12.37s").unwrap(), 12.37);
        assert_eq!(parse_secs("250ms").unwrap(), 0.25);
        assert_eq!(parse_secs("0s").unwrap(), 0.0);
        // Frame counts are refused — the source's fps isn't known here.
        assert!(parse_secs("12f").is_err());
        assert!(parse_secs("abc").is_err());
        assert!(parse_secs("soon").is_err());
        // Bare numbers are ambiguous (seconds? frames?); require a unit.
        assert!(parse_secs("1.5").is_err());
        assert!(parse_secs("-1s").is_err());
        // Same trap as gains — nan/inf must not reach a seek.
        assert!(parse_secs("nans").is_err());
        assert!(parse_secs("1e999s").is_err());
    }

    #[test]
    fn gain_parse() {
        assert_eq!(parse_gain_db("-14dB").unwrap(), -14.0);
        assert_eq!(parse_gain_db("0").unwrap(), 0.0);
        assert!(parse_gain_db("loud").is_err());
        // f64::parse accepts these — they must never reach a filtergraph.
        assert!(parse_gain_db("nan").is_err());
        assert!(parse_gain_db("inf").is_err());
        assert!(parse_gain_db("1e999").is_err());
    }
}
