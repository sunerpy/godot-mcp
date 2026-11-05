//! Android: an export's private Godot home, the debug keystore, the APK
//! check, the device list and the deploy.
//!
//! Godot reads the Android SDK, the JDK and the debug keystore from its editor
//! settings, so an Android export runs with a home of its own whose editor
//! settings name them. The SDK it sees there is a view of the operator's: the
//! real `build-tools`, and an `adb` that answers nothing, because an export
//! never reaches a device — Godot 4.7.1 runs `adb devices` and
//! `adb kill-server` during an export of a runnable preset (measured). The
//! debug keystore is the server's own, created once with the JDK's
//! `keytool` in its private storage and reused, so every debug build this
//! server exports is signed by the same key and installs over the previous
//! one. A release export signs with the keystore the project names in
//! Godot's credentials store.
//!
//! After the export, the APK is checked before it becomes the result: it is a
//! zip with a binary manifest and the game data, its package and launcher are
//! read from that manifest, and `apksigner verify` must accept it — Godot
//! 4.7.1 exits 0 with an unsigned APK when it cannot run `apksigner`
//! (measured).
//!
//! A deploy installs one finished export's APK on one device and proves it
//! runs, phase by phase; see [`deploy`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use godot_mcp_bridge::protocol::mutation::MutationStatus;
use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::android::adb::{
    Device, Install, Launch, parse_devices, parse_install, parse_launch, parse_pids,
};
use godot_mcp_engine::android::sdk::{AndroidSdk, Jdk, SdkError};
use godot_mcp_engine::android::tool::{ToolExit, ToolOutput, ToolRun, run};
use godot_mcp_engine::export::apk::read_apk;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::outcome_error;

/// The alias Android's debug keystore convention names.
pub(crate) const DEBUG_KEY_ALIAS: &str = "androiddebugkey";
/// The password Android's debug keystore convention uses; a debug key is
/// public by design.
pub(crate) const DEBUG_KEY_PASSWORD: &str = "android";
/// The most bytes of each stream of one tool run kept.
const TOOL_OUTPUT_CAP: usize = 64 * 1024;
/// The most characters of a phase's output an answer carries.
const PHASE_OUTPUT_CHARS: usize = 4096;
/// How long `adb devices -l` may take, a server start included.
const DEVICES_DEADLINE: Duration = Duration::from_secs(60);
/// How long one install may take.
const INSTALL_DEADLINE: Duration = Duration::from_secs(600);
/// How long `am start -W` may take.
const LAUNCH_DEADLINE: Duration = Duration::from_secs(120);
/// How long one `pidof` may take.
const PIDOF_DEADLINE: Duration = Duration::from_secs(30);
/// How long the logcat read may take.
const LOGCAT_DEADLINE: Duration = Duration::from_secs(60);
/// How long `keytool` may take to create the debug keystore.
const KEYTOOL_DEADLINE: Duration = Duration::from_secs(120);
/// How long `apksigner verify` may take.
const APKSIGNER_DEADLINE: Duration = Duration::from_secs(300);
/// How often the running proof asks for the process.
const PIDOF_INTERVAL: Duration = Duration::from_millis(500);

/// Serializes the one creation of the debug keystore.
static KEYSTORE: Mutex<()> = Mutex::new(());

fn unavailable(message: impl Into<String>) -> OutcomeError {
    outcome_error(ErrorKind::Unavailable, message)
}

/// The refusal for a configured SDK or JDK that lacks a tool.
pub(crate) fn sdk_error(error: &SdkError) -> OutcomeError {
    unavailable(format!(
        "{error}; this server uses the operator's Android SDK and JDK where they are and \
         installs nothing"
    ))
}

/// The operator's Android SDK and JDK, as configured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AndroidTools {
    pub(crate) sdk: Option<PathBuf>,
    pub(crate) java_home: Option<PathBuf>,
}

impl AndroidTools {
    pub(crate) fn from_config(config: &godot_mcp_domain::config::AndroidConfig) -> Self {
        Self {
            sdk: config.sdk.as_deref().map(PathBuf::from),
            java_home: config.java_home.as_deref().map(PathBuf::from),
        }
    }

    /// The configured SDK.
    pub(crate) fn sdk(&self) -> Result<AndroidSdk, OutcomeError> {
        self.sdk.as_ref().map(AndroidSdk::new).ok_or_else(|| {
            unavailable(
                "no Android SDK is configured: set `sdk` in the `[android]` section of the \
                 configuration file, GODOT_MCP_ANDROID_SDK with --config, or --android-sdk",
            )
        })
    }

    /// The configured JDK.
    pub(crate) fn jdk(&self) -> Result<Jdk, OutcomeError> {
        self.java_home.as_ref().map(Jdk::new).ok_or_else(|| {
            unavailable(
                "no JDK is configured: set `java_home` in the `[android]` section of the \
                 configuration file, GODOT_MCP_ANDROID_JAVA_HOME with --config, or \
                 --android-java-home",
            )
        })
    }

    /// The configured SDK's `adb`.
    pub(crate) fn adb(&self) -> Result<PathBuf, OutcomeError> {
        self.sdk()?.adb().map_err(|error| sdk_error(&error))
    }
}

/// The server's private Android storage: `<state>/android`.
#[derive(Debug, Clone)]
pub(crate) struct AndroidDirs {
    root: PathBuf,
}

impl AndroidDirs {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn private(&self, child: Option<&str>) -> Result<PathBuf, OutcomeError> {
        godot_mcp_domain::sysfs::open_private_state_dir(&self.root)
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        let Some(child) = child else {
            return Ok(self.root.clone());
        };
        let path = self.root.join(child);
        godot_mcp_domain::sysfs::open_private_state_dir(&path)
            .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
        Ok(path)
    }

