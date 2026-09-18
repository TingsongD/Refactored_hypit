//! Resolve an adapt source to a local media file: pass paths through,
//! fetch URLs with `yt-dlp` (the same external-tool pattern the rest of
//! the engine uses — spawned, stderr-captured, status-checked).

use crate::AdaptError;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A local media file ready for [`crate::analyze`].
#[derive(Debug)]
pub struct Ingested {
    pub path: PathBuf,
    /// `true` when we fetched it — caller may want it inside the
    /// project's `assets/` rather than a temp dir.
    pub fetched: bool,
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Resolve `source`: an existing path passes through; a URL is fetched
/// into `out_dir` via `yt-dlp` (best-quality mp4-ish single file).
pub fn ingest(source: &str, out_dir: &Path) -> Result<Ingested, AdaptError> {
    if !looks_like_url(source) {
        let path = PathBuf::from(source);
        if !path.is_file() {
            return Err(AdaptError::Ingest(format!(
                "no such file (and not a URL): {source}"
            )));
        }
        return Ok(Ingested {
            path,
            fetched: false,
        });
    }

    std::fs::create_dir_all(out_dir)?;
    // yt-dlp picks the extension; `-o` is a template, `--print` tells us
    // the real filename it wrote.
    let template = out_dir.join("adapt-source.%(ext)s");
    let output = Command::new("yt-dlp")
        .args([
            "-f",
            "best[ext=mp4]/best",
            "--no-playlist",
            "-o",
            &template.to_string_lossy(),
            "--print",
            "after_move:filepath",
            source,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| AdaptError::Ingest(format!("could not run yt-dlp: {e}")))?;

    if !output.status.success() {
        return Err(AdaptError::Ingest(format!(
            "yt-dlp exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    let path = printed
        .lines()
        .last()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .ok_or_else(|| AdaptError::Ingest("yt-dlp succeeded but reported no output file".into()))?;
    Ok(Ingested {
        path,
        fetched: true,
    })
}
