//! Todo 2 configuration tests: real layered precedence, strict keys, typed
//! enum-like values, secret indirection/redaction and every mode-specific
//! required/forbidden-field invariant.
//!
//! Precedence under test (highest wins): CLI overrides > environment > strict
//! TOML file > built-in defaults. Every case is table-driven or a single named
//! invariant so a regression names itself.

use std::collections::BTreeMap;

#[cfg(windows)]
use std::io::Write;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

#[cfg(windows)]
use godot_mcp_domain::sysfs::{open_private_lock_file, open_private_state_dir};

use godot_mcp_domain::CapabilityProfile;
use godot_mcp_domain::config::{
    CliOverrides, ConfigError, HttpMode, LogSink, ProcessAdapter, ProcessBoundary, ProcessTrust,
    ProxyHopCredential, SecretRef, ServerConfig, Transport, load_config,
};

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn no_env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

type PrecedenceCase<'a> = (Option<&'a str>, Option<&'a str>, Option<&'a str>, &'a str);

/// A complete, valid `trusted_local` HTTP file fixture (secret by indirection).
const TRUSTED_LOCAL_FILE: &str = r#"
transport = "http"

[http]
bind = "127.0.0.1:9000"
mode = "trusted_local"
host = "127.0.0.1:9000"
origin = "http://127.0.0.1:9000"

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"
"#;

/// A complete, valid `proxied_oidc` HTTP file fixture.
const PROXIED_OIDC_FILE: &str = r#"
transport = "http"

[http]
bind = "127.0.0.1:9443"
mode = "proxied_oidc"
host = "127.0.0.1:9443"
origin = "https://resource.example"
external_resource_url = "https://resource.example/mcp"

[http.proxy_hop.socket_path]
path = "/run/user/1000/godot-mcp-proxy.sock"

[http.principal]
issuer = "https://issuer.example"
subject = "user-123"
"#;

// ---------------------------------------------------------------------------
// Precedence: defaults -> file -> env -> CLI
// ---------------------------------------------------------------------------

#[test]
fn defaults_apply_when_no_file_env_or_cli() {
    let cfg = load_config(None, &no_env(), &CliOverrides::default()).expect("defaults validate");
    assert_eq!(cfg.transport, Transport::Stdio, "stdio is the default");
    assert!(cfg.http.is_none(), "HTTP is off by default");
    assert_eq!(cfg.log_sink, LogSink::Stderr);
    assert_eq!(cfg.profile, None);
    assert!(!cfg.disposable);
    assert_eq!(cfg.limits, godot_mcp_domain::config::Limits::default());
    assert!(
        cfg.bridge.endpoint.ip().is_loopback(),
        "default bridge endpoint is loopback"
    );
}

#[test]
fn file_overrides_defaults() {
    let file = r#"
project_root = "/srv/game"
profile = "editor"
disposable = true

[limits]
deadline_ms = 1234
"#;
    let cfg = load_config(Some(file), &no_env(), &CliOverrides::default()).expect("file validates");
    assert_eq!(cfg.project_root.as_deref(), Some("/srv/game"));
    assert_eq!(cfg.profile, Some(CapabilityProfile::Editor));
    assert!(cfg.disposable);
    assert_eq!(cfg.limits.deadline_ms, 1234);
    // Untouched keys keep their defaults.
    assert_eq!(cfg.limits.max_queue, 64);
    assert_eq!(cfg.transport, Transport::Stdio);
}

#[test]
fn env_overrides_file() {
    let file = r#"
project_root = "/srv/from-file"
profile = "core"

[limits]
deadline_ms = 1000
"#;
    let e = env(&[
        ("GODOT_MCP_PROJECT_ROOT", "/srv/from-env"),
        ("GODOT_MCP_PROFILE", "runtime"),
        ("GODOT_MCP_DEADLINE_MS", "2000"),
    ]);
    let cfg = load_config(Some(file), &e, &CliOverrides::default()).expect("env layer validates");
    assert_eq!(cfg.project_root.as_deref(), Some("/srv/from-env"));
    assert_eq!(cfg.profile, Some(CapabilityProfile::Runtime));
    assert_eq!(cfg.limits.deadline_ms, 2000);
}

