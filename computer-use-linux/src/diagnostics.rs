use crate::windowing::registry::{
    self, COSMIC_WAYLAND_BACKEND, GNOME_SHELL_EXTENSION_BACKEND, GNOME_SHELL_INTROSPECT_BACKEND,
    HYPRLAND_BACKEND, KWIN_BACKEND,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    fs::OpenOptions,
    io::Write,
    os::unix::{
        fs::MetadataExt,
        net::{UnixDatagram, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const DESKTOP_ENV_KEYS: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "DESKTOP_SESSION",
    "DISPLAY",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "XAUTHORITY",
    "YDOTOOL_SOCKET",
    "XDG_SESSION_DESKTOP",
    "WAYLAND_DISPLAY",
    "XDG_CURRENT_DESKTOP",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
];

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorReport {
    pub platform: PlatformReport,
    pub portals: PortalReport,
    pub accessibility: AccessibilityReport,
    pub windowing: WindowingReport,
    pub input: InputReport,
    pub readiness: ReadinessReport,
    /// Which interchangeable backends this environment supports, per layer, plus
    /// the one the tool prefers. Lets an agent (or selector) understand what's
    /// available and choose accordingly instead of assuming one fixed path.
    pub capabilities: CapabilityMap,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CapabilityMap {
    /// Pointer/keyboard injection backends, best-first.
    pub input: Vec<String>,
    /// Screen capture backends, best-first.
    pub screenshot: Vec<String>,
    /// Window listing/focus backends available.
    pub window_control: Vec<String>,
    /// Accessibility (element-targeted, non-pointer) backends.
    pub accessibility: Vec<String>,
    /// Display/session isolation contexts the host can provide.
    pub isolation: Vec<String>,
    /// The backend the tool will use by default for each selectable layer.
    pub preferred: PreferredBackends,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PreferredBackends {
    pub input: Option<String>,
    pub screenshot: Option<String>,
    pub window_control: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlatformReport {
    pub os: String,
    pub arch: String,
    pub desktop_session: Option<String>,
    pub xdg_session_type: Option<String>,
    pub xdg_current_desktop: Option<String>,
    pub wayland_display: Option<String>,
    pub display: Option<String>,
    pub xauthority: Option<String>,
    pub dbus_session_bus_address: Option<String>,
    pub xdg_runtime_dir: Option<String>,
    pub gnome_shell_version: Check,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PortalReport {
    pub desktop_portal: Check,
    pub remote_desktop: Check,
    pub screencast: Check,
    pub screenshot: Check,
    pub input_capture: Check,
    pub mutter_remote_desktop: Check,
    pub mutter_screencast: Check,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityReport {
    pub at_spi_bus: Check,
    pub toolkit_accessibility: Check,
    pub at_spi_enabled: Check,
    pub screen_reader_enabled: Check,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WindowingReport {
    pub gnome_shell_introspect: Check,
    pub codex_gnome_shell_extension: Check,
    pub cosmic_helper: Check,
    pub kwin: Check,
    pub hyprland: Check,
    pub backends: BTreeMap<String, Check>,
    pub can_list_windows: bool,
    pub can_focus_apps: bool,
    pub can_focus_windows: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InputReport {
    pub ydotool: Check,
    pub ydotoold: Check,
    pub ydotool_socket: Check,
    pub uinput: Check,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadinessReport {
    pub can_register_mcp_tools: bool,
    pub can_build_accessibility_tree: bool,
    pub can_query_windows: bool,
    pub can_focus_apps: bool,
    pub can_focus_windows: bool,
    pub can_send_development_input: bool,
    pub recommended_next_step: String,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SetupReport {
    pub before: DoctorReport,
    pub accessibility_command: Check,
    pub after: DoctorReport,
    pub changed_accessibility: bool,
    pub requires_target_app_restart: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct YdotoolSetupReport {
    pub before: DoctorReport,
    /// False when ydotool is not installed; install it first then call this tool again.
    pub ydotool_installed: bool,
    /// Result of writing /etc/udev/rules.d/70-uinput.rules so the input group can
    /// access /dev/uinput. Skipped when /dev/uinput is already readable/writable.
    pub udev_rule: Check,
    /// Result of adding the current user to the `input` group via pkexec usermod.
    /// Skipped when the user is already a member.
    pub group_membership: Check,
    /// Result of pkexec udevadm reload + trigger so the new device permissions take
    /// effect immediately. Skipped when /dev/uinput was already accessible.
    pub udev_reload: Check,
    /// Result of systemctl --user enable --now ydotoold.
    pub ydotoold_service: Check,
    pub after: DoctorReport,
    /// True when group membership was newly granted. The user must log out and back
    /// in before existing session processes (including ydotoold) see the new group.
    pub requires_relogin: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Check {
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn ok(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }
}

pub fn doctor_report() -> DoctorReport {
    hydrate_session_bus_env();

    let platform = platform_report();
    let portals = portal_report();
    let accessibility = accessibility_report();
    let windowing = windowing_report(&platform);
    let input = input_report();
    let readiness = readiness_report(&platform, &portals, &accessibility, &windowing, &input);

    let capabilities = capability_map(&platform, &portals, &accessibility, &windowing, &input);

    DoctorReport {
        platform,
        portals,
        accessibility,
        windowing,
        input,
        readiness,
        capabilities,
    }
}

/// Derive the per-layer backend capability map from the individual checks. Lists
/// are ordered best-first and mirror the order the tool actually tries them.
fn capability_map(
    platform: &PlatformReport,
    portals: &PortalReport,
    accessibility: &AccessibilityReport,
    windowing: &WindowingReport,
    input: &InputReport,
) -> CapabilityMap {
    let mut input_backends = Vec::new();
    // Absolute uinput pointer: accurate, non-blocking of coordinates; preferred.
    if input.uinput.ok {
        input_backends.push("abs_pointer".to_string());
    }
    if portals.remote_desktop.ok {
        input_backends.push("portal".to_string());
    }
    if input.ydotool_socket.ok {
        input_backends.push("ydotool".to_string());
    }

    let mut screenshot_backends = Vec::new();
    if platform.gnome_shell_version.ok {
        screenshot_backends.push("gnome_shell".to_string());
    }
    if portals.screenshot.ok {
        screenshot_backends.push("portal".to_string());
    }

    let mut window_backends = Vec::new();
    if windowing.codex_gnome_shell_extension.ok {
        window_backends.push("gnome_shell_extension".to_string());
    }
    if windowing.gnome_shell_introspect.ok {
        window_backends.push("gnome_introspect".to_string());
    }
    if windowing.kwin.ok {
        window_backends.push("kwin".to_string());
    }
    if windowing.hyprland.ok {
        window_backends.push("hyprland".to_string());
    }
    if windowing.cosmic_helper.ok {
        window_backends.push("cosmic".to_string());
    }

    let mut accessibility_backends = Vec::new();
    if accessibility.at_spi_enabled.ok || accessibility.toolkit_accessibility.ok {
        accessibility_backends.push("at_spi".to_string());
    }

    // Isolation contexts: the live shared session is always available; a headless
    // GNOME session is possible when gnome-shell is installed (it supports
    // --headless --virtual-monitor), giving the agent its own seat.
    let mut isolation = vec!["shared".to_string()];
    if platform.gnome_shell_version.ok {
        isolation.push("headless_gnome".to_string());
    }

    let preferred = PreferredBackends {
        input: input_backends.first().cloned(),
        screenshot: screenshot_backends.first().cloned(),
        window_control: window_backends.first().cloned(),
    };

    CapabilityMap {
        input: input_backends,
        screenshot: screenshot_backends,
        window_control: window_backends,
        accessibility: accessibility_backends,
        isolation,
        preferred,
    }
}

pub fn hydrate_session_bus_env() {
    hydrate_common_command_path();
    hydrate_desktop_env_from_process_tree();
    hydrate_desktop_env_from_systemd_user();

    if env_var("XDG_RUNTIME_DIR").is_none() {
        if let Some(runtime) = xdg_runtime_dir() {
            if runtime.exists() {
                env::set_var("XDG_RUNTIME_DIR", runtime);
            }
        }
    }

    if env_var("DBUS_SESSION_BUS_ADDRESS").is_none() {
        if let Some(runtime) = xdg_runtime_dir() {
            let bus = runtime.join("bus");
            if bus.exists() {
                env::set_var(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", bus.display()),
                );
            }
        }
    }
}

fn hydrate_common_command_path() {
    let mut entries = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    for path in [
        "/run/current-system/sw/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
    ] {
        let path = PathBuf::from(path);
        if path.exists() && !entries.iter().any(|entry| entry == &path) {
            entries.push(path);
        }
    }
    if let Ok(path) = env::join_paths(entries) {
        env::set_var("PATH", path);
    }
}

fn hydrate_desktop_env_from_process_tree() {
    for process_env in desktop_process_environments() {
        hydrate_desktop_env_from_map(&process_env);

        if DESKTOP_ENV_KEYS.iter().all(|key| env_var(key).is_some()) {
            break;
        }
    }
}

fn hydrate_desktop_env_from_systemd_user() {
    let Ok(output) = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let env_map = parse_line_environment(&output.stdout);
    hydrate_desktop_env_from_map(&env_map);
}

fn hydrate_desktop_env_from_map(process_env: &HashMap<String, String>) {
    for key in DESKTOP_ENV_KEYS {
        if env_var(key).is_some() {
            continue;
        }
        if let Some(value) = process_env
            .get(*key)
            .filter(|value| !value.trim().is_empty())
        {
            env::set_var(key, value);
        }
    }
}

fn desktop_process_environments() -> Vec<HashMap<String, String>> {
    let mut environments = Vec::new();
    let mut visited_pids = Vec::new();
    let mut pid = parent_pid("self");

    for _ in 0..8 {
        let Some(current_pid) = pid else {
            break;
        };
        if current_pid <= 1 {
            break;
        }

        visited_pids.push(current_pid);
        if let Some(process_env) = read_process_environ(current_pid) {
            environments.push(process_env);
        }
        pid = parent_pid(&current_pid.to_string());
    }

    if !visited_pids.contains(&1) && process_owner_matches_current_user(1) {
        if let Some(process_env) = read_process_environ(1).filter(process_env_has_graphical_display)
        {
            environments.push(process_env);
        }
    }

    environments
}

fn parent_pid(pid: &str) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_parent_pid(&status)
}

fn parse_parent_pid(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let value = line.strip_prefix("PPid:")?.trim();
        value.parse::<u32>().ok()
    })
}

fn read_process_environ(pid: u32) -> Option<HashMap<String, String>> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(parse_environ(&bytes))
}

fn process_owner_matches_current_user(pid: u32) -> bool {
    let Some(current_uid) = user_id().and_then(|uid| uid.parse::<u32>().ok()) else {
        return false;
    };
    fs::metadata(format!("/proc/{pid}"))
        .ok()
        .is_some_and(|metadata| metadata.uid() == current_uid)
}

fn process_env_has_graphical_display(process_env: &HashMap<String, String>) -> bool {
    process_env
        .get("DISPLAY")
        .or_else(|| process_env.get("WAYLAND_DISPLAY"))
        .is_some_and(|value| !value.trim().is_empty())
}

fn parse_environ(bytes: &[u8]) -> HashMap<String, String> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            if entry.is_empty() {
                return None;
            }
            let split = entry.iter().position(|byte| *byte == b'=')?;
            let (key, value) = entry.split_at(split);
            let value = &value[1..];
            let key = std::str::from_utf8(key).ok()?.to_string();
            let value = std::str::from_utf8(value).ok()?.to_string();
            Some((key, value))
        })
        .collect()
}

fn parse_line_environment(bytes: &[u8]) -> HashMap<String, String> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|entry| {
            if entry.is_empty() {
                return None;
            }
            let split = entry.iter().position(|byte| *byte == b'=')?;
            let (key, value) = entry.split_at(split);
            let value = &value[1..];
            let key = std::str::from_utf8(key).ok()?.to_string();
            let value = std::str::from_utf8(value).ok()?.to_string();
            Some((key, value))
        })
        .collect()
}

pub fn setup_ydotool_report() -> YdotoolSetupReport {
    hydrate_session_bus_env();
    let before = doctor_report();

    let ydotool_installed = before.input.ydotool.ok;

    let udev_rule_path = Path::new("/etc/udev/rules.d/70-uinput.rules");
    let udev_rule = if before.input.uinput.ok {
        Check::ok("/dev/uinput is already accessible; udev rule not needed")
    } else if udev_rule_path.exists() {
        Check::ok(format!("{} already exists", udev_rule_path.display()))
    } else {
        write_file_with_pkexec(
            udev_rule_path,
            r#"KERNEL=="uinput", GROUP="input", MODE="0660""#,
        )
    };

    let was_already_in_group = user_in_input_group();
    let group_membership = if was_already_in_group {
        Check::ok("user is already in the input group")
    } else {
        add_user_to_input_group()
    };

    let udev_reload = if !before.input.uinput.ok && udev_rule.ok {
        reload_udev_rules()
    } else {
        Check::ok("udev reload skipped (/dev/uinput already accessible or udev rule not written)")
    };

    let ydotoold_service = if !ydotool_installed {
        Check::fail(
            "ydotool is not installed; install it (e.g. sudo pacman -S ydotool) then call setup_ydotool again"
                .to_string(),
        )
    } else if before.input.ydotoold.ok && before.input.ydotool_socket.ok {
        Check::ok("ydotoold is already running with a connectable socket")
    } else {
        enable_ydotoold_service()
    };

    let after = doctor_report();
    let requires_relogin = !was_already_in_group && group_membership.ok;

    let message = ydotool_setup_message(
        &after,
        &ydotoold_service,
        ydotool_installed,
        requires_relogin,
    );

    YdotoolSetupReport {
        before,
        ydotool_installed,
        udev_rule,
        group_membership,
        udev_reload,
        ydotoold_service,
        after,
        requires_relogin,
        message,
    }
}

fn ydotool_setup_message(
    after: &DoctorReport,
    ydotoold_service: &Check,
    ydotool_installed: bool,
    requires_relogin: bool,
) -> String {
    let input_ready =
        after.input.uinput.ok || (after.input.ydotoold.ok && after.input.ydotool_socket.ok);

    if !ydotool_installed {
        return "Install ydotool first, then call setup_ydotool again.".to_string();
    }

    let mut parts: Vec<&str> = Vec::new();

    if requires_relogin {
        parts.push(
            "Log out and back in for the new input group membership to take effect.",
        );
    }

    if !ydotoold_service.ok {
        parts.push(
            "ydotoold service could not be started; run: systemctl --user enable --now ydotoold",
        );
    }

    if parts.is_empty() {
        if input_ready {
            "ydotool setup complete: /dev/uinput or a connectable ydotoold socket is now available.".to_string()
        } else {
            "Setup steps ran but input is still not available; check udev_rule, udev_reload, and ydotoold_service details.".to_string()
        }
    } else {
        parts.join(" ")
    }
}

fn write_file_with_pkexec(path: &Path, content: &str) -> Check {
    let path_str = path.to_string_lossy().into_owned();
    let mut child = match Command::new("pkexec")
        .args(["tee", path_str.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Check::fail(format!("pkexec not available: {e}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(content.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => Check::ok(format!("wrote {path_str}")),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Check::fail(format!(
                "pkexec tee {path_str} failed ({}): {}",
                out.status, stderr
            ))
        }
        Err(e) => Check::fail(format!("pkexec tee {path_str}: {e}")),
    }
}

fn user_in_input_group() -> bool {
    Command::new("id")
        .output()
        .ok()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("(input)"))
}

fn add_user_to_input_group() -> Check {
    let username = match Command::new("id").arg("-un").output() {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => return Check::fail("could not determine current username via id -un".to_string()),
    };
    if username.is_empty() {
        return Check::fail("id -un returned an empty username".to_string());
    }
    let output = Command::new("pkexec")
        .args(["usermod", "-aG", "input", username.as_str()])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            Check::ok(format!("added {username} to the input group"))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Check::fail(format!(
                "pkexec usermod -aG input {username} failed ({}): {}",
                out.status, stderr
            ))
        }
        Err(e) => Check::fail(format!("pkexec not available: {e}")),
    }
}

fn reload_udev_rules() -> Check {
    match Command::new("pkexec")
        .args(["udevadm", "control", "--reload-rules"])
        .output()
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Check::fail(format!(
                "udevadm control --reload-rules failed ({}): {}",
                out.status, stderr
            ));
        }
        Err(e) => return Check::fail(format!("pkexec udevadm control: {e}")),
    }
    match Command::new("pkexec")
        .args(["udevadm", "trigger", "--subsystem-match=misc"])
        .output()
    {
        Ok(out) if out.status.success() => {
            Check::ok("udev rules reloaded and misc subsystem triggered")
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Check::fail(format!(
                "udevadm trigger --subsystem-match=misc failed ({}): {}",
                out.status, stderr
            ))
        }
        Err(e) => Check::fail(format!("pkexec udevadm trigger: {e}")),
    }
}

