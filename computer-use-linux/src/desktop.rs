//! App launch via XDG `.desktop` files.
//!
//! Resolves an app name or desktop-file stem to an installed `.desktop` entry,
//! then launches it via `gtk-launch`, `gio open`, or a direct `Exec=` spawn —
//! whichever is available.

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LaunchAppResult {
    pub ok: bool,
    /// Resolved `Name=` from the matched `.desktop` entry.
    pub app_name: Option<String>,
    /// Absolute path of the `.desktop` file that was matched and used.
    pub desktop_file: Option<String>,
    /// The launch command that was invoked.
    pub launch_command: Option<String>,
    pub message: String,
}

pub fn launch_app(query: &str) -> LaunchAppResult {
    let query = query.trim();
    if query.is_empty() {
        return LaunchAppResult {
            ok: false,
            app_name: None,
            desktop_file: None,
            launch_command: None,
            message: "app_name must not be empty.".to_string(),
        };
    }

    match find_and_launch(query) {
        Ok(result) => result,
        Err(e) => LaunchAppResult {
            ok: false,
            app_name: None,
            desktop_file: None,
            launch_command: None,
            message: e.to_string(),
        },
    }
}

fn find_and_launch(query: &str) -> Result<LaunchAppResult> {
    if let Some((path, name)) = find_desktop_file(query) {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(query)
            .to_string();

        let launch = try_gtk_launch(&stem)
            .or_else(|_| try_gio_open(&path))
            .or_else(|_| try_exec_from_desktop(&path));

        return Ok(match launch {
            Ok(cmd) => LaunchAppResult {
                ok: true,
                app_name: Some(name.clone()),
                desktop_file: Some(path.display().to_string()),
                launch_command: Some(cmd),
                message: format!("Launched {name}."),
            },
            Err(e) => LaunchAppResult {
                ok: false,
                app_name: Some(name),
                desktop_file: Some(path.display().to_string()),
                launch_command: None,
                message: format!("Found .desktop file but all launch methods failed: {e:#}"),
            },
        });
    }

    // No .desktop file found; try the query as a bare command name if it looks safe.
    if looks_like_command(query) {
        if let Ok(cmd) = try_direct_command(query) {
            return Ok(LaunchAppResult {
                ok: true,
                app_name: Some(query.to_string()),
                desktop_file: None,
                launch_command: Some(cmd),
                message: format!(
                    "No .desktop file found for {query:?}; launched as a direct command."
                ),
            });
        }
    }

    Ok(LaunchAppResult {
        ok: false,
        app_name: None,
        desktop_file: None,
        launch_command: None,
        message: format!(
            "No installed application found matching {query:?}. \
             Pass a desktop-file stem (e.g. \"firefox\"), an app Name (e.g. \"Firefox\"), \
             or a reverse-DNS id (e.g. \"org.mozilla.firefox\"). \
             Use list_apps to see currently open apps."
        ),
    })
}

// ── Desktop file search ─────────────────────────────────────────────────────

fn find_desktop_file(query: &str) -> Option<(PathBuf, String)> {
    let query_lower = query.to_ascii_lowercase();
    // Strip trailing .desktop so "firefox.desktop" and "firefox" both work.
    let query_stem = query.strip_suffix(".desktop").unwrap_or(query);
    let query_stem_lower = query_stem.to_ascii_lowercase();

    let mut best: Option<(u8, PathBuf, String)> = None;

    for dir in desktop_search_dirs() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let stem_lower = stem.to_ascii_lowercase();

            let info = match parse_desktop_info(&path) {
                Some(i) if i.has_exec && i.app_type.as_deref() == Some("Application") => i,
                _ => continue,
            };
            if info.no_display {
                continue;
            }

            let name = info.name.unwrap_or_else(|| stem.clone());
            let name_lower = name.to_ascii_lowercase();

            let priority: u8 = if stem_lower == query_stem_lower {
                0 // exact filename stem
            } else if name_lower == query_lower {
                1 // exact Name= match
            } else if name_lower.contains(&query_lower) {
                2 // Name= contains query
            } else if stem_lower.contains(&query_stem_lower) {
                3 // filename contains query
            } else {
                continue;
            };

            let is_better = best
                .as_ref()
                .map(|(p, _, _)| priority < *p)
                .unwrap_or(true);
            if is_better {
                best = Some((priority, path, name));
            }
        }
    }

    best.map(|(_, path, name)| (path, name))
}

