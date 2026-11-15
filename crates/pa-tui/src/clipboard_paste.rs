//! Reading clipboard payloads for editor paste: a clipboard image (PNG, JPEG, GIF, WebP) and
//! the files a file manager copied. Every reader is best-effort: a missing tool, an empty
//! clipboard, or an unsupported type returns nothing (dropped, never sent).

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::image_load::{LoadedImage, SUPPORTED_IMAGE_MIME_TYPES};

const DEFAULT_LIST_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_POWERSHELL_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MAX_BUFFER_BYTES: usize = 50 * 1024 * 1024;

/// The `text/uri-list` clipboard flavor carrying copied files on X11/Wayland.
const URI_LIST: &str = "text/uri-list";

/// One clipboard image: the base64 payload plus its sniffed mime type.
pub type ClipboardImage = LoadedImage;

/// Strip parameters from a mime type (`image/png; charset=...`).
fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// The supported image type among `types`, in the `SUPPORTED_IMAGE_MIME_TYPES` order;
/// any other `image/*` type loses to `None` (it cannot be sent without a converter).
fn select_supported_image_mime_type(types: &[String]) -> Option<String> {
    let normalized: Vec<String> = types
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(base_mime_type)
        .collect();
    for preferred in SUPPORTED_IMAGE_MIME_TYPES {
        if let Some(found) = normalized.iter().find(|t| t.as_str() == preferred) {
            return Some(found.clone());
        }
    }
    None
}

/// Run one command with a hard timeout; `None` on spawn failure, nonzero
/// exit, or timeout.
async fn run_command(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let child = Command::new(program)
        .args(args)
        // Without the drop-kill, a hung converter keeps running past the
        // deadline — an orphaned direct child of the TUI.
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let Ok(Ok(output)) = tokio::time::timeout(timeout, child.wait_with_output()).await else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    if output.stdout.len() > DEFAULT_MAX_BUFFER_BYTES {
        return None;
    }
    Some(output.stdout)
}

/// Whether this is a WSL session. The env markers are Windows-side too (`WSLENV` is set
/// for interop), so they only count on a Linux build: a native-Windows process must never
/// take the WSL path conversions.
fn is_wsl() -> bool {
    if std::env::consts::OS != "linux" {
        return false;
    }
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSLENV").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/version").is_ok_and(|version| {
        version.contains("microsoft") || version.contains("Microsoft") || version.contains("WSL")
    })
}

/// Whether this is a Wayland session.
fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
}

/// Split a command's textual stdout into trimmed lines.
fn stdout_lines(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// A temp file for one clipboard read; the caller reads it back and removes it.
fn clipboard_temp_path(extension: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir();
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("prime-agent-clip-{}.{extension}", unique_suffix())))
}

/// The path as the Windows clipboard host sees it: a WSL distro path converts through
/// `wslpath`, native Windows is already a Windows path.
async fn windows_path_for(path: &Path) -> Option<String> {
    let raw = if is_wsl() {
        String::from_utf8_lossy(
            &run_command("wslpath", &["-w", path.to_str()?], DEFAULT_LIST_TIMEOUT).await?,
        )
        .trim()
        .to_string()
    } else {
        path.to_str()?.to_string()
    };
    (!raw.is_empty()).then_some(raw)
}

/// `wl-paste` (Wayland): list the offered types, pick the preferred
/// supported image type, read it without a trailing newline.
async fn read_via_wl_paste() -> Option<ClipboardImage> {
    let types =
        stdout_lines(&run_command("wl-paste", &["--list-types"], DEFAULT_LIST_TIMEOUT).await?);
    let selected = select_supported_image_mime_type(&types)?;
    let bytes = run_command(
        "wl-paste",
        &["--type", &selected, "--no-newline"],
        DEFAULT_READ_TIMEOUT,
    )
    .await?;
    clipboard_image_from_bytes(bytes)
}

