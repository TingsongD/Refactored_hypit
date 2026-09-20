//! Project-root confinement for authored paths. `src` attributes and
//! `<render target>` are markup, not operator input — they may not reach
//! outside the scene file's directory.
//!
//! Two layers: lexical `.`/`..` normalization covers paths that don't
//! exist yet; canonicalizing the deepest existing ancestor catches a
//! symlink *inside* the root pointing out (`link -> /etc`,
//! `src="link/x.mp4"`).

use std::path::{Component::*, Path, PathBuf};

/// `rel` resolved under `root` landed outside it — carries enough
/// detail for callers to format their own message.
#[derive(Debug)]
pub struct Escapes {
    /// What the markup asked for.
    pub rel: PathBuf,
    /// Where it actually resolved.
    pub resolved: PathBuf,
    /// The (canonicalized) root it must stay under.
    pub root: PathBuf,
}

impl std::fmt::Display for Escapes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "path `{}` escapes the project root (must land under {})",
            self.rel.display(),
            self.root.display()
        )
    }
}

/// Collapse `.`/`..` lexically — no fs access, so a not-yet-created path
/// still normalizes. `Path::pop` on a root-only path is a no-op, so
/// `a/../../x` ends up at the filesystem root and fails the confine
/// check rather than silently clamping.
fn normalize_lexical(p: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve authored `rel` against `root`, refusing escapes. Absolute
/// `rel`s and `..` components that land outside the root fail, and so do
/// paths that reach outside through a symlink inside the root.
///
/// `root` need not be canonical itself — it's canonicalized on the way
/// in, falling back to the lexical spelling if that fails (the check
/// still runs; a comparison against a non-canonical root is still sound
/// for `..` escapes).
pub fn confine_under_root(root: &Path, rel: &Path) -> Result<PathBuf, Escapes> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root_abs = normalize_lexical(cwd.join(root));
    let root_real = std::fs::canonicalize(&root_abs).unwrap_or(root_abs);
    let resolved = normalize_lexical(root_real.join(rel));
    let escapes = |resolved: PathBuf| Escapes {
        rel: rel.to_path_buf(),
        resolved,
        root: root_real.clone(),
    };
    if !resolved.starts_with(&root_real) {
        return Err(escapes(resolved));
    }
    // The path may not exist yet — canonicalize the deepest ancestor
    // that does, reattach the tail, re-check.
    let mut probe = resolved.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let real = loop {
        if let Ok(canon) = std::fs::canonicalize(&probe) {
            break canon;
        }
        match probe.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                probe.pop();
            }
            None => break probe,
        }
    };
    let mut resolved = real;
    for part in tail.iter().rev() {
        resolved.push(part);
    }
    if !resolved.starts_with(&root_real) {
        return Err(escapes(resolved));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_stays_absolute_outside_fails() {
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let root = Path::new(".");
        assert_eq!(
            confine_under_root(root, Path::new("assets/a.png")).unwrap(),
            cwd.join("assets/a.png")
        );
        assert!(confine_under_root(root, Path::new("../x.png")).is_err());
        assert!(confine_under_root(root, Path::new("/tmp/x.png")).is_err());
        assert!(confine_under_root(root, Path::new("a/../../x")).is_err());
        // An absolute path inside the root is fine.
        let inside = cwd.join("sub").join("x.png");
        assert_eq!(confine_under_root(root, &inside).unwrap(), inside);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_root_pointing_out_fails() {
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let tmp = std::env::temp_dir().join(format!("confine-sym-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let link = cwd.join(format!("confine-link-{}", std::process::id()));
        std::os::unix::fs::symlink(&tmp, &link).unwrap();
        let t = format!("confine-link-{}/x.png", std::process::id());
        let result = confine_under_root(Path::new("."), Path::new(&t));
        // Clean up before asserting so a failure leaves no stray symlink.
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(result.is_err());
    }
}