    /// Where the debug keystore is kept.
    pub(crate) fn keystore(&self) -> PathBuf {
        self.root.join("debug.keystore")
    }

    /// The home `adb` runs with, which keeps its key, so a device authorizes
    /// this server once.
    pub(crate) fn adb_home(&self) -> Result<PathBuf, OutcomeError> {
        self.private(Some("adb-home"))
    }

    /// Scratch for tool capture files and a deploy's copy of its APK.
    pub(crate) fn scratch(&self) -> Result<PathBuf, OutcomeError> {
        self.private(Some("scratch"))
    }
}

/// `PATH` holding the JDK's `bin` first, then the system's, for a tool such
/// as `apksigner`, which runs `java` from `PATH`.
fn java_path(jdk: &Jdk) -> String {
    let mut entries = vec![jdk.bin()];
    if cfg!(unix) {
        entries.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    }
    std::env::join_paths(entries).map_or_else(
        |_| jdk.bin().display().to_string(),
        |joined| joined.to_string_lossy().into_owned(),
    )
}

/// The environment of a JDK or build-tools run: a private home and the JDK.
pub(crate) fn java_environment(home: &Path, jdk: &Jdk) -> Vec<(String, String)> {
    vec![
        ("HOME".to_owned(), home.display().to_string()),
        ("JAVA_HOME".to_owned(), jdk.home().display().to_string()),
        ("PATH".to_owned(), java_path(jdk)),
    ]
}

/// Run one tool; a tool that could not start is `unavailable`.
fn run_tool(
    program: &Path,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    scratch: &Path,
    deadline: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<ToolOutput, String> {
    run(
        &ToolRun {
            program,
            arguments,
            environment,
            directory: scratch,
            deadline,
            output_cap: TOOL_OUTPUT_CAP,
            scratch,
        },
        cancelled,
    )
    .map_err(|error| error.to_string())
}

/// The bounded text of a tool's output an answer carries.
fn excerpt(text: &str) -> (String, bool) {
    let trimmed = text.trim_end();
    let mut kept: String = trimmed.chars().take(PHASE_OUTPUT_CHARS).collect();
    let truncated = trimmed.chars().count() > PHASE_OUTPUT_CHARS;
    if truncated {
        kept.push('…');
    }
    (kept, truncated)
}

/// The server's debug keystore, created once with the JDK's `keytool`.
///
/// # Errors
/// `unavailable` when the JDK has no `keytool` or it did not create the
/// keystore.
pub(crate) fn ensure_debug_keystore(
    dirs: &AndroidDirs,
    jdk: &Jdk,
) -> Result<PathBuf, OutcomeError> {
    let _held = KEYSTORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let keystore = dirs.keystore();
    if std::fs::metadata(&keystore).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0) {
        return Ok(keystore);
    }
    let keytool = jdk.keytool().map_err(|error| sdk_error(&error))?;
    let scratch = dirs.scratch()?;
    let fresh = dirs
        .private(None)?
        .join(format!("debug.keystore.new-{}", std::process::id()));
    let _ = std::fs::remove_file(&fresh);
    let output = run_tool(
        &keytool,
        [
            "-genkeypair",
            "-v",
            "-keystore",
            &fresh.display().to_string(),
            "-storepass",
            DEBUG_KEY_PASSWORD,
            "-alias",
            DEBUG_KEY_ALIAS,
            "-keypass",
            DEBUG_KEY_PASSWORD,
            "-keyalg",
            "RSA",
            "-keysize",
            "2048",
            "-validity",
            "10000",
            "-dname",
            "CN=Android Debug,O=Android,C=US",
            "-noprompt",
        ]
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect(),
        java_environment(&scratch, jdk),
        &scratch,
        KEYTOOL_DEADLINE,
        &|| false,
    )
    .map_err(unavailable)?;
    let created =
        std::fs::metadata(&fresh).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
    if !output.exit.succeeded() || !created {
        let _ = std::fs::remove_file(&fresh);
        return Err(unavailable(format!(
            "keytool did not create the debug keystore ({}): {}",
            output.exit.describe(),
            excerpt(&output.combined()).0
        )));
    }
    std::fs::rename(&fresh, &keystore)
        .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))?;
    Ok(keystore)
}

/// What an Android export runs with, fixed when it starts. Private: it names
/// host paths and is never shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AndroidExport {
    /// The operator's SDK.
    pub(crate) sdk: PathBuf,
    /// The operator's JDK.
    pub(crate) java_home: PathBuf,
    /// The export's own Godot home.
    pub(crate) home: PathBuf,
    /// The SDK view the export's editor settings name.
    pub(crate) sdk_view: PathBuf,
    /// `server-debug` (the server's debug keystore), `project-debug` or
    /// `project-release` (the project's credentials store).
    pub(crate) keystore: String,
}

/// A Godot string literal of `text`.
fn godot_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The editor settings file name of the build labelled `label`
/// (`4.7.1.stable` writes `editor_settings-4.7.tres`).
fn editor_settings_name(label: &str) -> String {
    let mut parts = label.split('.');
    let major = parts.next().unwrap_or("4");
    let minor = parts.next().unwrap_or("0");
    format!("editor_settings-{major}.{minor}.tres")
}

