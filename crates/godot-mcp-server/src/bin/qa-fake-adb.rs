//! `qa-fake-adb` — NON-SHIPPING, `test-hooks`-only stand-in for the Android
//! SDK and JDK tools the server runs.
//!
//! The Android acceptance suite builds a fake SDK and JDK whose tools are
//! links to this one binary, and the program it plays is the file name it
//! was started as:
//!
//! * `adb` answers `devices -l`, `install -r`, `shell am start -W -n`,
//!   `shell pidof`, `logcat -d -t <n> --pid=<pid>` and `kill-server` the way
//!   adb 36 prints them, for the device and the scenario chosen;
//! * `apksigner` answers `verify --print-certs` with one signer, once it finds
//!   `java` on `PATH` as the real script needs;
//! * `keytool` answers `-genkeypair` by writing the keystore it names;
//! * `java` answers `-version`.
//!
//! The server runs these tools with exactly the environment it chooses, so
//! the scenario cannot travel in the server's environment: it is read from
//! `fake-android.conf`, found in the tool's directory or one above it, with
//! `key=value` lines — `scenario` (`success` by default; `none`,
//! `unauthorized`, `offline`, `install-failure`, `install-lost`,
//! `install-hang`, `launch-failure`, `no-process` or `logcat-failure`),
//! `serial` (the one
//! device's serial, `emulator-5554` by default), `apksigner` and `keytool`
//! (`ok` by default, or `fail`). `QA_FAKE_ADB_SCENARIO`, when set, chooses the
//! scenario instead, for a run outside the server. Every call is appended to
//! `fake-android.log` beside the configuration file, with the `HOME` it ran
//! with, so a test can prove what ran and where.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const CONFIGURATION: &str = "fake-android.conf";
const LOG: &str = "fake-android.log";
/// The process ID the fake device reports for the launched app.
const PID: &str = "4242";

/// The scenario and the files that go with it.
struct Fake {
    scenario: String,
    serial: String,
    apksigner_fails: bool,
    keytool_fails: bool,
    directory: Option<PathBuf>,
}

impl Fake {
    fn load(program: &Path) -> Self {
        let mut directory = program.parent().map(Path::to_path_buf);
        let mut found = None;
        for _ in 0..6 {
            let Some(candidate) = directory.clone() else {
                break;
            };
            if candidate.join(CONFIGURATION).is_file() {
                found = Some(candidate);
                break;
            }
            directory = candidate.parent().map(Path::to_path_buf);
        }
        let text = found
            .as_ref()
            .and_then(|directory| std::fs::read_to_string(directory.join(CONFIGURATION)).ok())
            .unwrap_or_default();
        let value = |key: &str| {
            text.lines().find_map(|line| {
                line.trim()
                    .strip_prefix(key)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(|value| value.trim().to_owned())
            })
        };
        Self {
            scenario: std::env::var("QA_FAKE_ADB_SCENARIO")
                .ok()
                .or_else(|| value("scenario"))
                .unwrap_or_else(|| "success".to_owned()),
            serial: value("serial").unwrap_or_else(|| "emulator-5554".to_owned()),
            apksigner_fails: value("apksigner").as_deref() == Some("fail"),
            keytool_fails: value("keytool").as_deref() == Some("fail"),
            directory: found,
        }
    }

    fn log(&self, role: &str, arguments: &[String]) {
        let Some(directory) = &self.directory else {
            return;
        };
        let home = std::env::var("HOME").unwrap_or_default();
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(LOG))
        {
            let _ = writeln!(file, "{role} HOME={home} {}", arguments.join(" "));
        }
    }

    fn record(&self, name: &str, text: &str) {
        if let Some(directory) = &self.directory {
            let _ = std::fs::write(directory.join(name), text);
        }
    }
}

