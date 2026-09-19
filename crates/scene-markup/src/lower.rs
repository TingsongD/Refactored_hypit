//! Lowering: RawNode → typed IR. Collects every diagnostic it can rather
//! than failing on the first — an agent fixing a scene should see all of
//! its problems in one pass.

use std::collections::HashSet;

use scene_ir::*;

use crate::raw::{RawAttr, RawNode};

const COMMON_ATTRS: &[&str] = &["id", "during", "at", "anim"];

/// Element-specific attributes plus the common set every element accepts.
fn with_common(extra: &[&'static str]) -> Vec<&'static str> {
    let mut all = Vec::with_capacity(extra.len() + COMMON_ATTRS.len());
    all.extend_from_slice(extra);
    all.extend_from_slice(COMMON_ATTRS);
    all
}

pub fn lower(root: &RawNode) -> (Option<Scene>, Vec<Diagnostic>) {
    let mut lower = Lower {
        diags: Vec::new(),
        cue_ids: HashSet::new(),
        track_ids: HashSet::new(),
        script_track: None,
        script_span: None,
    };
    let scene = lower.scene(root);
    let scene = if has_errors(&lower.diags) {
        None
    } else {
        scene
    };
    (scene, lower.diags)
}

struct Lower {
    diags: Vec<Diagnostic>,
    /// `<line id>` values seen in the script — anchors resolve against them.
    cue_ids: HashSet<String>,
    track_ids: HashSet<String>,
    /// `script track="..."` — the timing source anchors may name.
    script_track: Option<String>,
    /// Span of the `<script>` element, for cross-reference diagnostics.
    script_span: Option<Span>,
}

impl Lower {
    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(Diagnostic::error(message, Some(span)));
    }

    fn warning(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(Diagnostic::warning(message, Some(span)));
    }

    /// Required attribute; on absence emits an error and returns None.
    fn req_attr<'a>(&mut self, node: &'a RawNode, name: &str) -> Option<&'a RawAttr> {
        match node.attr(name) {
            Some(attr) => Some(attr),
            None => {
                self.error(node.name_span, format!("<{}> requires `{name}`", node.name));
                None
            }
        }
    }

    /// Required `src` plus a parent-dir advisory — a project is meant to
    /// be self-contained, so `../` paths that escape the root are worth
    /// flagging even though authored scenes are trusted. URLs pass.
    fn src_attr<'a>(&mut self, node: &'a RawNode) -> Option<&'a RawAttr> {
        let attr = self.req_attr(node, "src")?;
        if !attr.value.contains("://")
            && std::path::Path::new(&attr.value)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            self.warning(
                attr.value_span,
                "src path contains `..` — it may escape the project root".to_string(),
            );
        }
        Some(attr)
    }

    /// Warn on attributes outside `allowed` so the vocabulary stays honest
    /// but forward-compatible.
    fn check_attrs(&mut self, node: &RawNode, allowed: &[&str]) {
        for attr in &node.attrs {
            if !allowed.contains(&attr.name.as_str()) {
                self.warning(
                    attr.name_span,
                    format!("unknown attribute `{}` on <{}>", attr.name, node.name),
                );
            }
        }
    }

    fn no_text(&mut self, node: &RawNode) {
        if let Some((_, span)) = node.text_content() {
            self.error(span, format!("unexpected text inside <{}>", node.name));
        }
    }

    fn parse_attr<T, F>(&mut self, node: &RawNode, name: &str, parse: F) -> Option<T>
    where
        F: Fn(&str) -> Result<T, String>,
    {
        let attr = node.attr(name)?;
        match parse(&attr.value) {
            Ok(v) => Some(v),
            Err(msg) => {
                self.error(attr.value_span, msg);
                None
            }
        }
    }

    /// Required attribute that must also parse.
    fn req_parse<T, F>(&mut self, node: &RawNode, name: &str, parse: F) -> Option<T>
    where
        F: Fn(&str) -> Result<T, String>,
    {
        self.req_attr(node, name)?;
        self.parse_attr(node, name, parse)
    }

    fn scene(&mut self, root: &RawNode) -> Option<Scene> {
        if root.name != "scene" {
            self.error(
                root.name_span,
                format!("expected <scene>, found <{}>", root.name),
            );
            return None;
        }
        self.check_attrs(root, &["canvas", "fps", "clear"]);
        let canvas = self.req_parse(root, "canvas", Canvas::parse);
        let frame_rate = self.req_parse(root, "fps", Rational::parse);
        let clear = self
            .parse_attr(root, "clear", Color::parse)
            .unwrap_or(Color::TRANSPARENT);

        let mut script = None;
        let mut tracks = Vec::new();
        let mut render = None;
        self.no_text(root);
        // `script_track` must be known before any track lowers — a
        // `<captions>` without `anchor` defaults to it regardless of
        // whether <script> is declared above or below the track.
        for child in root.element_children() {
            if child.name == "script"
                && let Some(t) = child.attr("track")
            {
                self.script_track = Some(t.value.clone());
                break;
            }
        }
        for (index, child) in root.element_children().enumerate() {
            match child.name.as_str() {
                "script" => {
                    if script.is_some() {
                        self.error(child.name_span, "duplicate <script>");
                    } else {
                        script = self.script(child);
                    }
                }
                "track" => {
                    if let Some(track) = self.track(child, index) {
                        tracks.push(track);
                    }
                }
                "render" => {
                    if render.is_some() {
                        self.error(child.name_span, "duplicate <render>");
                    } else {
                        render = self.render_target(child);
                    }
                }
                other => self.error(
                    child.name_span,
                    format!("unexpected <{other}> (allowed: script, track, render)"),
                ),
            }
        }

        // Cross-checks now that the symbol tables are full.
        self.check_references(&tracks);

        if tracks.is_empty() {
            self.diags.push(Diagnostic::warning(
                "scene declares no tracks; it will render nothing".to_string(),
                Some(root.span),
            ));
        }

        Some(Scene {
            canvas: canvas?,
            frame_rate: frame_rate?,
            clear,
            script,
            tracks,
            render,
        })
    }

    fn script(&mut self, node: &RawNode) -> Option<Script> {
        self.check_attrs(node, &["track", "voice"]);
        self.script_span = Some(node.span);
        let track = self.req_attr(node, "track").map(|a| a.value.clone());
        if let Some(t) = &track {
            self.script_track = Some(t.clone());
        }
        let voice = node.attr("voice").map(|a| a.value.clone());

        let mut lines = Vec::new();
        for child in node.element_children() {
            if child.name != "line" {
                self.error(
                    child.name_span,
                    format!(
                        "unexpected <{}> inside <script> (only <line> is allowed)",
                        child.name
                    ),
                );
                continue;
            }
            self.check_attrs(child, &["id"]);
            let Some(id_attr) = self.req_attr(child, "id") else {
                continue;
            };
            let id = id_attr.value.clone();
            if !self.cue_ids.insert(id.clone()) {
                self.error(id_attr.value_span, format!("duplicate line id `{id}`"));
            }
            for nested in child.element_children() {
                self.error(
                    nested.name_span,
                    "<line> takes text only, not elements".to_string(),
                );
            }
            let (text, span) = match child.text_content() {
                Some((text, span)) => (text, span),
                None => {
                    self.error(child.name_span, format!("<line id=\"{id}\"> is empty"));
                    continue;
                }
            };
            lines.push(ScriptLine { id, text, span });
        }
        if lines.is_empty() {
            self.error(node.name_span, "<script> has no <line> children");
        }

        track.map(|track| Script {
            track,
            voice,
            lines,
            span: node.span,
        })
    }

    fn track(&mut self, node: &RawNode, index: usize) -> Option<Track> {
        self.check_attrs(node, &["kind", "id", "anchor"]);
        let kind = self.req_parse(node, "kind", |v| match v {
            "visual" => Ok(TrackKind::Visual),
            "audio" => Ok(TrackKind::Audio),
            other => Err(format!("unknown track kind `{other}` (visual|audio)")),
        })?;
        let id = node
            .attr("id")
            .map(|a| a.value.clone())
            .unwrap_or_else(|| format!("track{index}"));
        if !self.track_ids.insert(id.clone()) {
            self.error(node.name_span, format!("duplicate track id `{id}`"));
        }
        let anchor = self.parse_attr(node, "anchor", AnchorRef::parse);

        let mut elements = Vec::new();
        self.no_text(node);
        for child in node.element_children() {
            if let Some(element) = self.element(child) {
                elements.push(element);
            }
        }
        Some(Track {
            id,
            kind,
            anchor,
            elements,
            span: node.span,
        })
    }

    fn element(&mut self, node: &RawNode) -> Option<Element> {
        let kind = match node.name.as_str() {
            "clip" => {
                self.check_attrs(node, &with_common(&["src"]));
                self.src_attr(node).map(|a| ElementKind::Clip {
                    src: a.value.clone(),
                })
            }
            "image" => {
                self.check_attrs(node, &with_common(&["src"]));
                self.src_attr(node).map(|a| ElementKind::Image {
                    src: a.value.clone(),
                })
            }
            "text" => {
                self.check_attrs(node, &with_common(&["bind"]));
                let bind = self.parse_attr(node, "bind", BindPath::parse);
                let text = node.text_content();
                match (bind, text) {
                    (Some(path), _) => {
                        if let Some((_, span)) = node.text_content() {
                            self.warning(span, "text content is ignored when `bind` is set");
                        }
                        Some(ElementKind::Text {
                            content: TextContent::Bind(path),
                        })
                    }
                    (None, Some((text, _))) => Some(ElementKind::Text {
                        content: TextContent::Literal(text),
                    }),
                    (None, None) => {
                        self.error(node.name_span, "<text> needs content or a `bind` attribute");
                        None
                    }
                }
            }
            "board" => {
                self.check_attrs(node, COMMON_ATTRS);
                Some(ElementKind::Board)
            }
            "captions" => {
                self.check_attrs(node, &with_common(&["style", "anchor"]));
                let style = self
                    .parse_attr(node, "style", |v| match v {
                        "karaoke" => Ok(CaptionStyle::Karaoke),
                        "block" => Ok(CaptionStyle::Block),
                        other => Err(format!("unknown caption style `{other}` (karaoke|block)")),
                    })
                    .unwrap_or(CaptionStyle::Karaoke);
                let source = self.parse_attr(node, "anchor", AnchorRef::parse);
                match (source, &self.script_track) {
                    (Some(source), _) => Some(ElementKind::Captions { style, source }),
                    (None, Some(track)) => Some(ElementKind::Captions {
                        style,
                        source: AnchorRef {
                            source: track.clone(),
                            granularity: Granularity::Word,
                        },
                    }),
                    (None, None) => {
                        self.error(
                            node.name_span,
                            "<captions> needs `anchor` when the scene has no <script>",
                        );
                        None
                    }
                }
            }
            "music" | "sound" => {
                self.check_attrs(node, &with_common(&["src", "gain", "duck"]));
                let src = self.src_attr(node).map(|a| a.value.clone());
                let gain_db = self.parse_attr(node, "gain", parse_gain_db).unwrap_or(0.0);
                let duck = node.attr("duck").map(|a| a.value.clone());
                src.map(|src| {
                    if node.name == "music" {
                        ElementKind::Music { src, gain_db, duck }
                    } else {
                        if duck.is_some() {
                            self.warning(
                                node.attr("duck").unwrap().name_span,
                                "`duck` has no effect on <sound> (use <music>)",
                            );
                        }
                        ElementKind::Sound { src, gain_db }
                    }
                })
            }
            "program" => {
                self.check_attrs(node, &with_common(&["src", "with"]));
                let src = self.src_attr(node).map(|a| a.value.clone());
                let with = node.attr("with").map(|a| a.value.clone());
                if let Some(w) = &with
                    && !matches!(
                        serde_json::from_str::<serde_json::Value>(w),
                        Ok(serde_json::Value::Object(_))
                    )
                {
                    self.error(
                        node.attr("with").unwrap().name_span,
                        "`with` must be a JSON object literal",
                    );
                }
                src.map(|src| ElementKind::Program { src, with })
            }
            other => {
                self.error(
                    node.name_span,
                    format!(
                        "unknown element <{other}> (known: clip, image, text, board, captions, music, sound, program)"
                    ),
                );
                None
            }
        }?;

        // Common attributes on every element.
        let id = node.attr("id").map(|a| a.value.clone());
        let timing = self.parse_attr(node, "during", AnchorRange::parse);
        let placement = self.parse_attr(node, "at", Placement::parse);
        let anim = self.parse_attr(node, "anim", AnimKind::parse);

        let mut children = Vec::new();
        for child in node.element_children() {
            if let Some(element) = self.element(child) {
                children.push(element);
            }
        }
        if node.name != "board" && !children.is_empty() {
            self.error(
                node.name_span,
                format!("<{}> does not take child elements", node.name),
            );
            children.clear();
        }
        if node.name != "text" {
            self.no_text(node);
        }

        Some(Element {
            id,
            kind,
            timing,
            placement,
            anim,
            children,
            span: node.span,
        })
    }

    fn render_target(&mut self, node: &RawNode) -> Option<RenderTarget> {
        self.check_attrs(node, &["target"]);
        self.no_text(node);
        for child in node.element_children() {
            self.error(child.name_span, "<render> takes no elements");
        }
        self.req_attr(node, "target").map(|a| RenderTarget {
            target: a.value.clone(),
            span: node.span,
        })
    }

    /// Reference checks that need the full symbol tables. The tables are
    /// snapshot into locals so lookups can borrow them while `self` stays
    /// free to emit diagnostics.
    fn check_references(&mut self, tracks: &[Track]) {
        let script_track = self.script_track.clone();
        let script_span = self.script_span;
        let track_ids = std::mem::take(&mut self.track_ids);
        let cue_ids = std::mem::take(&mut self.cue_ids);

        // The script's timing source must be a real audio track.
        if let (Some(source), Some(span)) = (&script_track, script_span) {
            match tracks.iter().find(|t| &t.id == source) {
                Some(t) if t.kind != TrackKind::Audio => self.error(
                    span,
                    format!("script track `{source}` names a visual track"),
                ),
                None => self.error(
                    span,
                    format!("script track `{source}` has no matching <track id>"),
                ),
                _ => {}
            }
        }

        let valid_source =
            |source: &str| script_track.as_deref() == Some(source) || track_ids.contains(source);
        let mut known_cues: Vec<&str> = cue_ids.iter().map(String::as_str).collect();
        known_cues.sort_unstable();

        for track in tracks {
            if let Some(aref) = &track.anchor
                && !valid_source(&aref.source)
            {
                self.error(
                    track.span,
                    format!("unknown timing source `{}`", aref.source),
                );
            }
            let mut stack: Vec<&Element> = track.elements.iter().collect();
            while let Some(element) = stack.pop() {
                if let Some(timing) = &element.timing {
                    for anchor in [&timing.start, &timing.end] {
                        if let Anchor::Cue { cue, .. } = anchor
                            && !cue_ids.contains(cue)
                        {
                            self.error(
                                element.span,
                                format!("unknown cue `{cue}` (known: {})", known_cues.join(", ")),
                            );
                        }
                    }
                }
                match &element.kind {
                    ElementKind::Captions { source, .. } => {
                        if !valid_source(&source.source) {
                            self.error(
                                element.span,
                                format!("unknown timing source `{}`", source.source),
                            );
                        }
                    }
                    ElementKind::Text {
                        content: TextContent::Bind(path),
                    } => {
                        if !cue_ids.contains(&path.line) {
                            self.error(
                                element.span,
                                format!("bind target `{}` is not a script line", path.line),
                            );
                        }
                    }
                    ElementKind::Music {
                        duck: Some(target), ..
                    } => {
                        if !track_ids.contains(target) {
                            self.error(
                                element.span,
                                format!("duck target `{target}` is not a track"),
                            );
                        }
                    }
                    _ => {}
                }
                stack.extend(&element.children);
            }
        }

        self.track_ids = track_ids;
        self.cue_ids = cue_ids;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_document;

    fn lower_str(src: &str) -> (Option<Scene>, Vec<Diagnostic>) {
        let raw = parse_document(src).unwrap();
        lower(&raw)
    }

    fn errors(diags: &[Diagnostic]) -> Vec<&str> {
        diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.message.as_str())
            .collect()
    }

    const SCENE: &str = r##"<scene canvas="1080x1920" fps="30" clear="#000">
  <script track="voice">
    <line id="hook">Hello.</line>
    <line id="payoff">Goodbye.</line>
  </script>
  <track id="voice" kind="audio">
    <sound src="n.wav" during="hook..payoff"/>
  </track>
  <track kind="visual" anchor="voice">
    <clip src="a.mp4" during="hook..payoff"/>
    <board during="payoff" at="center" anim="rise">
      <text bind="payoff.text"/>
    </board>
    <captions anchor="voice.words"/>
  </track>
  <track kind="audio">
    <music src="bed.mp3" gain="-14dB" duck="voice" during="hook..payoff"/>
  </track>
  <render target="out/final.mp4"/>