/// Prepare the private home and SDK view of one Android export inside its
/// session directory.
///
/// # Errors
/// `internal` when the private files cannot be written; `unsupported` on a
/// host where the SDK view cannot be linked.
pub(crate) fn prepare_export(
    session_directory: &Path,
    label: &str,
    sdk: &AndroidSdk,
    jdk: &Jdk,
    keystore: Option<&Path>,
    keystore_kind: &str,
) -> Result<AndroidExport, OutcomeError> {
    if !cfg!(unix) {
        return Err(outcome_error(
            ErrorKind::Unsupported,
            "Android exports run on Linux hosts only",
        ));
    }
    let internal = |error: std::io::Error| outcome_error(ErrorKind::Internal, error.to_string());
    let home = session_directory.join("android-home");
    let sdk_view = session_directory.join("android-sdk");
    let settings = home.join(".config").join("godot");
    std::fs::create_dir_all(&settings).map_err(internal)?;
    let tools = sdk_view.join("platform-tools");
    std::fs::create_dir_all(&tools).map_err(internal)?;
    let adb = tools.join("adb");
    std::fs::write(
        &adb,
        "#!/bin/sh\n# godot-mcp: an export never reaches a device; deploy with android.deploy\nexit 1\n",
    )
    .map_err(internal)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o700)).map_err(internal)?;
        std::os::unix::fs::symlink(sdk.root().join("build-tools"), sdk_view.join("build-tools"))
            .map_err(internal)?;
    }
    let mut text = String::from("[gd_resource type=\"EditorSettings\" format=3]\n\n[resource]\n\n");
    text.push_str(&format!(
        "export/android/android_sdk_path = {}\n",
        godot_string(&sdk_view.display().to_string())
    ));
    text.push_str(&format!(
        "export/android/java_sdk_path = {}\n",
        godot_string(&jdk.home().display().to_string())
    ));
    if let Some(keystore) = keystore {
        text.push_str(&format!(
            "export/android/debug_keystore = {}\n",
            godot_string(&keystore.display().to_string())
        ));
        text.push_str(&format!(
            "export/android/debug_keystore_user = {}\n",
            godot_string(DEBUG_KEY_ALIAS)
        ));
        text.push_str(&format!(
            "export/android/debug_keystore_pass = {}\n",
            godot_string(DEBUG_KEY_PASSWORD)
        ));
    }
    std::fs::write(settings.join(editor_settings_name(label)), text).map_err(internal)?;
    Ok(AndroidExport {
        sdk: sdk.root().to_path_buf(),
        java_home: jdk.home().to_path_buf(),
        home,
        sdk_view,
        keystore: keystore_kind.to_owned(),
    })
}

/// The environment an Android export's Godot runs with: its own home, and
/// the JDK on `PATH` for the `apksigner` Godot signs with.
pub(crate) fn export_environment(plan: &AndroidExport) -> Vec<(String, String)> {
    java_environment(&plan.home, &Jdk::new(&plan.java_home))
}

/// What an exported APK declares and how it was verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ApkRecord {
    pub(crate) output: String,
    pub(crate) package: String,
    pub(crate) launcher: Option<String>,
    pub(crate) launcher_kind: Option<String>,
    pub(crate) target_activity: Option<String>,
    pub(crate) version_code: Option<i64>,
    pub(crate) version_name: Option<String>,
    pub(crate) min_sdk: Option<i64>,
    pub(crate) target_sdk: Option<i64>,
    pub(crate) debuggable: Option<bool>,
    pub(crate) abis: Vec<String>,
    pub(crate) entries: usize,
    pub(crate) asset_entries: usize,
    pub(crate) apksigner_version: String,
    pub(crate) signer_sha256: Option<String>,
    /// Which keystore Godot signed with: `server-debug`, `project-debug` or
    /// `project-release`.
    #[serde(default)]
    pub(crate) keystore: String,
}

impl ApkRecord {
    /// The component `am start -n` names: `<package>/<launcher class>`.
    pub(crate) fn component(&self) -> Option<String> {
        self.launcher
            .as_ref()
            .map(|launcher| format!("{}/{launcher}", self.package))
    }

    pub(crate) fn public(&self) -> Value {
        json!({
            "output": self.output,
            "package": self.package,
            "launcher": self.launcher,
            "launcher_kind": self.launcher_kind,
            "target_activity": self.target_activity,
            "version_code": self.version_code,
            "version_name": self.version_name,
            "min_sdk": self.min_sdk,
            "target_sdk": self.target_sdk,
            "debuggable": self.debuggable,
            "abis": self.abis,
            "entries": self.entries,
            "asset_entries": self.asset_entries,
            "signature": {
                "verified": true,
                "verifier": format!("apksigner {}", self.apksigner_version),
                "signer_sha256": self.signer_sha256,
                "keystore": self.keystore,
            },
        })
    }
}

