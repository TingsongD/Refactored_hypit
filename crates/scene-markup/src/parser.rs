//! `.scene` syntax: an XML-flavored document with exactly one root element.
//! The parser is deliberately dumb — it produces [`RawNode`] trees with
//! spans and leaves all vocabulary decisions to the lowering stage.

use scene_ir::{Diagnostic, Span};

use crate::raw::{RawAttr, RawChild, RawNode};

/// Deepest element nesting accepted. Real scenes nest a handful of
/// levels; the cap exists because every stage below the parser
/// (lowering, resolution, layout, raster) recurses over this depth —
/// uncapped input is a stack-overflow crash, not a diagnostic.
const MAX_DEPTH: usize = 128;

/// Parse a whole document; on syntax failure returns one fatal diagnostic.
/// Recovery past a syntax error is a known limitation — the first malformed
/// position is reported precisely rather than guessing at structure.
pub fn parse_document(src: &str) -> Result<RawNode, Diagnostic> {
    let mut p = Parser { src, pos: 0 };
    p.skip_misc()?;
    let node = p.parse_node(0)?;
    p.skip_misc()?;
    if !p.eof() {
        return Err(p.err_at(p.pos, "unexpected content after the root element"));
    }
    Ok(node)
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn starts_with(&self, s: &str) -> bool {
        self.rest().starts_with(s)
    }

    fn bump(&mut self, n: usize) {
        self.pos += n;
    }

    fn err_at(&self, pos: usize, message: impl Into<String>) -> Diagnostic {
        Diagnostic::error(message, Some(Span::point(pos)))
    }

    fn skip_ws(&mut self) {
        // Advance by the char's full width — a multibyte whitespace
        // (e.g. U+00A0) bumped as one byte leaves `pos` mid-char and the
        // next `rest()` slice panics.
        while let Some(c) = self.rest().chars().next()
            && c.is_whitespace()
        {
            self.bump(c.len_utf8());
        }
    }

    /// Whitespace, `<!-- comments -->` and `<? processing instructions ?>`.
    fn skip_misc(&mut self) -> Result<(), Diagnostic> {
        loop {
            self.skip_ws();
            if self.starts_with("<!--") {
                match self.src[self.pos + 4..].find("-->") {
                    Some(n) => self.bump(4 + n + 3),
                    None => return Err(self.err_at(self.pos, "unterminated comment")),
                }
            } else if self.starts_with("<?") {
                match self.src[self.pos + 2..].find("?>") {
                    Some(n) => self.bump(2 + n + 2),
                    None => {
                        return Err(self.err_at(self.pos, "unterminated processing instruction"));
                    }
                }
            } else {
                return Ok(());
            }
        }
    }

    fn parse_name(&mut self) -> Result<(String, Span), Diagnostic> {
        let start = self.pos;
        let mut end = start;
        for c in self.rest().chars() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                end += c.len_utf8();
            } else {
                break;
            }
        }
        if end == start {
            return Err(self.err_at(start, "expected a name"));
        }
        self.pos = end;
        Ok((self.src[start..end].to_string(), Span::new(start, end)))
    }

    fn parse_quoted(&mut self) -> Result<(String, Span), Diagnostic> {
        let quote = match self.rest().chars().next() {
            Some('"') => '"',
            Some('\'') => '\'',
            _ => return Err(self.err_at(self.pos, "expected a quoted value")),
        };
        let open = self.pos;
        self.bump(1);
        let value_start = self.pos;
        match self.rest().find(quote) {
            Some(n) => {
                let raw = &self.src[value_start..value_start + n];
                let value = decode_entities(raw, value_start)?;
                self.pos = value_start + n + 1;
                Ok((value, Span::new(value_start, value_start + n)))
            }
            None => Err(self.err_at(open, "unterminated quoted value")),
        }
    }

    fn parse_node(&mut self, depth: usize) -> Result<RawNode, Diagnostic> {
        let start = self.pos;
        if depth >= MAX_DEPTH {
            return Err(self.err_at(
                start,
                format!("document nested too deeply (max {MAX_DEPTH} levels)"),
            ));
        }
        if !self.starts_with("<") {
            return Err(self.err_at(start, "expected `<`"));
        }
        if self.starts_with("</") {
            return Err(self.err_at(start, "unexpected closing tag"));
        }
        self.bump(1);
        let (name, name_span) = self.parse_name()?;
        if !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            return Err(Diagnostic::error(
                format!("invalid element name `{name}`"),
                Some(name_span),
            ));
        }

        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            if self.starts_with("/>") {
                self.bump(2);
                return Ok(RawNode {
                    name,
                    name_span,
                    attrs,
                    children: Vec::new(),
                    span: Span::new(start, self.pos),
                });
            }
            if self.starts_with(">") {
                self.bump(1);
                break;
            }
            if self.eof() {
                return Err(self.err_at(start, format!("unterminated `<{name}>`")));
            }
            let (attr_name, attr_name_span) = self.parse_name()?;
            if attrs.iter().any(|a: &RawAttr| a.name == attr_name) {
                return Err(Diagnostic::error(
                    format!("duplicate attribute `{attr_name}`"),
                    Some(attr_name_span),
                ));
            }
            self.skip_ws();
            if !self.starts_with("=") {
                return Err(self.err_at(self.pos, format!("expected `=` after `{attr_name}`")));
            }
            self.bump(1);
            self.skip_ws();
            let (value, value_span) = self.parse_quoted()?;
            attrs.push(RawAttr {
                name: attr_name,
                name_span: attr_name_span,
                value,
                value_span,
            });
        }

        let mut children = Vec::new();
        loop {
            if self.eof() {
                return Err(self.err_at(start, format!("unterminated `<{name}>`")));
            }
            if self.starts_with("<!--") {
                self.skip_misc()?;
                continue;
            }
            if self.starts_with("</") {
                let close_start = self.pos;
                self.bump(2);
                let (close_name, close_span) = self.parse_name()?;
                self.skip_ws();
                if !self.starts_with(">") {
                    return Err(self.err_at(self.pos, "expected `>`"));
                }
                self.bump(1);
                if close_name != name {
                    return Err(Diagnostic::error(
                        format!("expected `</{name}>`, found `</{close_name}>`"),
                        Some(Span::new(close_start, close_span.end)),
                    ));
                }
                return Ok(RawNode {
                    name,
                    name_span,
                    attrs,
                    children,
                    span: Span::new(start, self.pos),
                });
            }
            if self.starts_with("<") {
                children.push(RawChild::Node(self.parse_node(depth + 1)?));
                continue;
            }
            // Text run up to the next `<`.
            let text_start = self.pos;
            let len = self.rest().find('<').unwrap_or(self.rest().len());
            let raw = &self.src[text_start..text_start + len];
            let text = decode_entities(raw, text_start)?;
            self.bump(len);
            children.push(RawChild::Text {
                text,
                span: Span::new(text_start, text_start + len),
            });
        }
    }
}

