use crate::diagnostics::hydrate_session_bus_env;
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use schemars::JsonSchema;
use serde::Serialize;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zbus::{
    message::{Message, Type as MessageType},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
    MatchRule, MessageStream, Proxy,
};

const PORTAL_REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const PORTAL_REQUEST_PATH_NAMESPACE: &str = "/org/freedesktop/portal/desktop/request";

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ScreenshotCapture {
    pub mime_type: String,
    pub data_url: String,
    pub source: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScreenshotCleanup {
    DeletePath(PathBuf),
    Preserve,
}

pub async fn capture_screenshot() -> Result<ScreenshotCapture> {
    hydrate_session_bus_env();

    let gnome_err = match capture_with_gnome_shell().await {
        Ok(capture) => return Ok(capture),
        Err(e) => e,
    };

    // Spectacle (KDE/Plasma) is silent and requires no interactive approval
    // dialog, so try it before the XDG portal which may prompt the user.
    let spectacle_err = match capture_with_spectacle() {
        Ok(capture) => return Ok(capture),
        Err(e) => e,
    };

    // X11 fallback: silent, no daemon required, works on pure X11 sessions
    // where neither GNOME Shell nor Spectacle is available.
    let x11_err = match capture_with_x11() {
        Ok(capture) => return Ok(capture),
        Err(e) => e,
    };

    match capture_with_portal().await {
        Ok(capture) => Ok(capture),
        Err(portal_err) => Err(anyhow!(
            "GNOME Shell screenshot failed: {gnome_err:#}; \
             spectacle screenshot failed: {spectacle_err:#}; \
             X11 screenshot failed: {x11_err:#}; \
             XDG portal screenshot failed: {portal_err:#}"
        )),
    }
}

fn capture_with_spectacle() -> Result<ScreenshotCapture> {
    let path = temp_png_path("spectacle");
    let result = try_spectacle_capture(&path);
    let _ = fs::remove_file(&path);
    result
}

fn try_spectacle_capture(path: &Path) -> Result<ScreenshotCapture> {
    let filename = path
        .to_str()
        .context("temporary screenshot path is not valid UTF-8")?;

    let output = Command::new("spectacle")
        .args(["--background", "--nonotify", "--fullscreen", "--output", filename])
        .output()
        .context("spectacle is not installed or could not be run")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        bail!(
            "spectacle exited with {}: {}",
            output.status,
            if stderr.is_empty() { "no output" } else { stderr }
        );
    }

    read_png_as_capture_inner(path, "spectacle")
}

fn capture_with_x11() -> Result<ScreenshotCapture> {
    if std::env::var_os("DISPLAY").is_none() {
        bail!("DISPLAY not set, not an X11 session");
    }
    let path = temp_png_path("x11");
    let result = try_scrot_capture(&path).or_else(|_| try_maim_capture(&path));
    let _ = fs::remove_file(&path);
    result
}

fn try_scrot_capture(path: &Path) -> Result<ScreenshotCapture> {
    let filename = path
        .to_str()
        .context("temporary screenshot path is not valid UTF-8")?;
    let output = Command::new("scrot")
        .args(["--silent", filename])
        .output()
        .context("scrot is not installed or could not be run")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        bail!(
            "scrot exited with {}: {}",
            output.status,
            if stderr.is_empty() { "no output" } else { stderr }
        );
    }
    read_png_as_capture_inner(path, "x11-scrot")
}

fn try_maim_capture(path: &Path) -> Result<ScreenshotCapture> {
    let filename = path
        .to_str()
        .context("temporary screenshot path is not valid UTF-8")?;
    let output = Command::new("maim")
        .arg(filename)
        .output()
        .context("maim is not installed or could not be run")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        bail!(
            "maim exited with {}: {}",
            output.status,
            if stderr.is_empty() { "no output" } else { stderr }
        );
    }
    read_png_as_capture_inner(path, "x11-maim")
}

