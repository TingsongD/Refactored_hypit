//! scene-cap — capability registry + connector fulfillment.
//!
//! Projects declare external services in `scene.toml`; the engine calls
//! them through one contract (JSON request → asset file). Credentials
//! resolve via env vars or the OS keychain and always reach the
//! connector the same way: the `SCENE_CAP_AUTH` env var.

mod auth;
mod fulfill;
mod registry;

pub use auth::{AUTH_ENV, AuthError, keychain_args, resolve, resolve_with};
pub use fulfill::{CapError, CapRequest, fulfill, http_args};
pub use registry::{AuthRef, Capability, Connector, Registry};

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    const TOML: &str = r#"
[project]
name = "x"

[capabilities.tts]
command = ["python3", "tools/tts.py"]
auth = { env = "TTS_API_KEY" }

[capabilities.image]
endpoint = "https://api.example.com/v1/image"
auth = { keyring = "scene/image" }

[capabilities.local]
command = ["cat"]
"#;

    #[test]
    fn registry_parses_both_connector_kinds() {
        let reg = Registry::from_toml(TOML).unwrap();
        assert_eq!(reg.names().collect::<Vec<_>>(), ["image", "local", "tts"]);
        let tts = reg.get("tts").unwrap();
        assert_eq!(
            tts.connector,
            Connector::Subprocess {
                argv: vec!["python3".into(), "tools/tts.py".into()]
            }
        );
        assert_eq!(tts.auth, Some(AuthRef::Env("TTS_API_KEY".into())));
        let image = reg.get("image").unwrap();
        assert_eq!(
            image.connector,
            Connector::Http {
                endpoint: "https://api.example.com/v1/image".into()
            }
        );
        assert_eq!(
            image.auth,
            Some(AuthRef::Keychain {
                service: "scene".into(),
                account: "image".into()
            })
        );
        assert_eq!(reg.get("local").unwrap().auth, None);
    }

    #[test]
    fn registry_rejects_ambiguous_and_empty_connectors() {
        for body in [
            r#"command = ["a"], endpoint = "https://x""#,
            r#"command = []"#,
            r#""#,
        ] {
            let doc = format!("[capabilities.c]\n{body}");
            assert!(Registry::from_toml(&doc).is_err(), "{body}");
        }
    }

    #[test]
    fn registry_rejects_bad_auth_shapes() {
        let both = "[capabilities.c]\ncommand=[\"a\"]\nauth={env=\"V\",keyring=\"s/a\"}";
        assert!(Registry::from_toml(both).is_err());
        let no_slash = "[capabilities.c]\ncommand=[\"a\"]\nauth={keyring=\"noslash\"}";
        assert!(Registry::from_toml(no_slash).is_err());
    }

    #[test]
    fn env_auth_resolves_and_missing_errors() {
        let auth = AuthRef::Env("MY_VAR".into());
        let got = resolve_with(
            &auth,
            |n| (n == "MY_VAR").then(|| "s3cret".into()),
            |_, _| unreachable!(),
        );
        assert_eq!(got.unwrap(), "s3cret");
        let err = resolve_with(&auth, |_| None, |_, _| unreachable!()).unwrap_err();
        assert!(matches!(err, AuthError::MissingEnv(v) if v == "MY_VAR"));
    }

    #[test]
    fn keychain_args_match_platform_tool() {
        let (tool, args) = keychain_args("svc", "acct");
        if cfg!(target_os = "macos") {
            assert_eq!(tool, "security");
            assert_eq!(
                args,
                vec!["find-generic-password", "-s", "svc", "-a", "acct", "-w"]
            );
        } else {
            assert_eq!(tool, "secret-tool");
            assert_eq!(args, vec!["lookup", "service", "svc", "account", "acct"]);
        }
    }

    #[test]
    fn http_args_build_curl_post() {
        let args = http_args(
            "https://api.x/v1",
            Some(&PathBuf::from("/tmp/x.curlrc")),
            &PathBuf::from("/o.png"),
        );
        assert_eq!(args[0], "curl");
        assert!(args.windows(2).any(|w| w == ["-X", "POST"]));
        // The bearer goes through `-K <file>` — never a literal argv header.
        assert!(args.windows(2).any(|w| w == ["-K", "/tmp/x.curlrc"]));
        assert!(!args.iter().any(|a| a.contains("authorization")));
        assert!(args.windows(2).any(|w| w == ["-o", "/o.png"]));
        assert_eq!(args.last().map(String::as_str), Some("https://api.x/v1"));
        // No resolved secret → no config file reference.
        let args2 = http_args("https://api.x/v1", None, &PathBuf::from("/o.png"));
        assert!(!args2.iter().any(|a| a == "-K"));
    }

    fn registry(argv: &[String]) -> Registry {
        // JSON string arrays are also valid TOML, including Windows paths.
        Registry::from_toml(&format!(
            "[capabilities.fake]\ncommand = {}\n",
            serde_json::to_string(argv).unwrap()
        ))
        .unwrap()
    }

    fn fixture_registry() -> Registry {
        registry(&[
            std::env::current_exe().unwrap().display().to_string(),
            "--exact".into(),
            "tests::connector_fixture".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ])
    }

    /// A real executable fixture on every CI platform. Flooding stderr before
    /// reading stdin also catches regressions in simultaneous pipe handling.
    #[test]
    #[ignore = "subprocess fixture"]
    fn connector_fixture() {
        use std::io::{Read, Write};
        std::io::stderr().write_all(&vec![b'x'; 262_144]).unwrap();
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).unwrap();
        let req: serde_json::Value = serde_json::from_str(&input).unwrap();
        if let Some(log) = req["params"]["reqlog"].as_str() {
            std::fs::write(log, &input).unwrap();
        }
        let out = req["out"].as_str().unwrap();
        match req["params"]["mode"].as_str().unwrap_or("success") {
            "noop" => (),
            "empty" => std::fs::write(out, "").unwrap(),
            "fail" => {
                std::fs::write(out, "PARTIAL").unwrap();
                eprintln!("nope");
                std::process::exit(3);
            }
            "success" => std::fs::write(out, "FAKE-ASSET").unwrap(),
            mode => panic!("unknown fixture mode: {mode}"),
        }
    }

    #[test]
    fn subprocess_fulfill_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let reqlog = dir.path().join("req.json");
        let out = dir.path().join("asset.bin");
        std::fs::write(&out, "OLD ASSET").unwrap();
        let got = fulfill(
            &fixture_registry(),
            &CapRequest {
                capability: "fake",
                params: json!({"text": "hello", "voice": "n", "reqlog": reqlog}),
                out: &out,
            },
        )
        .unwrap();
        assert_eq!(got, out);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "FAKE-ASSET");
        let req: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&reqlog).unwrap()).unwrap();
        assert_eq!(req["capability"], "fake");
        assert_eq!(req["params"]["text"], "hello");
        let staged = PathBuf::from(req["out"].as_str().unwrap());
        assert!(staged.is_absolute());
        assert_ne!(staged, out);
        assert_eq!(staged.file_name(), out.file_name());
        assert_eq!(
            staged.ancestors().nth(3).unwrap(),
            dir.path().canonicalize().unwrap()
        );
        assert!(
            !staged.parent().unwrap().exists(),
            "staging must be cleaned"
        );
    }

    #[test]
    fn failed_or_missing_connector_output_preserves_existing_asset() {
        for mode in ["fail", "noop", "empty"] {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("asset.bin");
            std::fs::write(&out, "OLD ASSET").unwrap();
            let err = fulfill(
                &fixture_registry(),
                &CapRequest {
                    capability: "fake",
                    params: json!({"mode": mode}),
                    out: &out,
                },
            )
            .unwrap_err();
            if mode == "fail" {
                assert!(matches!(err, CapError::Failed { .. }), "{err}");
                assert!(err.to_string().contains("nope"), "{err}");
            } else {
                assert!(matches!(err, CapError::NoOutput { .. }), "{err}");
            }
            assert_eq!(std::fs::read_to_string(&out).unwrap(), "OLD ASSET");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn spawn_failure_preserves_existing_asset() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("asset.bin");
        std::fs::write(&out, "OLD ASSET").unwrap();
        let reg = registry(&[dir.path().join("missing-executable").display().to_string()]);
        let err = fulfill(
            &reg,
            &CapRequest {
                capability: "fake",
                params: json!({}),
                out: &out,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CapError::Spawn { .. }), "{err}");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "OLD ASSET");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn unknown_capability_is_structured() {
        let reg = Registry::from_toml("").unwrap();
        let err = fulfill(
            &reg,
            &CapRequest {
                capability: "ghost",
                params: json!({}),
                out: &PathBuf::from("/tmp/x"),
            },
        )
        .unwrap_err();
        assert!(matches!(err, CapError::Unknown(n) if n == "ghost"));
    }

    #[test]
    fn chatty_connector_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("asset.bin");
        let reg = fixture_registry();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = fulfill(
                &reg,
                &CapRequest {
                    capability: "fake",
                    // Larger than a pipe buffer, while stderr fills first.
                    params: json!({"text": "x".repeat(262_144)}),
                    out: &out,
                },
            );
            let _ = tx.send(result);
        });
        rx.recv_timeout(std::time::Duration::from_secs(15))
            .expect("fulfill deadlocked")
            .unwrap();
    }
}
