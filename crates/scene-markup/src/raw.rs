//! Raw parse tree — the syntax-level view of a `.scene` document before
//! lowering validates it into typed IR.

use scene_ir::Span;

#[derive(Debug, Clone)]
pub struct RawAttr {
    pub name: String,
    pub name_span: Span,
    pub value: String,
    pub value_span: Span,
}

#[derive(Debug, Clone)]
pub enum RawChild {
    Node(RawNode),
    Text { text: String, span: Span },
}

#[derive(Debug, Clone)]
pub struct RawNode {
    pub name: String,
    pub name_span: Span,
    pub attrs: Vec<RawAttr>,
    pub children: Vec<RawChild>,
    /// Span of the entire element, `<` through `>`.
    pub span: Span,
}

impl RawNode {
    pub fn attr(&self, name: &str) -> Option<&RawAttr> {
        self.attrs.iter().find(|a| a.name == name)
    }

    /// Concatenated non-whitespace text children, with the joined span.
    /// Nested elements are ignored — lowering decides whether text or
    /// children are legal in a given position.
    pub fn text_content(&self) -> Option<(String, Span)> {
        let mut text = String::new();
        let mut span: Option<Span> = None;
        for child in &self.children {
            if let RawChild::Text { text: t, span: s } = child {
                if t.trim().is_empty() {
                    continue;
                }
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(t.trim());
                span = Some(match span {
                    None => *s,
                    Some(prev) => prev.join(*s),
                });
            }
        }
        span.map(|s| (text, s))
    }

    /// Element children only.
    pub fn element_children(&self) -> impl Iterator<Item = &RawNode> {
        self.children.iter().filter_map(|c| match c {
            RawChild::Node(n) => Some(n),
            RawChild::Text { .. } => None,
        })
    }
}