async fn capture_with_gnome_shell() -> Result<ScreenshotCapture> {
    let connection = zbus::Connection::session()
        .await
        .context("failed to connect to session bus")?;
    let proxy = Proxy::new(
        &connection,
        "org.gnome.Shell.Screenshot",
        "/org/gnome/Shell/Screenshot",
        "org.gnome.Shell.Screenshot",
    )
    .await
    .context("failed to create GNOME Shell screenshot proxy")?;
    let path = temp_png_path("gnome-shell");
    let filename = path
        .to_str()
        .context("temporary screenshot path is not valid UTF-8")?;
    let result = proxy.call("Screenshot", &(false, false, filename)).await;
    let (success, filename_used): (bool, String) = match result {
        Ok(result) => result,
        Err(error) => {
            cleanup_gnome_requested_path(&path);
            return Err(error).context("GNOME Shell Screenshot call failed");
        }
    };

    if !success {
        cleanup_gnome_requested_path(&path);
        bail!("GNOME Shell reported screenshot failure");
    }

    read_png_as_capture(
        PathBuf::from(filename_used),
        "gnome-shell",
        ScreenshotCleanup::DeletePath(path),
    )
    .await
}

async fn capture_with_portal() -> Result<ScreenshotCapture> {
    let connection = zbus::Connection::session()
        .await
        .context("failed to connect to session bus")?;
    let token = request_token();
    // Some portals rewrite the request handle, so subscribe before calling Screenshot
    // and filter by the returned handle instead of subscribing after the call.
    let mut response_stream = portal_response_stream(&connection).await?;

    let portal_proxy = Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Screenshot",
    )
    .await
    .context("failed to create XDG portal screenshot proxy")?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("interactive", Value::from(false));
    let handle: OwnedObjectPath = portal_proxy
        .call("Screenshot", &("", options))
        .await
        .context("XDG portal Screenshot call failed")?;

    let (response_code, results) = tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_portal_response(&mut response_stream, handle.as_str()),
    )
    .await
    .context("timed out waiting for XDG portal screenshot response")??;

    if response_code != 0 {
        bail!("XDG portal screenshot was denied or cancelled with response code {response_code}");
    }

    let uri_value = results
        .get("uri")
        .context("XDG portal screenshot response did not include a uri")?;
    let uri: String = uri_value
        .try_clone()
        .context("failed to clone XDG portal screenshot uri")?
        .try_into()
        .context("XDG portal screenshot uri was not a string")?;
    let path = file_uri_to_path(&uri)?;

    read_png_as_capture(path, "xdg-desktop-portal", ScreenshotCleanup::Preserve).await
}

async fn portal_response_stream(connection: &zbus::Connection) -> Result<MessageStream> {
    let response_rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(PORTAL_REQUEST_INTERFACE)?
        .member("Response")?
        .path_namespace(PORTAL_REQUEST_PATH_NAMESPACE)?
        .build();

    MessageStream::for_match_rule(response_rule, connection, None)
        .await
        .context("failed to subscribe to XDG portal screenshot responses")
}

async fn wait_for_portal_response(
    response_stream: &mut MessageStream,
    request_path: &str,
) -> Result<(u32, HashMap<String, OwnedValue>)> {
    loop {
        let response = response_stream
            .next()
            .await
            .context("XDG portal screenshot response stream ended")?
            .context("XDG portal screenshot response stream failed")?;

        if !portal_response_matches_path(&response, request_path) {
            continue;
        }

        return response
            .body()
            .deserialize()
            .context("failed to decode XDG portal screenshot response");
    }
}

fn portal_response_matches_path(response: &Message, request_path: &str) -> bool {
    response
        .header()
        .path()
        .is_some_and(|path| path.as_str() == request_path)
}

