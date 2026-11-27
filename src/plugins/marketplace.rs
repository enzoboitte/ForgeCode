//! A local extension marketplace: a folder of module directories (no server yet) that can be
//! installed into, updated in and removed from the plugins directory.

use std::path::{Path, PathBuf};

use super::{Plugin, Plugins, plugins_dir};

/// Extensions shipped with ForgeCode, written into the marketplace folder on startup:
/// (directory, [(file, contents, executable)]).
const BUNDLED: &[(&str, &[(&str, &str, bool)])] = &[(
    "sonarqube",
    &[
        ("plugin.toml", include_str!("../../extensions/sonarqube/plugin.toml"), false),
        ("sonar.py", include_str!("../../extensions/sonarqube/sonar.py"), false),
        ("sonar", include_str!("../../extensions/sonarqube/sonar"), true),
        ("sonar.cmd", include_str!("../../extensions/sonarqube/sonar.cmd"), false),
        ("docker-compose.yml", include_str!("../../extensions/sonarqube/docker-compose.yml"), false),
        ("README.md", include_str!("../../extensions/sonarqube/README.md"), false),
    ],
), (
    "hermes",
    &[
        ("plugin.toml", include_str!("../../extensions/hermes/plugin.toml"), false),
        ("README.md", include_str!("../../extensions/hermes/README.md"), false),
    ],
), (
    "databases",
    &[
        ("plugin.toml", include_str!("../../extensions/databases/plugin.toml"), false),
        ("db.py", include_str!("../../extensions/databases/db.py"), false),
        ("README.md", include_str!("../../extensions/databases/README.md"), false),
    ],
), (
    "conan",
    &[
        ("plugin.toml", include_str!("../../extensions/conan/plugin.toml"), false),
        ("conan_module.py", include_str!("../../extensions/conan/conan_module.py"), false),
        ("conan-forge", include_str!("../../extensions/conan/conan-forge"), true),
        ("conan-forge.cmd", include_str!("../../extensions/conan/conan-forge.cmd"), false),
        ("README.md", include_str!("../../extensions/conan/README.md"), false),
    ],
)];

/// The marketplace folder: the configured one, or `<config>/marketplace`.
pub fn marketplace_dir(configured: &str) -> PathBuf {
    if configured.trim().is_empty() { crate::settings::config_dir().join("marketplace") } else { PathBuf::from(configured.trim()) }
}

/// Put the bundled extensions in the marketplace, refreshing them when ForgeCode ships a newer version.
pub fn seed(market: &Path) -> std::io::Result<()> {
    for (name, files) in BUNDLED {
        let dir = market.join(name);
        let shipped = files.iter().find(|f| f.0 == "plugin.toml").map(|f| version_of(f.1)).unwrap_or_default();
        let present = std::fs::read_to_string(dir.join("plugin.toml")).map(|t| version_of(&t)).ok();
        if present.is_some_and(|v| !newer(&shipped, &v)) {
            continue;
        }
        std::fs::create_dir_all(&dir)?;
        for (file, text, exec) in *files {
            std::fs::write(dir.join(file), text)?;
            #[cfg(unix)]
            if *exec {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(0o755))?;
            }
            let _ = exec;
        }
    }
    Ok(())
}

/// Installed copies of the bundled extensions follow ForgeCode: when it ships a newer version,
/// the installed module is updated in place (its `runtime/`, such as an installed agent, stays).
/// Returns the names updated.
pub fn update_bundled(market: &Path) -> Vec<String> {
    let mut updated = vec![];
    for (name, _) in BUNDLED {
        let installed = plugins_dir().join(name);
        let (Ok(have), Ok(ship)) = (std::fs::read_to_string(installed.join("plugin.toml")), std::fs::read_to_string(market.join(name).join("plugin.toml"))) else { continue };
        if newer(&version_of(&ship), &version_of(&have)) && copy_dir(&market.join(name), &installed).is_ok() {
            updated.push(name.to_string());
        }
    }
    updated
}

