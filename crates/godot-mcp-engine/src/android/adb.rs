//! What `adb` answers, read without guessing.
//!
//! Each reader keeps what the output proves apart from what it does not: an
//! install is installed only on adb's `Success` line, refused only on the
//! package manager's `Failure [...]`, and unknown otherwise — a lost
//! connection or a killed client says nothing about what the device did.

/// One device `adb devices -l` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Its serial, as `-s` names it.
    pub serial: String,
    /// Its state: `device`, `unauthorized`, `offline`, `no permissions`, ...
    pub state: String,
    /// `model:`, when listed.
    pub model: Option<String>,
    /// `product:`, when listed.
    pub product: Option<String>,
    /// `device:`, when listed.
    pub device: Option<String>,
    /// `transport_id:`, when listed.
    pub transport_id: Option<String>,
    /// `usb:`, when listed: the USB port of a device attached by USB.
    pub usb: Option<String>,
}

impl Device {
    /// Whether adb can reach it: its state is `device`.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.state == "device"
    }

    /// How it is attached: `usb`, `tcp` for a serial that is a network
    /// address, `emulator` for an emulator console serial, or `unknown`.
    #[must_use]
    pub fn transport(&self) -> &'static str {
        if self.usb.is_some() {
            "usb"
        } else if self.serial.starts_with("emulator-") {
            "emulator"
        } else if self
            .serial
            .rsplit_once(':')
            .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
        {
            "tcp"
        } else {
            "unknown"
        }
    }
}

/// Read the device list of `adb devices -l`.
///
/// Lines adb prints while starting its server (`* daemon ...`) and the list
/// header are skipped; every other non-empty line is one device.
#[must_use]
pub fn parse_devices(output: &str) -> Vec<Device> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty() && !line.starts_with('*') && !line.starts_with("List of devices")
        })
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let serial = tokens.next()?.to_owned();
            let first = tokens.next()?;
            let mut rest: Vec<&str> = tokens.collect();
            let state = if first == "no" && rest.first() == Some(&"permissions") {
                rest.remove(0);
                "no permissions".to_owned()
            } else {
                first.to_owned()
            };
            let mut device = Device {
                serial,
                state,
                model: None,
                product: None,
                device: None,
                transport_id: None,
                usb: None,
            };
            for token in rest {
                let Some((key, value)) = token.split_once(':') else {
                    continue;
                };
                let slot = match key {
                    "model" => &mut device.model,
                    "product" => &mut device.product,
                    "device" => &mut device.device,
                    "transport_id" => &mut device.transport_id,
                    "usb" => &mut device.usb,
                    _ => continue,
                };
                if !value.is_empty() {
                    *slot = Some(value.to_owned());
                }
            }
            Some(device)
        })
        .collect()
}

/// What one `adb install -r` proves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// adb printed `Success`: the package manager installed the APK.
    Installed,
    /// The package manager refused it, with its reason (`INSTALL_FAILED_...`).
    Refused(String),
    /// adb never reached the device, so nothing was sent.
    NotReached(String),
    /// Nothing proves what the device did.
    Unknown(String),
}

/// The first line of `text` that holds `needle`, trimmed and bounded.
fn line_with(text: &str, needle: &str) -> Option<String> {
    text.lines()
        .find(|line| line.contains(needle))
        .map(|line| line.trim().chars().take(512).collect())
}

/// Whether adb said it could not find or reach the device.
fn not_reached(text: &str) -> Option<String> {
    [
        "not found",
        "no devices/emulators found",
        "device offline",
        "device unauthorized",
    ]
    .iter()
    .find_map(|needle| {
        line_with(text, needle)
            .filter(|line| line.starts_with("adb:") || line.starts_with("error:"))
    })
}

/// Read what `adb install -r` printed, and whether it exited cleanly.
#[must_use]
pub fn parse_install(output: &str, exited_cleanly: bool) -> Install {
    if let Some(at) = output.find("Failure [") {
        let reason: String = output[at + "Failure [".len()..]
            .split(']')
            .next()
            .unwrap_or_default()
            .chars()
            .take(512)
            .collect();
        return Install::Refused(reason);
    }
    if exited_cleanly && output.lines().any(|line| line.trim() == "Success") {
        return Install::Installed;
    }
    if let Some(line) = not_reached(output) {
        return Install::NotReached(line);
    }
    Install::Unknown(
        output
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map_or_else(
                || "adb printed nothing".to_owned(),
                |line| line.trim().chars().take(512).collect(),
            ),
    )
}

/// What one `am start -W -n` proves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// The activity manager started the component, or brought its running
    /// task to the front.
    Started {
        /// The activity it reports, when it does.
        activity: Option<String>,
        /// `LaunchState:`, when reported (`COLD`, `WARM`, `HOT`).
        state: Option<String>,
    },
    /// It refused, with its error line.
    Failed(String),
}

