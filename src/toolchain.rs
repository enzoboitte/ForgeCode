//! Compiler environments set up by a script: MSVC's `vcvarsall.bat` (Windows), Conan's `conanrun`.
//! ForgeCode runs the script once and keeps the variables it changed, like a "Developer Prompt".

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// Variables `script` sets or changes (`args` passed to it), read back from the shell after it ran.
pub fn script_env(script: &Path, args: &str) -> Result<Vec<(String, String)>> {
    let windows = script.extension().is_some_and(|e| e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"));
    let out = if windows {
        crate::util::command("cmd").arg("/d").arg("/c").arg(format!("call \"{}\" {args} >nul 2>&1 && set", script.display())).output()?
    } else {
        crate::util::command("sh").arg("-c").arg(format!(". \"{}\" {args} >/dev/null 2>&1 && env", script.display())).output()?
    };
    if !out.status.success() {
        bail!("{} failed", script.display());
    }
    Ok(changed(&String::from_utf8_lossy(&out.stdout), |k| std::env::var(k).ok()))
}

/// `NAME=value` lines whose value differs from `current` (Windows names compare case-insensitively
/// there, but the shell prints them with their own case, which is kept).
fn changed(listing: &str, current: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    listing
        .lines()
        .filter_map(|l| l.split_once('='))
        // `=C:` (cmd's drive folders) and the shell's own bookkeeping are not the compilers'.
        .filter(|(k, _)| !k.is_empty() && !k.starts_with('=') && !matches!(*k, "SHLVL" | "_" | "PWD" | "OLDPWD"))
        .filter(|(k, v)| current(k).as_deref() != Some(*v))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `vcvarsall.bat` from the setting (the file, or a Visual Studio / Build Tools folder), else the
/// installation vswhere knows, else the usual folders of every drive.
pub fn vcvarsall(setting: &str) -> Option<PathBuf> {
    let from = |p: &Path| -> Option<PathBuf> {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        [p.join("VC/Auxiliary/Build/vcvarsall.bat"), p.join("Auxiliary/Build/vcvarsall.bat"), p.join("vcvarsall.bat")].into_iter().find(|f| f.is_file())
    };
    if !setting.trim().is_empty() {
        return from(Path::new(setting.trim()));
    }
    let vswhere = Path::new(&std::env::var("ProgramFiles(x86)").unwrap_or(r"C:\Program Files (x86)".into())).join(r"Microsoft Visual Studio\Installer\vswhere.exe");
    if let Ok(out) = crate::util::command(&vswhere)
        .args(["-latest", "-prerelease", "-products", "*", "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"])
        .output()
    {
        if let Some(found) = String::from_utf8_lossy(&out.stdout).lines().map(str::trim).find(|l| !l.is_empty()).and_then(|l| from(Path::new(l))) {
            return Some(found);
        }
    }
    // Installed outside vswhere's view (copied, another drive): `<drive>:\Program Files*\Microsoft Visual Studio\<year>\<edition>`.
    ('C'..='Z')
        .flat_map(|d| [format!(r"{d}:\Program Files\Microsoft Visual Studio"), format!(r"{d}:\Program Files (x86)\Microsoft Visual Studio"), format!(r"{d}:\Microsoft Visual Studio")])
        .filter(|root| Path::new(root).is_dir())
        .flat_map(|root| newest_children(Path::new(&root)))
        .flat_map(|year| newest_children(&year))
        .find_map(|edition| from(&edition))
}

/// Sub-folders of `dir`, the highest name (year, version) first.
fn newest_children(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir).map(|e| e.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
    dirs.sort_by(|a, b| b.cmp(a));
    dirs
}

/// Make ForgeCode's own environment the compilers' (like a Developer Prompt), so every process it
/// starts (builds, Conan, CMake, the terminal, clangd) uses the chosen compiler:
/// - the environment script, or on Windows MSVC wherever it is installed when no compiler is on the PATH;
/// - `CC` / `CXX`, their folders first on the PATH.
pub fn adopt(settings: &crate::settings::Settings) {
    let (script, cc, cxx) = (settings.compiler_env.trim().to_string(), settings.cc.trim().to_string(), settings.cxx.trim().to_string());
    let work = move || {
        // The script first: CC / CXX chosen in the settings win over what it sets.
        let script_vars = environment(&script).unwrap_or_else(|e| {
            eprintln!("compiler environment: {e}");
            vec![]
        });
        let vars = [script_vars, compiler_vars(&cc, &cxx)].concat();
        let path_extra: Vec<PathBuf> = [&cc, &cxx].iter().filter_map(|c| Path::new(c.as_str()).parent()).filter(|d| d.is_dir()).map(Path::to_path_buf).collect();
        for (k, v) in vars {
            set(&k, &v);
        }
        if !path_extra.is_empty() {
            set("PATH", &crate::util::path_with(path_extra));
        }
    };
    // vcvarsall takes seconds on Windows, whose environment is safe to change from a thread.
    #[cfg(windows)]
    std::thread::spawn(work);
    #[cfg(not(windows))]
    work();
}

fn compiler_vars(cc: &str, cxx: &str) -> Vec<(String, String)> {
    [("CC", cc), ("CXX", cxx)].into_iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// The variables of the environment script (a Visual Studio folder means its vcvarsall); empty
/// setting: MSVC's on Windows when no compiler is on the PATH.
fn environment(script: &str) -> Result<Vec<(String, String)>> {
    let arch = if std::env::consts::ARCH == "aarch64" { "arm64" } else { "x64" };
    if script.is_empty() {
        let has_compiler = ["cl", "gcc", "clang"].iter().any(|c| crate::lsp::install::which(c).is_some());
        if !cfg!(windows) || has_compiler {
            return Ok(vec![]);
        }
        return match vcvarsall("") {
            Some(bat) => script_env(&bat, arch),
            None => Ok(vec![]),
        };
    }
    let path = Path::new(script);
    let is_vcvars = path.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("vcvarsall.bat"));
    if path.is_dir() || is_vcvars {
        let bat = vcvarsall(script).with_context(|| format!("no vcvarsall.bat in {script}"))?;
        return script_env(&bat, arch);
    }
    if !path.is_file() {
        bail!("{script} not found");
    }
    script_env(path, "")
}

fn set(key: &str, value: &str) {
    // SAFETY: called on Windows (environment guarded by the OS) or at startup before other threads.
    unsafe { std::env::set_var(key, value) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_what_the_script_changed() {
        let listing = "PATH=C:\\VS\\bin;C:\\Windows\nHOME=/h\nINCLUDE=C:\\VS\\include\n=C:=C:\\x\nbroken\n";
        let current = |k: &str| match k {
            "HOME" => Some("/h".to_string()),
            "PATH" => Some("C:\\Windows".to_string()),
            _ => None,
        };
        let got = changed(listing, current);
        assert_eq!(got, vec![("PATH".into(), "C:\\VS\\bin;C:\\Windows".into()), ("INCLUDE".into(), "C:\\VS\\include".into())]);
    }

    #[test]
    fn finds_vcvarsall_from_a_folder_setting() {
        let dir = std::env::temp_dir().join(format!("forge-vs-{}", std::process::id()));
        let bat = dir.join("VC/Auxiliary/Build/vcvarsall.bat");
        std::fs::create_dir_all(bat.parent().unwrap()).unwrap();
        std::fs::write(&bat, "").unwrap();
        assert_eq!(vcvarsall(&dir.display().to_string()), Some(bat.clone()));
        assert_eq!(vcvarsall(&bat.display().to_string()), Some(bat));
        assert_eq!(vcvarsall("/nonexistent/vs"), None);
        assert!(environment("/nonexistent/env.sh").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn reads_a_run_script_env() {
        let dir = std::env::temp_dir().join(format!("forge-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("conanrun.sh");
        std::fs::write(&script, "export FORGE_TEST_LIB=/deps/lib\necho noise\n").unwrap();
        let env = script_env(&script, "").unwrap();
        assert!(env.contains(&("FORGE_TEST_LIB".into(), "/deps/lib".into())), "{env:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