fn enable_ydotoold_service() -> Check {
    match Command::new("systemctl")
        .args(["--user", "enable", "--now", "ydotoold"])
        .output()
    {
        Ok(out) if out.status.success() => Check::ok("ydotoold user service enabled and started"),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Check::fail(format!(
                "systemctl --user enable --now ydotoold failed ({}): {}",
                out.status, stderr
            ))
        }
        Err(e) => Check::fail(format!("systemctl not available: {e}")),
    }
}

pub fn setup_accessibility_report() -> SetupReport {
    hydrate_session_bus_env();

    let before = doctor_report();
    let accessibility_command = if can_build_accessibility_tree(&before.accessibility) {
        Check::ok("AT-SPI accessibility is already enabled")
    } else {
        enable_atspi()
    };
    let after = doctor_report();
    let before_ready = before.readiness.can_build_accessibility_tree;
    let after_ready = after.readiness.can_build_accessibility_tree;
    let changed_accessibility = !before_ready && after_ready;
    let requires_target_app_restart = changed_accessibility;
    let message = if after_ready {
        if changed_accessibility {
            "AT-SPI accessibility is enabled. Restart already-running target apps if their AT-SPI tree is still empty."
        } else {
            "AT-SPI accessibility is ready."
        }
    } else {
        "Could not enable AT-SPI accessibility automatically. \
         Check accessibility_command.detail for the attempted commands. \
         On GNOME: gsettings set org.gnome.desktop.interface toolkit-accessibility true. \
         On KDE/Plasma: kwriteconfig6 --file kdeglobals --group Accessibility --key ScreenReaderEnabled true (then log out and back in). \
         On XFCE: xfconf-query --channel xfce4-session --property /general/StartAssistiveTechnologies --set true (then log out and back in)."
    }
    .to_string();

    SetupReport {
        before,
        accessibility_command,
        after,
        changed_accessibility,
        requires_target_app_restart,
        message,
    }
}