/// Read what `am start -W -n <component>` printed, and whether the shell
/// exited cleanly.
#[must_use]
pub fn parse_launch(output: &str, exited_cleanly: bool) -> Launch {
    let lines = || output.lines().map(str::trim);
    if let Some(line) = lines()
        .find(|line| line.starts_with("Error:"))
        .or_else(|| lines().find(|line| line.starts_with("Error") || line.starts_with("Exception")))
    {
        return Launch::Failed(line.chars().take(512).collect());
    }
    let value = |key: &str| {
        output
            .lines()
            .find_map(|line| line.trim().strip_prefix(key))
            .map(|value| value.trim().to_owned())
    };
    let started = value("Status:").as_deref() == Some("ok")
        || output.contains("its current task has been brought to the front");
    if exited_cleanly && started {
        Launch::Started {
            activity: value("Activity:"),
            state: value("LaunchState:"),
        }
    } else if let Some(line) = not_reached(output) {
        Launch::Failed(line)
    } else {
        Launch::Failed(format!(
            "the activity manager did not report the start ({})",
            if exited_cleanly {
                "no `Status: ok`"
            } else {
                "the shell did not exit cleanly"
            }
        ))
    }
}

/// The process IDs `pidof` printed.
#[must_use]
pub fn parse_pids(output: &str) -> Vec<u32> {
    output
        .split_whitespace()
        .filter_map(|token| token.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_adb_lists_is_read_with_its_details() {
        let output = "* daemon not running; starting now at tcp:5037\n\
                      * daemon started successfully\n\
                      List of devices attached\n\
                      emulator-5554          device product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 device:emu64xa transport_id:1\n\
                      R5CR1234567            unauthorized usb:1-1 transport_id:2\n\
                      0123456789ABCDEF       offline transport_id:3\n\
                      HT7A1B2C3D4            no permissions (missing udev rules? user is in the plugdev group); see [http://developer.android.com/tools/device.html] usb:3-2 transport_id:4\n\
                      192.168.1.5:5555       device product:redfin model:Pixel_5 device:redfin transport_id:5\n\
                      \n";
        let devices = parse_devices(output);
        assert_eq!(devices.len(), 5);
        assert_eq!(devices[0].serial, "emulator-5554");
        assert!(devices[0].ready());
        assert_eq!(devices[0].model.as_deref(), Some("sdk_gphone64_x86_64"));
        assert_eq!(devices[0].transport(), "emulator");
        assert_eq!(devices[1].state, "unauthorized");
        assert!(!devices[1].ready());
        assert_eq!(devices[1].transport(), "usb");
        assert_eq!(devices[2].state, "offline");
        assert_eq!(devices[2].transport(), "unknown");
        assert_eq!(devices[3].state, "no permissions");
        assert_eq!(devices[3].usb.as_deref(), Some("3-2"));
        assert_eq!(devices[3].transport_id.as_deref(), Some("4"));
        assert_eq!(devices[4].transport(), "tcp");
        assert_eq!(devices[4].product.as_deref(), Some("redfin"));
        assert!(parse_devices("List of devices attached\n\n").is_empty());
    }

    #[test]
    fn an_install_is_installed_only_on_success() {
        assert_eq!(
            parse_install("Performing Streamed Install\nSuccess\n", true),
            Install::Installed
        );
        assert_eq!(
            parse_install(
                "Performing Streamed Install\nadb: failed to install /tmp/game.apk: Failure \
                 [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Package org.example signatures do not \
                 match previously installed version; ignoring!]\n",
                false
            ),
            Install::Refused(
                "INSTALL_FAILED_UPDATE_INCOMPATIBLE: Package org.example signatures do not match \
                 previously installed version; ignoring!"
                    .to_owned()
            )
        );
        assert!(matches!(
            parse_install("adb: device 'R5CR1234567' not found\n", false),
            Install::NotReached(_)
        ));
        // A lost connection, or a client killed mid-install, proves nothing.
        for (output, clean) in [
            ("Performing Streamed Install\n", false),
            (
                "Performing Streamed Install\nadb: failed to install x: closed\n",
                false,
            ),
            ("", false),
            ("Success\n", false),
        ] {
            assert!(
                matches!(parse_install(output, clean), Install::Unknown(_)),
                "{output:?}"
            );
        }
    }

    #[test]
    fn a_launch_is_started_only_when_the_activity_manager_says_so() {
        assert_eq!(
            parse_launch(
                "Starting: Intent { cmp=org.example/com.godot.game.GodotAppLauncher }\nStatus: \
                 ok\nLaunchState: COLD\nActivity: org.example/com.godot.game.GodotApp\nTotalTime: \
                 812\nWaitTime: 820\nComplete\n",
                true
            ),
            Launch::Started {
                activity: Some("org.example/com.godot.game.GodotApp".to_owned()),
                state: Some("COLD".to_owned()),
            }
        );
        assert!(matches!(
            parse_launch(
                "Starting: Intent { cmp=org.example/.Main }\nWarning: Activity not started, its \
                 current task has been brought to the front\nStatus: ok\n",
                true
            ),
            Launch::Started { .. }
        ));
        assert_eq!(
            parse_launch(
                "Starting: Intent { cmp=org.example/.Missing }\nError type 3\nError: Activity \
                 class {org.example/org.example.Missing} does not exist.\n",
                false
            ),
            Launch::Failed(
                "Error: Activity class {org.example/org.example.Missing} does not exist."
                    .to_owned()
            )
        );
        assert!(matches!(
            parse_launch("Starting: Intent { cmp=org.example/.Main }\n", true),
            Launch::Failed(_)
        ));
    }

    #[test]
    fn pids_are_read_from_pidof() {
        assert_eq!(parse_pids("12345\n"), [12345]);
        assert_eq!(parse_pids("12345 12346\n"), [12345, 12346]);
        assert!(parse_pids("").is_empty());
    }
}