/// Decode the five XML entities. Unknown or bare `&` is an error pointing
/// at the ampersand.
fn decode_entities(raw: &str, base: usize) -> Result<String, Diagnostic> {
    if !raw.contains('&') {
        return Ok(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    let mut offset = 0usize;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let amp = &rest[i..];
        let (entity, replacement) = if amp.starts_with("&amp;") {
            ("&amp;", "&")
        } else if amp.starts_with("&lt;") {
            ("&lt;", "<")
        } else if amp.starts_with("&gt;") {
            ("&gt;", ">")
        } else if amp.starts_with("&quot;") {
            ("&quot;", "\"")
        } else if amp.starts_with("&apos;") {
            ("&apos;", "'")
        } else {
            return Err(Diagnostic::error(
                "unknown entity (supported: &amp; &lt; &gt; &quot; &apos;)",
                Some(Span::point(base + offset + i)),
            ));
        };
        out.push_str(replacement);
        rest = &amp[entity.len()..];
        offset += i + entity.len();
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Result<RawNode, Diagnostic> {
        parse_document(src)
    }

    #[test]
    fn simple_element() {
        let node = parse("<scene></scene>").unwrap();
        assert_eq!(node.name, "scene");
        assert_eq!(node.span, Span::new(0, 15));
    }

    #[test]
    fn self_closing_and_attrs() {
        let node = parse(r#"<scene canvas="1080x1920" fps="30"/>"#).unwrap();
        assert_eq!(node.attr("canvas").unwrap().value, "1080x1920");
        assert_eq!(node.attr("fps").unwrap().value, "30");
    }

    #[test]
    fn nested_and_text() {
        let node = parse("<a><b>hi</b> tail <c/></a>").unwrap();
        let kids: Vec<_> = node.element_children().collect();
        assert_eq!(kids.len(), 2);
        assert_eq!(node.text_content().unwrap().0, "tail");
    }

    #[test]
    fn multibyte_whitespace_does_not_corrupt_the_cursor() {
        // U+00A0 is whitespace but two bytes — bumping one byte leaves
        // the cursor mid-char and the next slice panics.
        let node = parse("<a\u{00A0}x=\"1\"\u{00A0}\u{2003}/>").unwrap();
        assert_eq!(node.attr("x").unwrap().value, "1");
        // Same between elements.
        let node = parse("<a>\u{00A0}<b/></a>").unwrap();
        assert_eq!(node.element_children().count(), 1);
    }

    #[test]
    fn comments_and_pis_are_skipped() {
        let node =
            parse("<?xml version=\"1.0\"?><!-- hi --><a><!-- in --></a><!-- tail -->").unwrap();
        assert_eq!(node.name, "a");
    }

    #[test]
    fn entities() {
        let node = parse("<a x=\"a &amp; b\">x &lt; y</a>").unwrap();
        assert_eq!(node.attr("x").unwrap().value, "a & b");
        assert_eq!(node.text_content().unwrap().0, "x < y");
    }

    #[test]
    fn errors() {
        assert!(parse("<a></b>").is_err());
        assert!(parse("<a>").is_err());
        assert!(parse("<a x=1/>").is_err());
        assert!(parse("<a x=\"1\" x=\"2\"/>").is_err());
        assert!(parse("<a>&foo;</a>").is_err());
        assert!(parse("<a/><b/>").is_err());
        assert!(parse("text <a/>").is_err());
    }

    #[test]
    fn error_spans_point_at_the_fault() {
        let err = parse("<a></b>").unwrap_err();
        let span = err.span.unwrap();
        // The span covers the mismatched `</b` close tag.
        assert_eq!(span.start, 3);
    }

    #[test]
    fn nesting_beyond_the_cap_is_a_diagnostic_not_a_crash() {
        let deep = format!(
            "{}{}",
            "<a>".repeat(MAX_DEPTH + 10),
            "</a>".repeat(MAX_DEPTH + 10)
        );
        let err = parse(&deep).unwrap_err();
        assert!(err.message.contains("nested too deeply"), "{err:?}");
        // At the cap the document is still legal.
        let ok = format!("{}{}", "<a>".repeat(MAX_DEPTH), "</a>".repeat(MAX_DEPTH));
        assert!(parse(&ok).is_ok());
    }
}