#[test]
fn cli_overrides_env_and_file() {
    let file = r#"
project_root = "/srv/from-file"
profile = "core"
"#;
    let e = env(&[
        ("GODOT_MCP_PROJECT_ROOT", "/srv/from-env"),
        ("GODOT_MCP_PROFILE", "runtime"),
    ]);
    let cli = CliOverrides {
        project_root: Some("/srv/from-cli".to_string()),
        profile: Some(CapabilityProfile::Full),
        ..CliOverrides::default()
    };
    let cfg = load_config(Some(file), &e, &cli).expect("cli layer validates");
    assert_eq!(cfg.project_root.as_deref(), Some("/srv/from-cli"));
    assert_eq!(cfg.profile, Some(CapabilityProfile::Full));
}

#[test]
fn precedence_table_is_deterministic() {
    // (file value, env value, cli value, expected winner)
    let cases: &[PrecedenceCase<'_>] = &[
        (Some("/f"), None, None, "/f"),
        (Some("/f"), Some("/e"), None, "/e"),
        (Some("/f"), Some("/e"), Some("/c"), "/c"),
        (None, Some("/e"), Some("/c"), "/c"),
        (None, None, Some("/c"), "/c"),
    ];
    for (file_v, env_v, cli_v, expected) in cases {
        let file = file_v.map(|v| format!("project_root = \"{v}\"\n"));
        let e = env_v.map_or_else(no_env, |v| env(&[("GODOT_MCP_PROJECT_ROOT", v)]));
        let cli = CliOverrides {
            project_root: cli_v.map(std::string::ToString::to_string),
            ..CliOverrides::default()
        };
        let cfg = load_config(file.as_deref(), &e, &cli).expect("case validates");
        assert_eq!(
            cfg.project_root.as_deref(),
            Some(*expected),
            "file={file_v:?} env={env_v:?} cli={cli_v:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Strictness: unknown / invalid keys and values
// ---------------------------------------------------------------------------

#[test]
fn unknown_file_key_is_rejected() {
    let err = load_config(
        Some("transport = \"stdio\"\nbogus_key = 1\n"),
        &no_env(),
        &CliOverrides::default(),
    )
    .expect_err("unknown file key must be rejected");
    assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
}

#[test]
fn unknown_namespaced_env_var_is_rejected() {
    let e = env(&[("GODOT_MCP_NOT_A_KEY", "1")]);
    let err = load_config(None, &e, &CliOverrides::default())
        .expect_err("unknown GODOT_MCP_* env var must be rejected");
    match err {
        ConfigError::UnknownEnvVar { ref name } => assert_eq!(name, "GODOT_MCP_NOT_A_KEY"),
        other => panic!("expected UnknownEnvVar, got {other:?}"),
    }
}

#[test]
fn active_namespaced_secret_env_var_is_allowed_after_precedence() {
    let e = env(&[("GODOT_MCP_LAUNCH_SECRET", "actual-secret-value")]);
    let cfg = load_config(Some(TRUSTED_LOCAL_FILE), &e, &CliOverrides::default())
        .expect("the final active secret reference permits its namespaced environment variable");
    assert_eq!(
        cfg.http.and_then(|http| http.local_secret),
        Some(SecretRef::Env {
            var: "GODOT_MCP_LAUNCH_SECRET".to_string()
        })
    );
}

#[test]
fn inactive_namespaced_secret_env_var_is_still_rejected() {
    let e = env(&[("GODOT_MCP_LAUNCH_SECRET", "stale-secret-value")]);
    let cli = CliOverrides {
        http_local_secret: Some(SecretRef::Env {
            var: "OTHER_SECRET".to_string(),
        }),
        ..CliOverrides::default()
    };
    let err = load_config(Some(TRUSTED_LOCAL_FILE), &e, &cli)
        .expect_err("an env var displaced by final CLI precedence is unknown");
    assert!(
        matches!(err, ConfigError::UnknownEnvVar { ref name } if name == "GODOT_MCP_LAUNCH_SECRET"),
        "got {err:?}"
    );
}

#[test]
fn unrelated_env_vars_are_ignored() {
    let e = env(&[("PATH", "/usr/bin"), ("HOME", "/root")]);
    load_config(None, &e, &CliOverrides::default()).expect("non-namespaced env is ignored");
}

#[test]
fn invalid_env_values_are_rejected_by_name() {
    let cases: &[(&str, &str)] = &[
        ("GODOT_MCP_TRANSPORT", "quic"),
        ("GODOT_MCP_PROFILE", "godmode"),
        ("GODOT_MCP_DEADLINE_MS", "soon"),
        ("GODOT_MCP_DISPOSABLE", "maybe"),
        ("GODOT_MCP_BRIDGE_ENDPOINT", "not-an-addr"),
        ("GODOT_MCP_HTTP_MODE", "trust_me"),
        ("GODOT_MCP_LOG_SINK", "stdout"),
    ];
    for (name, value) in cases {
        let e = env(&[(*name, *value)]);
        let err = load_config(None, &e, &CliOverrides::default())
            .unwrap_err_or_panic_for(&format!("{name}={value}"));
        match err {
            ConfigError::InvalidEnvValue {
                name: ref got_name, ..
            } => assert_eq!(got_name, name, "wrong var named for {name}={value}"),
            other => panic!("expected InvalidEnvValue for {name}={value}, got {other:?}"),
        }
    }
}

#[test]
fn unknown_profile_name_in_file_is_rejected() {
    let err = load_config(
        Some("profile = \"godmode\"\n"),
        &no_env(),
        &CliOverrides::default(),
    )
    .expect_err("profile is a typed enum, not a free string");
    assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Secrets: indirection only, redacted everywhere
// ---------------------------------------------------------------------------

#[test]
fn inline_plaintext_secret_in_file_is_rejected() {
    let file = r#"
transport = "http"

[http]
bind = "127.0.0.1:9000"
mode = "trusted_local"
host = "127.0.0.1:9000"
origin = "http://127.0.0.1:9000"
local_secret = "super-secret-inline"
"#;
    let err = load_config(Some(file), &no_env(), &CliOverrides::default())
        .expect_err("an inline secret string must not be accepted");
    let rendered = format!("{err}");
    assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
    assert!(
        !rendered.contains("super-secret-inline"),
        "the rejection message must not echo secret material: {rendered}"
    );
}

#[test]
fn secret_indirection_resolves_and_stays_redacted() {
    let cfg = load_config(
        Some(TRUSTED_LOCAL_FILE),
        &no_env(),
        &CliOverrides::default(),
    )
    .expect("valid");
    let http = cfg.http.as_ref().expect("http section");
    let secret_ref = http.local_secret.as_ref().expect("secret indirection");
    assert_eq!(
        secret_ref,
        &SecretRef::Env {
            var: "GODOT_MCP_LAUNCH_SECRET".to_string()
        }
    );

    // Debug of the whole config never contains secret *material* (there is none
    // in the config at all — only the indirection).
    let dumped = format!("{cfg:?}");
    assert!(!dumped.contains("actual-secret-value"), "{dumped}");

    // Resolving through the environment yields a redacting Secret.
    let resolver = env(&[("GODOT_MCP_LAUNCH_SECRET", "actual-secret-value")]);
    let resolved = secret_ref.resolve(&resolver).expect("resolves");
    assert_eq!(resolved.expose(), "actual-secret-value");
    assert_eq!(format!("{resolved:?}"), "[REDACTED]");
    assert_eq!(format!("{resolved}"), "[REDACTED]");
}

#[test]
fn secret_file_indirection_resolves_and_stays_redacted() {
    let dir = tempfile::tempdir().expect("tempdir");
    #[cfg(windows)]
    let private_dir = open_private_state_dir(&dir.path().join("private"))
        .expect("create protected owner-only secret directory");
    #[cfg(windows)]
    let protected_file = open_private_lock_file(&private_dir, "launch.secret")
        .expect("create protected owner-only secret file");
    #[cfg(windows)]
    let path = dir.path().join("private").join("launch.secret");
    #[cfg(not(windows))]
    let path = dir.path().join("launch.secret");
    #[cfg(windows)]
    {
        // Open the already-created object without CREATE, then truncate/write it
        // in place so the backend-installed protected DACL cannot be replaced.
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("open protected secret file for in-place write");
        writer
            .write_all(b"file-secret-value\n")
            .expect("write protected secret file");
        writer.flush().expect("flush protected secret file");
    }
    #[cfg(not(windows))]
    std::fs::write(&path, "file-secret-value\n").expect("write secret file");
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("protect secret file");
    let secret_ref = SecretRef::File {
        path: path.display().to_string(),
    };
    let resolved = secret_ref.resolve(&no_env()).expect("resolves from file");
    assert_eq!(resolved.expose(), "file-secret-value");
    assert_eq!(format!("{resolved:?}"), "[REDACTED]");
    assert_eq!(format!("{resolved}"), "[REDACTED]");
    assert!(!format!("{resolved:?}").contains("file-secret-value"));
    #[cfg(windows)]
    drop(protected_file);
}

#[cfg(windows)]
#[test]
fn inherited_default_acl_secret_file_is_rejected_without_leaking_material() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("inherited.secret");
    let secret_material = "inherited-secret-must-stay-hidden";
    std::fs::write(&path, secret_material).expect("write ordinary inherited-ACL secret file");

    let error = SecretRef::File {
        path: path.display().to_string(),
    }
    .resolve(&no_env())
    .expect_err("an ordinary inherited/default Windows ACL must be rejected");
    assert!(
        matches!(error, ConfigError::SecretResolution { .. }),
        "unexpected resolver error: {error:?}"
    );
    let displayed = format!("{error}");
    let debugged = format!("{error:?}");
    let diagnostic = format!("secret resolution failed: {error}");
    assert!(!displayed.contains(secret_material), "{displayed}");
    assert!(!debugged.contains(secret_material), "{debugged}");
    assert!(!diagnostic.contains(secret_material), "{diagnostic}");
}

#[cfg(unix)]
#[test]
fn secret_file_owner_predicate_accepts_current_euid_and_rejects_other_uid() {
    let current = u64::from(rustix::process::geteuid().as_raw());
    assert!(godot_mcp_domain::sysfs::owner_matches_effective_user(
        current, current
    ));
    let other = if current == 0 { 1 } else { 0 };
    assert!(!godot_mcp_domain::sysfs::owner_matches_effective_user(
        other, current
    ));
}

#[cfg(unix)]
#[test]
fn group_or_world_accessible_secret_file_is_rejected_even_in_private_tempdir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("launch.secret");
    std::fs::write(&path, "file-secret-value\n").expect("write secret file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
        .expect("set insecure permissions");

    let err = SecretRef::File {
        path: path.display().to_string(),
    }
    .resolve(&no_env())
    .expect_err("group-readable secret files must fail regardless of parent permissions");
    assert!(
        matches!(err, ConfigError::SecretResolution { ref reason, .. } if reason.contains("group/world")),
        "got {err:?}"
    );
}

#[cfg(unix)]
#[test]
fn secret_file_symlink_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("target.secret");
    let link = dir.path().join("launch.secret");
    std::fs::write(&target, "file-secret-value\n").expect("write secret file");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
        .expect("protect target");
    symlink(&target, &link).expect("create symlink");

    SecretRef::File {
        path: link.display().to_string(),
    }
    .resolve(&no_env())
    .expect_err("secret symlinks must fail closed");
}

#[test]
fn empty_secret_indirection_is_rejected() {
    let file = r#"
transport = "http"

[http]
bind = "127.0.0.1:9000"
mode = "trusted_local"
host = "127.0.0.1:9000"
origin = "http://127.0.0.1:9000"

[http.local_secret.env]
var = ""
"#;
    let err = load_config(Some(file), &no_env(), &CliOverrides::default())
        .expect_err("an empty secret indirection is not a configured secret");
    assert!(matches!(err, ConfigError::EmptyValue { .. }), "got {err:?}");
}

#[test]
fn missing_local_secret_for_trusted_local_is_rejected() {
    let file = r#"
transport = "http"

[http]
bind = "127.0.0.1:9000"
mode = "trusted_local"
host = "127.0.0.1:9000"
origin = "http://127.0.0.1:9000"
"#;
    let err = load_config(Some(file), &no_env(), &CliOverrides::default())
        .expect_err("trusted_local requires a secret indirection");
    assert!(
        matches!(err, ConfigError::MissingLocalSecret),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Transport / listener invariants
// ---------------------------------------------------------------------------

#[test]
fn http_section_without_http_transport_fails_closed() {
    let file = r#"
transport = "stdio"

[http]
bind = "127.0.0.1:9000"
mode = "trusted_local"
host = "127.0.0.1:9000"
origin = "http://127.0.0.1:9000"

[http.local_secret.env]
var = "S"
"#;
    let err = load_config(Some(file), &no_env(), &CliOverrides::default())
        .expect_err("a stale [http] section under stdio must fail closed");
    assert!(
        matches!(err, ConfigError::HttpSectionForbidden),
        "got {err:?}"
    );
}

#[test]
fn non_loopback_bind_and_bridge_are_rejected() {
    let file = TRUSTED_LOCAL_FILE.replace("127.0.0.1:9000", "0.0.0.0:9000");
    let err = load_config(Some(&file), &no_env(), &CliOverrides::default())
        .expect_err("a public bind is forbidden");
    assert!(
        matches!(err, ConfigError::NonLoopbackBind { .. }),
        "got {err:?}"
    );

    let bridge = "[bridge]\nendpoint = \"0.0.0.0:6009\"\n";
    let err = load_config(Some(bridge), &no_env(), &CliOverrides::default())
        .expect_err("a public bridge endpoint is forbidden");
    assert!(
        matches!(err, ConfigError::NonLoopbackBridge { .. }),
        "got {err:?}"
    );
}

#[test]
fn incomplete_http_env_override_is_rejected() {
    // An HTTP-only env override with no [http] section and no mode is
    // incomplete: it must fail closed rather than invent a mode.
    let e = env(&[
        ("GODOT_MCP_TRANSPORT", "http"),
        ("GODOT_MCP_HTTP_BIND", "127.0.0.1:9100"),
    ]);
    let err = load_config(None, &e, &CliOverrides::default())
        .expect_err("an incomplete HTTP override must fail closed");
    assert!(
        matches!(err, ConfigError::IncompleteHttpOverride { .. }),
        "got {err:?}"
    );
}

#[test]
fn http_can_be_fully_configured_from_env_alone() {
    let e = env(&[
        ("GODOT_MCP_TRANSPORT", "http"),
        ("GODOT_MCP_HTTP_BIND", "127.0.0.1:9100"),
        ("GODOT_MCP_HTTP_MODE", "trusted_local"),
        ("GODOT_MCP_HTTP_HOST", "127.0.0.1:9100"),
        ("GODOT_MCP_HTTP_ORIGIN", "http://127.0.0.1:9100"),
        ("GODOT_MCP_HTTP_LOCAL_SECRET_ENV", "MY_LAUNCH_SECRET"),
    ]);
    let cfg = load_config(None, &e, &CliOverrides::default()).expect("env-only http validates");
    let http = cfg.http.as_ref().expect("http section built from env");
    assert_eq!(cfg.transport, Transport::Http);
    assert_eq!(http.mode, HttpMode::TrustedLocal);
    assert_eq!(
        http.local_secret,
        Some(SecretRef::Env {
            var: "MY_LAUNCH_SECRET".to_string()
        })
    );
}

#[test]
fn stdout_alias_names_are_deferred_to_runtime() {
    for path in ["-", "/dev/stdout", "/dev/fd/1", "/proc/self/fd/1"] {
        let file = format!("[log_sink.file]\npath = \"{path}\"\n");
        let cfg = load_config(Some(&file), &no_env(), &CliOverrides::default())
            .unwrap_or_else(|error| panic!("path {path} must reach runtime validation: {error:?}"));
        assert_eq!(
            cfg.log_sink,
            LogSink::File {
                path: path.to_string()
            },
            "path {path}"
        );
    }
}

#[test]
fn empty_file_log_sink_path_is_rejected() {
    let err = match load_config(
        Some("[log_sink.file]\npath = \"\"\n"),
        &no_env(),
        &CliOverrides::default(),
    ) {
        Ok(_) => panic!("an empty diagnostics file path must be rejected"),
        Err(error) => error,
    };
    assert!(
        matches!(
            err,
            ConfigError::EmptyValue { ref field } if field == "log_sink.file.path"
        ),
        "got {err:?}"
    );
}

trait UnwrapErrFor {
    fn unwrap_err_or_panic_for(self, ctx: &str) -> ConfigError;
}

impl UnwrapErrFor for Result<ServerConfig, ConfigError> {
    fn unwrap_err_or_panic_for(self, ctx: &str) -> ConfigError {
        match self {
            Ok(_) => panic!("expected rejection for {ctx}"),
            Err(error) => error,
        }
    }
}

#[test]
fn file_log_sink_to_a_regular_path_is_accepted() {
    let cfg = load_config(
        Some("[log_sink.file]\npath = \"/var/log/godot-mcp.log\"\n"),
        &no_env(),
        &CliOverrides::default(),
    )
    .expect("a regular rolling-file sink is fine");
    assert_eq!(
        cfg.log_sink,
        LogSink::File {
            path: "/var/log/godot-mcp.log".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// Mode-specific required / forbidden fields
// ---------------------------------------------------------------------------

#[test]
fn valid_trusted_local_and_proxied_oidc_configs_are_accepted() {
    let a = load_config(
        Some(TRUSTED_LOCAL_FILE),
        &no_env(),
        &CliOverrides::default(),
    )
    .expect("valid");
    assert_eq!(a.transport, Transport::Http);
    let b =
        load_config(Some(PROXIED_OIDC_FILE), &no_env(), &CliOverrides::default()).expect("valid");
    let http = b.http.as_ref().expect("http");
    assert_eq!(http.mode, HttpMode::ProxiedOidc);
    assert_eq!(
        http.proxy_hop,
        Some(ProxyHopCredential::SocketPath {
            path: "/run/user/1000/godot-mcp-proxy.sock".to_string()
        })
    );
}

#[test]
fn trusted_local_rejects_proxied_oidc_only_fields() {
    let extras = [
        (
            "proxy_hop",
            "[http.proxy_hop.socket_path]\npath = \"/run/p.sock\"\n",
        ),
        (
            "principal",
            "[http.principal]\nissuer = \"https://i.example\"\nsubject = \"s\"\n",
        ),
        (
            "external_resource_url",
            "external_resource_url_extra = true\n",
        ),
    ];
    // The first two are separate tables appended to the fixture; the third is
    // expressed inline below because it is a scalar in the [http] table.
    for (field, extra) in &extras[..2] {
        let file = format!("{TRUSTED_LOCAL_FILE}\n{extra}");
        let err = load_config(Some(&file), &no_env(), &CliOverrides::default())
            .expect_err("a proxied_oidc-only field under trusted_local must fail closed");
        match err {
            ConfigError::ForbiddenFieldForMode { field: f, .. } => assert_eq!(&f, field),
            other => panic!("expected ForbiddenFieldForMode({field}), got {other:?}"),
        }
    }

    let file = TRUSTED_LOCAL_FILE.replace(
        "origin = \"http://127.0.0.1:9000\"",
        "origin = \"http://127.0.0.1:9000\"\nexternal_resource_url = \"https://resource.example\"",
    );
    let err = load_config(Some(&file), &no_env(), &CliOverrides::default())
        .expect_err("an external resource url under trusted_local must fail closed");
    match err {
        ConfigError::ForbiddenFieldForMode { ref field, .. } => {
            assert_eq!(field, "external_resource_url");
        }
        other => panic!("expected ForbiddenFieldForMode, got {other:?}"),
    }
}

#[test]
fn proxied_oidc_rejects_trusted_local_only_fields() {
    let file = format!("{PROXIED_OIDC_FILE}\n[http.local_secret.env]\nvar = \"S\"\n");
    let err = load_config(Some(&file), &no_env(), &CliOverrides::default())
        .expect_err("a local secret under proxied_oidc must fail closed");
    match err {
        ConfigError::ForbiddenFieldForMode { ref field, .. } => assert_eq!(field, "local_secret"),
        other => panic!("expected ForbiddenFieldForMode, got {other:?}"),
    }
}

#[test]
fn missing_host_or_origin_is_rejected() {
    let no_host = TRUSTED_LOCAL_FILE.replace("host = \"127.0.0.1:9000\"\n", "");
    let err = load_config(Some(&no_host), &no_env(), &CliOverrides::default())
        .expect_err("Host is required under HTTP");
    assert!(matches!(err, ConfigError::MissingHost), "got {err:?}");

    let no_origin = TRUSTED_LOCAL_FILE.replace("origin = \"http://127.0.0.1:9000\"\n", "");
    let err = load_config(Some(&no_origin), &no_env(), &CliOverrides::default())
        .expect_err("Origin is required under HTTP");
    assert!(matches!(err, ConfigError::MissingOrigin), "got {err:?}");
}

#[test]
fn malformed_host_and_origin_values_are_rejected() {
    let bad_hosts = [
        "",
        "http://127.0.0.1:9000",
        "127.0.0.1:9000/path",
        "127.0.0.1:notaport",
        "host name",
        "*",
    ];
    for bad in bad_hosts {
        let file =
            TRUSTED_LOCAL_FILE.replace("host = \"127.0.0.1:9000\"", &format!("host = \"{bad}\""));
        let err = load_config(Some(&file), &no_env(), &CliOverrides::default())
            .unwrap_err_or_panic_for(bad);
        assert!(
            matches!(
                err,
                ConfigError::InvalidHost { .. } | ConfigError::EmptyValue { .. }
            ),
            "host {bad:?} => {err:?}"
        );
    }

    let bad_origins = [
        "",
        "127.0.0.1:9000",
        "*",
        "null",
        "ftp://x.example",
        "http://",
    ];
    for bad in bad_origins {
        let file = TRUSTED_LOCAL_FILE.replace(
            "origin = \"http://127.0.0.1:9000\"",
            &format!("origin = \"{bad}\""),
        );
        let err = load_config(Some(&file), &no_env(), &CliOverrides::default())
            .unwrap_err_or_panic_for(bad);
        assert!(
            matches!(
                err,
                ConfigError::InvalidOrigin { .. } | ConfigError::EmptyValue { .. }
            ),
            "origin {bad:?} => {err:?}"
        );
    }
}

#[test]
fn malformed_external_resource_url_is_rejected() {
    let bad = [
        "http://resource.example",
        "https:/resource.example",
        "https://",
        "https://user@resource.example",
        "https://resource.example/#frag",
        "resource.example",
        "https://127.0.0.1/mcp",
    ];
    for url in bad {
        let file = PROXIED_OIDC_FILE.replace(
            "external_resource_url = \"https://resource.example/mcp\"",
            &format!("external_resource_url = \"{url}\""),
        );
        let err = load_config(Some(&file), &no_env(), &CliOverrides::default())
            .unwrap_err_or_panic_for(url);
        assert!(
            matches!(
                err,
                ConfigError::InvalidResourceUrl { .. } | ConfigError::EmptyValue { .. }
            ),
            "url {url:?} => {err:?}"
        );
    }
}

#[test]
fn missing_proxy_hop_or_principal_or_url_is_rejected() {
    let no_hop = PROXIED_OIDC_FILE.replace(
        "[http.proxy_hop.socket_path]\npath = \"/run/user/1000/godot-mcp-proxy.sock\"\n",
        "",
    );
    let err = load_config(Some(&no_hop), &no_env(), &CliOverrides::default())
        .expect_err("proxy hop identity is required");
    assert!(matches!(err, ConfigError::MissingProxyHop), "got {err:?}");

    let no_principal = PROXIED_OIDC_FILE.replace(
        "[http.principal]\nissuer = \"https://issuer.example\"\nsubject = \"user-123\"\n",
        "",
    );
    let err = load_config(Some(&no_principal), &no_env(), &CliOverrides::default())
        .expect_err("one frozen principal is required");
    assert!(matches!(err, ConfigError::MissingPrincipal), "got {err:?}");

    let no_url = PROXIED_OIDC_FILE.replace(
        "external_resource_url = \"https://resource.example/mcp\"\n",
        "",
    );
    let err = load_config(Some(&no_url), &no_env(), &CliOverrides::default())
        .expect_err("the external https resource url is required");
    assert!(
        matches!(err, ConfigError::MissingExternalResourceUrl),
        "got {err:?}"
    );
}

#[test]
fn empty_principal_and_proxy_hop_values_are_rejected() {
    let cases = [
        ("issuer = \"https://issuer.example\"", "issuer = \"\""),
        ("subject = \"user-123\"", "subject = \"\""),
        (
            "path = \"/run/user/1000/godot-mcp-proxy.sock\"",
            "path = \"\"",
        ),
    ];
    for (from, to) in cases {
        let file = PROXIED_OIDC_FILE.replace(from, to);
        let err = load_config(Some(&file), &no_env(), &CliOverrides::default())
            .unwrap_err_or_panic_for(to);
        assert!(
            matches!(err, ConfigError::EmptyValue { .. }),
            "{to} => {err:?}"
        );
    }
}

#[test]
fn non_https_issuer_is_rejected() {
    let file = PROXIED_OIDC_FILE.replace(
        "issuer = \"https://issuer.example\"",
        "issuer = \"http://issuer.example\"",
    );
    let err = load_config(Some(&file), &no_env(), &CliOverrides::default())
        .expect_err("an OIDC issuer must be an https url");
    assert!(
        matches!(err, ConfigError::InvalidIssuer { .. }),
        "got {err:?}"
    );
}

#[test]
fn process_containment_defaults_to_the_host_boundary_for_a_trusted_project() {
    let config = load_config(None, &no_env(), &CliOverrides::default()).expect("defaults");
    assert_eq!(config.process.boundary, None);
    assert_eq!(config.process.trust, ProcessTrust::Trusted);
    assert!(config.process.adapter.is_none());
}

#[test]
fn process_containment_is_layered_cli_over_env_over_file() {
    let file = "[process]\nboundary = \"cgroup\"\ntrust = \"untrusted\"\n";
    let config = load_config(Some(file), &no_env(), &CliOverrides::default()).expect("file");
    assert_eq!(config.process.boundary, Some(ProcessBoundary::Cgroup));
    assert_eq!(config.process.trust, ProcessTrust::Untrusted);

    let environment = env(&[
        ("GODOT_MCP_PROCESS_BOUNDARY", "weak"),
        ("GODOT_MCP_PROCESS_TRUST", "trusted"),
    ]);
    let config = load_config(Some(file), &environment, &CliOverrides::default()).expect("env");
    assert_eq!(config.process.boundary, Some(ProcessBoundary::Weak));
    assert_eq!(config.process.trust, ProcessTrust::Trusted);

    let cli = CliOverrides {
        process_boundary: Some(ProcessBoundary::JobObject),
        process_trust: Some(ProcessTrust::Untrusted),
        ..CliOverrides::default()
    };
    let config = load_config(Some(file), &environment, &cli).expect("cli");
    assert_eq!(config.process.boundary, Some(ProcessBoundary::JobObject));
    assert_eq!(config.process.trust, ProcessTrust::Untrusted);
}

#[test]
fn process_containment_rejects_unknown_values_and_keys() {
    for text in [
        "[process]\nboundary = \"docker\"\n",
        "[process]\ntrust = \"mostly\"\n",
        "[process]\nstrength = \"strong\"\n",
    ] {
        assert!(
            matches!(
                load_config(Some(text), &no_env(), &CliOverrides::default()),
                Err(ConfigError::Parse(_))
            ),
            "{text}"
        );
    }
    for (name, value) in [
        ("GODOT_MCP_PROCESS_BOUNDARY", "docker"),
        ("GODOT_MCP_PROCESS_TRUST", "mostly"),
    ] {
        let rejected = load_config(None, &env(&[(name, value)]), &CliOverrides::default())
            .expect_err("an unknown value is refused");
        assert!(
            matches!(&rejected, ConfigError::InvalidEnvValue { name: named, .. } if named == name),
            "{rejected:?}"
        );
    }
}

/// An absolute adapter program and boundary root on this host: `/`-rooted on
/// Unix, drive-rooted on Windows.
const ADAPTER_PROGRAM: &str = if cfg!(windows) {
    r"C:\godot-mcp\container-adapter.exe"
} else {
    "/usr/local/bin/godot-mcp-container"
};
const ADAPTER_ROOT: &str = if cfg!(windows) {
    r"C:\ProgramData\godot-mcp\boundaries"
} else {
    "/var/lib/godot-mcp/boundaries"
};

#[test]
fn an_adapter_boundary_takes_exactly_its_adapter() {
    let accepted = format!(
        "[process]\nboundary = \"adapter\"\n\n[process.adapter]\nprogram = '{ADAPTER_PROGRAM}'\n\
         root = '{ADAPTER_ROOT}'\n"
    );
    let config =
        load_config(Some(&accepted), &no_env(), &CliOverrides::default()).expect("adapter");
    assert_eq!(
        config.process.adapter,
        Some(ProcessAdapter {
            program: ADAPTER_PROGRAM.to_owned(),
            root: ADAPTER_ROOT.to_owned(),
        })
    );

    let missing = load_config(
        Some("[process]\nboundary = \"adapter\"\n"),
        &no_env(),
        &CliOverrides::default(),
    )
    .expect_err("an adapter boundary needs its adapter");
    assert!(
        matches!(missing, ConfigError::MissingProcessAdapter),
        "{missing:?}"
    );

    let stray = format!(
        "[process]\nboundary = \"cgroup\"\n\n[process.adapter]\nprogram = '{ADAPTER_PROGRAM}'\n\
         root = '{ADAPTER_ROOT}'\n"
    );
    let forbidden = load_config(Some(&stray), &no_env(), &CliOverrides::default())
        .expect_err("an adapter is only for the adapter boundary");
    assert!(
        matches!(forbidden, ConfigError::ProcessAdapterForbidden),
        "{forbidden:?}"
    );

    let relative = format!(
        "[process]\nboundary = \"adapter\"\n\n[process.adapter]\nprogram = 'bin/godot-mcp-container'\n\
         root = '{ADAPTER_ROOT}'\n"
    );
    let invalid = load_config(Some(&relative), &no_env(), &CliOverrides::default())
        .expect_err("an adapter is named by absolute paths");
    assert!(
        matches!(
            invalid,
            ConfigError::InvalidProcessAdapter {
                field: "process.adapter.program"
            }
        ),
        "{invalid:?}"
    );
}