/// `xclip` (X11): enumerate `TARGETS`, then read the preferred supported
/// image type (trying each in order when no target matched).
async fn read_via_xclip() -> Option<ClipboardImage> {
    let targets = run_command(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        DEFAULT_LIST_TIMEOUT,
    )
    .await
    .map(|stdout| stdout_lines(&stdout))
    .unwrap_or_default();
    let mut candidates: Vec<String> = Vec::new();
    if let Some(preferred) = select_supported_image_mime_type(&targets) {
        candidates.push(preferred);
    }
    candidates.extend(SUPPORTED_IMAGE_MIME_TYPES.iter().map(ToString::to_string));
    for mime_type in candidates {
        let bytes = run_command(
            "xclip",
            &["-selection", "clipboard", "-t", &mime_type, "-o"],
            DEFAULT_READ_TIMEOUT,
        )
        .await
        .filter(|bytes| !bytes.is_empty());
        if let Some(image) = bytes.and_then(clipboard_image_from_bytes) {
            return Some(image);
        }
    }
    None
}

/// The copied files an X11/Wayland clipboard carries as `text/uri-list`.
async fn read_files_via_wl_paste() -> Vec<PathBuf> {
    let Some(types) = run_command("wl-paste", &["--list-types"], DEFAULT_LIST_TIMEOUT).await else {
        return Vec::new();
    };
    if !stdout_lines(&types).iter().any(|kind| kind == URI_LIST) {
        return Vec::new();
    }
    run_command(
        "wl-paste",
        &["--type", URI_LIST, "--no-newline"],
        DEFAULT_READ_TIMEOUT,
    )
    .await
    .map(|bytes| crate::paste_paths::uri_list_paths(&String::from_utf8_lossy(&bytes)))
    .unwrap_or_default()
}

/// The copied files an X11 clipboard carries as `text/uri-list` (a missing target reads
/// nothing).
async fn read_files_via_xclip() -> Vec<PathBuf> {
    run_command(
        "xclip",
        &["-selection", "clipboard", "-t", URI_LIST, "-o"],
        DEFAULT_READ_TIMEOUT,
    )
    .await
    .map(|bytes| crate::paste_paths::uri_list_paths(&String::from_utf8_lossy(&bytes)))
    .unwrap_or_default()
}

/// Validate clipboard bytes by content (the clipboard-reported type may be wrong and
/// there is no converter to fall back on — content sniffing is this port's equivalent of
/// TS's Photon conversion). Returns the payload with the sniffed mime type, or `None`.
fn clipboard_image_from_bytes(bytes: Vec<u8>) -> Option<ClipboardImage> {
    use base64::Engine;
    if bytes.is_empty() {
        return None;
    }
    let mime_type = crate::image_load::detect_supported_image_mime_from_bytes(&bytes)?;
    Some(ClipboardImage {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type: mime_type.to_string(),
    })
}

/// PowerShell (WSL reads the Windows host clipboard; native Windows its own): the script
/// saves the clipboard bitmap as a PNG at a temp path, which this process reads back and
/// removes.
async fn read_via_powershell() -> Option<ClipboardImage> {
    let temp_path = clipboard_temp_path("png")?;
    let windows_path = windows_path_for(&temp_path).await?;
    let escaped_path = windows_path.replace('\'', "''");
    let script = [
        "Add-Type -AssemblyName System.Windows.Forms",
        "Add-Type -AssemblyName System.Drawing",
        format!("$path = '{escaped_path}'").as_str(),
        "$img = [System.Windows.Forms.Clipboard]::GetImage()",
        "if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' } else { Write-Output 'empty' }",
    ]
    .join("; ");
    let output = run_command(
        "powershell.exe",
        &["-NoProfile", "-Command", &script],
        DEFAULT_POWERSHELL_TIMEOUT,
    )
    .await?;
    if String::from_utf8_lossy(&output).trim() != "ok" {
        return None;
    }
    let image = crate::image_load::load_image_from_path(&temp_path)
        .ok()
        .flatten();
    let _ = std::fs::remove_file(&temp_path);
    image
}

/// PowerShell's copied-file drop list (the Windows clipboard; WSL reads the host's):
/// one path per line, UTF-8. WSL gets the host's `C:\...` paths, so each one converts
/// through `wslpath` to a path this process can open.
async fn read_files_via_powershell() -> Vec<PathBuf> {
    let script = [
        "Add-Type -AssemblyName System.Windows.Forms",
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8",
        "$files = [System.Windows.Forms.Clipboard]::GetFileDropList()",
        "foreach ($file in $files) { Write-Output $file }",
    ]
    .join("; ");
    let Some(stdout) = run_command(
        "powershell.exe",
        &["-NoProfile", "-Command", &script],
        DEFAULT_POWERSHELL_TIMEOUT,
    )
    .await
    else {
        return Vec::new();
    };
    let paths: Vec<PathBuf> = stdout_lines(&stdout)
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if !is_wsl() {
        return paths;
    }
    let mut distro_paths = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(raw) = path.to_str() else { continue };
        let Some(bytes) = run_command("wslpath", &["-u", raw], DEFAULT_LIST_TIMEOUT).await else {
            continue;
        };
        let converted = String::from_utf8_lossy(&bytes).trim().to_string();
        if !converted.is_empty() {
            distro_paths.push(PathBuf::from(converted));
        }
    }
    distro_paths
}

