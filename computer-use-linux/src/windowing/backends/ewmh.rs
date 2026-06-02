//! Generic ICCCM/EWMH window backend via `wmctrl`.
//!
//! Covers any X11-compatible window manager (Openbox, XFWM, Fluxbox, Sawfish,
//! Enlightenment, …) that implements EWMH conventions.  The Wayland-specific
//! backends (GNOME, COSMIC, KWin, Hyprland, i3/sway) are tried first; this
//! backend activates only when all of them are absent.

use crate::terminal::enrich_terminal_windows;
use crate::windowing::registry::BackendProbe;
use crate::windowing::types::{WindowBounds, WindowInfo};
use anyhow::{bail, Context, Result};
use std::process::Command;

pub const EWMH_BACKEND: &str = "ewmh";

pub fn probe() -> BackendProbe {
    match Command::new("wmctrl").arg("-l").output() {
        Ok(output) if output.status.success() => BackendProbe {
            id: EWMH_BACKEND,
            ok: true,
            can_list_windows: true,
            can_focus_apps: true,
            can_focus_windows: true,
            detail: "wmctrl -l succeeded; ICCCM/EWMH window listing is available".to_string(),
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            BackendProbe {
                id: EWMH_BACKEND,
                ok: false,
                can_list_windows: false,
                can_focus_apps: false,
                can_focus_windows: false,
                detail: if stderr.is_empty() {
                    format!("wmctrl -l exited with {}", output.status)
                } else {
                    stderr
                },
            }
        }
        Err(e) => BackendProbe {
            id: EWMH_BACKEND,
            ok: false,
            can_list_windows: false,
            can_focus_apps: false,
            can_focus_windows: false,
            detail: format!("wmctrl not installed or not in PATH: {e}"),
        },
    }
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let output = Command::new("wmctrl")
        .args(["-lGpx"])
        .output()
        .context("failed to run wmctrl -lGpx")?;
    if !output.status.success() {
        bail!(
            "wmctrl -lGpx failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let focused_id = active_window_id();
    let mut windows = parse_wmctrl_output(&text, focused_id);
    enrich_terminal_windows(&mut windows);
    Ok(windows)
}

pub fn activate_window(window_id: u64) -> Result<()> {
    let hex_id = format!("0x{window_id:x}");
    let output = Command::new("wmctrl")
        .args(["-i", "-a", &hex_id])
        .output()
        .with_context(|| format!("failed to run wmctrl -i -a {hex_id}"))?;
    if !output.status.success() {
        bail!(
            "wmctrl -i -a {hex_id} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn active_window_id() -> Option<u64> {
    // xprop -root _NET_ACTIVE_WINDOW prints, e.g.:
    //   _NET_ACTIVE_WINDOW(WINDOW): window id # 0x7800020
    let output = Command::new("xprop")
        .args(["-root", "_NET_ACTIVE_WINDOW"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let hex_start = stdout.find("0x")?;
    let hex_str = stdout[hex_start + 2..]
        .split(|c: char| !c.is_ascii_hexdigit())
        .next()?;
    u64::from_str_radix(hex_str, 16).ok()
}

fn parse_wmctrl_output(text: &str, focused_id: Option<u64>) -> Vec<WindowInfo> {
    text.lines()
        .filter_map(|line| parse_wmctrl_line(line.trim(), focused_id))
        .collect()
}

/// Parse one line of `wmctrl -lGpx` output.
///
/// Column layout:
///   `0x<id>  <desktop>  <pid>  <x>  <y>  <width>  <height>  <instance.Class>  <title…>`
pub(crate) fn parse_wmctrl_line(line: &str, focused_id: Option<u64>) -> Option<WindowInfo> {
    if line.is_empty() {
        return None;
    }
    let tokens: Vec<&str> = line.split_whitespace().collect();
    // Minimum: id desktop pid x y width height wm_class (title may be absent)
    if tokens.len() < 8 {
        return None;
    }

    let window_id = tokens[0]
        .strip_prefix("0x")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())?;

    // Desktop -1 means "sticky / visible on all desktops" → no specific workspace.
    let workspace = tokens[1].parse::<i32>().ok().filter(|&d| d != -1);

    let pid = tokens[2].parse::<u32>().ok().filter(|&p| p != 0);

    let x = tokens[3].parse::<i32>().ok();
    let y = tokens[4].parse::<i32>().ok();
    let width = tokens[5].parse::<u32>().ok()?;
    let height = tokens[6].parse::<u32>().ok()?;

    // WM_CLASS field from wmctrl is "resource_name.ResourceClass".
    // We store resource_name as app_id and ResourceClass as wm_class to match
    // the convention used by the i3 and GNOME backends.
    let wm_class_raw = tokens[7];
    let (app_id, wm_class) = match wm_class_raw.rfind('.') {
        Some(dot) => (
            non_empty(wm_class_raw[..dot].to_string()),
            non_empty(wm_class_raw[dot + 1..].to_string()),
        ),
        None => (
            non_empty(wm_class_raw.to_string()),
            non_empty(wm_class_raw.to_string()),
        ),
    };

    let title = if tokens.len() > 8 {
        non_empty(tokens[8..].join(" "))
    } else {
        None
    };

    Some(WindowInfo {
        window_id,
        title,
        app_id,
        wm_class,
        pid,
        bounds: Some(WindowBounds { x, y, width, height }),
        workspace,
        focused: focused_id == Some(window_id),
        hidden: false,
        client_type: Some("x11".to_string()),
        backend: EWMH_BACKEND.to_string(),
        terminal: None,
    })
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wmctrl_line_extracts_all_fields() {
        let line =
            "0x07800020  0 12345     0     0  1920 1080 firefox.Firefox  Firefox - Mozilla Firefox";
        let w = parse_wmctrl_line(line, Some(0x07800020)).expect("should parse");
        assert_eq!(w.window_id, 0x07800020);
        assert_eq!(w.pid, Some(12345));
        assert_eq!(w.wm_class.as_deref(), Some("Firefox"));
        assert_eq!(w.app_id.as_deref(), Some("firefox"));
        assert_eq!(
            w.title.as_deref(),
            Some("Firefox - Mozilla Firefox")
        );
        assert_eq!(w.bounds.as_ref().map(|b| b.width), Some(1920));
        assert_eq!(w.bounds.as_ref().map(|b| b.height), Some(1080));
        assert!(w.focused);
        assert_eq!(w.client_type.as_deref(), Some("x11"));
    }

    #[test]
    fn parse_wmctrl_line_marks_unfocused_when_id_differs() {
        let line = "0x07800021  0 12345     0     0   800  600 openbox.Openbox  Desktop";
        let w = parse_wmctrl_line(line, Some(0x07800020)).expect("should parse");
        assert!(!w.focused);
    }

    #[test]
    fn parse_wmctrl_line_treats_desktop_minus1_as_no_workspace() {
        let line = "0x07800020  -1     0     0     0   400  300 panel.Panel  Desktop Panel";
        let w = parse_wmctrl_line(line, None).expect("should parse");
        assert_eq!(w.workspace, None);
    }

    #[test]
    fn parse_wmctrl_line_handles_no_dot_in_wm_class() {
        let line = "0x00000001  0     0     0     0   100  100 Openbox  some window";
        let w = parse_wmctrl_line(line, None).expect("should parse");
        assert_eq!(w.wm_class.as_deref(), Some("Openbox"));
        assert_eq!(w.app_id.as_deref(), Some("Openbox"));
    }

    #[test]
    fn parse_wmctrl_line_rejects_short_lines() {
        assert!(parse_wmctrl_line("0x00000001  0 12345", None).is_none());
    }

    #[test]
    fn parse_wmctrl_line_handles_missing_title() {
        // 8 tokens exactly — no title column
        let line = "0x00000001  0     0     0     0   800  600 xterm.XTerm";
        let w = parse_wmctrl_line(line, None).expect("should parse");
        assert!(w.title.is_none());
    }

    #[test]
    fn parse_wmctrl_output_skips_empty_lines() {
        let output =
            "\n0x07800020  0 12345     0     0  1920 1080 firefox.Firefox  Firefox\n\n";
        let windows = parse_wmctrl_output(output, None);
        assert_eq!(windows.len(), 1);
    }

    #[test]
    fn parse_wmctrl_output_marks_focused_window() {
        let output = "0x00000001  0     0     0     0   800  600 xterm.XTerm  xterm\n\
                      0x00000002  0     0   800     0   800  600 nvim.nvim    nvim\n";
        let windows = parse_wmctrl_output(output, Some(0x00000002));
        assert!(!windows[0].focused);
        assert!(windows[1].focused);
    }
}