async fn read_png_as_capture(
    path: PathBuf,
    source: &str,
    cleanup: ScreenshotCleanup,
) -> Result<ScreenshotCapture> {
    let result = read_png_as_capture_inner(&path, source);
    if let ScreenshotCleanup::DeletePath(path) = cleanup {
        let _ = fs::remove_file(path);
    }
    result
}

fn read_png_as_capture_inner(path: &Path, source: &str) -> Result<ScreenshotCapture> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read screenshot file {}", path.display()))?;
    if bytes.is_empty() {
        bail!("screenshot file was empty: {}", path.display());
    }
    let (width, height) = png_dimensions(&bytes)?;
    let encoded = STANDARD.encode(bytes);
    Ok(ScreenshotCapture {
        mime_type: "image/png".to_string(),
        data_url: format!("data:image/png;base64,{encoded}"),
        source: source.to_string(),
        width,
        height,
    })
}

fn cleanup_gnome_requested_path(path: &Path) {
    let _ = fs::remove_file(path);
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        bail!("screenshot file was not a valid PNG");
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    if width == 0 || height == 0 {
        bail!("screenshot PNG had invalid dimensions {width}x{height}");
    }
    Ok((width, height))
}

fn file_uri_to_path(uri: &str) -> Result<PathBuf> {
    let Some(rest) = uri.strip_prefix("file://") else {
        bail!("unsupported screenshot uri: {uri}");
    };
    Ok(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    decoded.push(byte);
                    index += 3;
                    continue;
                }
            }
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn temp_png_path(source: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "codex-computer-use-{source}-{}.png",
        unique_suffix()
    ))
}