/// Check the APK `output` an Android export wrote at `path`.
///
/// # Errors
/// The reason the APK is not a usable export.
pub(crate) fn verify_apk(
    output: &str,
    path: &Path,
    bytes: &[u8],
    plan: &AndroidExport,
    scratch: &Path,
) -> Result<ApkRecord, String> {
    let facts = read_apk(bytes).map_err(|error| error.to_string())?;
    let jdk = Jdk::new(&plan.java_home);
    let (version, apksigner) = AndroidSdk::new(&plan.sdk)
        .apksigner()
        .map_err(|error| error.to_string())?;
    let verified = run_tool(
        &apksigner,
        vec![
            "verify".to_owned(),
            "--print-certs".to_owned(),
            path.display().to_string(),
        ],
        java_environment(scratch, &jdk),
        scratch,
        APKSIGNER_DEADLINE,
        &|| false,
    )?;
    if !verified.exit.succeeded() {
        return Err(format!(
            "apksigner {version} does not verify the APK ({}): {}",
            verified.exit.describe(),
            excerpt(&verified.combined()).0
        ));
    }
    let signer_sha256 = signer_sha256(&verified.stdout.text);
    let launcher = facts.manifest.launcher.as_ref();
    Ok(ApkRecord {
        output: output.to_owned(),
        package: facts.manifest.package.clone(),
        launcher: launcher.map(|launcher| launcher.name.clone()),
        launcher_kind: launcher.map(|launcher| launcher.kind.as_str().to_owned()),
        target_activity: launcher.and_then(|launcher| launcher.target_activity.clone()),
        version_code: facts.manifest.version_code,
        version_name: facts.manifest.version_name.clone(),
        min_sdk: facts.manifest.min_sdk,
        target_sdk: facts.manifest.target_sdk,
        debuggable: facts.manifest.debuggable,
        abis: facts.abis,
        entries: facts.entries,
        asset_entries: facts.asset_entries,
        apksigner_version: version,
        signer_sha256,
        keystore: plan.keystore.clone(),
    })
}

/// The first signer's certificate digest `apksigner verify --print-certs`
/// printed. build-tools 35.0.0 and 36.0.0 print it as `Signer #1 certificate
/// SHA-256 digest: <hex>`, 37.0.0 as `V3.0 Signer: certificate SHA-256
/// digest: <hex>`.
fn signer_sha256(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|line| {
            line.split_once("certificate SHA-256 digest:")
                .map(|(_, digest)| digest.trim().to_owned())
        })
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// What every adb call of one deploy runs with: the SDK's `adb`, a scratch
/// directory and adb's environment.
type AdbSetup = (PathBuf, PathBuf, Vec<(String, String)>);

fn adb_setup(tools: &AndroidTools, dirs: &AndroidDirs) -> Result<AdbSetup, OutcomeError> {
    Ok((tools.adb()?, dirs.scratch()?, adb_environment(dirs)?))
}

/// The environment `adb` runs with: the server's private adb home.
fn adb_environment(dirs: &AndroidDirs) -> Result<Vec<(String, String)>, OutcomeError> {
    Ok(vec![(
        "HOME".to_owned(),
        dirs.adb_home()?.display().to_string(),
    )])
}

fn device_value(device: &Device) -> Value {
    json!({
        "serial": device.serial,
        "state": device.state,
        "ready": device.ready(),
        "model": device.model,
        "product": device.product,
        "device": device.device,
        "transport": device.transport(),
        "transport_id": device.transport_id,
        "usb": device.usb,
    })
}

/// Whether adb said it started its server for this call.
fn started_server(output: &ToolOutput) -> bool {
    output.combined().contains("daemon started successfully")
}

/// The devices `adb devices -l` lists now.
///
/// # Errors
/// `unavailable` when no SDK is configured, it has no `adb`, or `adb` did not
/// answer.
pub(crate) fn list_devices(
    tools: &AndroidTools,
    dirs: &AndroidDirs,
) -> Result<Value, OutcomeError> {
    let adb = tools.adb()?;
    let scratch = dirs.scratch()?;
    let output = run_tool(
        &adb,
        vec!["devices".to_owned(), "-l".to_owned()],
        adb_environment(dirs)?,
        &scratch,
        DEVICES_DEADLINE,
        &|| false,
    )
    .map_err(unavailable)?;
    if !output.exit.succeeded() {
        return Err(unavailable(format!(
            "adb devices did not answer ({}): {}",
            output.exit.describe(),
            excerpt(&output.combined()).0
        )));
    }
    let devices: Vec<Value> = parse_devices(&output.stdout.text)
        .iter()
        .map(device_value)
        .collect();
    Ok(json!({
        "devices": devices,
        "started_server": started_server(&output),
        "output_truncated": output.stdout.truncated,
    }))
}

/// What one deploy is asked to do, after its export was checked.
#[derive(Debug, Clone)]
pub(crate) struct DeployPlan {
    pub(crate) export: String,
    pub(crate) serial: String,
    pub(crate) apk: Vec<u8>,
    pub(crate) package: String,
    pub(crate) component: String,
    pub(crate) running_timeout: Duration,
    pub(crate) logcat_lines: u32,
}

/// How a deploy ended: the three-state answer of its outward effect, the
/// error naming the phase that failed, and every phase's report.
#[derive(Debug, Clone)]
pub(crate) struct DeployReport {
    pub(crate) status: MutationStatus,
    pub(crate) error: Option<(ErrorKind, String)>,
    pub(crate) payload: Value,
}

/// One phase's report.
struct Phase {
    name: &'static str,
    outcome: &'static str,
    detail: String,
    exit: Option<String>,
    output: Option<(String, bool)>,
    elapsed: Duration,
}

impl Phase {
    fn value(&self) -> Value {
        json!({
            "phase": self.name,
            "outcome": self.outcome,
            "detail": self.detail,
            "exit": self.exit,
            "output": self.output.as_ref().map(|(text, _)| text),
            "output_truncated": self.output.as_ref().is_some_and(|(_, truncated)| *truncated),
            "elapsed_ms": u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX),
        })
    }
}

/// The deploy's phases, in order, and what is known so far.
struct Deploy<'a> {
    plan: &'a DeployPlan,
    phases: Vec<Phase>,
    pid: Option<u32>,
    logcat: Option<Value>,
}

