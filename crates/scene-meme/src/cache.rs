//! Stage cache — content-addressed JSON under `out/.cache/`. Every key
//! names its inputs, so a stale artifact can't be reused silently:
//! changing the video, encoder, brief, prompt, or timings derives a new
//! key and the downstream stage simply misses.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::error::MemeError;

/// Cache rooted at `{out}/.cache/`. Values are JSON files named
/// `{key}.json`; keys embed every input the stage depends on.
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(out_dir: &Path) -> Cache {
        Cache {
            dir: out_dir.join(".cache").join("v2"),
        }
    }

    /// Where a cache-named file lives — also the `out` path handed to
    /// connector requests so responses land in the cache directly.
    pub fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// sha256 of a file's bytes — the video's identity. Streams in 1 MiB
    /// blocks; a 100 MB clip hashes in well under a second.
    pub fn file_sha256(path: &Path) -> Result<String, MemeError> {
        let mut f = fs::File::open(path).map_err(MemeError::io(path))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).map_err(MemeError::io(path))?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(format!("{:x}", h.finalize()))
    }

    /// A stage key from its inputs: `{stage}-{sha256}` where the hash
    /// covers length-prefixed inputs so `["ab","c"]` and
    /// `["a","bc"]` can't collide.
    pub fn key(stage: &str, inputs: &[&str]) -> String {
        let mut h = Sha256::new();
        h.update(b"scene-cache-v2");
        for part in inputs {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        format!("{stage}-{:x}", h.finalize())
    }

    /// Read a cached value. A missing or corrupt entry is a miss, not
    /// an error — corruption just means the stage reruns.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let path = self.path(key);
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Write a stage output — temp file + rename so a crash mid-write
    /// can't leave a half-JSON that `get` would later misread.
    pub fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<(), MemeError> {
        let path = self.path(key);
        let text = serde_json::to_vec(value)
            .map_err(|e| MemeError::Stage(format!("cache serialize {key}: {e}")))?;
        write_atomic(&path, &text)
    }
}

/// Publish only a complete file; failed writes leave previous results intact.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), MemeError> {
    let staged = scene_media::StagedOutput::new(path).map_err(MemeError::io(path))?;
    fs::write(staged.path(), bytes).map_err(MemeError::io(path))?;
    staged.publish().map_err(MemeError::io(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meme-cache-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_names_all_inputs() {
        let a = Cache::key("peaks", &["vid", "enc", "brief"]);
        let b = Cache::key("peaks", &["vid", "enc", "brief"]);
        assert_eq!(a, b);
        assert!(a.starts_with("peaks-"));
        for inputs in [
            &["VID", "enc", "brief"][..],
            &["vid", "ENC", "brief"][..],
            &["vid", "enc", "BRIEF"][..],
            &["videnc", "brief"][..], // join-boundary safety
        ] {
            assert_ne!(a, Cache::key("peaks", inputs), "{inputs:?}");
        }
        // Different stage name, same inputs → different key.
        assert_ne!(a, Cache::key("route", &["vid", "enc", "brief"]));
    }

    #[test]
    fn get_put_roundtrip_and_miss() {
        let dir = tempdir("roundtrip");
        let cache = Cache::new(&dir);
        assert!(cache.get::<serde_json::Value>("nope").is_none());
        cache
            .put("keeps", &serde_json::json!({"ids": ["f1", "f2"]}))
            .unwrap();
        let v: serde_json::Value = cache.get("keeps").unwrap();
        assert_eq!(v["ids"][1], "f2");
        // Corrupt JSON reads as a miss, not a panic.
        fs::write(cache.path("bad"), "{not json").unwrap();
        assert!(cache.get::<serde_json::Value>("bad").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_hash_is_content_addressed() {
        let dir = tempdir("hash");
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        fs::write(&a, b"same bytes").unwrap();
        fs::write(&b, b"same bytes").unwrap();
        assert_eq!(
            Cache::file_sha256(&a).unwrap(),
            Cache::file_sha256(&b).unwrap()
        );
        fs::write(&b, b"different").unwrap();
        assert_ne!(
            Cache::file_sha256(&a).unwrap(),
            Cache::file_sha256(&b).unwrap()
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    #[test]
    fn failed_json_publication_preserves_existing_file_and_old_namespace() {
        let dir = std::env::temp_dir().join(format!("meme-publication-{}", std::process::id()));
        fs::create_dir_all(dir.join(".cache")).unwrap();
        let path = dir.join("report.json");
        fs::write(&path, b"old report").unwrap();
        assert!(write_atomic(&path, b"").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old report");
        let old = dir.join(".cache/entry.json");
        fs::write(&old, b"42").unwrap();
        assert!(Cache::new(&dir).get::<u32>("entry").is_none());
        assert!(old.exists());
        let _ = fs::remove_dir_all(dir);
    }
}