/// Try every applicable method to enable AT-SPI, returning the first that succeeds.
fn enable_atspi() -> Check {
    // 1. Cross-desktop D-Bus toggle — works on any desktop when at-spi2-core is
    //    already running. This is the fastest path and desktop-agnostic.
    let busctl = command_check_with_session_bus(
        "busctl",
        &[
            "--user",
            "set-property",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Status",
            "IsEnabled",
            "b",
            "true",
        ],
    );
    if busctl.ok {
        return busctl;
    }

    // 2. Start the at-spi-dbus-bus user service directly — works on any systemd
    //    desktop where at-spi2-core ships a user unit (most modern distros).
    let systemd = command_check_with_session_bus(
        "systemctl",
        &["--user", "enable", "--now", "at-spi-dbus-bus"],
    );
    if systemd.ok {
        return systemd;
    }

    // 3. Desktop-specific session settings. These persist across reboots and tell
    //    the session manager to start at-spi2 on next login; they also sometimes
    //    trigger an immediate start on GNOME / KDE.
    let desktop = env_var("XDG_CURRENT_DESKTOP")
        .or_else(|| env_var("DESKTOP_SESSION"))
        .unwrap_or_default()
        .to_ascii_lowercase();

    let detected = if desktop.contains("kde") || desktop.contains("plasma") {
        enable_atspi_kde()
    } else if desktop.contains("xfce") {
        enable_atspi_xfce()
    } else {
        // GNOME, COSMIC, and unknown desktops — try gsettings.
        enable_atspi_gnome()
    };
    if detected.ok {
        return detected;
    }

    // 4. Shotgun: try all three desktop-specific tools in case the env vars are
    //    wrong or the session is mixed (e.g. GTK apps under KDE).
    for check in [enable_atspi_gnome(), enable_atspi_kde(), enable_atspi_xfce()] {
        if check.ok {
            return check;
        }
    }

    Check::fail(format!(
        "All AT-SPI enable methods failed. \
         busctl: {}; systemctl: {}; {}",
        busctl.detail, systemd.detail, detected.detail,
    ))
}

