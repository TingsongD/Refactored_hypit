//! Fulfilling a capability request: one JSON document in, one asset
//! file out.
//!
//! The wire contract, both connector shapes:
//!   request:  {"capability":"tts","params":{...},"out":"/abs/path"}
//!   success:  exit 0 / HTTP 2xx, and `out` exists and is non-empty
//!   failure:  nonzero / non-2xx; stderr (or body) is the error message
//!
//! Subprocess: argv spawns, request goes to stdin.
//! HTTP: `curl` POSTs the request; the response body *is* the asset.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::auth::{self, AUTH_ENV, AuthError};
use crate::registry::{AuthRef, Capability, Connector, Registry};

#[derive(Debug, thiserror::Error)]
pub enum CapError {
    #[error("unknown capability `{0}`")]
    Unknown(String),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("could not run {tool}: {source}")]
    Spawn {
        tool: &'static str,
        source: std::io::Error,
    },
    #[error("capability `{cap}` failed ({status}): {detail}")]
    Failed {
        cap: String,
        status: String,
        detail: String,
    },
    #[error("capability `{cap}` succeeded but produced no output at {path}")]
    NoOutput { cap: String, path: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One request against a registered capability.
pub struct CapRequest<'a> {
    pub capability: &'a str,
    pub params: Value,
    /// Where the asset must land — absolute or caller-relative.
    pub out: &'a Path,
}

impl CapRequest<'_> {
    /// The wire document — what stdin / the POST body carries.
    pub fn document(&self) -> Value {
        json!({
            "capability": self.capability,
            "params": self.params,
            "out": self.out.display().to_string(),
        })
    }
}

/// Run the request to completion. On success `out` exists with bytes.
pub fn fulfill(reg: &Registry, req: &CapRequest) -> Result<PathBuf, CapError> {
    let cap = reg
        .get(req.capability)
        .ok_or_else(|| CapError::Unknown(req.capability.to_string()))?;
    let secret = match &cap.auth {
        Some(auth) => Some(auth::resolve(auth)?),
        None => None,
    };
    let staged = scene_media::StagedOutput::new(req.out)?;
    let mut document = req.document();
    document["out"] = json!(staged.path().display().to_string());
    let doc = document.to_string();
    match &cap.connector {
        Connector::Subprocess { argv } => {
            run_process(cap, argv, &doc, secret.as_deref(), staged.path())
        }
        Connector::Http { endpoint } => {
            // The bearer header travels in a `-K` config file — argv is
            // visible in `ps`, so a literal `authorization:` argument
            // would leak the credential to any local process inspector.
            let _guard;
            let config = match secret.as_deref() {
                Some(s) => {
                    let path = staged.directory().join("credentials.curlrc");
                    write_secret_file(&path, &curl_config(s))?;
                    _guard = CurlRc(path.clone());
                    Some(path)
                }
                None => None,
            };
            let argv = http_args(endpoint, config.as_deref(), staged.path());
            run_process(cap, &argv, &doc, None, staged.path())
        }
    }?;
    staged.publish()?;
    Ok(req.out.to_path_buf())
}

/// curl argv for the HTTP connector — pure construction, testable.
/// Response body goes straight to `out`; `-f` turns non-2xx into a
/// failure we can read from stderr. `config` is the `-K` file carrying
/// the auth header — its *path* is safe for argv; its contents are not.
/// `--max-time` makes curl itself abort a stalled transfer rather than
/// relying solely on our process-level timeout to notice.
pub fn http_args(endpoint: &str, config: Option<&Path>, out: &Path) -> Vec<String> {
    let mut args = vec![
        "curl".to_string(),
        "-sfS".into(),
        "--max-time".into(),
        "300".into(),
        "-X".into(),
        "POST".into(),
        "-H".into(),
        "content-type: application/json".into(),
    ];
    if let Some(cfg) = config {
        args.push("-K".into());
        args.push(cfg.display().to_string());
    }
    for a in [
        "--data-binary",
        "@-",
        "-o",
        &out.display().to_string(),
        endpoint,
    ] {
        args.push(a.to_string());
    }
    args
}

