//! Error type for the flash-cut pipeline. One enum, `stage`-prefixed
//! messages — the CLI prints them verbatim.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum MemeError {
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),

    #[error("brief: {0}")]
    Brief(String),

    #[error("{0}")]
    Media(#[from] scene_media::MediaError),

    #[error("{0}")]
    Cap(#[from] scene_cap::CapError),

    #[error("{0}")]
    Stage(String),
}

impl MemeError {
    pub fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |e| MemeError::Io(path, e)
    }
}