fn enable_atspi_gnome() -> Check {
    command_check_with_session_bus(
        "gsettings",
        &[
            "set",
            "org.gnome.desktop.interface",
            "toolkit-accessibility",
            "true",
        ],
    )
}

fn enable_atspi_kde() -> Check {
    // kwriteconfig6 (KDE 6) writes to ~/.config/kdeglobals.  Setting
    // ScreenReaderEnabled=true causes plasma-session and Qt apps to register
    // their AT-SPI trees.  kwriteconfig5 is the KDE 5 equivalent.
    for bin in &["kwriteconfig6", "kwriteconfig5"] {
        let r = command_check_with_session_bus(
            bin,
            &[
                "--file",
                "kdeglobals",
                "--group",
                "Accessibility",
                "--key",
                "ScreenReaderEnabled",
                "true",
            ],
        );
        if r.ok {
            return r;
        }
    }
    Check::fail(
        "kwriteconfig6 and kwriteconfig5 both unavailable or failed; \
         is ydotool / kde-cli-tools installed?"
            .to_string(),
    )
}

fn enable_atspi_xfce() -> Check {
    // xfce4-session reads StartAssistiveTechnologies and starts at-spi-bus-launcher
    // on login when it is true.  --create is needed if the property does not exist yet.
    let r = command_check_with_session_bus(
        "xfconf-query",
        &[
            "--channel",
            "xfce4-session",
            "--property",
            "/general/StartAssistiveTechnologies",
            "--set",
            "true",
        ],
    );
    if r.ok {
        return r;
    }
    // Retry with --create in case the property is missing entirely.
    command_check_with_session_bus(
        "xfconf-query",
        &[
            "--channel",
            "xfce4-session",
            "--property",
            "/general/StartAssistiveTechnologies",
            "--create",
            "--type",
            "bool",
            "--set",
            "true",
        ],
    )
}

fn platform_report() -> PlatformReport {
    PlatformReport {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        desktop_session: env_var("DESKTOP_SESSION"),
        xdg_session_type: env_var("XDG_SESSION_TYPE"),
        xdg_current_desktop: env_var("XDG_CURRENT_DESKTOP"),
        wayland_display: env_var("WAYLAND_DISPLAY"),
        display: env_var("DISPLAY"),
        xauthority: env_var("XAUTHORITY"),
        dbus_session_bus_address: dbus_session_address(),
        xdg_runtime_dir: xdg_runtime_dir().map(|path| path.display().to_string()),
        gnome_shell_version: command_check("gnome-shell", &["--version"]),
    }
}