fn desktop_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // User-local overrides first.
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/share/applications"));
        dirs.push(home.join(".local/share/flatpak/exports/share/applications"));
    }

    // XDG_DATA_DIRS (colon-separated), default to standard paths.
    let data_dirs = env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for segment in data_dirs.split(':') {
        if segment.is_empty() {
            continue;
        }
        let path = PathBuf::from(segment).join("applications");
        if !dirs.contains(&path) {
            dirs.push(path);
        }
    }

    // Flatpak system-wide apps.
    let flatpak_system = PathBuf::from("/var/lib/flatpak/exports/share/applications");
    if !dirs.contains(&flatpak_system) {
        dirs.push(flatpak_system);
    }

    dirs.into_iter().filter(|d| d.is_dir()).collect()
}

struct DesktopInfo {
    name: Option<String>,
    exec: Option<String>,
    app_type: Option<String>,
    has_exec: bool,
    no_display: bool,
    comment: Option<String>,
}

fn parse_desktop_info(path: &Path) -> Option<DesktopInfo> {
    let content = fs::read_to_string(path).ok()?;
    let mut in_section = false;
    let mut info = DesktopInfo {
        name: None,
        exec: None,
        app_type: None,
        has_exec: false,
        no_display: false,
        comment: None,
    };

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_section = line == "[Desktop Entry]";
            continue;
        }
        if !in_section || line.starts_with('#') || line.is_empty() {
            continue;
        }
        // Only use the unlocalized keys (no bracket suffix like Name[de]=).
        if let Some(value) = line.strip_prefix("Name=") {
            if info.name.is_none() {
                info.name = Some(value.to_string());
            }
        } else if let Some(value) = line.strip_prefix("Comment=") {
            if info.comment.is_none() {
                info.comment = Some(value.to_string());
            }
        } else if let Some(value) = line.strip_prefix("Exec=") {
            info.exec = Some(value.to_string());
            info.has_exec = true;
        } else if let Some(value) = line.strip_prefix("Type=") {
            info.app_type = Some(value.to_string());
        } else if line == "NoDisplay=true" || line == "Hidden=true" {
            info.no_display = true;
        }
    }

    Some(info)
}

// ── Installed-app enumeration ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InstalledApp {
    pub name: String,
    pub desktop_file: String,
    pub comment: Option<String>,
}

pub fn list_installed_apps() -> Vec<InstalledApp> {
    let mut apps: Vec<InstalledApp> = Vec::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for dir in desktop_search_dirs() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let info = match parse_desktop_info(&path) {
                Some(i)
                    if i.has_exec
                        && i.app_type.as_deref() == Some("Application")
                        && !i.no_display =>
                {
                    i
                }
                _ => continue,
            };
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let name = info.name.unwrap_or_else(|| stem.clone());
            if seen_names.contains(&name) {
                continue;
            }
            seen_names.insert(name.clone());
            apps.push(InstalledApp {
                name,
                desktop_file: path.display().to_string(),
                comment: info.comment,
            });
        }
    }
    apps.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    apps
}

// ── Launch methods ──────────────────────────────────────────────────────────

fn try_gtk_launch(stem: &str) -> Result<String> {
    // gtk-launch searches XDG_DATA_DIRS automatically; no path needed.
    let status = Command::new("gtk-launch")
        .arg(stem)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("gtk-launch not available")?
        .wait()
        .context("gtk-launch wait failed")?;
    if status.success() {
        Ok(format!("gtk-launch {stem}"))
    } else {
        anyhow::bail!("gtk-launch {stem} exited with {status}");
    }
}

fn try_gio_open(path: &Path) -> Result<String> {
    let path_str = path.to_string_lossy().into_owned();
    let status = Command::new("gio")
        .args(["open", &path_str])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("gio not available")?
        .wait()
        .context("gio open wait failed")?;
    if status.success() {
        Ok(format!("gio open {path_str}"))
    } else {
        anyhow::bail!("gio open {path_str} exited with {status}");
    }
}

