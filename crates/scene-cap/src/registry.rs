//! The capability registry: `[capabilities.*]` sections of `scene.toml`.
//!
//! A capability is a named external service the scene may call — TTS,
//! image generation, alignment — declared by the project, invoked by
//! the engine. Two connector shapes:
//!
//! ```toml
//! [capabilities.tts]
//! command = ["python3", "tools/tts.py"]   # request JSON on stdin
//! auth = { env = "TTS_API_KEY" }          # forwarded into the process env
//!
//! [capabilities.image]
//! endpoint = "https://api.gen/v1/image"   # POST, body → asset file
//! auth = { keyring = "scene/image" }      # OS keychain service/account
//! ```

use std::collections::BTreeMap;

use serde::Deserialize;

/// Where a credential lives. Resolution never embeds secrets in argv —
/// env vars forward by *name*, keychain lookups go through OS tools.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthRef {
    /// Forward `VAR=<value>` into the connector's environment.
    Env(String),
    /// `service/account` — macOS `security`, Linux `secret-tool`.
    Keychain { service: String, account: String },
}

/// How the engine reaches the connector.
#[derive(Debug, Clone, PartialEq)]
pub enum Connector {
    /// Spawn argv; request JSON on stdin; connector writes `out`.
    Subprocess { argv: Vec<String> },
    /// POST request JSON via curl; response body becomes `out`.
    Http { endpoint: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Capability {
    pub name: String,
    pub connector: Connector,
    pub auth: Option<AuthRef>,
}

#[derive(Debug, Default)]
pub struct Registry {
    caps: BTreeMap<String, Capability>,
}

impl Registry {
    pub fn get(&self, name: &str) -> Option<&Capability> {
        self.caps.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.caps.keys().map(String::as_str)
    }

    /// Parse a `scene.toml` document. Errors are strings (toml's own
    /// messages carry line/col); a capability declaring both `command`
    /// and `endpoint` — or neither — is rejected.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Doc {
            #[serde(default)]
            capabilities: BTreeMap<String, CapToml>,
        }
        #[derive(Deserialize)]
        struct CapToml {
            command: Option<Vec<String>>,
            endpoint: Option<String>,
            auth: Option<AuthToml>,
        }
        #[derive(Deserialize)]
        struct AuthToml {
            env: Option<String>,
            keyring: Option<String>,
        }

        let doc: Doc = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut caps = BTreeMap::new();
        for (name, c) in doc.capabilities {
            let connector = match (c.command, c.endpoint) {
                (Some(argv), None) if !argv.is_empty() => Connector::Subprocess { argv },
                (None, Some(endpoint)) => Connector::Http { endpoint },
                _ => {
                    return Err(format!(
                        "capability `{name}` needs exactly one of `command` or `endpoint`"
                    ));
                }
            };
            let auth = match c.auth {
                Some(AuthToml {
                    env: Some(var),
                    keyring: None,
                }) => Some(AuthRef::Env(var)),
                Some(AuthToml {
                    env: None,
                    keyring: Some(sa),
                }) => {
                    let (service, account) = sa.split_once('/').ok_or_else(|| {
                        format!(
                            "capability `{name}` keyring auth wants `service/account`, got `{sa}`"
                        )
                    })?;
                    Some(AuthRef::Keychain {
                        service: service.to_string(),
                        account: account.to_string(),
                    })
                }
                None => None,
                _ => {
                    return Err(format!(
                        "capability `{name}` auth needs exactly one of `env` or `keyring`"
                    ));
                }
            };
            caps.insert(
                name.clone(),
                Capability {
                    name,
                    connector,
                    auth,
                },
            );
        }
        Ok(Registry { caps })
    }
}
