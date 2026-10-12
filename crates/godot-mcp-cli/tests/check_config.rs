//! Todo 2 CLI gate: `godot-mcp check-config` maps typed-config validation to a
//! process exit code (0 valid local/proxy fixtures; 2 public-bind, missing
//! proxy-hop identity and missing principal fixtures).

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cli() -> Command {
    let mut command = Command::cargo_bin("godot-mcp").expect("godot-mcp binary");
    for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GODOT_MCP_")) {
        command.env_remove(name);
    }
    command
}

fn check(config: &str, expected_exit: i32) {
    let file = assert_fs::NamedTempFile::new("config.toml").expect("temp config");
    file.write_str(config).expect("write config");
    cli()
        .arg("check-config")
        .arg("--config")
        .arg(file.path())
        .assert()
        .code(expected_exit)
        .stdout(predicate::str::is_empty());
}

#[test]
fn valid_stdio_default_exits_zero() {
    check("transport = \"stdio\"\n", 0);
}

#[test]
fn valid_trusted_local_loopback_exits_zero() {
    check(
        r#"
transport = "http"

[http]
bind = "127.0.0.1:9000"
mode = "trusted_local"
host = "127.0.0.1:9000"
origin = "http://127.0.0.1:9000"

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"
"#,
        0,
    );
}

#[test]
fn valid_proxied_oidc_exits_zero() {
    check(
        r#"
transport = "http"

[http]
bind = "127.0.0.1:9000"
mode = "proxied_oidc"
host = "127.0.0.1:9000"
origin = "https://resource.example"
external_resource_url = "https://resource.example/mcp"

[http.proxy_hop.socket_path]
path = "/run/user/1000/godot-mcp-proxy.sock"

[http.principal]
issuer = "https://issuer.example"
subject = "user-123"
"#,
        0,
    );
}

#[test]
fn public_bind_exits_two() {
    check(
        r#"
transport = "http"

[http]
bind = "0.0.0.0:9000"
mode = "trusted_local"
host = "resource.example:9000"
origin = "http://resource.example:9000"

[http.local_secret.env]
var = "GODOT_MCP_LAUNCH_SECRET"
"#,
        2,
    );
}

#[test]
fn missing_proxy_hop_exits_two() {
    check(
        r#"
transport = "http"

[http]
bind = "127.0.0.1:9000"
mode = "proxied_oidc"
host = "127.0.0.1:9000"
origin = "https://resource.example"
external_resource_url = "https://resource.example/mcp"

[http.principal]
issuer = "https://issuer.example"
subject = "user-123"
"#,
        2,
    );
}

#[test]
fn missing_principal_exits_two() {
    check(
        r#"
transport = "http"

[http]
bind = "127.0.0.1:9000"
mode = "proxied_oidc"
host = "127.0.0.1:9000"
origin = "https://resource.example"
external_resource_url = "https://resource.example/mcp"

[http.proxy_hop.socket_path]
path = "/run/user/1000/p.sock"
"#,
        2,
    );
}

#[test]
fn unknown_key_exits_two() {
    check("transport = \"stdio\"\nbogus_key = 1\n", 2);
}

#[test]
fn stdout_alias_name_is_deferred_by_check_config() {
    check("[log_sink.file]\npath = \"/dev/stdout\"\n", 0);
}

#[test]
fn inline_secret_rejection_does_not_echo_secret() {
    let marker = "cli-inline-secret-must-not-leak";
    let config = format!(
        r#"
transport = "http"

[http]
bind = "127.0.0.1:9000"
mode = "trusted_local"
host = "127.0.0.1:9000"
origin = "http://127.0.0.1:9000"
local_secret = "{marker}"
"#
    );
    let file = assert_fs::NamedTempFile::new("inline-secret.toml").expect("temp config");
    file.write_str(&config).expect("write config");

    cli()
        .arg("check-config")
        .arg("--config")
        .arg(file.path())
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(marker).not());
}
