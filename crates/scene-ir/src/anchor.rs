//! Symbolic time anchors.
//!
//! Elements never store seconds. They store references into the script —
//! `during="hook..payoff"`, `during="beat+2w"` — which the realize pass
//! resolves into frame positions once aligned audio exists. Literal
//! anchors (`during="1.5s..4s"`, `during="0f..90f"`) are the escape hatch.
//!
//! Grammar:
//! ```text
//! range      := anchor (".." anchor)?
//! anchor     := cue | literal
//! cue        := ident ("." edge)? offset?
//! edge       := "start" | "end"
//! offset     := ("+" | "-") number ("s" | "f" | "w")
//! literal    := number ("s" | "f")
//! ident      := [a-zA-Z] [a-zA-Z0-9_]* ("-" [a-zA-Z_] [a-zA-Z0-9_]*)*
//! ```
//! A hyphen inside an identifier must precede a non-digit; that keeps
//! `hook-6f` unambiguous (cue `hook`, offset `-6f`) while `my-cue`
//! still parses as one identifier.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Offset {
    /// Signed value; the leading sign is folded in.
    pub value: f64,
    pub unit: OffsetUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OffsetUnit {
    Seconds,
    Frames,
    /// Words relative to the cue's first word. `hook+2w` is two words in.
    Words,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Edge {
    Start,
    End,
}

/// One edge of a timing expression.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Anchor {
    /// Reference to a `<line id>` in the script.
    Cue {
        cue: String,
        edge: Edge,
        offset: Option<Offset>,
    },
    /// Explicit program position — the authored escape hatch.
    Literal { value: f64, unit: OffsetUnit },
}

/// `during="a..b"` — start edge of `a` through end edge of `b`.
/// `during="a"` — the whole span of `a`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnchorRange {
    pub start: Anchor,
    pub end: Anchor,
}

/// What a track or caption source anchors to: a named timing source
/// (`voice`) optionally at word granularity (`voice.words`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnchorRef {
    pub source: String,
    pub granularity: Granularity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Granularity {
    Line,
    Word,
}

/// `bind="payoff.text"` — pull a property off a script line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BindPath {
    pub line: String,
    pub property: BindProp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BindProp {
    Text,
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Length of the identifier starting at `s`, or 0 if none. A hyphen
/// counts only when the character after it is not a digit.
fn ident_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !is_ident_start(bytes[0] as char) {
        return 0;
    }
    let mut i = 1;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if !is_ident_char(c) {
            break;
        }
        if c == '-' && i + 1 < bytes.len() && (bytes[i + 1] as char).is_ascii_digit() {
            break;
        }
        i += 1;
    }
    i
}

/// Parse `number unit` at the start of `s`; returns (value, unit, consumed).
fn parse_number_unit(s: &str) -> Option<(f64, OffsetUnit, usize)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && ((bytes[i] as char).is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let value: f64 = s[..i].parse().ok()?;
    let (unit, ulen) = match s.as_bytes().get(i) {
        Some(b's') => (OffsetUnit::Seconds, 1),
        Some(b'f') => (OffsetUnit::Frames, 1),
        Some(b'w') => (OffsetUnit::Words, 1),
        _ => return None,
    };
    Some((value, unit, i + ulen))
}

impl Anchor {
    pub fn parse(s: &str) -> Result<Anchor, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("expected an anchor".to_string());
        }
        // Literal: "1.5s", "90f".
        if s.as_bytes()[0].is_ascii_digit() {
            return match parse_number_unit(s) {
                Some((value, unit, n)) if n == s.len() => match unit {
                    OffsetUnit::Words => Err("literal anchors use s or f, not w".to_string()),
                    _ => Ok(Anchor::Literal { value, unit }),
                },
                _ => Err(format!(
                    "invalid literal anchor `{s}` (try `1.5s` or `90f`)"
                )),
            };
        }
        let n = ident_len(s);
        if n == 0 {
            return Err(format!("expected a cue name, found `{s}`"));
        }
        let cue = &s[..n];
        let mut rest = &s[n..];
        let mut edge = Edge::Start;
        for suffix in [".start", ".end"] {
            if let Some(tail) = rest.strip_prefix(suffix) {
                edge = if suffix == ".end" {
                    Edge::End
                } else {
                    Edge::Start
                };
                rest = tail;
                break;
            }
        }
        let offset = if rest.is_empty() {
            None
        } else {
            let (sign, digits) = match rest.as_bytes()[0] {
                b'+' => (1.0, &rest[1..]),
                b'-' => (-1.0, &rest[1..]),
                _ => return Err(format!("unexpected `{rest}` after cue `{cue}`")),
            };
            match parse_number_unit(digits) {
                Some((value, unit, n)) if n == digits.len() => Some(Offset {
                    value: sign * value,
                    unit,
                }),
                _ => {
                    return Err(format!(
                        "invalid offset `{rest}` (try `+0.4s`, `-6f`, `+2w`)"
                    ));
                }
            }
        };
        Ok(Anchor::Cue {
            cue: cue.to_string(),
            edge,
            offset,
        })
    }
}

