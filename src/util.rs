//! Small shared helpers.

use std::path::Path;

/// Write a file readable only by the user (tokens, API keys).
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// `canonicalize` without Windows' `\\?\` prefix, which cmd, make, CMake and Conan don't understand.
pub fn canonical(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    path.canonicalize().map(plain_path)
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\srv\x` → `\\srv\x`; other paths unchanged.
pub fn plain_path(path: std::path::PathBuf) -> std::path::PathBuf {
    let s = path.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}").into();
    }
    s.strip_prefix(r"\\?\").map_or(path, Into::into)
}

#[cfg(test)]
#[test]
fn plain_paths_drop_the_verbatim_prefix() {
    use std::path::PathBuf;
    assert_eq!(plain_path(PathBuf::from(r"\\?\C:\Users\me")), PathBuf::from(r"C:\Users\me"));
    assert_eq!(plain_path(PathBuf::from(r"\\?\UNC\srv\share")), PathBuf::from(r"\\srv\share"));
    assert_eq!(plain_path(PathBuf::from("/home/me")), PathBuf::from("/home/me"));
}

/// A program to run, without a console window on Windows: ForgeCode is a window app there, and
/// each program it starts (git, gh, language servers, Python…) would otherwise open one.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// A command running `script` through the platform shell (`sh -c` or `cmd /C`).
pub fn shell_command(script: &str) -> std::process::Command {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = command("cmd");
        c.arg("/C");
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = command("sh");
        c.arg("-c");
        c
    };
    cmd.arg(script);
    cmd
}

/// `PATH` with `dirs` in front, joined with the platform separator.
pub fn path_with(dirs: impl IntoIterator<Item = std::path::PathBuf>) -> String {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let all: Vec<_> = dirs.into_iter().chain(std::env::split_paths(&current)).collect();
    std::env::join_paths(all).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Apps started from the Finder or a desktop launcher get a bare PATH (no Homebrew, cargo, npm):
/// take the login shell's, as a terminal would see it. Call before any thread starts.
#[cfg(unix)]
pub fn adopt_login_path() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let Ok(mut child) = crate::util::command(shell)
        .args(["-l", "-c", "printf %s \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return;
    };
    // A broken shell profile must not hang the launch.
    let start = std::time::Instant::now();
    while child.try_wait().ok().flatten().is_none() {
        if start.elapsed() > std::time::Duration::from_secs(3) {
            let _ = child.kill();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut path = String::new();
    let _ = std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut path);
    if !path.trim().is_empty() {
        // SAFETY: called first thing in `main`, before other threads exist.
        unsafe { std::env::set_var("PATH", path.trim()) };
    }
}

/// A shortcut written with macOS symbols (`⌘⇧P`), as other platforms spell it (`Ctrl+Shift+P`).
pub fn keys(mac: &str) -> String {
    if cfg!(target_os = "macos") {
        return mac.to_string();
    }
    let mut out = String::new();
    for c in mac.chars() {
        out.push_str(match c {
            '⌘' | '⌃' => "Ctrl+",
            '⇧' => "Shift+",
            '⌥' => "Alt+",
            '⏎' => "Enter",
            _ => {
                out.push(c);
                continue;
            }
        });
    }
    out
}

/// Standard base64 (RFC 4648) for data URIs and image blocks.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            out.push(if i <= chunk.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Files ForgeCode shows in its image viewer.
pub fn is_viewable_image(path: &Path) -> bool {
    image_media_type(path).is_some() || matches!(path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(), Some("svg" | "bmp" | "ico"))
}

/// Image media type from a file extension, for files the chat can send as images.
pub fn image_media_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recognizes_viewable_images() {
        assert!(super::is_viewable_image(std::path::Path::new("a/Logo.SVG")));
        assert!(super::is_viewable_image(std::path::Path::new("x.png")));
        assert!(!super::is_viewable_image(std::path::Path::new("doc.pdf")));
    }

    #[test]
    fn spells_shortcuts_for_the_platform() {
        let k = super::keys("⌘⇧P");
        assert!(k == "⌘⇧P" || k == "Ctrl+Shift+P", "{k}");
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, out) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(super::base64(input.as_bytes()), out);
        }
    }
}

/// The image on the clipboard saved to a temp file: None when the clipboard holds no image.
pub fn save_clipboard_image(cx: &mut gpui_kit::App) -> Option<std::io::Result<std::path::PathBuf>> {
    let image = cx.read_from_clipboard()?.into_entries().find_map(|e| match e {
        gpui_kit::ClipboardEntry::Image(i) => Some(i),
        _ => None,
    })?;
    let dir = std::env::temp_dir().join("forgecode-images");
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let path = dir.join(format!("image-{stamp}.{}", image.format.extension()));
    Some(std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &image.bytes)).map(|_| path))
}