</scene>"##;

    #[test]
    fn full_scene_lowers() {
        let (scene, diags) = lower_str(SCENE);
        assert!(errors(&diags).is_empty(), "errors: {:?}", errors(&diags));
        let scene = scene.unwrap();
        assert_eq!(scene.canvas.width, 1080);
        assert_eq!(scene.script.as_ref().unwrap().lines.len(), 2);
        assert_eq!(scene.tracks.len(), 3);
        let (srcs, _): (Vec<_>, Vec<_>) = scene.asset_refs().into_iter().unzip();
        assert!(srcs.contains(&"a.mp4"));
        // Inputs only — the render target is an output.
        assert!(!srcs.contains(&"out/final.mp4"));
    }

    #[test]
    fn missing_required_attrs() {
        let (_, diags) = lower_str("<scene></scene>");
        let errs = errors(&diags);
        assert!(errs.iter().any(|m| m.contains("`canvas`")), "{errs:?}");
        assert!(errs.iter().any(|m| m.contains("`fps`")), "{errs:?}");
    }

    #[test]
    fn unknown_cue_is_an_error() {
        let src = SCENE.replace("hook..payoff", "hook..missing");
        let (_, diags) = lower_str(&src);
        assert!(
            errors(&diags)
                .iter()
                .any(|m| m.contains("unknown cue `missing`")),
            "{:?}",
            errors(&diags)
        );
    }

    #[test]
    fn duplicate_line_ids() {
        let src = SCENE.replace("payoff", "hook");
        let (_, diags) = lower_str(&src);
        assert!(
            errors(&diags)
                .iter()
                .any(|m| m.contains("duplicate line id")),
            "{:?}",
            errors(&diags)
        );
    }

    #[test]
    fn script_track_must_be_an_audio_track() {
        let src = SCENE.replace(
            r#"<track id="voice" kind="audio">"#,
            r#"<track id="voice" kind="visual">"#,
        );
        let (_, diags) = lower_str(&src);
        assert!(
            errors(&diags)
                .iter()
                .any(|m| m.contains("names a visual track")),
            "{:?}",
            errors(&diags)
        );
    }

    #[test]
    fn unknown_element_is_an_error() {
        let src = SCENE.replace("<clip", "<florp");
        let (_, diags) = lower_str(&src);
        assert!(
            errors(&diags)
                .iter()
                .any(|m| m.contains("unknown element <florp>")),
            "{:?}",
            errors(&diags)
        );
    }

    #[test]
    fn unknown_attr_is_a_warning() {
        let src = SCENE.replace("<clip src", "<clip bogus=\"1\" src");
        let (_, diags) = lower_str(&src);
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Severity::Warning && d.message.contains("`bogus`")),
            "{diags:?}"
        );
    }

    #[test]
    fn program_requires_src() {
        let src = SCENE.replace(
            "<clip src=\"a.mp4\" during=\"hook..payoff\"/>",
            "<program/>",
        );
        let (_, diags) = lower_str(&src);
        assert!(errors(&diags).iter().any(|m| m.contains("src")));
    }

    #[test]
    fn captions_default_to_script_words() {
        let src = SCENE.replace(r#" anchor="voice.words""#, "");
        let (scene, diags) = lower_str(&src);
        assert!(errors(&diags).is_empty());
        let scene = scene.unwrap();
        let captions = scene.tracks[1]
            .elements
            .iter()
            .find_map(|e| match &e.kind {
                ElementKind::Captions { source, .. } => Some(source),
                _ => None,
            })
            .unwrap();
        assert_eq!(captions.source, "voice");
        assert_eq!(captions.granularity, Granularity::Word);
    }

    #[test]
    fn captions_default_works_when_script_comes_later() {
        // Declaration order must not matter: <captions> with no `anchor`
        // defaults to the script's track even when <script> sits *below*
        // the track in the document.
        let src = r##"<scene canvas="1080x1920" fps="30">
  <track kind="visual">
    <captions/>
  </track>
  <track id="voice" kind="audio">
    <sound src="n.wav" during="hook"/>
  </track>
  <script track="voice">
    <line id="hook">Hello.</line>
  </script>
</scene>"##;
        let (scene, diags) = lower_str(src);
        assert!(errors(&diags).is_empty(), "errors: {:?}", errors(&diags));
        let scene = scene.unwrap();
        let captions = scene.tracks[0]
            .elements
            .iter()
            .find_map(|e| match &e.kind {
                ElementKind::Captions { source, .. } => Some(source),
                _ => None,
            })
            .expect("captions element survived lowering");
        assert_eq!(captions.source, "voice");
        assert_eq!(captions.granularity, Granularity::Word);
    }

    #[test]
    fn text_requires_content_or_bind() {
        let src = SCENE.replace("<text bind=\"payoff.text\"/>", "<text/>");
        let (_, diags) = lower_str(&src);
        assert!(errors(&diags).iter().any(|m| m.contains("needs content")));
    }

    #[test]
    fn bind_must_hit_a_line() {
        let src = SCENE.replace("payoff.text", "missing.text");
        let (_, diags) = lower_str(&src);
        assert!(
            errors(&diags)
                .iter()
                .any(|m| m.contains("not a script line"))
        );
    }

    #[test]
    fn program_lowers_with_src_and_with() {
        let src = SCENE.replace(
            "<clip src=\"a.mp4\" during=\"hook..payoff\"/>",
            r#"<program src="fx.js" with='{"hue": 2}'/>"#,
        );
        let (scene, diags) = lower_str(&src);
        assert!(errors(&diags).is_empty());
        let scene = scene.unwrap();
        let (src, with) = scene.tracks[1]
            .elements
            .iter()
            .find_map(|e| match &e.kind {
                ElementKind::Program { src, with } => Some((src, with)),
                _ => None,
            })
            .unwrap();
        assert_eq!(src, "fx.js");
        assert_eq!(with.as_deref(), Some("{\"hue\": 2}"));
    }

    #[test]
    fn program_rejects_bad_with_json() {
        let src = SCENE.replace(
            "<clip src=\"a.mp4\" during=\"hook..payoff\"/>",
            r#"<program src="fx.js" with="nope"/>"#,
        );
        let (_, diags) = lower_str(&src);
        assert!(
            errors(&diags)
                .iter()
                .any(|m| m.contains("JSON object literal"))
        );
    }

    #[test]
    fn program_rejects_non_object_with() {
        for bad in ["5", "\"x\"", "[1,2]", "null"] {
            let src = SCENE.replace(
                "<clip src=\"a.mp4\" during=\"hook..payoff\"/>",
                &format!(r#"<program src="fx.js" with='{bad}'/>"#),
            );
            let (_, diags) = lower_str(&src);
            assert!(
                errors(&diags)
                    .iter()
                    .any(|m| m.contains("JSON object literal")),
                "with={bad} should error: {diags:?}"
            );
        }
        // An empty object is still an object.
        let ok = SCENE.replace(
            "<clip src=\"a.mp4\" during=\"hook..payoff\"/>",
            r#"<program src="fx.js" with='{}'/>"#,
        );
        let (_, diags) = lower_str(&ok);
        assert!(errors(&diags).is_empty());
    }
}