impl AnchorRange {
    /// `a..b` runs from the start edge of `a` to the end edge of `b`;
    /// a lone `a` is the whole span of `a`.
    pub fn parse(s: &str) -> Result<AnchorRange, String> {
        let s = s.trim();
        if let Some((left, right)) = s.split_once("..") {
            let mut start = Anchor::parse(left)?;
            let mut end = Anchor::parse(right)?;
            if let Anchor::Cue { edge, .. } = &mut start {
                *edge = Edge::Start;
            }
            if let Anchor::Cue { edge, .. } = &mut end {
                *edge = Edge::End;
            }
            Ok(AnchorRange { start, end })
        } else {
            let anchor = Anchor::parse(s)?;
            let end = match &anchor {
                Anchor::Cue { cue, offset, .. } => Anchor::Cue {
                    cue: cue.clone(),
                    edge: Edge::End,
                    offset: *offset,
                },
                literal => literal.clone(),
            };
            Ok(AnchorRange { start: anchor, end })
        }
    }
}

impl AnchorRef {
    /// `voice` or `voice.words`.
    pub fn parse(s: &str) -> Result<AnchorRef, String> {
        let s = s.trim();
        if let Some(base) = s.strip_suffix(".words") {
            let n = ident_len(base);
            if n == 0 || n != base.len() {
                return Err(format!("invalid anchor source `{s}`"));
            }
            return Ok(AnchorRef {
                source: base.to_string(),
                granularity: Granularity::Word,
            });
        }
        let n = ident_len(s);
        if n == 0 {
            return Err(format!("expected a timing source name, found `{s}`"));
        }
        if n != s.len() {
            return Err(format!(
                "unexpected `{}` (only `.words` is supported)",
                &s[n..]
            ));
        }
        Ok(AnchorRef {
            source: s.to_string(),
            granularity: Granularity::Line,
        })
    }
}

impl BindPath {
    /// `line.property` — currently only `.text`.
    pub fn parse(s: &str) -> Result<BindPath, String> {
        let s = s.trim();
        let (line, prop) = s
            .split_once('.')
            .ok_or_else(|| format!("expected `line.property`, found `{s}`"))?;
        let n = ident_len(line);
        if n == 0 || n != line.len() {
            return Err(format!("invalid line reference `{line}`"));
        }
        let property = match prop {
            "text" => BindProp::Text,
            other => return Err(format!("unsupported property `{other}` (supported: text)")),
        };
        Ok(BindPath {
            line: line.to_string(),
            property,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_cue() {
        let a = Anchor::parse("hook").unwrap();
        assert_eq!(
            a,
            Anchor::Cue {
                cue: "hook".into(),
                edge: Edge::Start,
                offset: None
            }
        );
    }

    #[test]
    fn cue_with_edge_and_offset() {
        assert_eq!(
            Anchor::parse("payoff.end-6f").unwrap(),
            Anchor::Cue {
                cue: "payoff".into(),
                edge: Edge::End,
                offset: Some(Offset {
                    value: -6.0,
                    unit: OffsetUnit::Frames
                })
            }
        );
    }

    #[test]
    fn word_offset() {
        assert_eq!(
            Anchor::parse("hook+2w").unwrap(),
            Anchor::Cue {
                cue: "hook".into(),
                edge: Edge::Start,
                offset: Some(Offset {
                    value: 2.0,
                    unit: OffsetUnit::Words
                })
            }
        );
    }

    #[test]
    fn hyphenated_cue_vs_negative_offset() {
        // `my-cue` is one identifier; `hook-6f` is cue + offset.
        assert_eq!(
            Anchor::parse("my-cue").unwrap(),
            Anchor::Cue {
                cue: "my-cue".into(),
                edge: Edge::Start,
                offset: None
            }
        );
        assert_eq!(
            Anchor::parse("hook-6f").unwrap(),
            Anchor::Cue {
                cue: "hook".into(),
                edge: Edge::Start,
                offset: Some(Offset {
                    value: -6.0,
                    unit: OffsetUnit::Frames
                })
            }
        );
    }

    #[test]
    fn literals() {
        assert_eq!(
            Anchor::parse("1.5s").unwrap(),
            Anchor::Literal {
                value: 1.5,
                unit: OffsetUnit::Seconds
            }
        );
        assert_eq!(
            Anchor::parse("90f").unwrap(),
            Anchor::Literal {
                value: 90.0,
                unit: OffsetUnit::Frames
            }
        );
        assert!(Anchor::parse("3w").is_err());
    }

    #[test]
    fn ranges() {
        let r = AnchorRange::parse("hook..payoff").unwrap();
        assert_eq!(
            r.start,
            Anchor::Cue {
                cue: "hook".into(),
                edge: Edge::Start,
                offset: None
            }
        );
        assert_eq!(
            r.end,
            Anchor::Cue {
                cue: "payoff".into(),
                edge: Edge::End,
                offset: None
            }
        );
    }

    #[test]
    fn single_cue_expands_to_span() {
        let r = AnchorRange::parse("beat").unwrap();
        assert_eq!(
            r.end,
            Anchor::Cue {
                cue: "beat".into(),
                edge: Edge::End,
                offset: None
            }
        );
    }

    #[test]
    fn anchor_refs() {
        assert_eq!(
            AnchorRef::parse("voice").unwrap(),
            AnchorRef {
                source: "voice".into(),
                granularity: Granularity::Line
            }
        );
        assert_eq!(
            AnchorRef::parse("voice.words").unwrap(),
            AnchorRef {
                source: "voice".into(),
                granularity: Granularity::Word
            }
        );
        assert!(AnchorRef::parse("voice.foo").is_err());
    }

    #[test]
    fn bind_paths() {
        assert_eq!(
            BindPath::parse("payoff.text").unwrap(),
            BindPath {
                line: "payoff".into(),
                property: BindProp::Text
            }
        );
        assert!(BindPath::parse("payoff").is_err());
        assert!(BindPath::parse("payoff.words").is_err());
    }
}