/// A unique temp-file suffix: the per-process random hasher seeded by the
/// OS plus the current nanosecond clock.
fn unique_suffix() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64),
    );
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}

/// macOS: a JavaScript-for-Automation script reads the pasteboard's PNG data and
/// writes it to a temp file, which we read back and remove. Non-PNG flavors are not read.
async fn read_via_osascript() -> Option<ClipboardImage> {
    let temp_path = clipboard_temp_path("png")?;
    let path_str = temp_path.to_str()?.to_string();
    let script = format!(
        r"ObjC.import('Cocoa');
const pb = $.NSPasteboard.generalPasteboard;
const types = pb.types.allObjects;
if (!types.containsObject($.NSPasteboardTypePNG)) {{
    'empty'
}} else {{
    const data = pb.dataForType($.NSPasteboardTypePNG);
    const text = $.NSString.alloc.initWithDataEncoding(data, $.NSISOLatin1StringEncoding);
    text.writeToFileAtomicallyEncodingError('{path_str}', false, $.NSISOLatin1StringEncoding, null);
    'ok'
}}"
    );
    let output = run_command(
        "osascript",
        &["-l", "JavaScript", "-e", &script],
        DEFAULT_READ_TIMEOUT,
    )
    .await?;
    if String::from_utf8_lossy(&output).trim() != "ok" {
        let _ = std::fs::remove_file(&temp_path);
        return None;
    }
    let image = crate::image_load::load_image_from_path(&temp_path)
        .ok()
        .flatten();
    let _ = std::fs::remove_file(&temp_path);
    image
}

