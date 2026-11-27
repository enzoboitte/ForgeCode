//! Finds language servers and installs the ones ForgeCode ships support for (rust-analyzer, clangd,
//! TypeScript, HTML/CSS/JSON, PHP, Python, YAML, Svelte and Bash servers) into `<config>/servers` when they are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

pub fn tools_dir() -> PathBuf {
    crate::settings::config_dir().join("servers")
}

/// Directories holding tools ForgeCode installed itself, searched before PATH.
fn tool_bins() -> Vec<PathBuf> {
    let tools = tools_dir();
    let mut dirs = vec![tools.join("bin"), tools.join("node").join("node_modules").join(".bin")];
    for entry in std::fs::read_dir(&tools).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Unpacked releases: `clangd_19.1.2/bin`, `node-v22.11.0-darwin-arm64/bin` (no `bin` on Windows).
        if name.starts_with("clangd_") || name.starts_with("node-v") {
            let bin = entry.path().join("bin");
            dirs.push(if bin.is_dir() { bin } else { entry.path() });
        }
    }
    // Runtimes ForgeCode downloaded for servers: a JDK (jdtls, Kotlin) and the .NET SDK (C#).
    for entry in std::fs::read_dir(&tools).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("jdk-") {
            let mac = entry.path().join("Contents/Home/bin");
            dirs.push(if mac.is_dir() { mac } else { entry.path().join("bin") });
        }
    }
    dirs.extend([tools.join("dotnet"), tools.join("dotnet-tools")]);
    dirs.extend(node_manager_bins());
    dirs
}

/// Folders holding a `node`: ForgeCode's portable one and the version managers'.
pub fn node_bins() -> Vec<PathBuf> {
    tool_bins().into_iter().filter(|d| d.join("node").is_file()).chain(node_manager_bins()).collect()
}

/// Node.js from version managers (nvm, fnm, volta, asdf, mise) and Homebrew. They're set up by
/// the interactive shell's rc file, which an app launched from the Finder never reads, so npm
/// servers (`#!/usr/bin/env node`) wouldn't start. Newest version first.
fn node_manager_bins() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return vec![] };
    let mut dirs = vec![];
    // `<root>/<version>/<bin>`, newest version first.
    let versions = |root: PathBuf, bin: &str| -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(root).into_iter().flatten().flatten().map(|e| e.path().join(bin)).filter(|b| b.join("node").is_file()).collect();
        found.sort_by_key(|b| std::cmp::Reverse(version_key(b)));
        found
    };
    let nvm = std::env::var_os("NVM_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".nvm"));
    dirs.extend(versions(nvm.join("versions/node"), "bin"));
    dirs.extend(versions(home.join(".local/share/fnm/node-versions"), "installation/bin"));
    dirs.extend(versions(home.join("Library/Application Support/fnm/node-versions"), "installation/bin"));
    dirs.extend(versions(home.join(".asdf/installs/nodejs"), "bin"));
    dirs.extend(versions(home.join(".local/share/mise/installs/node"), "bin"));
    dirs.extend([home.join(".volta/bin"), "/opt/homebrew/bin".into(), "/usr/local/bin".into()].into_iter().filter(|d| d.join("node").is_file()));
    dirs
}

/// `…/v24.22.1/bin` → [24, 22, 1], to sort versions numerically.
fn version_key(bin: &Path) -> Vec<u64> {
    let name = bin.components().rev().find_map(|c| {
        let s = c.as_os_str().to_string_lossy();
        s.trim_start_matches('v').chars().next().is_some_and(|ch| ch.is_ascii_digit()).then(|| s.trim_start_matches('v').to_string())
    });
    name.unwrap_or_default().split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// PATH for servers and installers: ForgeCode's tools first.
pub fn search_path() -> String {
    crate::util::path_with(tool_bins())
}

/// Full path of `program` on the search path, trying the platform's executable extensions.
pub fn which(program: &str) -> Option<PathBuf> {
    if Path::new(program).is_absolute() {
        return Path::new(program).is_file().then(|| PathBuf::from(program));
    }
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    std::env::split_paths(&search_path())
        .flat_map(|dir| exts.iter().map(move |ext| dir.join(format!("{program}{ext}"))))
        .find(|p| p.is_file())
}

/// A `#!/usr/bin/env node` script (what npm puts in `.bin`).
fn needs_node(path: &Path) -> bool {
    let mut head = [0u8; 64];
    let n = std::fs::File::open(path).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).unwrap_or(0);
    head[..n].starts_with(b"#!") && String::from_utf8_lossy(&head[..n]).lines().next().is_some_and(|l| l.ends_with("node") || l.contains("node "))
}