/// The `-K` config body carrying the bearer header. Quotes/backslashes
/// are escaped so an unusual secret can't break the config line.
pub fn curl_config(secret: &str) -> String {
    let s = secret.replace('\\', "\\\\").replace('"', "\\\"");
    format!("header = \"authorization: Bearer {s}\"\n")
}

/// Write a file that must never be world-readable — `create_new` refuses
/// to follow a planted symlink, 0600 on unix.
fn write_secret_file(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(contents.as_bytes())
}

/// Removes the curl config when the request ends — the bearer must not
/// linger on disk past the child process's lifetime.
struct CurlRc(PathBuf);
impl Drop for CurlRc {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A connector is a network call — a wedged subprocess or stalled curl
/// must not hang the engine forever. Ten minutes is a hang, not a slow
/// render step (curl self-aborts at five via `--max-time`).
const CONNECTOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

fn run_process(
    cap: &Capability,
    argv: &[String],
    stdin_json: &str,
    secret: Option<&str>,
    out: &Path,
) -> Result<PathBuf, CapError> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(s) = secret {
        cmd.env(AUTH_ENV, s);
    }
    // Env credentials forward the *variable itself* under the contract
    // name — the connector always reads SCENE_CAP_AUTH.
    if let Some(AuthRef::Env(var)) = &cap.auth
        && let Ok(v) = std::env::var(var)
    {
        cmd.env(AUTH_ENV, v);
    }
    let output = scene_media::run_with_input_timeout(
        &mut cmd,
        "connector",
        CONNECTOR_TIMEOUT,
        stdin_json.as_bytes(),
    )
    .map_err(|e| match e {
        scene_media::MediaError::Io(io) => CapError::Io(io),
        scene_media::MediaError::Spawn { tool, source } => CapError::Spawn { tool, source },
        other => CapError::Failed {
            cap: cap.name.clone(),
            status: "timeout".to_string(),
            detail: other.to_string(),
        },
    })?;
    let status = output.status;
    let stderr_bytes = output.stderr;
    if !status.success() {
        return Err(CapError::Failed {
            cap: cap.name.clone(),
            status: status.to_string(),
            detail: String::from_utf8_lossy(&stderr_bytes).trim().to_string(),
        });
    }
    if !out.is_file() || std::fs::metadata(out).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err(CapError::NoOutput {
            cap: cap.name.clone(),
            path: out.display().to_string(),
        });
    }
    Ok(out.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_never_carries_the_secret() {
        let cfg = Path::new("/tmp/x.curlrc");
        let args = http_args(
            "https://api.example/tts",
            Some(cfg),
            Path::new("/tmp/o.bin"),
        );
        assert!(
            !args.iter().any(|a| a.contains("s3cr3t")),
            "no argv element may contain a credential"
        );
        let k = args.iter().position(|a| a == "-K").expect("config flag");
        assert_eq!(args[k + 1], "/tmp/x.curlrc");
    }

    #[test]
    fn no_auth_means_no_config() {
        let args = http_args("https://api.example/tts", None, Path::new("/tmp/o.bin"));
        assert!(!args.iter().any(|a| a == "-K"));
    }

    #[test]
    fn config_holds_the_header() {
        let c = curl_config("tok-123");
        assert_eq!(c, "header = \"authorization: Bearer tok-123\"\n");
        // Escaping keeps a quote-bearing secret inside the line.
        let c = curl_config("a\"b");
        assert!(c.contains("a\\\"b"));
    }

    #[test]
    fn secret_file_is_private() {
        let path = std::env::temp_dir().join(format!("curlrc-{}", std::process::id()));
        write_secret_file(&path, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // create_new refuses a clobber — a planted file isn't followed.
        assert!(write_secret_file(&path, "y").is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
