//! Transactional outputs shared by rendering and capability connectors.

use std::io;
use std::path::{Path, PathBuf};

/// An exclusively created private directory beside the destination. External
/// tools may create/overwrite files inside it without touching existing assets.
/// Keep this guard alive until all child processes have closed their handles.
pub struct StagedOutput {
    directory: tempfile::TempDir,
    path: PathBuf,
    target: PathBuf,
}

impl StagedOutput {
    pub fn new(target: &Path) -> io::Result<Self> {
        let name = target.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "output needs a file name")
        })?;
        let parent = target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let parent = parent.canonicalize()?;
        let mut builder = tempfile::Builder::new();
        builder.prefix(".scene-output-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir_in(&parent)?;
        // Keep the chosen basename separate from auxiliary files: an asset
        // named credentials.curlrc must not overwrite the HTTP auth config.
        let payload = directory.path().join("payload");
        std::fs::create_dir(&payload)?;
        let path = payload.join(name);
        Ok(Self {
            directory,
            path,
            target: parent.join(name),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Additional transient files (audio mix, connector credentials) belong
    /// under this same protected directory.
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }

    pub fn validate(&self) -> io::Result<()> {
        let metadata = std::fs::symlink_metadata(&self.path)?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged output must be a nonempty regular file",
            ));
        }
        Ok(())
    }

    /// One replacement operation, on the same filesystem. Rust's rename uses
    /// replacement semantics on Windows too. Any error leaves the destination
    /// alone; in particular, never unlink it and retry.
    pub fn publish(&self) -> io::Result<()> {
        self.validate()?;
        std::fs::rename(&self.path, &self.target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_empty_output_preserves_previous_asset() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("final.mp4");
        std::fs::write(&target, b"old-good").unwrap();
        let stage = StagedOutput::new(&target).unwrap();
        assert!(stage.publish().is_err());
        std::fs::write(stage.path(), b"").unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old-good");
    }

    #[test]
    fn publishes_and_cleans_private_staging() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("final.mp4");
        std::fs::write(&target, b"old-good").unwrap();
        let stage = StagedOutput::new(&target).unwrap();
        let second = StagedOutput::new(&target).unwrap();
        assert_ne!(stage.directory(), second.directory());
        assert_eq!(stage.path().extension().unwrap(), "mp4");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(stage.directory())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        std::fs::write(stage.path(), b"new-good").unwrap();
        stage.publish().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-good");
        let staging_dir = stage.directory().to_path_buf();
        drop(stage);
        assert!(!staging_dir.exists());
    }

    #[test]
    fn failed_replacement_preserves_destination_and_staged_result() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("occupied");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), b"old-good").unwrap();
        let stage = StagedOutput::new(&target).unwrap();
        std::fs::write(stage.path(), b"new-good").unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(std::fs::read(target.join("keep")).unwrap(), b"old-good");
        assert_eq!(std::fs::read(stage.path()).unwrap(), b"new-good");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_as_completed_output() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("asset.wav");
        let victim = root.path().join("other.wav");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&victim, b"keep").unwrap();
        let stage = StagedOutput::new(&target).unwrap();
        std::os::unix::fs::symlink(&victim, stage.path()).unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    }
}
