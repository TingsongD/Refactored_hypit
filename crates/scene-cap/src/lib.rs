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

    /// A connector test double: captures the request JSON next to the
    /// out path and writes a marker asset — exercises the whole real
    /// subprocess path hermetically.
    fn fake_connector(dir: &std::path::Path) -> PathBuf {
        let script = dir.join("fake.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\njson=$(cat)\necho \"$json\" > \"$SCENE_CAP_REQLOG\"\nout=$(echo \"$json\" | sed -n 's/.*\"out\" *: *\"\\([^\"]*\\)\".*/\\1/p')\nprintf 'FAKE-ASSET' > \"$out\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    #[test]
    fn subprocess_fulfill_roundtrip() {
        let dir = std::env::temp_dir().join(format!("scene-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = fake_connector(&dir);
        let reqlog = dir.join("req.json");
        let out = dir.join("asset.bin");

        // Registry + request, with the double wired in and a reqlog env
        // var only this process sees.
        let toml = format!(
            "[capabilities.fake]\ncommand = [\"{}\"]\n",
            script.display()
        );
        let reg = Registry::from_toml(&toml).unwrap();
        unsafe {
            std::env::set_var("SCENE_CAP_REQLOG", &reqlog);
        }
        let got = fulfill(
            &reg,
            &CapRequest {
                capability: "fake",
                params: json!({"text": "hello", "voice": "n"}),
                out: &out,
            },
        )
        .unwrap();
        unsafe {
            std::env::remove_var("SCENE_CAP_REQLOG");
        }
        assert_eq!(got, out);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "FAKE-ASSET");
        // And the connector saw the real request document.
        let req: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&reqlog).unwrap()).unwrap();
        assert_eq!(req["capability"], "fake");
        assert_eq!(req["params"]["text"], "hello");
        assert_eq!(req["out"], out.display().to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failing_connector_reports_stderr() {
        let dir = std::env::temp_dir().join(format!("scene-cap-f-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fail.sh");
        std::fs::write(&script, "#!/bin/sh\necho 'nope' >&2\nexit 3\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let toml = format!("[capabilities.bad]\ncommand = [\"{}\"]\n", script.display());
        let reg = Registry::from_toml(&toml).unwrap();
        let err = fulfill(
            &reg,
            &CapRequest {
                capability: "bad",
                params: json!({}),
                out: &dir.join("x"),
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
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
    fn stale_out_does_not_masquerade_as_output() {
        let dir = std::env::temp_dir().join(format!("scene-cap-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Connector exits 0 but writes nothing; `out` already holds an
        // old asset. Success must not be claimed on the stale file.
        let script = dir.join("noop.sh");
        std::fs::write(&script, "#!/bin/sh\ncat >/dev/null\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = dir.join("asset.bin");
        std::fs::write(&out, "OLD ASSET").unwrap();
        let toml = format!(
            "[capabilities.noop]\ncommand = [\"{}\"]\n",
            script.display()
        );
        let reg = Registry::from_toml(&toml).unwrap();
        let err = fulfill(
            &reg,
            &CapRequest {
                capability: "noop",
                params: json!({}),
                out: &out,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CapError::NoOutput { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A connector that floods stderr *before* reading the request used
    /// to deadlock: we wrote all of stdin before draining stderr. The
    /// channel timeout turns a regression into a test failure instead of
    /// a hung suite.
    #[test]
    fn chatty_connector_does_not_deadlock() {
        let dir = std::env::temp_dir().join(format!("scene-cap-chatty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("chatty.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nhead -c 262144 /dev/zero | tr '\\0' 'x' >&2\njson=$(cat)\nout=$(echo \"$json\" | sed -n 's/.*\"out\" *: *\"\\([^\"]*\\)\".*/\\1/p')\nprintf 'OK' > \"$out\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = dir.join("asset.bin");
        let toml = format!(
            "[capabilities.chatty]\ncommand = [\"{}\"]\n",
            script.display()
        );
        let reg = Registry::from_toml(&toml).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = fulfill(
                &reg,
                &CapRequest {
                    capability: "chatty",
                    params: json!({"n": 1}),
                    out: &out,
                },
            );
            let _ = tx.send(r.is_ok());
        });
        let ok = rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("fulfill deadlocked — stderr not drained during stdin write");
        assert!(ok);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
