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
use std::process::{Command, Stdio};

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
    let doc = req.document().to_string();
    match &cap.connector {
        Connector::Subprocess { argv } => run_process(cap, argv, &doc, secret.as_deref(), req.out),
        Connector::Http { endpoint } => {
            let argv = http_args(endpoint, cap.auth.as_ref(), secret.as_deref(), req.out);
            run_process(cap, &argv, &doc, None, req.out)
        }
    }
}

/// curl argv for the HTTP connector — pure construction, testable.
/// Response body goes straight to `out`; `-f` turns non-2xx into a
/// failure we can read from stderr.
pub fn http_args(
    endpoint: &str,
    auth: Option<&AuthRef>,
    secret: Option<&str>,
    out: &Path,
) -> Vec<String> {
    let mut args = vec![
        "curl".to_string(),
        "-sfS".into(),
        "-X".into(),
        "POST".into(),
        "-H".into(),
        "content-type: application/json".into(),
    ];
    // Only *keychain-resolved* secrets travel in headers; env-var creds
    // would put the variable name in the header, not a secret.
    if let (Some(AuthRef::Keychain { .. }), Some(s)) = (auth, secret) {
        args.push("-H".into());
        args.push(format!("authorization: Bearer {s}"));
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

fn run_process(
    cap: &Capability,
    argv: &[String],
    stdin_json: &str,
    secret: Option<&str>,
    out: &Path,
) -> Result<PathBuf, CapError> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
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
    let mut child = cmd.spawn().map_err(|e| CapError::Spawn {
        tool: "connector",
        source: e,
    })?;
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(stdin_json.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(CapError::Failed {
            cap: cap.name.clone(),
            status: output.status.to_string(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
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