fn portal_report() -> PortalReport {
    PortalReport {
        desktop_portal: bus_name_check("org.freedesktop.portal.Desktop"),
        remote_desktop: portal_interface_check("org.freedesktop.portal.RemoteDesktop"),
        screencast: portal_interface_check("org.freedesktop.portal.ScreenCast"),
        screenshot: portal_interface_check("org.freedesktop.portal.Screenshot"),
        input_capture: portal_interface_check("org.freedesktop.portal.InputCapture"),
        mutter_remote_desktop: bus_name_check("org.gnome.Mutter.RemoteDesktop"),
        mutter_screencast: bus_name_check("org.gnome.Mutter.ScreenCast"),
    }
}

fn accessibility_report() -> AccessibilityReport {
    AccessibilityReport {
        at_spi_bus: atspi_bus_address_check(),
        toolkit_accessibility: command_check_with_session_bus(
            "gsettings",
            &[
                "get",
                "org.gnome.desktop.interface",
                "toolkit-accessibility",
            ],
        ),
        at_spi_enabled: atspi_status_property_check("IsEnabled"),
        screen_reader_enabled: atspi_status_property_check("ScreenReaderEnabled"),
    }
}

fn windowing_report(platform: &PlatformReport) -> WindowingReport {
    let probes = registry::probe_backends();
    let backend_check = |id: &str| {
        probes
            .iter()
            .find(|probe| probe.id == id)
            .map(check_from_backend_probe)
            .unwrap_or_else(|| Check::fail("backend probe did not run"))
    };
    let gnome_shell_introspect = backend_check(GNOME_SHELL_INTROSPECT_BACKEND);
    let codex_gnome_shell_extension = backend_check(GNOME_SHELL_EXTENSION_BACKEND);
    let cosmic_helper = backend_check(COSMIC_WAYLAND_BACKEND);
    let kwin = backend_check(KWIN_BACKEND);
    let hyprland = backend_check(HYPRLAND_BACKEND);
    let backends = probes
        .iter()
        .map(|probe| (probe.id.to_string(), check_from_backend_probe(probe)))
        .collect::<BTreeMap<_, _>>();
    let can_list_windows = probes.iter().any(|probe| probe.can_list_windows);
    let can_focus_apps = probes.iter().any(|probe| probe.can_focus_apps);
    let can_focus_windows = probes.iter().any(|probe| probe.can_focus_windows);
    let note = if can_list_windows {
        if cosmic_helper.ok && is_cosmic_wayland_platform(platform) {
            "A COSMIC Wayland window backend is available for list_windows, focused_window, and targeted input verification."
        } else if kwin.ok {
            "A KWin/Plasma window backend is available for list_windows, focused_window, and targeted input verification."
        } else if hyprland.ok {
            "A Hyprland window backend is available for list_windows, focused_window, and targeted input verification."
        } else {
            "A GNOME window listing backend is available for list_windows, focused_window, and targeted input verification."
        }
    } else {
        "Window listing is unavailable or denied. Computer Use can still use screenshots, AT-SPI, and global ydotool input, but targeted window input cannot be verified. On GNOME, run setup_window_targeting to install the optional GNOME Shell extension backend. On COSMIC, ensure the bundled COSMIC helper is present and can connect to the session. On KDE/Plasma, ensure KWin exposes org.kde.KWin scripting on the session bus. On Hyprland, ensure hyprctl is available in the session."
    }
    .to_string();

    WindowingReport {
        gnome_shell_introspect,
        codex_gnome_shell_extension,
        cosmic_helper,
        kwin,
        hyprland,
        backends,
        can_list_windows,
        can_focus_apps,
        can_focus_windows,
        note,
    }
}

fn check_from_backend_probe(probe: &registry::BackendProbe) -> Check {
    if probe.ok {
        Check::ok(probe.detail.clone())
    } else {
        Check::fail(probe.detail.clone())
    }
}

fn input_report() -> InputReport {
    InputReport {
        ydotool: command_path_check("ydotool"),
        ydotoold: process_check("ydotoold"),
        ydotool_socket: ydotool_socket_check(),
        uinput: read_write_path_check(Path::new("/dev/uinput")),
    }
}

fn readiness_report(
    platform: &PlatformReport,
    portals: &PortalReport,
    accessibility: &AccessibilityReport,
    windowing: &WindowingReport,
    input: &InputReport,
) -> ReadinessReport {
    let mut blockers = Vec::new();
    let can_build_accessibility_tree = can_build_accessibility_tree(accessibility);
    let can_query_windows = windowing.can_list_windows;
    let can_focus_apps = windowing.can_focus_apps;
    let can_focus_windows = windowing.can_focus_windows;
    let can_send_development_input = can_send_development_input(portals, input);

    if !can_build_accessibility_tree {
        blockers.push(
            "AT-SPI accessibility is disabled; enable org.a11y.Status IsEnabled or org.gnome.desktop.interface toolkit-accessibility for tree extraction."
                .to_string(),
        );
    }

    if !can_query_windows {
        blockers.push(if is_cosmic_wayland_platform(platform) {
            "COSMIC Wayland window introspection is unavailable; targeted window focus and verification will be disabled.".to_string()
        } else {
            "Window introspection is unavailable; targeted window focus and verification will be disabled."
                .to_string()
        });
    }

    if can_query_windows && !can_focus_windows {
        blockers.push(
            "Exact window activation is unavailable; app-level focus may work, but window_id/title/terminal-targeted input cannot be verified."
                .to_string(),
        );
    }

    if !can_send_development_input {
        blockers.push(
            "Development input is unavailable; enable read/write /dev/uinput, XDG RemoteDesktop portal input, or ydotool with a connectable ydotoold socket."
                .to_string(),
        );
    }

    let recommended_next_step = if !can_build_accessibility_tree {
        "Run setup_accessibility to enable AT-SPI accessibility before element-aware actions."
            .to_string()
    } else if !can_query_windows {
        format!(
            "Enable a supported window backend before using targeted keyboard input: {}",
            registry::descriptors()
                .iter()
                .map(|descriptor| descriptor.missing_hint)
                .collect::<Vec<_>>()
                .join(" ")
        )
    } else if !can_focus_windows {
        "Enable an exact-focus window backend before using window_id, title, or terminal-targeted input.".to_string()
    } else if !can_send_development_input {
        "Enable a supported input backend: grant read/write /dev/uinput, enable the XDG RemoteDesktop portal, or start ydotoold with a socket accessible to this desktop user."
            .to_string()
    } else {
        "Computer Use is ready: AT-SPI tree support, window targeting, and a Linux input backend are available."
            .to_string()
    };

    ReadinessReport {
        can_register_mcp_tools: true,
        can_build_accessibility_tree,
        can_query_windows,
        can_focus_apps,
        can_focus_windows,
        can_send_development_input,
        recommended_next_step,
        blockers,
    }
}