impl Deploy<'_> {
    fn report(
        mut self,
        status: MutationStatus,
        error: Option<(ErrorKind, &'static str, String)>,
    ) -> DeployReport {
        let failed = error.as_ref().map(|(_, phase, _)| *phase);
        // A phase that failed before its tool answered is reported failed
        // with the reason.
        if let Some((_, phase, message)) = &error
            && !self.phases.iter().any(|reported| reported.name == *phase)
        {
            self.phases.push(Phase {
                name: phase,
                outcome: "failed",
                detail: message.clone(),
                exit: None,
                output: None,
                elapsed: Duration::ZERO,
            });
        }
        // Every phase after the one that failed did not run.
        let ran: Vec<&str> = self.phases.iter().map(|phase| phase.name).collect();
        for name in ["device", "install", "launch", "running", "logcat"] {
            if !ran.contains(&name) {
                self.phases.push(Phase {
                    name,
                    outcome: "not_run",
                    detail: failed.map_or_else(String::new, |phase| {
                        format!("not run: the {phase} phase did not succeed")
                    }),
                    exit: None,
                    output: None,
                    elapsed: Duration::ZERO,
                });
            }
        }
        let payload = json!({
            "export": self.plan.export,
            "serial": self.plan.serial,
            "package": (!self.plan.package.is_empty()).then_some(&self.plan.package),
            "component": (!self.plan.component.is_empty()).then_some(&self.plan.component),
            // Installed only when adb said so, not installed only when the
            // device refused or was never reached, and unknown otherwise.
            "installed": match status {
                MutationStatus::Committed => Some(true),
                MutationStatus::NotStarted => Some(false),
                _ => None,
            },
            "pid": self.pid,
            "failed_phase": failed,
            "phases": self.phases.iter().map(Phase::value).collect::<Vec<_>>(),
            "logcat": self.logcat,
        });
        DeployReport {
            status,
            error: error.map(|(kind, phase, message)| {
                (
                    kind,
                    format!("android.deploy stopped at the {phase} phase: {message}"),
                )
            }),
            payload,
        }
    }
}

fn tool_phase(
    name: &'static str,
    outcome: &'static str,
    detail: String,
    output: &ToolOutput,
) -> Phase {
    Phase {
        name,
        outcome,
        detail,
        exit: Some(output.exit.describe()),
        output: Some(excerpt(&output.combined())),
        elapsed: output.elapsed,
    }
}