fn version_of(manifest: &str) -> String {
    toml::from_str::<super::Manifest>(manifest).map(|m| m.version).unwrap_or_default()
}

/// Whether version `a` is newer than `b`, comparing dot-separated numbers (`1.10.0` > `1.9`).
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let (mut a, mut b) = (parts(a), parts(b));
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a > b
}

/// The marketplace's extensions, with the installed copy of each (same directory name).
pub fn entries(market: &Path, installed: &Plugins) -> (Vec<(Plugin, Option<Plugin>)>, Vec<(PathBuf, String)>) {
    let available = Plugins::load_from(market);
    let list = available
        .plugins
        .into_iter()
        .map(|p| {
            let local = installed.plugins.iter().find(|i| i.dir.file_name() == p.dir.file_name()).cloned();
            (p, local)
        })
        .collect();
    (list, available.errors)
}

/// Install or update: copy the extension over the installed one, keeping files the module wrote
/// itself (its settings, caches).
pub fn install(from: &Plugin) -> std::io::Result<PathBuf> {
    let to = plugins_dir().join(from.dir.file_name().unwrap_or_default());
    copy_dir(&from.dir, &to)?;
    Ok(to)
}

pub fn remove(installed: &Plugin) -> std::io::Result<()> {
    std::fs::remove_dir_all(&installed.dir)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            // fs::copy keeps the executable bit on Unix.
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("1.10.0", "1.9"));
        assert!(!newer("1.0", "1.0.0"));
        assert!(!newer("0.9.9", "1.0"));
    }

    #[test]
    fn seeds_the_bundled_sonarqube_extension() {
        let market = std::env::temp_dir().join(format!("forgecode-market-{}", std::process::id()));
        seed(&market).unwrap();
        let found = Plugins::load_from(&market);
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        let sonar = &found.plugins.iter().find(|p| p.manifest.name == "SonarQube").unwrap().manifest;
        assert!(sonar.commands.len() >= 5 && sonar.tools.iter().any(|t| t.name == "sonarqube_issues"));
        // Conan: its launcher is executable, its icons exist, its tools are there.
        let conan = found.plugins.iter().find(|p| p.manifest.name == "Conan").expect("Conan module");
        assert!(conan.manifest.tools.iter().any(|t| t.name == "conan_add"));
        for c in &conan.manifest.commands {
            if let Some(icon) = &c.icon {
                assert!(crate::icons::by_name(icon).is_some(), "icon {icon}");
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(std::fs::metadata(market.join("conan/conan-forge")).unwrap().permissions().mode() & 0o111 != 0);
        }
        std::fs::remove_dir_all(market).unwrap();
    }

    /// The Conan module's Python: adding a package to conanfile.txt, versions compared as numbers.
    #[test]
    fn conan_module_edits_requires() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions/conan");
        let script = "import sys; sys.dont_write_bytecode = True; sys.path.insert(0, sys.argv[1]); import conan_module as c
t = '[requires]\\nsfml/2.6.1\\n\\n[generators]\\nCMakeDeps\\n'
r = c.add_requirement(t, 'fmt/10.2.1')
assert r == '[requires]\\nsfml/2.6.1\\nfmt/10.2.1\\n\\n[generators]\\nCMakeDeps\\n', repr(r)
assert c.add_requirement(r, 'sfml/3.0.0').startswith('[requires]\\nsfml/3.0.0\\nfmt/10.2.1')
assert c.add_requirement('[generators]\\nCMakeDeps\\n', 'zlib/1.3').startswith('[requires]\\nzlib/1.3\\n')
assert c.version_key('1.10.2') > c.version_key('1.9.9')
assert c.requirements('[requires]\\nsfml/2.6.1 # rendu\\nfmt/10.2.1\\n[generators]\\nX\\n') == ['sfml/2.6.1', 'fmt/10.2.1']
assert c.requirements(c.remove_requirement(r, 'sfml')) == ['fmt/10.2.1']";
        let Ok(out) = crate::util::command("python3").args(["-c", script]).arg(&dir).output() else { return eprintln!("no python3: skipped") };
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
}