/// Whether starting `program` will need an install first (cheap: no process is run).
pub fn needs_install(program: &str) -> bool {
    which(program).is_none()
}

/// `argv` with its program resolved to a full path, installing the server if needed. Blocking.
pub fn resolve(argv: &[String]) -> Result<Vec<String>> {
    let program = argv.first().context("empty command")?;
    let path = match program.as_str() {
        "rust-analyzer" => rust_analyzer()?,
        _ => match which(program) {
            Some(p) => p,
            None => {
                install(program)?;
                which(program).with_context(|| format!("{program} not found after installing it"))?
            }
        },
    };
    // npm servers are Node scripts: without Node they die at once. Get a portable one.
    if needs_node(&path) {
        ensure_node()?;
    }
    Ok(std::iter::once(path.display().to_string()).chain(argv[1..].iter().cloned()).collect())
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.env("PATH", search_path()).output().with_context(|| format!("failed to run {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// rustup's `rust-analyzer` proxy exists even without the component, so ask rustup first.
fn rust_analyzer() -> Result<PathBuf> {
    if which("rustup").is_some() {
        let which_ra = || run(Command::new("rustup").args(["which", "rust-analyzer"]));
        if let Ok(p) = which_ra().or_else(|_| run(Command::new("rustup").args(["component", "add", "rust-analyzer"])).and_then(|_| which_ra())) {
            return Ok(PathBuf::from(p));
        }
    }
    if let Some(p) = which("rust-analyzer") {
        return Ok(p);
    }
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", _) => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", _) => "x86_64-unknown-linux-gnu",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        ("windows", _) => "x86_64-pc-windows-msvc",
        (os, arch) => bail!("no rust-analyzer build for {os}/{arch}"),
    };
    let url = format!("https://github.com/rust-lang/rust-analyzer/releases/latest/download/rust-analyzer-{target}.gz");
    let archive = download(&url)?;
    let mut gz = flate2::read::GzDecoder::new(archive.as_slice());
    let mut bin = vec![];
    std::io::Read::read_to_end(&mut gz, &mut bin)?;
    let dest = tools_dir().join("bin").join(format!("rust-analyzer{}", std::env::consts::EXE_SUFFIX));
    write_executable(&dest, &bin)?;
    Ok(dest)
}

fn install(program: &str) -> Result<()> {
    std::fs::create_dir_all(tools_dir())?;
    match program {
        "clangd" => clangd(),
        "jdtls" => jdtls(),
        "kotlin-lsp" => kotlin_lsp(),
        "csharp-ls" => csharp_ls(),
        "glsl_analyzer" => {
            let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
                ("macos", "aarch64") => "aarch64-macos.zip",
                ("macos", _) => "x86_64-macos.zip",
                ("windows", "aarch64") => "aarch64-windows.zip",
                ("windows", _) => "x86_64-windows.zip",
                (_, "aarch64") => "aarch64-linux-musl.zip",
                _ => "x86_64-linux-musl.zip",
            };
            binary_from_archive(&format!("https://github.com/nolanderc/glsl_analyzer/releases/latest/download/{asset}"), asset, "glsl_analyzer")
        }
        // Go's own installer, into ForgeCode's tools.
        "gopls" => {
            let go = which("go").context("Go is not installed (https://go.dev/dl)")?;
            run(Command::new(go).args(["install", "golang.org/x/tools/gopls@latest"]).env("GOBIN", tools_dir().join("bin"))).map(drop)
        }
        _ if npm_packages(program).is_some() => {
            let npm = npm()?;
            let packages = npm_packages(program).unwrap();
            run(Command::new(npm).arg("install").arg("--prefix").arg(tools_dir().join("node")).args(packages)).map(drop)
        }
        _ => bail!("`{program}` is not installed and ForgeCode does not know how to install it"),
    }
}