/// Install `plan`'s APK on its device and prove it runs.
///
/// The phases run in order, each with its own report: the device check (the
/// device must be listed in the `device` state, or nothing is sent),
/// `adb -s <serial> install -r`, the launch (`am start -W -n` of the
/// component the APK's manifest names as its launcher), the running proof
/// (`pidof <package>` until a process appears or the deadline passes) and a
/// bounded `logcat` excerpt of that process. The deploy succeeds only when
/// every phase did. A failure before the install changed nothing on the
/// device; a definite refusal by the package manager changed nothing either;
/// an install whose outcome nothing proves — a lost connection, a killed
/// client — is unresolved; a failure after the install is committed, with
/// the error naming its phase.
pub(crate) fn deploy(
    tools: &AndroidTools,
    dirs: &AndroidDirs,
    plan: &DeployPlan,
    cancelled: &dyn Fn() -> bool,
) -> DeployReport {
    let mut deploy = Deploy {
        plan,
        phases: Vec::new(),
        pid: None,
        logcat: None,
    };
    let (adb, scratch, environment) = match adb_setup(tools, dirs) {
        Ok(setup) => setup,
        Err(error) => {
            return deploy.report(
                MutationStatus::NotStarted,
                Some((error.kind(), "device", error.message().to_owned())),
            );
        }
    };
    let adb_run = |arguments: Vec<String>, deadline: Duration| {
        run_tool(
            &adb,
            arguments,
            environment.clone(),
            &scratch,
            deadline,
            cancelled,
        )
    };
    let serial = plan.serial.clone();
    let on_device = |rest: &[&str]| -> Vec<String> {
        let mut arguments = vec!["-s".to_owned(), serial.clone()];
        arguments.extend(rest.iter().map(|argument| (*argument).to_owned()));
        arguments
    };

    // 1. The device must be listed, ready.
    let listed = match adb_run(
        vec!["devices".to_owned(), "-l".to_owned()],
        DEVICES_DEADLINE,
    ) {
        Ok(output) => output,
        Err(error) => {
            return deploy.report(
                MutationStatus::NotStarted,
                Some((ErrorKind::Unavailable, "device", error)),
            );
        }
    };
    let devices = parse_devices(&listed.stdout.text);
    let found = devices.iter().find(|device| device.serial == plan.serial);
    let refusal = if !listed.exit.succeeded() {
        Some((
            ErrorKind::Unavailable,
            format!("adb devices did not answer ({})", listed.exit.describe()),
        ))
    } else {
        match found {
            None => Some((
                ErrorKind::NotFound,
                format!(
                    "no device `{}` is attached; adb lists {}",
                    plan.serial,
                    if devices.is_empty() {
                        "no device".to_owned()
                    } else {
                        devices
                            .iter()
                            .map(|device| format!("{} ({})", device.serial, device.state))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ),
            )),
            Some(device) if !device.ready() => Some((
                ErrorKind::Unavailable,
                format!(
                    "device `{}` is {}, not ready: {}",
                    device.serial,
                    device.state,
                    match device.state.as_str() {
                        "unauthorized" => "accept the USB debugging prompt on the device",
                        "offline" => "reconnect it or restart its adb connection",
                        _ => "make it reachable to adb",
                    }
                ),
            )),
            Some(_) => None,
        }
    };
    if let Some((kind, detail)) = refusal {
        deploy
            .phases
            .push(tool_phase("device", "failed", detail.clone(), &listed));
        return deploy.report(MutationStatus::NotStarted, Some((kind, "device", detail)));
    }
    deploy.phases.push(tool_phase(
        "device",
        "succeeded",
        format!("device `{}` is ready", plan.serial),
        &listed,
    ));

    // 2. Install.
    let apk = scratch.join(format!(
        "deploy-{}-{}.apk",
        std::process::id(),
        crate::artifacts::sha256_hex(format!("{}\n{}", plan.export, plan.serial).as_bytes())
            .get(..16)
            .unwrap_or_default()
    ));
    if let Err(error) = std::fs::write(&apk, &plan.apk) {
        return deploy.report(
            MutationStatus::NotStarted,
            Some((
                ErrorKind::Internal,
                "install",
                format!("the APK could not be staged for adb: {error}"),
            )),
        );
    }
    let installed = adb_run(
        on_device(&["install", "-r", &apk.display().to_string()]),
        INSTALL_DEADLINE,
    );
    let _ = std::fs::remove_file(&apk);
    let installed = match installed {
        Ok(output) => output,
        Err(error) => {
            return deploy.report(
                MutationStatus::NotStarted,
                Some((ErrorKind::Unavailable, "install", error)),
            );
        }
    };
    match parse_install(&installed.combined(), installed.exit.succeeded()) {
        Install::Installed => deploy.phases.push(tool_phase(
            "install",
            "succeeded",
            format!("adb installed {}", plan.package),
            &installed,
        )),
        Install::Refused(reason) => {
            deploy.phases.push(tool_phase(
                "install",
                "failed",
                format!("the device refused the APK: {reason}"),
                &installed,
            ));
            return deploy.report(
                MutationStatus::NotStarted,
                Some((
                    ErrorKind::Conflict,
                    "install",
                    format!("the device's package manager refused the APK: {reason}"),
                )),
            );
        }
        Install::NotReached(line) => {
            deploy.phases.push(tool_phase(
                "install",
                "failed",
                format!("adb did not reach the device: {line}"),
                &installed,
            ));
            return deploy.report(
                MutationStatus::NotStarted,
                Some((
                    ErrorKind::Unavailable,
                    "install",
                    format!("adb did not reach the device: {line}"),
                )),
            );
        }
        Install::Unknown(line) => {
            let detail = match installed.exit {
                ToolExit::Cancelled => "the call was cancelled while adb was installing".to_owned(),
                ToolExit::TimedOut => {
                    format!("adb did not finish within {} s", INSTALL_DEADLINE.as_secs())
                }
                _ => format!(
                    "adb ended without a verdict ({}): {line}",
                    installed.exit.describe()
                ),
            };
            deploy
                .phases
                .push(tool_phase("install", "unknown", detail.clone(), &installed));
            return deploy.report(
                MutationStatus::Unresolved,
                Some((
                    ErrorKind::Uncertain,
                    "install",
                    format!("whether the APK was installed is unknown: {detail}"),
                )),
            );
        }
    }

    // 3. Launch the component the APK names as its launcher.
    let launched = match adb_run(
        on_device(&["shell", "am", "start", "-W", "-n", &plan.component]),
        LAUNCH_DEADLINE,
    ) {
        Ok(output) => output,
        Err(error) => {
            return deploy.report(
                MutationStatus::Committed,
                Some((ErrorKind::Unavailable, "launch", error)),
            );
        }
    };
    match parse_launch(&launched.combined(), launched.exit.succeeded()) {
        Launch::Started { activity, state } => deploy.phases.push(tool_phase(
            "launch",
            "succeeded",
            format!(
                "the activity manager started {}{}",
                activity.unwrap_or_else(|| plan.component.clone()),
                state.map(|state| format!(" ({state})")).unwrap_or_default()
            ),
            &launched,
        )),
        Launch::Failed(line) => {
            let kind = if launched.exit == ToolExit::Cancelled {
                ErrorKind::Cancelled
            } else {
                ErrorKind::Unavailable
            };
            deploy
                .phases
                .push(tool_phase("launch", "failed", line.clone(), &launched));
            return deploy.report(
                MutationStatus::Committed,
                Some((
                    kind,
                    "launch",
                    format!(
                        "the APK is installed, but {} did not start: {line}",
                        plan.component
                    ),
                )),
            );
        }
    }

    // 4. Prove a process runs.
    let started = Instant::now();
    let mut polls = 0_u32;
    let mut last: Option<ToolOutput> = None;
    while deploy.pid.is_none() {
        if cancelled() {
            break;
        }
        polls += 1;
        match adb_run(
            on_device(&["shell", "pidof", &plan.package]),
            PIDOF_DEADLINE,
        ) {
            Ok(output) => {
                deploy.pid = parse_pids(&output.stdout.text).first().copied();
                last = Some(output);
            }
            Err(error) => {
                return deploy.report(
                    MutationStatus::Committed,
                    Some((ErrorKind::Unavailable, "running", error)),
                );
            }
        }
        if deploy.pid.is_some() || started.elapsed() >= plan.running_timeout {
            break;
        }
        std::thread::sleep(
            PIDOF_INTERVAL.min(plan.running_timeout.saturating_sub(started.elapsed())),
        );
    }
    let waited = started.elapsed();
    let Some(pid) = deploy.pid else {
        let detail = if cancelled() {
            "the call was cancelled before a process was seen".to_owned()
        } else {
            format!(
                "no process of {} after {} ms ({polls} pidof checks)",
                plan.package,
                waited.as_millis()
            )
        };
        deploy.phases.push(Phase {
            name: "running",
            outcome: "failed",
            detail: detail.clone(),
            exit: last.as_ref().map(|output| output.exit.describe()),
            output: last.as_ref().map(|output| excerpt(&output.combined())),
            elapsed: waited,
        });
        return deploy.report(
            MutationStatus::Committed,
            Some((
                if cancelled() {
                    ErrorKind::Cancelled
                } else {
                    ErrorKind::Timeout
                },
                "running",
                format!("the app was installed and launched, but {detail}"),
            )),
        );
    };
    deploy.phases.push(Phase {
        name: "running",
        outcome: "succeeded",
        detail: format!(
            "process {pid} of {} runs ({polls} pidof checks)",
            plan.package
        ),
        exit: last.as_ref().map(|output| output.exit.describe()),
        output: last.as_ref().map(|output| excerpt(&output.combined())),
        elapsed: waited,
    });

    // 5. A bounded logcat excerpt of that process.
    let lines = plan.logcat_lines.to_string();
    let pid_filter = format!("--pid={pid}");
    let logcat = match adb_run(
        on_device(&["logcat", "-d", "-t", &lines, &pid_filter]),
        LOGCAT_DEADLINE,
    ) {
        Ok(output) => output,
        Err(error) => {
            return deploy.report(
                MutationStatus::Committed,
                Some((ErrorKind::Unavailable, "logcat", error)),
            );
        }
    };
    if !logcat.exit.succeeded() {
        let detail = format!("adb logcat did not answer ({})", logcat.exit.describe());
        deploy
            .phases
            .push(tool_phase("logcat", "failed", detail.clone(), &logcat));
        return deploy.report(
            MutationStatus::Committed,
            Some((ErrorKind::Unavailable, "logcat", detail)),
        );
    }
    let kept: Vec<&str> = logcat.stdout.text.lines().collect();
    let limit = usize::try_from(plan.logcat_lines).unwrap_or(usize::MAX);
    let start = kept.len().saturating_sub(limit);
    deploy.logcat = Some(json!({
        "pid": pid,
        "lines": kept[start..],
        "truncated": logcat.stdout.truncated || start > 0,
        "bytes": logcat.stdout.bytes,
    }));
    deploy.phases.push(Phase {
        name: "logcat",
        outcome: "succeeded",
        detail: format!("{} lines of process {pid}", kept.len() - start),
        exit: Some(logcat.exit.describe()),
        output: None,
        elapsed: logcat.elapsed,
    });
    deploy.report(MutationStatus::Committed, None)
}

/// What one deploy is asked for, as its journaled command carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeployRequest {
    /// The export session whose APK is installed.
    pub(crate) export: String,
    /// The device's serial, as `adb devices -l` lists it.
    pub(crate) serial: String,
    /// How long the running proof waits for a process.
    pub(crate) running_timeout: Duration,
    /// How many logcat lines of that process are read.
    pub(crate) logcat_lines: u32,
}