fn adb(fake: &Fake, arguments: &[String]) -> ExitCode {
    // `-s <serial>` names the device every later command reaches.
    let (serial, rest) = match arguments {
        [flag, serial, rest @ ..] if flag == "-s" => (Some(serial.as_str()), rest),
        rest => (None, rest),
    };
    let words: Vec<&str> = rest.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["devices", "-l"] => {
            let home = std::env::var("HOME").map(PathBuf::from).ok();
            let marker = home.map(|home| home.join("fake-adb-server.started"));
            if marker.as_ref().is_some_and(|marker| !marker.exists()) {
                eprintln!("* daemon not running; starting now at tcp:5037");
                eprintln!("* daemon started successfully");
                if let Some(marker) = &marker {
                    let _ = std::fs::write(marker, b"");
                }
            }
            println!("List of devices attached");
            match fake.scenario.as_str() {
                "none" => {}
                "unauthorized" => println!("{}\tunauthorized usb:1-1 transport_id:1", fake.serial),
                "offline" => println!("{}\toffline transport_id:1", fake.serial),
                _ => println!(
                    "{}\tdevice product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 \
                     device:emu64xa transport_id:1",
                    fake.serial
                ),
            }
            println!();
            ExitCode::SUCCESS
        }
        ["kill-server" | "start-server"] => ExitCode::SUCCESS,
        _ if serial != Some(fake.serial.as_str()) => {
            eprintln!("adb: device '{}' not found", serial.unwrap_or_default());
            ExitCode::from(1)
        }
        ["install", "-r", apk] => {
            println!("Performing Streamed Install");
            let package = std::fs::read(apk)
                .ok()
                .and_then(|bytes| godot_mcp_engine::export::apk::read_apk(&bytes).ok())
                .map(|facts| facts.manifest.package);
            let Some(package) = package else {
                eprintln!("adb: failed to install {apk}: Failure [INSTALL_PARSE_FAILED_NOT_APK]");
                return ExitCode::from(1);
            };
            match fake.scenario.as_str() {
                "install-failure" => {
                    eprintln!(
                        "adb: failed to install {apk}: Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: \
                         Package {package} signatures do not match previously installed version; \
                         ignoring!]"
                    );
                    ExitCode::from(1)
                }
                "install-lost" => {
                    eprintln!("adb: failed to install {apk}: closed");
                    ExitCode::from(1)
                }
                "install-hang" => {
                    // An install that never answers, as when the device is
                    // lost mid-transfer: the call records its process ID and
                    // waits, at most 60 s, for `fake-android.release`.
                    fake.record("fake-android.hang-pid", &std::process::id().to_string());
                    let release = fake
                        .directory
                        .as_ref()
                        .map(|directory| directory.join("fake-android.release"));
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                    while std::time::Instant::now() < deadline
                        && !release.as_ref().is_some_and(|release| release.exists())
                    {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    ExitCode::from(1)
                }
                _ => {
                    fake.record("fake-android.installed", &package);
                    println!("Success");
                    ExitCode::SUCCESS
                }
            }
        }
        ["shell", "am", "start", "-W", "-n", component] => {
            println!("Starting: Intent {{ cmp={component} }}");
            if fake.scenario == "launch-failure" {
                println!("Error type 3");
                println!("Error: Activity class {{{component}}} does not exist.");
                return ExitCode::SUCCESS;
            }
            fake.record("fake-android.launched", component);
            println!("Status: ok");
            println!("LaunchState: COLD");
            println!("Activity: {component}");
            println!("TotalTime: 812");
            println!("WaitTime: 820");
            println!("Complete");
            ExitCode::SUCCESS
        }
        ["shell", "pidof", _package] => {
            if fake.scenario == "no-process" {
                return ExitCode::from(1);
            }
            println!("{PID}");
            ExitCode::SUCCESS
        }
        ["logcat", "-d", "-t", lines, pid] if pid.starts_with("--pid=") => {
            if fake.scenario == "logcat-failure" {
                eprintln!("error: closed");
                return ExitCode::from(1);
            }
            let limit: usize = lines.parse().unwrap_or(0);
            let all = [
                "--------- beginning of main".to_owned(),
                format!(
                    "11-05 20:00:00.000  {PID}  {PID} I godot   : Godot Engine v4.7.1.stable.official"
                ),
                format!("11-05 20:00:00.100  {PID}  {PID} I godot   : fake game started"),
                format!("11-05 20:00:00.200  {PID}  {PID} I godot   : fake game ready"),
            ];
            for line in &all[all.len().saturating_sub(limit)..] {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("adb: unknown command {}", rest.join(" "));
            ExitCode::from(1)
        }
    }
}

/// Whether `PATH` names a directory holding `java`, as the real `apksigner`
/// script needs.
fn java_on_path() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join("java").is_file())
    })
}

fn apksigner(fake: &Fake, arguments: &[String]) -> ExitCode {
    if !java_on_path() || std::env::var_os("JAVA_HOME").is_none() {
        eprintln!("apksigner: java: command not found");
        return ExitCode::from(127);
    }
    match arguments.first().map(String::as_str) {
        Some("verify") => {
            if fake.apksigner_fails {
                eprintln!("DOES NOT VERIFY");
                eprintln!("ERROR: Missing META-INF/MANIFEST.MF");
                return ExitCode::from(1);
            }
            if !arguments.last().is_some_and(|apk| Path::new(apk).is_file()) {
                eprintln!("ERROR: no APK to verify");
                return ExitCode::from(1);
            }
            println!("Signer #1 certificate DN: CN=Android Debug, O=Android, C=US");
            println!(
                "Signer #1 certificate SHA-256 digest: \
                 6f8b7d5b2f0e3c1a9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c"
            );
            println!(
                "Signer #1 certificate SHA-1 digest: 0123456789abcdef0123456789abcdef01234567"
            );
            ExitCode::SUCCESS
        }
        Some("sign") => ExitCode::SUCCESS,
        _ => {
            eprintln!("apksigner: unknown command");
            ExitCode::from(2)
        }
    }
}

fn keytool(fake: &Fake, arguments: &[String]) -> ExitCode {
    if fake.keytool_fails {
        eprintln!("keytool error: java.lang.Exception: the fake keytool was told to fail");
        return ExitCode::from(1);
    }
    let keystore = arguments
        .iter()
        .position(|argument| argument == "-keystore")
        .and_then(|at| arguments.get(at + 1));
    let Some(keystore) = keystore else {
        eprintln!("keytool error: no -keystore");
        return ExitCode::from(1);
    };
    if std::fs::write(keystore, b"FAKE-KEYSTORE").is_err() {
        eprintln!("keytool error: the keystore could not be written");
        return ExitCode::from(1);
    }
    println!(
        "Generating 2,048 bit RSA key pair and self-signed certificate (SHA384withRSA) with a \
         validity of 10,000 days"
    );
    println!("[Storing {keystore}]");
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let mut all = std::env::args();
    let program = PathBuf::from(all.next().unwrap_or_default());
    let arguments: Vec<String> = all.collect();
    let fake = Fake::load(&program);
    let role = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    fake.log(&role, &arguments);
    match role.as_str() {
        "adb" => adb(&fake, &arguments),
        "apksigner" => apksigner(&fake, &arguments),
        "keytool" => keytool(&fake, &arguments),
        "java" => {
            eprintln!("openjdk version \"21.0.10\" 2026-01-20 LTS (qa-fake-adb)");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!(
                "qa-fake-adb: started as `{other}`, which is none of adb, apksigner, keytool or java"
            );
            ExitCode::from(64)
        }
    }
}