fn can_send_development_input(portals: &PortalReport, input: &InputReport) -> bool {
    input.uinput.ok
        || portals.remote_desktop.ok
        || input.ydotool.ok && input.ydotoold.ok && input.ydotool_socket.ok
}

fn is_cosmic_wayland_platform(platform: &PlatformReport) -> bool {
    platform
        .xdg_current_desktop
        .as_deref()
        .is_some_and(|desktop| desktop.to_ascii_lowercase().contains("cosmic"))
        && platform.xdg_session_type.as_deref() == Some("wayland")
}

fn can_build_accessibility_tree(accessibility: &AccessibilityReport) -> bool {
    accessibility.at_spi_bus.ok
        && (check_detail_contains_true(&accessibility.at_spi_enabled)
            || check_detail_contains_true(&accessibility.toolkit_accessibility))
}

fn check_detail_contains_true(check: &Check) -> bool {
    check.ok && check.detail.to_ascii_lowercase().contains("true")
}

fn env_var(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn xdg_runtime_dir() -> Option<PathBuf> {
    if let Some(value) = env_var("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(value));
    }
    user_id().map(|uid| PathBuf::from(format!("/run/user/{uid}")))
}

fn dbus_session_address() -> Option<String> {
    if let Some(value) = env_var("DBUS_SESSION_BUS_ADDRESS") {
        return Some(value);
    }
    xdg_runtime_dir()
        .map(|runtime| format!("unix:path={}", runtime.join("bus").display()))
        .filter(|address| {
            address
                .strip_prefix("unix:path=")
                .is_some_and(|p| Path::new(p).exists())
        })
}

fn ydotool_socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = env_var("YDOTOOL_SOCKET") {
        candidates.push(PathBuf::from(value));
    }

    if let Some(runtime_socket) = xdg_runtime_dir().map(|runtime| runtime.join(".ydotool_socket")) {
        candidates.push(runtime_socket);
    }
    candidates.push(PathBuf::from("/tmp/.ydotool_socket"));
    candidates
}

fn ydotool_socket_check() -> Check {
    let mut checked = Vec::new();
    for candidate in ydotool_socket_candidates() {
        match socket_connect_result(&candidate) {
            Ok(()) => return Check::ok(format!("connectable: {}", candidate.display())),
            Err(detail) => checked.push(detail),
        }
    }

    Check::fail(format!(
        "no connectable ydotool socket ({})",
        checked.join("; ")
    ))
}

fn user_id() -> Option<String> {
    let output = Command::new("id").arg("-u").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn command_path_check(command: &str) -> Check {
    command_check("sh", &["-c", &format!("command -v {command}")])
}

fn process_check(process_name: &str) -> Check {
    command_check("pgrep", &["-a", process_name])
}

#[cfg(test)]
fn socket_connect_check(path: &Path) -> Check {
    match socket_connect_result(path) {
        Ok(()) => Check::ok(format!("connectable: {}", path.display())),
        Err(detail) => Check::fail(detail),
    }
}

fn socket_connect_result(path: &Path) -> std::result::Result<(), String> {
    if !path.exists() {
        return Err(format!("missing: {}", path.display()));
    }

    match UnixStream::connect(path) {
        Ok(_) => Ok(()),
        Err(stream_error) => {
            match UnixDatagram::unbound().and_then(|socket| socket.connect(path)) {
                Ok(()) => Ok(()),
                Err(datagram_error) => Err(format!(
                    "{}: stream: {}; datagram: {}",
                    path.display(),
                    stream_error,
                    datagram_error
                )),
            }
        }
    }
}

fn read_write_path_check(path: &Path) -> Check {
    if !path.exists() {
        return Check::fail(format!("missing: {}", path.display()));
    }

    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(_) => Check::ok(format!("read/write: {}", path.display())),
        Err(error) => Check::fail(format!("{}: {error}", path.display())),
    }
}

fn bus_name_check(name: &str) -> Check {
    command_check_with_session_bus("busctl", &["--user", "status", name])
}

fn portal_interface_check(interface: &str) -> Check {
    command_check_with_session_bus(
        "busctl",
        &[
            "--user",
            "introspect",
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            interface,
        ],
    )
}

fn atspi_bus_address_check() -> Check {
    let busctl = command_check_with_session_bus(
        "busctl",
        &[
            "--user",
            "call",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Bus",
            "GetAddress",
        ],
    );
    if busctl.ok {
        return busctl;
    }

    gdbus_call_check(
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Bus.GetAddress",
        &[],
    )
}

fn atspi_status_property_check(property: &str) -> Check {
    let busctl = command_check_with_session_bus(
        "busctl",
        &[
            "--user",
            "get-property",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Status",
            property,
        ],
    );
    if busctl.ok {
        return busctl;
    }

    gdbus_call_check(
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.freedesktop.DBus.Properties.Get",
        &["org.a11y.Status", property],
    )
}

fn gdbus_call_check(destination: &str, object_path: &str, method: &str, args: &[&str]) -> Check {
    let mut command_args = vec![
        "call",
        "--session",
        "--dest",
        destination,
        "--object-path",
        object_path,
        "--method",
        method,
    ];
    command_args.extend_from_slice(args);
    command_check_with_session_bus("gdbus", &command_args)
}