/// Whether ForgeCode installs `program` by itself when it is missing.
pub fn can_install(program: &str) -> bool {
    matches!(program, "clangd" | "gopls" | "rust-analyzer" | "jdtls" | "kotlin-lsp" | "csharp-ls" | "glsl_analyzer") || npm_packages(program).is_some()
}

/// One program out of a release archive, into the tools' `bin`.
fn binary_from_archive(url: &str, archive: &str, program: &str) -> Result<()> {
    let tmp = tools_dir().join(format!(".unpack-{program}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    unpack_into(&download(url)?, archive, &tmp)?;
    let exe = format!("{program}{}", std::env::consts::EXE_SUFFIX);
    let found = find_file(&tmp, &exe, 3).with_context(|| format!("{exe} not found in {archive}"))?;
    write_executable(&tools_dir().join("bin").join(&exe), &std::fs::read(found)?)?;
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/// Khronos' reference GLSL compiler (the shader linter): installed, else its latest build.
pub fn ensure_glslang() -> Result<PathBuf> {
    if let Some(p) = which("glslang").or_else(|| which("glslangValidator")) {
        return Ok(p);
    }
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => "glslang-main-macos-universal-release.zip",
        ("windows", "aarch64") => "glslang-main-windows-aarch64-release.zip",
        ("windows", _) => "glslang-main-windows-x86_64-release.zip",
        ("linux", "x86_64") => "glslang-main-linux-x86_64-release.zip",
        (os, arch) => bail!("no glslang build for {os}/{arch}"),
    };
    std::fs::create_dir_all(tools_dir())?;
    binary_from_archive(&format!("https://github.com/KhronosGroup/glslang/releases/download/main-tot/{asset}"), asset, "glslang")?;
    which("glslang").context("glslang not found after installing it")
}

/// How the user installs a server ForgeCode can't install itself.
pub fn manual_install(program: &str) -> Option<&'static str> {
    let brew = cfg!(target_os = "macos");
    Some(match program {
        "lua-language-server" if brew => "brew install lua-language-server",
        "ruby-lsp" => "gem install ruby-lsp",
        _ => return None,
    })
}

/// npm packages providing a language server's command.
fn npm_packages(program: &str) -> Option<&'static [&'static str]> {
    Some(match program {
        // All of them at once: one npm run instead of one per language.
        "typescript-language-server" | "vscode-html-language-server" | "vscode-css-language-server" | "vscode-json-language-server" => {
            &["typescript", "typescript-language-server", "vscode-langservers-extracted"]
        }
        "intelephense" => &["intelephense"],
        "pyright-langserver" => &["pyright"],
        "yaml-language-server" => &["yaml-language-server"],
        "svelteserver" => &["svelte-language-server"],
        "bash-language-server" => &["bash-language-server"],
        "taplo" => &["@taplo/cli"],
        _ => return None,
    })
}

/// Major version printed by `java -version` (`"21.0.2"` → 21, `"1.8.0_392"` → 8).
fn java_major(version_output: &str) -> Option<u32> {
    let quoted = version_output.split('"').nth(1)?;
    let mut parts = quoted.split(['.', '_', '-', '+']);
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 { parts.next()?.parse().ok() } else { Some(first) }
}