/// macOS: the copied files (`NSFilenamesPboardType`), one path per line through a temp
/// file — the pasteboard carries no image-bitmap equivalent for a file copy.
async fn read_files_via_osascript() -> Vec<PathBuf> {
    let Some(temp_path) = clipboard_temp_path("txt") else {
        return Vec::new();
    };
    let Some(path_str) = temp_path.to_str().map(str::to_string) else {
        return Vec::new();
    };
    let script = format!(
        r"ObjC.import('Cocoa');
const pb = $.NSPasteboard.generalPasteboard;
const list = pb.propertyListForType('NSFilenamesPboardType');
if (!list) {{
    'empty'
}} else {{
    const paths = ObjC.unwrap(list);
    const text = $.NSString.alloc.initWithUTF8String(paths.join('\n'));
    text.writeToFileAtomicallyEncodingError('{path_str}', false, $.NSUTF8StringEncoding, null);
    'ok'
}}"
    );
    let output = run_command(
        "osascript",
        &["-l", "JavaScript", "-e", &script],
        // The same JavaScript-for-Automation launch the image reader grants 3 s: a 1 s
        // list timeout routinely expires before the interpreter has started.
        DEFAULT_READ_TIMEOUT,
    )
    .await;
    let files = match output {
        Some(stdout) if String::from_utf8_lossy(&stdout).trim() == "ok" => {
            std::fs::read_to_string(&temp_path)
                .map(|text| {
                    text.lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let _ = std::fs::remove_file(&temp_path);
    files
}

/// The test seams: a harness without a display server cannot drive the real clipboard
/// readers, so a fixture stands in for the clipboard and travels the exact paste path from
/// there. Never set by the product.
fn test_seam(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// Read an image from the system clipboard, if one of the supported types is present.
/// Wayland tries `wl-paste` then `xclip`; WSL and native Windows read through PowerShell;
/// X11 uses `xclip`; macOS reads through osascript. Termux exposes no clipboard image path.
pub async fn read_clipboard_image() -> Option<ClipboardImage> {
    // Verification seam (never set by the product): a fixture file stands in for the
    // clipboard image.
    if let Some(path) = test_seam("PRIME_AGENT_TEST_CLIPBOARD_IMAGE") {
        return std::fs::read(path)
            .ok()
            .and_then(clipboard_image_from_bytes);
    }
    if std::env::var_os("TERMUX_VERSION").is_some() {
        return None;
    }
    match std::env::consts::OS {
        "linux" => {
            let wsl = is_wsl();
            let wayland = is_wayland_session();
            let mut image = None;
            if wayland || wsl {
                image = read_via_wl_paste().await;
                if image.is_none() {
                    image = read_via_xclip().await;
                }
            }
            if image.is_none() && wsl {
                image = read_via_powershell().await;
            }
            if image.is_none() && !wayland {
                image = read_via_xclip().await;
            }
            image
        }
        "windows" => read_via_powershell().await,
        "macos" => read_via_osascript().await,
        _ => None,
    }
}

/// The files a file manager copied, by path. Wayland reads the `text/uri-list` flavor
/// through `wl-paste` then `xclip`; WSL and native Windows read the PowerShell drop list;
/// X11 uses `xclip`; macOS reads the pasteboard's filename property. Empty when the
/// clipboard holds no files.
pub async fn read_clipboard_files() -> Vec<PathBuf> {
    // Verification seam (never set by the product): newline-separated fixture paths.
    if let Some(path) = test_seam("PRIME_AGENT_TEST_CLIPBOARD_FILES") {
        return std::fs::read_to_string(path)
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
    }
    if std::env::var_os("TERMUX_VERSION").is_some() {
        return Vec::new();
    }
    match std::env::consts::OS {
        "linux" => {
            let wsl = is_wsl();
            let wayland = is_wayland_session();
            let mut files = Vec::new();
            if wayland || wsl {
                files = read_files_via_wl_paste().await;
            }
            // The image reader's same fallback: `wl-paste` may be absent (or the copy came
            // from an XWayland app), so `xclip` gets its turn either way.
            if files.is_empty() {
                files = read_files_via_xclip().await;
            }
            if files.is_empty() && wsl {
                files = read_files_via_powershell().await;
            }
            files
        }
        "windows" => read_files_via_powershell().await,
        "macos" => read_files_via_osascript().await,
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_mime_strips_parameters() {
        assert_eq!(base_mime_type(" image/PNG; charset=utf8 "), "image/png");
    }

    #[test]
    fn supported_type_selection_prefers_png_over_other_images() {
        let types = vec!["text/plain".to_string(), "image/webp".to_string()];
        assert_eq!(
            select_supported_image_mime_type(&types),
            Some("image/webp".to_string())
        );
        let mixed = vec![
            "image/webp".to_string(),
            "image/png".to_string(),
            "image/jpeg".to_string(),
        ];
        assert_eq!(
            select_supported_image_mime_type(&mixed),
            Some("image/png".to_string())
        );
        // An unsupported image type is never selected.
        assert_eq!(
            select_supported_image_mime_type(&["image/bmp".to_string()]),
            None
        );
        assert_eq!(
            select_supported_image_mime_type(&["text/plain".to_string()]),
            None
        );
        assert_eq!(select_supported_image_mime_type(&[]), None);
    }

    #[test]
    fn stdout_lines_trim_and_drop_empty() {
        let stdout = b"image/png\r\ntext/plain\n\n\r\n";
        assert_eq!(stdout_lines(stdout), vec!["image/png", "text/plain"]);
    }

    /// The verification seam stands in for a file-manager copy: newline-separated fixture
    /// paths, trimmed, blanks dropped (the harness has no display server to read a real
    /// clipboard from).
    #[tokio::test]
    async fn the_test_seam_stands_in_for_the_copied_file_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let list = dir.path().join("clipboard-files.txt");
        std::fs::write(&list, "\n /tmp/shot.png \r\n/tmp/notes.txt\n").expect("write fixture");
        std::env::set_var("PRIME_AGENT_TEST_CLIPBOARD_FILES", &list);
        let files = read_clipboard_files().await;
        std::env::remove_var("PRIME_AGENT_TEST_CLIPBOARD_FILES");
        assert_eq!(
            files,
            vec![
                PathBuf::from("/tmp/shot.png"),
                PathBuf::from("/tmp/notes.txt")
            ]
        );
    }

    #[test]
    fn unique_suffix_is_unique() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert_ne!(a, b);
    }
}