fn request_token() -> String {
    format!("codex_{}", unique_suffix().replace('-', "_"))
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("codex-screenshot-test-{name}-{}", unique_suffix()))
    }

    fn valid_png(width: u32, height: u32) -> Vec<u8> {
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13_u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&width.to_be_bytes());
        png.extend_from_slice(&height.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        png
    }

    #[test]
    fn decodes_file_uri_percent_escapes() {
        assert_eq!(
            file_uri_to_path("file:///tmp/Codex%20Screenshot.png").unwrap(),
            PathBuf::from("/tmp/Codex Screenshot.png")
        );
    }

    #[test]
    fn request_token_is_portal_safe() {
        let token = request_token();
        assert!(token.starts_with("codex_"));
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn reads_png_dimensions_from_ihdr() {
        let png = valid_png(3840, 1080);

        assert_eq!(png_dimensions(&png).unwrap(), (3840, 1080));
    }

    #[tokio::test]
    async fn portal_capture_preserves_valid_returned_path() {
        let path = test_path("portal-valid");
        fs::write(&path, valid_png(1, 1)).unwrap();

        let capture = read_png_as_capture(
            path.clone(),
            "xdg-desktop-portal",
            ScreenshotCleanup::Preserve,
        )
        .await
        .unwrap();

        assert_eq!(capture.source, "xdg-desktop-portal");
        assert!(path.exists());
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn portal_capture_preserves_invalid_returned_path() {
        let path = test_path("portal-invalid");
        fs::write(&path, b"").unwrap();

        let error = read_png_as_capture(
            path.clone(),
            "xdg-desktop-portal",
            ScreenshotCleanup::Preserve,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("screenshot file was empty"));
        assert!(path.exists());
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn gnome_capture_deletes_backend_temp_path_on_success() {
        let path = test_path("gnome-valid");
        fs::write(&path, valid_png(1, 1)).unwrap();

        let capture = read_png_as_capture(
            path.clone(),
            "gnome-shell",
            ScreenshotCleanup::DeletePath(path.clone()),
        )
        .await
        .unwrap();

        assert_eq!(capture.source, "gnome-shell");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn gnome_capture_deletes_backend_temp_path_on_parse_failure() {
        let path = test_path("gnome-invalid");
        fs::write(&path, b"").unwrap();

        let error = read_png_as_capture(
            path.clone(),
            "gnome-shell",
            ScreenshotCleanup::DeletePath(path.clone()),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("screenshot file was empty"));
        assert!(!path.exists());
    }

    #[test]
    fn gnome_failure_cleanup_removes_requested_temp_path() {
        let path = test_path("gnome-pre-read-failure");
        fs::write(&path, b"partial").unwrap();

        cleanup_gnome_requested_path(&path);

        assert!(!path.exists());
    }

    // X11 tests use read_png_as_capture_inner directly — try_scrot_capture and
    // try_maim_capture run external binaries not available in the test environment.

    #[test]
    fn x11_scrot_source_label_propagates_from_valid_png() {
        let path = test_path("x11-scrot-valid");
        fs::write(&path, valid_png(1920, 1080)).unwrap();

        let capture = read_png_as_capture_inner(&path, "x11-scrot").unwrap();

        assert_eq!(capture.source, "x11-scrot");
        assert_eq!(capture.width, 1920);
        assert_eq!(capture.height, 1080);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn x11_maim_source_label_propagates_from_valid_png() {
        let path = test_path("x11-maim-valid");
        fs::write(&path, valid_png(2560, 1440)).unwrap();

        let capture = read_png_as_capture_inner(&path, "x11-maim").unwrap();

        assert_eq!(capture.source, "x11-maim");
        assert_eq!(capture.width, 2560);
        assert_eq!(capture.height, 1440);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn capture_with_x11_fails_gracefully_without_display() {
        // If DISPLAY is set (running inside an X11/XWayland session), the guard
        // passes and we hit the binary check instead — skip in that case.
        if std::env::var_os("DISPLAY").is_some() {
            return;
        }
        let err = capture_with_x11().unwrap_err();
        assert!(
            err.to_string().contains("DISPLAY"),
            "error should mention DISPLAY, got: {err}"
        );
    }

    // spectacle tests use read_png_as_capture_inner directly — try_spectacle_capture
    // runs the spectacle binary which is not available in the test environment.

    #[test]
    fn spectacle_source_label_propagates_from_valid_png() {
        let path = test_path("spectacle-valid");
        fs::write(&path, valid_png(2560, 1440)).unwrap();

        let capture = read_png_as_capture_inner(&path, "spectacle").unwrap();

        assert_eq!(capture.source, "spectacle");
        assert_eq!(capture.width, 2560);
        assert_eq!(capture.height, 1440);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn spectacle_empty_output_file_returns_error() {
        let path = test_path("spectacle-empty");
        fs::write(&path, b"").unwrap();

        let err = read_png_as_capture_inner(&path, "spectacle").unwrap_err();

        assert!(err.to_string().contains("screenshot file was empty"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn spectacle_corrupt_output_file_returns_error() {
        let path = test_path("spectacle-corrupt");
        fs::write(&path, b"not a png").unwrap();

        let err = read_png_as_capture_inner(&path, "spectacle").unwrap_err();

        assert!(err.to_string().contains("not a valid PNG"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn capture_with_spectacle_fails_gracefully_when_not_installed() {
        // spectacle is not available in the test environment; the function must
        // return an error (not panic) and must not leave a temp file behind.
        let err = capture_with_spectacle().unwrap_err();
        assert!(
            err.to_string().contains("spectacle"),
            "error should mention spectacle, got: {err}"
        );
    }

    #[tokio::test]
    async fn gnome_deletes_requested_temp_path_and_preserves_unexpected_returned_path() {
        let requested = test_path("gnome-requested");
        let returned = test_path("gnome-returned");
        fs::write(&requested, b"partial").unwrap();
        fs::write(&returned, valid_png(1, 1)).unwrap();

        let capture = read_png_as_capture(
            returned.clone(),
            "gnome-shell",
            ScreenshotCleanup::DeletePath(requested.clone()),
        )
        .await
        .unwrap();

        assert_eq!(capture.source, "gnome-shell");
        assert!(!requested.exists());
        assert!(returned.exists());
        let _ = fs::remove_file(returned);
    }
}