fn try_exec_from_desktop(path: &Path) -> Result<String> {
    let info = parse_desktop_info(path)
        .with_context(|| format!("could not parse {}", path.display()))?;
    let exec = info
        .exec
        .with_context(|| format!("no Exec= in {}", path.display()))?;

    // Strip field-code substitutions (%u, %U, %f, %F, %i, %c, %k, …).
    let clean = strip_field_codes(&exec);
    let clean = clean.trim();
    if clean.is_empty() {
        anyhow::bail!(
            "Exec= in {} is empty after stripping field codes",
            path.display()
        );
    }

    let argv = shell_split(clean)?;
    if argv.is_empty() {
        anyhow::bail!("empty argv after parsing Exec=");
    }

    Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {clean}"))?;

    Ok(clean.to_string())
}

fn try_direct_command(name: &str) -> Result<String> {
    Command::new(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("{name} is not an installed command"))?;
    Ok(name.to_string())
}

fn looks_like_command(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Strip XDG desktop-entry field codes (`%u`, `%U`, `%f`, `%F`, `%i`, `%c`,
/// `%k`, `%d`, `%D`, `%n`, `%N`, `%v`, `%m`) from an `Exec=` value.
/// `%%` becomes a literal `%`.
fn strip_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.peek() {
                Some('%') => {
                    chars.next();
                    out.push('%');
                }
                Some(code)
                    if matches!(
                        code,
                        'u' | 'U' | 'f' | 'F' | 'i' | 'c' | 'k' | 'd' | 'D' | 'n' | 'N'
                            | 'v' | 'm'
                    ) =>
                {
                    chars.next(); // consume the code letter
                }
                _ => out.push('%'), // unknown code — keep as-is
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Minimal POSIX-ish word-split for `Exec=` lines (handles single/double
/// quotes and backslash escapes; does not expand variables or globs).
fn shell_split(s: &str) -> Result<Vec<String>> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;

    for c in s.chars() {
        if escape {
            current.push(c);
            escape = false;
            continue;
        }
        match c {
            '\\' if !in_single => escape = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    args.push(current.split_off(0));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_field_codes_removes_substitution_tokens() {
        assert_eq!(strip_field_codes("firefox %u"), "firefox ");
        assert_eq!(strip_field_codes("app --file=%F --icon %i"), "app --file= --icon ");
        assert_eq!(strip_field_codes("echo %%"), "echo %");
    }

    #[test]
    fn strip_field_codes_preserves_unknown_codes() {
        assert_eq!(strip_field_codes("app %z"), "app %z");
    }

    #[test]
    fn shell_split_handles_plain_args() {
        assert_eq!(
            shell_split("firefox --new-window").unwrap(),
            vec!["firefox", "--new-window"]
        );
    }

    #[test]
    fn shell_split_handles_quoted_args() {
        assert_eq!(
            shell_split(r#"env "MY VAR=1" firefox"#).unwrap(),
            vec!["env", "MY VAR=1", "firefox"]
        );
    }

    #[test]
    fn shell_split_handles_single_quotes() {
        assert_eq!(
            shell_split("sh -c 'echo hello'").unwrap(),
            vec!["sh", "-c", "echo hello"]
        );
    }

    #[test]
    fn shell_split_handles_backslash_escape() {
        assert_eq!(
            shell_split(r"app --arg=hello\ world").unwrap(),
            vec!["app", "--arg=hello world"]
        );
    }

    #[test]
    fn looks_like_command_accepts_simple_names() {
        assert!(looks_like_command("firefox"));
        assert!(looks_like_command("org.gnome.Nautilus"));
        assert!(looks_like_command("code-oss"));
    }

    #[test]
    fn looks_like_command_rejects_paths_and_spaces() {
        assert!(!looks_like_command("/usr/bin/firefox"));
        assert!(!looks_like_command("my app"));
        assert!(!looks_like_command(""));
    }

    #[test]
    fn launch_app_returns_error_for_empty_query() {
        let result = launch_app("");
        assert!(!result.ok);
        assert!(result.message.contains("empty"));
    }

    #[test]
    fn launch_app_returns_not_found_for_unknown_app() {
        let result = launch_app("this-app-does-not-exist-xyzzy-12345");
        assert!(!result.ok);
        assert!(result.desktop_file.is_none());
    }
}
