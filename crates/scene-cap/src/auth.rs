//! Credential resolution. Secrets are never embedded in argv — an env
//! credential forwards *the variable* into the connector's environment;
//! a keychain credential is read through the OS's own tool at fulfill
//! time and passed as an env var to the connector anyway, so connector
//! authors handle one mechanism: `SCENE_CAP_AUTH`.

use std::process::Command;

use crate::registry::AuthRef;

/// The env var connectors read their credential from.
pub const AUTH_ENV: &str = "SCENE_CAP_AUTH";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("credential env var `{0}` is not set")]
    MissingEnv(String),
    #[error("keychain lookup failed for `{service}/{account}`: {detail}")]
    Keychain {
        service: String,
        account: String,
        detail: String,
    },
}

/// Resolve a credential to its secret value.
pub fn resolve(auth: &AuthRef) -> Result<String, AuthError> {
    resolve_with(auth, |n| std::env::var(n).ok(), keychain_lookup)
}

/// Injectable for tests — no env mutation, no real keychain.
pub fn resolve_with(
    auth: &AuthRef,
    env: impl Fn(&str) -> Option<String>,
    keychain: impl Fn(&str, &str) -> Result<String, AuthError>,
) -> Result<String, AuthError> {
    match auth {
        AuthRef::Env(var) => env(var).ok_or_else(|| AuthError::MissingEnv(var.clone())),
        AuthRef::Keychain { service, account } => keychain(service, account),
    }
}

/// The OS keychain read, as argv — pure construction, unit-testable.
/// macOS: `security find-generic-password -s S -a A -w`
/// Linux: `secret-tool lookup service S account A`
pub fn keychain_args(service: &str, account: &str) -> (&'static str, Vec<String>) {
    if cfg!(target_os = "macos") {
        (
            "security",
            vec![
                "find-generic-password".into(),
                "-s".into(),
                service.into(),
                "-a".into(),
                account.into(),
                "-w".into(),
            ],
        )
    } else {
        (
            "secret-tool",
            vec![
                "lookup".into(),
                "service".into(),
                service.into(),
                "account".into(),
                account.into(),
            ],
        )
    }
}

/// Keychain reads are local IPC — a wedged `security`/`secret-tool`
/// shouldn't hang a render. Fifteen seconds is already absurd.
const KEYCHAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn keychain_lookup(service: &str, account: &str) -> Result<String, AuthError> {
    let (tool, args) = keychain_args(service, account);
    let err = |detail: String| AuthError::Keychain {
        service: service.into(),
        account: account.into(),
        detail,
    };
    let output =
        scene_media::output_timeout(Command::new(tool).args(&args), tool, KEYCHAIN_TIMEOUT)
            .map_err(|e| err(e.to_string()))?;
    if !output.status.success() {
        return Err(err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_string()));
    }
    let secret = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if secret.is_empty() {
        return Err(err("empty secret".into()));
    }
    Ok(secret)
}