/// A Java runtime of at least `min`: the installed one, else Temurin (Eclipse Adoptium)
/// downloaded into the tools dir.
pub fn ensure_java(min: u32) -> Result<PathBuf> {
    if let Some(java) = which("java") {
        let out = Command::new(&java).arg("-version").env("PATH", search_path()).output()?;
        // macOS ships a `java` stub that only asks to install Java: it fails here.
        if out.status.success() && java_major(&String::from_utf8_lossy(&out.stderr)).is_some_and(|v| v >= min) {
            return Ok(java);
        }
    }
    let os = match std::env::consts::OS {
        "macos" => "mac",
        "windows" => "windows",
        _ => "linux",
    };
    let arch = if std::env::consts::ARCH == "aarch64" { "aarch64" } else { "x64" };
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    // Below: tool_bins() puts `jdk-*/bin` (or `Contents/Home/bin`) on the search path.
    let url = format!("https://api.adoptium.net/v3/binary/latest/{}/ga/{os}/{arch}/jdk/hotspot/normal/eclipse", min.max(21));
    unpack_url(&url, &format!("temurin.{ext}"))?;
    which("java").context("java not found in the downloaded JDK")
}

/// Eclipse JDT Language Server, started by a small launcher script written next to it.
fn jdtls() -> Result<()> {
    ensure_java(21)?;
    let dir = tools_dir().join("jdtls");
    std::fs::create_dir_all(&dir)?;
    unpack_url_into("https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz", "jdtls.tar.gz", &dir)?;
    let launcher = std::fs::read_dir(dir.join("plugins"))?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("org.eclipse.equinox.launcher_")))
        .context("jdtls launcher jar not found")?;
    let config = dir.join(match std::env::consts::OS {
        "macos" => if std::env::consts::ARCH == "aarch64" { "config_mac_arm" } else { "config_mac" },
        "windows" => "config_win",
        _ => if std::env::consts::ARCH == "aarch64" { "config_linux_arm" } else { "config_linux" },
    });
    let data = crate::settings::config_dir().join("jdtls-data");
    let args = format!(
        "-Declipse.application=org.eclipse.jdt.ls.core.id1 -Dosgi.bundles.defaultStartLevel=4 -Declipse.product=org.eclipse.jdt.ls.core.product \
         -Xmx2G --add-modules=ALL-SYSTEM --add-opens java.base/java.util=ALL-UNNAMED --add-opens java.base/java.lang=ALL-UNNAMED \
         -jar \"{}\" -configuration \"{}\"",
        launcher.display(),
        config.display()
    );
    // One workspace data folder per project (jdtls keeps its index there).
    if cfg!(windows) {
        let script = format!("@echo off\r\njava {} -data \"{}\\%~n1\" %*\r\n", args.replace(" \\\n         ", " "), data.display());
        write_executable(&tools_dir().join("bin").join("jdtls.cmd"), script.as_bytes())
    } else {
        let script = format!("#!/bin/sh\nexec java {args} -data \"{}/$(pwd | cksum | cut -d' ' -f1)\" \"$@\"\n", data.display());
        write_executable(&tools_dir().join("bin").join("jdtls"), script.as_bytes())
    }
}

/// JetBrains' Kotlin language server (the one VS Code's "Kotlin by JetBrains" uses), standalone.
const KOTLIN_LSP_VERSION: &str = "263.7084.0";