impl DeployRequest {
    /// The journaled command's payload.
    pub(crate) fn payload(&self) -> Value {
        json!({
            "export": self.export,
            "serial": self.serial,
            "running_timeout_ms": u64::try_from(self.running_timeout.as_millis()).unwrap_or(u64::MAX),
            "logcat_lines": self.logcat_lines,
        })
    }

    /// The request a journaled command's payload carries.
    pub(crate) fn from_payload(payload: &Value) -> Option<Self> {
        Some(Self {
            export: payload.get("export")?.as_str()?.to_owned(),
            serial: payload.get("serial")?.as_str()?.to_owned(),
            running_timeout: Duration::from_millis(payload.get("running_timeout_ms")?.as_u64()?),
            logcat_lines: u32::try_from(payload.get("logcat_lines")?.as_u64()?).ok()?,
        })
    }
}

/// Whether `serial` is one adb could name: printable, without whitespace or
/// a character a device shell would read.
pub(crate) fn valid_serial(serial: &str) -> bool {
    !serial.is_empty()
        && serial.len() <= 256
        && !serial.starts_with('-')
        && serial
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !b"'\"`$;&|<>\\".contains(&byte))
}

impl super::SessionManager {
    /// The APK a deploy of export `id` installs: the export must be a
    /// finished, successful Android export whose APK artifact is still
    /// served.
    ///
    /// # Errors
    /// `not_found` for an unknown session, `invalid_input` for one that is
    /// not an Android export, and `unavailable` for an export that did not
    /// succeed or whose artifact expired.
    pub(crate) fn deployable(
        &self,
        id: &str,
    ) -> Result<(ApkRecord, super::export::ExportOutput), OutcomeError> {
        let session = self.session(id)?;
        let record = session.export_record().ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "{id} is a {} session, not an export",
                    session.kind().as_str()
                ),
            )
        })?;
        if record.outcome.as_deref() != Some("succeeded") {
            return Err(unavailable(format!(
                "export {id} has not succeeded (it is {}); deploy a finished Android export",
                record.outcome.as_deref().unwrap_or("still running")
            )));
        }
        let apk = record.apk.clone().ok_or_else(|| {
            outcome_error(
                ErrorKind::InvalidInput,
                format!(
                    "export {id} exported a {} {}, not an Android APK",
                    record.platform,
                    record.kind.as_str()
                ),
            )
        })?;
        let output = record.apk_output().cloned().ok_or_else(|| {
            outcome_error(
                ErrorKind::Internal,
                format!("export {id} names no output for its APK"),
            )
        })?;
        let store = self.export_store();
        let live = output.expires_at_unix_ms > super::now_unix_ms()
            && store
                .as_ref()
                .is_some_and(|store| store.contract(&output.artifact).is_some());
        if !live {
            return Err(unavailable(format!(
                "the APK of export {id} is no longer served: its artifact expired; export it again"
            )));
        }
        Ok((apk, output))
    }

    /// Install the APK of the export `request` names on its device and prove
    /// it runs; see [`deploy`].
    pub(crate) fn deploy(
        &self,
        request: &DeployRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> DeployReport {
        // Nothing was sent to any device: every phase is reported not run.
        let refused = |kind: ErrorKind, message: String| {
            let plan = DeployPlan {
                export: request.export.clone(),
                serial: request.serial.clone(),
                apk: Vec::new(),
                package: String::new(),
                component: String::new(),
                running_timeout: request.running_timeout,
                logcat_lines: request.logcat_lines,
            };
            Deploy {
                plan: &plan,
                phases: Vec::new(),
                pid: None,
                logcat: None,
            }
            .report(MutationStatus::NotStarted, Some((kind, "device", message)))
        };
        let (apk, output) = match self.deployable(&request.export) {
            Ok(found) => found,
            Err(error) => return refused(error.kind(), error.message().to_owned()),
        };
        let Some(component) = apk.component() else {
            return refused(
                ErrorKind::Unsupported,
                format!(
                    "the APK of export {} declares no launcher activity to start",
                    request.export
                ),
            );
        };
        let bytes = self.export_store().map_or_else(
            || Err(unavailable("this server keeps no artifact store")),
            |store| {
                godot_mcp_domain::contracts::ByteRange::new(0, output.bytes.saturating_sub(1))
                    .map_err(|error| outcome_error(ErrorKind::Internal, error.to_string()))
                    .and_then(|range| store.read_range(&output.artifact, &range))
            },
        );
        let bytes = match bytes {
            Ok(bytes) if crate::artifacts::sha256_hex(&bytes) == output.sha256 => bytes,
            Ok(_) => {
                return refused(
                    ErrorKind::Internal,
                    format!(
                        "the APK artifact of export {} no longer has its recorded SHA-256",
                        request.export
                    ),
                );
            }
            Err(error) => return refused(error.kind(), error.message().to_owned()),
        };
        deploy(
            &self.android,
            &self.android_dirs,
            &DeployPlan {
                export: request.export.clone(),
                serial: request.serial.clone(),
                apk: bytes,
                package: apk.package.clone(),
                component,
                running_timeout: request.running_timeout,
                logcat_lines: request.logcat_lines,
            },
            cancelled,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deploy_request_round_trips_through_its_journaled_payload() {
        let request = DeployRequest {
            export: format!("export_{}", "a".repeat(32)),
            serial: "emulator-5554".to_owned(),
            running_timeout: Duration::from_millis(15_000),
            logcat_lines: 200,
        };
        assert_eq!(
            DeployRequest::from_payload(&request.payload()),
            Some(request)
        );
        assert_eq!(DeployRequest::from_payload(&json!({ "export": "x" })), None);
    }

    #[test]
    fn the_signer_is_read_in_either_format_apksigner_prints() {
        let digest = "1106e3b38443b6960ff0a199bc54330d5287d4087dc22f138d9bb098dffffbfa";
        // build-tools 35.0.0 and 36.0.0.
        let older = format!(
            "Signer #1 certificate DN: CN=Android Debug, O=Android, C=US\n\
             Signer #1 certificate SHA-256 digest: {digest}\n\
             Signer #1 certificate SHA-1 digest: f13e0055a2da9e76d368f394f6f08a6411dccdd4\n"
        );
        // build-tools 37.0.0.
        let newer = format!(
            "V3.0 Signer: certificate DN: CN=Android Debug, O=Android, C=US\n\
             V3.0 Signer: certificate SHA-256 digest: {digest}\n\
             V3.0 Signer: certificate SHA-1 digest: f13e0055a2da9e76d368f394f6f08a6411dccdd4\n"
        );
        for printed in [older, newer] {
            assert_eq!(
                signer_sha256(&printed).as_deref(),
                Some(digest),
                "{printed}"
            );
        }
        assert_eq!(signer_sha256("Verifies\n"), None);
        assert_eq!(
            signer_sha256("Signer #1 certificate SHA-256 digest: not-hex\n"),
            None
        );
    }

    #[test]
    fn a_serial_is_one_word_adb_can_name() {
        for serial in [
            "emulator-5554",
            "R5CR1234567",
            "192.168.1.5:5555",
            "adb-XYZ._adb-tls-connect._tcp",
        ] {
            assert!(valid_serial(serial), "{serial}");
        }
        for serial in [
            "", "-s", "a b", "a;b", "a$(x)", "a`b`", "a\\b", "a'b", "a\"b", "a|b", "a&b",
        ] {
            assert!(!valid_serial(serial), "{serial}");
        }
    }

    #[test]
    fn editor_settings_are_named_after_the_builds_minor_version() {
        assert_eq!(
            editor_settings_name("4.7.1.stable"),
            "editor_settings-4.7.tres"
        );
        assert_eq!(
            editor_settings_name("4.4.stable"),
            "editor_settings-4.4.tres"
        );
    }

    #[test]
    fn a_path_in_the_editor_settings_is_one_godot_string() {
        assert_eq!(godot_string("/tmp/a\"b\\c"), "\"/tmp/a\\\"b\\\\c\"");
    }

    #[test]
    fn a_phase_output_is_bounded_and_says_so() {
        let long = "x".repeat(PHASE_OUTPUT_CHARS + 10);
        let (kept, truncated) = excerpt(&long);
        assert!(truncated);
        assert_eq!(kept.chars().count(), PHASE_OUTPUT_CHARS + 1);
        assert_eq!(excerpt("short\n"), ("short".to_owned(), false));
    }
}