fn command_check(command: &str, args: &[&str]) -> Check {
    run_command(command, args, false)
}

fn command_check_with_session_bus(command: &str, args: &[&str]) -> Check {
    run_command(command, args, true)
}

fn run_command(command: &str, args: &[&str], with_session_bus: bool) -> Check {
    let mut cmd = Command::new(command);
    cmd.args(args);

    if with_session_bus {
        if let Some(address) = dbus_session_address() {
            cmd.env("DBUS_SESSION_BUS_ADDRESS", address);
        }
        if let Some(runtime) = xdg_runtime_dir() {
            cmd.env("XDG_RUNTIME_DIR", runtime);
        }
    }

    match cmd.output() {
        Ok(output) if output.status.success() => {
            let detail = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Check::ok(if detail.is_empty() {
                "ok".into()
            } else {
                detail
            })
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if !stderr.is_empty() { stderr } else { stdout };
            Check::fail(if detail.is_empty() {
                format!("exit status {}", output.status)
            } else {
                detail
            })
        }
        Err(error) => Check::fail(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_report() -> PlatformReport {
        PlatformReport {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            desktop_session: None,
            xdg_session_type: Some("wayland".to_string()),
            xdg_current_desktop: Some("GNOME".to_string()),
            wayland_display: Some("wayland-0".to_string()),
            display: Some(":0".to_string()),
            xauthority: Some("/run/user/1000/Xauthority".to_string()),
            dbus_session_bus_address: Some("unix:path=/run/user/1000/bus".to_string()),
            xdg_runtime_dir: Some("/run/user/1000".to_string()),
            gnome_shell_version: Check::ok("GNOME Shell 46.0"),
        }
    }

    fn portal_report(remote_desktop: Check) -> PortalReport {
        PortalReport {
            desktop_portal: Check::ok("ok"),
            remote_desktop,
            screencast: Check::fail("missing"),
            screenshot: Check::fail("missing"),
            input_capture: Check::fail("missing"),
            mutter_remote_desktop: Check::fail("missing"),
            mutter_screencast: Check::fail("missing"),
        }
    }

    fn accessibility_report(
        at_spi_bus: Check,
        toolkit_accessibility: Check,
    ) -> AccessibilityReport {
        AccessibilityReport {
            at_spi_bus,
            toolkit_accessibility,
            at_spi_enabled: Check::fail("(<false>,)"),
            screen_reader_enabled: Check::fail("(<false>,)"),
        }
    }

    fn windowing_report(can_list_windows: bool, can_focus_windows: bool) -> WindowingReport {
        WindowingReport {
            gnome_shell_introspect: if can_list_windows {
                Check::ok("ok")
            } else {
                Check::fail("denied")
            },
            codex_gnome_shell_extension: if can_focus_windows {
                Check::ok("ok")
            } else {
                Check::fail("missing")
            },
            cosmic_helper: Check::fail("missing"),
            kwin: Check::fail("not a KWin session"),
            hyprland: Check::fail("not a Hyprland session"),
            backends: BTreeMap::new(),
            can_list_windows,
            can_focus_apps: true,
            can_focus_windows,
            note: String::new(),
        }
    }

    fn input_report(can_send_input: bool) -> InputReport {
        let check = if can_send_input {
            Check::ok("ok")
        } else {
            Check::fail("missing")
        };
        input_report_parts(check.clone(), check.clone(), check.clone(), check)
    }

    fn input_report_parts(
        ydotool: Check,
        ydotoold: Check,
        ydotool_socket: Check,
        uinput: Check,
    ) -> InputReport {
        InputReport {
            ydotool,
            ydotoold,
            ydotool_socket,
            uinput,
        }
    }

    #[test]
    fn accessibility_tree_requires_reachable_at_spi_bus() {
        let report = accessibility_report(Check::fail("permission denied"), Check::ok("true"));

        assert!(!can_build_accessibility_tree(&report));
    }

    #[test]
    fn accessibility_tree_is_ready_when_bus_and_toolkit_are_ready() {
        let report = accessibility_report(
            Check::ok("('unix:path=/run/user/1000/at-spi/bus',)"),
            Check::ok("true"),
        );

        assert!(can_build_accessibility_tree(&report));
    }

    #[test]
    fn parses_parent_pid_from_proc_status() {
        let status = "Name:\ttest\nPid:\t42\nPPid:\t7\n";

        assert_eq!(parse_parent_pid(status), Some(7));
    }

    #[test]
    fn parses_nul_separated_process_environment() {
        let environment = parse_environ(
            b"DISPLAY=:0\0WAYLAND_DISPLAY=wayland-0\0EMPTY=\0NO_EQUALS\0XDG_SESSION_TYPE=wayland\0",
        );

        assert_eq!(environment.get("DISPLAY").map(String::as_str), Some(":0"));
        assert_eq!(
            environment.get("WAYLAND_DISPLAY").map(String::as_str),
            Some("wayland-0")
        );
        assert_eq!(environment.get("EMPTY").map(String::as_str), Some(""));
        assert!(!environment.contains_key("NO_EQUALS"));
    }

    #[test]
    fn parses_systemd_show_environment_output() {
        let environment = parse_line_environment(
            b"DISPLAY=:0\nHYPRLAND_INSTANCE_SIGNATURE=abc\nNO_EQUALS\nYDOTOOL_SOCKET=/run/ydotoold/socket\n",
        );

        assert_eq!(environment.get("DISPLAY").map(String::as_str), Some(":0"));
        assert_eq!(
            environment
                .get("HYPRLAND_INSTANCE_SIGNATURE")
                .map(String::as_str),
            Some("abc")
        );
        assert_eq!(
            environment.get("YDOTOOL_SOCKET").map(String::as_str),
            Some("/run/ydotoold/socket")
        );
        assert!(!environment.contains_key("NO_EQUALS"));
    }

    #[test]
    fn desktop_env_hydration_includes_xauthority() {
        assert!(DESKTOP_ENV_KEYS.contains(&"XAUTHORITY"));
    }

    #[test]
    fn graphical_process_env_requires_display() {
        let with_display = HashMap::from([("DISPLAY".to_string(), ":0".to_string())]);
        let with_wayland =
            HashMap::from([("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string())]);
        let without_display = HashMap::from([("XAUTHORITY".to_string(), "/tmp/xauth".to_string())]);

        assert!(process_env_has_graphical_display(&with_display));
        assert!(process_env_has_graphical_display(&with_wayland));
        assert!(!process_env_has_graphical_display(&without_display));
    }

    #[test]
    fn readiness_requires_exact_window_focus_for_targeted_input() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, false);
        let input = input_report(true);

        let readiness = readiness_report(
            &platform,
            &portal_report(Check::fail("missing")),
            &accessibility,
            &windowing,
            &input,
        );

        assert!(readiness.can_query_windows);
        assert!(!readiness.can_focus_windows);
        assert!(readiness
            .recommended_next_step
            .contains("exact-focus window backend"));
        assert!(readiness
            .blockers
            .iter()
            .any(|blocker| blocker.contains("Exact window activation")));
    }

    #[test]
    fn readiness_treats_kwin_as_full_window_backend() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let mut windowing = windowing_report(false, false);
        windowing.kwin = Check::ok("KWin scripting is available");
        windowing.can_list_windows = true;
        windowing.can_focus_apps = true;
        windowing.can_focus_windows = true;
        let input = input_report(true);

        let readiness = readiness_report(
            &platform,
            &portal_report(Check::fail("missing")),
            &accessibility,
            &windowing,
            &input,
        );

        assert!(readiness.can_query_windows);
        assert!(readiness.can_focus_apps);
        assert!(readiness.can_focus_windows);
        assert!(readiness.blockers.is_empty());
    }

    #[test]
    fn readiness_message_mentions_generic_window_targeting() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report(true);

        let readiness = readiness_report(
            &platform,
            &portal_report(Check::fail("missing")),
            &accessibility,
            &windowing,
            &input,
        );

        assert!(readiness.blockers.is_empty());
        assert!(readiness
            .recommended_next_step
            .contains("AT-SPI tree support"));
        assert!(readiness.recommended_next_step.contains("window targeting"));
        assert!(!readiness
            .recommended_next_step
            .contains("GNOME window targeting"));
    }

    #[test]
    fn readiness_accepts_connectable_ydotool_socket_without_direct_uinput_access() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::ok("ydotool"),
            Check::ok("ydotoold"),
            Check::ok("connectable: /tmp/.ydotool_socket"),
            Check::fail("/dev/uinput: Permission denied"),
        );

        let readiness = readiness_report(
            &platform,
            &portal_report(Check::fail("missing")),
            &accessibility,
            &windowing,
            &input,
        );

        assert!(readiness.can_send_development_input);
        assert!(readiness.blockers.is_empty());
    }

    #[test]
    fn readiness_accepts_direct_uinput_without_connectable_ydotool_socket() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::ok("ydotool"),
            Check::fail("ydotoold not running"),
            Check::fail("no connectable ydotool socket"),
            Check::ok("read/write: /dev/uinput"),
        );

        let readiness = readiness_report(
            &platform,
            &portal_report(Check::fail("missing")),
            &accessibility,
            &windowing,
            &input,
        );

        assert!(readiness.can_send_development_input);
        assert!(readiness.blockers.is_empty());
    }

    #[test]
    fn readiness_accepts_remote_desktop_portal_without_local_input_backend() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::fail("missing ydotool"),
            Check::fail("ydotoold not running"),
            Check::fail("no connectable ydotool socket"),
            Check::fail("/dev/uinput: Permission denied"),
        );

        let readiness = readiness_report(
            &platform,
            &portal_report(Check::ok("org.freedesktop.portal.RemoteDesktop")),
            &accessibility,
            &windowing,
            &input,
        );

        assert!(readiness.can_send_development_input);
        assert!(readiness.blockers.is_empty());
    }

    #[test]
    fn readiness_rejects_inaccessible_ydotool_paths() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::ok("ydotool"),
            Check::ok("ydotoold"),
            Check::fail("/tmp/.ydotool_socket: Permission denied"),
            Check::fail("/dev/uinput: Permission denied"),
        );

        let readiness = readiness_report(
            &platform,
            &portal_report(Check::fail("missing")),
            &accessibility,
            &windowing,
            &input,
        );

        assert!(!readiness.can_send_development_input);
        assert!(readiness
            .recommended_next_step
            .contains("Enable a supported input backend"));
        assert!(readiness
            .blockers
            .iter()
            .any(|blocker| blocker.contains("Development input is unavailable")));
    }

    #[test]
    fn ydotool_socket_check_requires_a_connectable_socket() {
        let dir = std::env::temp_dir().join(format!(
            "codex-computer-use-diagnostics-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp diagnostics dir");
        let socket = dir.join("ydotool.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket).expect("bind temp diagnostics socket");

        let check = socket_connect_check(&socket);

        assert!(check.ok, "{check:?}");
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ydotool_socket_check_accepts_datagram_socket() {
        let dir = std::env::temp_dir().join(format!(
            "codex-computer-use-diagnostics-dgram-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp diagnostics dir");
        let socket = dir.join("ydotool.sock");
        let datagram =
            std::os::unix::net::UnixDatagram::bind(&socket).expect("bind temp datagram socket");

        let check = socket_connect_check(&socket);

        assert!(check.ok, "{check:?}");
        drop(datagram);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readiness_reports_cosmic_window_blocker_on_cosmic() {
        let mut platform = platform_report();
        platform.xdg_current_desktop = Some("COSMIC".to_string());
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(false, false);
        let input = input_report(true);

        let readiness = readiness_report(
            &platform,
            &portal_report(Check::fail("missing")),
            &accessibility,
            &windowing,
            &input,
        );

        assert!(readiness
            .blockers
            .iter()
            .any(|blocker| blocker.contains("COSMIC Wayland window introspection")));
    }
}