fn kotlin_lsp() -> Result<()> {
    let v = KOTLIN_LSP_VERSION;
    let arm = std::env::consts::ARCH == "aarch64";
    let file = match (std::env::consts::OS, arm) {
        ("macos", true) => format!("kotlin-server-{v}-aarch64.sit"),
        ("macos", false) => format!("kotlin-server-{v}.sit"),
        ("windows", true) => format!("kotlin-server-{v}-aarch64.win.zip"),
        ("windows", false) => format!("kotlin-server-{v}.win.zip"),
        (_, true) => format!("kotlin-server-{v}-aarch64.tar.gz"),
        _ => format!("kotlin-server-{v}.tar.gz"),
    };
    let dir = tools_dir().join("kotlin-lsp");
    std::fs::create_dir_all(&dir)?;
    // The macOS `.sit` is a zip archive: the system tar reads it.
    let local = if file.ends_with(".sit") { "kotlin-lsp.zip".to_string() } else { file.clone() };
    unpack_url_into(&format!("https://download.jetbrains.com/language-server/kotlin-server/{v}/{file}"), &local, &dir)?;
    let script_name = if cfg!(windows) { "kotlin-lsp.cmd" } else { "kotlin-lsp.sh" };
    let script = find_file(&dir, script_name, 3).with_context(|| format!("{script_name} not found in the Kotlin LSP archive"))?;
    if cfg!(windows) {
        write_executable(&tools_dir().join("bin").join("kotlin-lsp.cmd"), format!("@echo off\r\n\"{}\" %*\r\n", script.display()).as_bytes())
    } else {
        write_executable(&script, &std::fs::read(&script)?)?;
        write_executable(&tools_dir().join("bin").join("kotlin-lsp"), format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", script.display()).as_bytes())
    }
}

/// A file named `name` within `depth` levels of `dir`.
fn find_file(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
        if depth > 0 && path.is_dir() {
            if let Some(found) = find_file(&path, name, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// The .NET SDK (csharp-ls needs 10 or later): the installed one, else Microsoft's official
/// dotnet-install script into the tools dir.
fn ensure_dotnet() -> Result<PathBuf> {
    if let Some(dotnet) = which("dotnet") {
        let sdks = Command::new(&dotnet).arg("--list-sdks").env("PATH", search_path()).output()?;
        let ok = String::from_utf8_lossy(&sdks.stdout).lines().any(|l| l.split('.').next().and_then(|m| m.parse::<u32>().ok()).is_some_and(|m| m >= 10));
        if ok {
            return Ok(dotnet);
        }
    }
    let dir = tools_dir().join("dotnet");
    if cfg!(windows) {
        let script = tools_dir().join("dotnet-install.ps1");
        std::fs::write(&script, download("https://dot.net/v1/dotnet-install.ps1")?)?;
        run(Command::new("powershell").args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&script).args(["-Channel", "10.0", "-InstallDir"]).arg(&dir))?;
    } else {
        let script = tools_dir().join("dotnet-install.sh");
        write_executable(&script, &download("https://dot.net/v1/dotnet-install.sh")?)?;
        run(Command::new("/bin/sh").arg(&script).args(["--channel", "10.0", "--install-dir"]).arg(&dir))?;
    }
    which("dotnet").context("dotnet not found after installing the .NET SDK")
}

/// csharp-ls as a .NET tool in ForgeCode's tools.
fn csharp_ls() -> Result<()> {
    let dotnet = ensure_dotnet()?;
    let tools = tools_dir().join("dotnet-tools");
    run(Command::new(dotnet).args(["tool", "install", "csharp-ls", "--tool-path"]).arg(&tools).envs(dotnet_env())).map(drop)
}

/// DOTNET_ROOT for .NET tools when the SDK is ForgeCode's own.
pub fn dotnet_env() -> Vec<(String, String)> {
    let own = tools_dir().join("dotnet");
    if own.join(if cfg!(windows) { "dotnet.exe" } else { "dotnet" }).is_file() { vec![("DOTNET_ROOT".into(), own.display().to_string())] } else { vec![] }
}

fn clangd() -> Result<()> {
    #[cfg(target_os = "macos")]
    if let Ok(p) = run(Command::new("xcrun").args(["--find", "clangd"])) {
        // Link Xcode's clangd where `which` looks.
        let link = tools_dir().join("bin").join("clangd");
        std::fs::create_dir_all(link.parent().unwrap())?;
        let _ = std::fs::remove_file(&link);
        return Ok(std::os::unix::fs::symlink(p, link)?);
    }
    let release: serde_json::Value = ureq::get("https://api.github.com/repos/clangd/clangd/releases/latest").call()?.into_json()?;
    let os = match std::env::consts::OS {
        "macos" => "mac",
        "windows" => "windows",
        _ => "linux",
    };
    let url = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["browser_download_url"].as_str())
        .find(|u| u.rsplit('/').next().is_some_and(|n| n.starts_with(&format!("clangd-{os}-")) && n.ends_with(".zip")))
        .context("no clangd release for this platform")?;
    unpack(&download(url)?, "clangd.zip")
}

/// npm from PATH, or from a portable Node.js LTS downloaded into the tools dir.
fn npm() -> Result<PathBuf> {
    if let Some(npm) = which("npm") {
        return Ok(npm);
    }
    let index: Vec<serde_json::Value> = ureq::get("https://nodejs.org/dist/index.json").call()?.into_json()?;
    let version = index.iter().find(|r| r["lts"].is_string()).and_then(|r| r["version"].as_str()).context("no Node.js LTS release")?;
    let arch = if std::env::consts::ARCH == "aarch64" { "arm64" } else { "x64" };
    let (os, ext) = match std::env::consts::OS {
        "macos" => ("darwin", "tar.gz"),
        "windows" => ("win", "zip"),
        _ => ("linux", "tar.gz"),
    };
    let name = format!("node-{version}-{os}-{arch}.{ext}");
    unpack(&download(&format!("https://nodejs.org/dist/{version}/{name}"))?, &name)?;
    which("npm").context("npm not found in the downloaded Node.js")
}

fn download(url: &str) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    std::io::Read::read_to_end(&mut ureq::get(url).call().with_context(|| format!("download {url}"))?.into_reader(), &mut bytes)?;
    Ok(bytes)
}

/// Download an archive and extract it into the tools dir.
pub fn unpack_url(url: &str, name: &str) -> Result<()> {
    unpack(&download(url)?, name)
}

/// Download an archive and extract it into `dir`.
pub fn unpack_url_into(url: &str, name: &str, dir: &Path) -> Result<()> {
    unpack_into(&download(url)?, name, dir)
}

/// Node.js from PATH, or the portable one ForgeCode downloads with npm.
pub fn ensure_node() -> Result<PathBuf> {
    if let Some(node) = which("node") {
        return Ok(node);
    }
    npm()?;
    which("node").context("node not found in the downloaded Node.js")
}

fn unpack(archive: &[u8], name: &str) -> Result<()> {
    std::fs::create_dir_all(tools_dir())?;
    unpack_into(archive, name, &tools_dir())
}

/// Extract an archive into `dir` with the system's `tar` (zip-capable on macOS and Windows) or
/// `unzip` on Linux.
fn unpack_into(archive: &[u8], name: &str, dir: &Path) -> Result<()> {
    let file = dir.join(name);
    std::fs::write(&file, archive)?;
    let result = if name.ends_with(".zip") && cfg!(target_os = "linux") {
        run(Command::new("unzip").args(["-q", "-o"]).arg(&file).arg("-d").arg(dir))
    } else {
        run(Command::new("tar").arg("-xf").arg(&file).arg("-C").arg(dir))
    };
    let _ = std::fs::remove_file(&file);
    result.map(drop)
}

fn write_executable(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_java_versions() {
        assert_eq!(java_major("openjdk version \"21.0.2\" 2024-01-16"), Some(21));
        assert_eq!(java_major("java version \"1.8.0_392\""), Some(8));
        assert_eq!(java_major("openjdk version \"17-ea\""), Some(17));
        assert_eq!(java_major("No Java runtime present"), None);
    }

    #[test]
    fn sorts_node_versions_and_detects_node_scripts() {
        let mut v = vec![PathBuf::from("/n/v9.1.0/bin"), PathBuf::from("/n/v24.22.1/bin"), PathBuf::from("/n/v24.20.0/bin")];
        v.sort_by_key(|b| std::cmp::Reverse(version_key(b)));
        assert_eq!(v[0], PathBuf::from("/n/v24.22.1/bin"));
        let dir = std::env::temp_dir().join(format!("forge-node-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s"), "#!/usr/bin/env node\nrequire('x')").unwrap();
        std::fs::write(dir.join("b"), "#!/bin/sh\necho").unwrap();
        assert!(needs_node(&dir.join("s")) && !needs_node(&dir.join("b")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finds_programs_and_keeps_arguments() {
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(which(shell).is_some());
        assert!(which("surely-not-a-real-program-xyz").is_none());
        let argv = resolve(&[shell.into(), "--stdio".into()]).unwrap();
        assert!(Path::new(&argv[0]).is_absolute());
        assert_eq!(argv[1], "--stdio");
        assert!(resolve(&["surely-not-a-real-program-xyz".into()]).is_err());
    }
}
