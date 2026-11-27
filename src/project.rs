//! Project build systems (Cargo, CMake, Make, npm) and new-project templates.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::language::{self, LanguageInfo};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BuildSystem {
    Cargo,
    CMake,
    Make,
    Npm,
    None,
}

pub fn detect(root: &Path) -> BuildSystem {
    if root.join("Cargo.toml").is_file() {
        BuildSystem::Cargo
    } else if root.join("CMakeLists.txt").is_file() {
        BuildSystem::CMake
    } else if ["Makefile", "makefile", "GNUmakefile"].iter().any(|m| root.join(m).is_file()) {
        BuildSystem::Make
    } else if root.join("package.json").is_file() {
        BuildSystem::Npm
    } else {
        BuildSystem::None
    }
}

/// Configure and build the CMake project in `build/` (Debug).
pub fn cmake_build(root: &Path) -> String {
    format!("{} && cmake --build build", cmake_configure(root))
}

fn has(program: &str) -> bool {
    crate::lsp::install::which(program).is_some()
}

/// CMake's generator for `build/`, from the tools installed: on Windows NMake (CMake's default
/// without Visual Studio's generator) only works in Visual Studio's environment, MinGW has its
/// own make. Empty: CMake's default, or the one `build/` was configured with.
fn cmake_generator(build: &Path) -> &'static str {
    drop_stale_cache(build);
    if build.join("CMakeCache.txt").exists() || !cfg!(windows) {
        return "";
    }
    if has("ninja") {
        " -G Ninja"
    } else if has("cl") && has("nmake") {
        " -G \"NMake Makefiles\""
    } else if has("mingw32-make") {
        " -G \"MinGW Makefiles\""
    } else {
        ""
    }
}

/// A `build/` configured with a generator whose tool is missing (an NMake attempt without Visual
/// Studio): its cache would fail every time, configure from scratch.
fn drop_stale_cache(build: &Path) {
    let Ok(cache) = std::fs::read_to_string(build.join("CMakeCache.txt")) else { return };
    let generator = cache.lines().find_map(|l| l.strip_prefix("CMAKE_GENERATOR:INTERNAL=")).unwrap_or("");
    let tool = match generator {
        "NMake Makefiles" => "nmake",
        "MinGW Makefiles" => "mingw32-make",
        "Ninja" => "ninja",
        "Unix Makefiles" => "make",
        _ => return,
    };
    if !has(tool) {
        let _ = std::fs::remove_file(build.join("CMakeCache.txt"));
        let _ = std::fs::remove_dir_all(build.join("CMakeFiles"));
    }
}

fn cmake_configure(root: &Path) -> String {
    format!("cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug -DCMAKE_EXPORT_COMPILE_COMMANDS=ON{}", cmake_generator(&root.join("build")))
}

/// Shell snippet running the most recently built executable under `dir`.
#[cfg(windows)]
fn run_newest(dir: &str) -> String {
    format!(
        "powershell -NoProfile -Command \"$e = Get-ChildItem -Path {dir} -Recurse -Depth 3 -Filter *.exe | Where-Object {{ $_.FullName -notmatch 'CMakeFiles' }} | Sort-Object LastWriteTime -Descending | Select-Object -First 1; if ($e) {{ & $e.FullName }} else {{ 'No executable found' }}\""
    )
}

/// Shell snippet running the most recently built executable under `dir`.
#[cfg(not(windows))]
fn run_newest(dir: &str) -> String {
    format!(
        "exe=$(ls -t $(find {dir} -maxdepth 3 -type f -perm -u+x -not -path '*/CMakeFiles/*' -not -path '*/.git/*' -not -name '*.sh' 2>/dev/null) 2>/dev/null | head -1); \
         if [ -n \"$exe\" ]; then \"$exe\"; else echo 'No executable found'; fi"
    )
}

/// Command that builds and runs the project (or the file when there is no project).
pub fn run_command(root: &Path, file: &Path, lang: &LanguageInfo) -> Option<String> {
    let c_like = matches!(lang.lsp_id.as_str(), "c" | "cpp") || matches!(lang.highlight.as_str(), "cmake" | "make");
    let script = match detect(root) {
        BuildSystem::Cargo if lang.lsp_id == "rust" || lang.highlight == "toml" => "cargo run".to_string(),
        BuildSystem::CMake if c_like => format!("{} && {}", cmake_build(root), run_newest("build")),
        BuildSystem::Make if c_like => format!("make && {}", run_newest(".")),
        BuildSystem::Npm if has_npm_start(root) && matches!(lang.lsp_id.as_str(), "javascript" | "typescript" | "javascriptreact" | "typescriptreact" | "json") => "npm start".into(),
        _ => return lang.run.as_ref().map(|t| language::expand(t, Some(file), root)),
    };
    Some(script)
}

/// Apply a language's settings to its run command: the chosen interpreter replaces the default
/// one (`python3 x.py` → `/opt/py/bin/python x.py`) and the program arguments go at the end.
pub fn with_lang_settings(command: String, settings: &crate::settings::LangSettings) -> String {
    const INTERPRETERS: [&str; 6] = ["python3 ", "python ", "php ", "node ", "ruby ", "lua "];
    let mut command = match INTERPRETERS.iter().find(|i| command.starts_with(*i)) {
        Some(default) if !settings.runtime.is_empty() => format!("\"{}\" {}", settings.runtime, &command[default.len()..]),
        _ => command,
    };
    if !settings.args.trim().is_empty() {
        // cargo and npm take the program's arguments after `--`.
        let separator = if command.starts_with("cargo run") || command.starts_with("npm start") { " --" } else { "" };
        command = format!("{command}{separator} {}", settings.args.trim());
    }
    command
}

fn has_npm_start(root: &Path) -> bool {
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["scripts"]["start"].is_string())
}

/// Where the program to debug comes from once the build finished.
#[derive(Clone, Debug, PartialEq)]
pub enum Program {
    Path(String),
    /// Read from `cargo build --message-format=json` output.
    FromCargo,
    /// The newest executable in this directory.
    Newest(PathBuf),
}

/// Most recently modified executable file under `dir` (build outputs), skipping CMake internals.
pub fn newest_executable(dir: &Path) -> Option<PathBuf> {
    executables(dir).into_iter().next()
}

/// Executable files under `dir` (build outputs), the newest first, skipping CMake internals.
pub fn executables(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<_> = ignore::WalkBuilder::new(dir)
        .max_depth(Some(3))
        .hidden(false)
        .git_ignore(false)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| !p.components().any(|c| c.as_os_str() == "CMakeFiles" || c.as_os_str() == ".git"))
        .filter(|p| !matches!(p.extension().and_then(|e| e.to_str()), Some("sh" | "so" | "dylib" | "a" | "o" | "py")))
        .filter_map(|p| {
            let meta = std::fs::metadata(&p).ok()?;
            let modified = meta.modified().ok()?;
            is_executable(&p, &meta).then_some((modified, p))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

#[cfg(unix)]
fn is_executable(_: &Path, meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn is_executable(path: &Path, _: &std::fs::Metadata) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

/// How CTest runs the program `exe` when it is one of its tests: (working folder, environment).
/// Asks `ctest --show-only=json-v1` in the topmost build folder above it that CTest knows.
pub fn ctest_context(root: &Path, exe: &Path) -> Option<(PathBuf, Vec<String>)> {
    let top = exe.ancestors().skip(1).take_while(|d| d.starts_with(root) && *d != root).filter(|d| d.join("CTestTestfile.cmake").is_file()).last()?;
    let out = crate::util::command("ctest").args(["--show-only=json-v1"]).current_dir(top).output().ok()?;
    ctest_lookup(&String::from_utf8_lossy(&out.stdout), exe)
}

fn ctest_lookup(json: &str, exe: &Path) -> Option<(PathBuf, Vec<String>)> {
    // Same file whatever the slashes, case (Windows) or `\\?\` prefix.
    let key = |p: &Path| {
        let s = crate::util::plain_path(p.to_path_buf()).display().to_string().replace('\\', "/");
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    let tests: serde_json::Value = serde_json::from_str(json).ok()?;
    let test = tests["tests"].as_array()?.iter().find(|t| t["command"][0].as_str().is_some_and(|c| key(Path::new(c)) == key(exe)))?;
    let prop = |name: &str| test["properties"].as_array().into_iter().flatten().find(|p| p["name"] == name).map(|p| p["value"].clone());
    let dir = prop("WORKING_DIRECTORY").and_then(|v| v.as_str().map(PathBuf::from)).or_else(|| exe.parent().map(Path::to_path_buf))?;
    let env = prop("ENVIRONMENT").and_then(|v| v.as_array().cloned()).unwrap_or_default().iter().filter_map(|e| e.as_str().map(String::from)).collect();
    Some((dir, env))
}

/// clangd finds `build/compile_commands.json`: put there the newest one a build wrote deeper
/// (`build/windows-gcc-16-debug/`, a preset's folder). Whether there is one now.
pub fn adopt_compile_commands(root: &Path) -> bool {
    let target = root.join("build").join("compile_commands.json");
    if target.is_file() || root.join("compile_commands.json").is_file() {
        return true;
    }
    let newest = ignore::WalkBuilder::new(root.join("build"))
        .max_depth(Some(3))
        .hidden(false)
        .git_ignore(false)
        .build()
        .flatten()
        .filter(|e| e.file_name() == "compile_commands.json")
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.into_path())))
        .max_by_key(|(t, _)| *t);
    newest.is_some_and(|(_, found)| std::fs::copy(found, &target).is_ok())
}

/// How to get `compile_commands.json` for clangd in a CMake project that has none: through its
/// presets (Conan's included), else a plain configure with an installed generator. None for a
/// Conan project not installed yet (its dependencies come first) or when one exists already.
pub fn configure_command(root: &Path) -> Option<String> {
    if adopt_compile_commands(root) {
        return None;
    }
    configure(root)
}

fn configure(root: &Path) -> Option<String> {
    if !root.join("CMakeLists.txt").is_file() || !has("cmake") {
        return None;
    }
    let presets = crate::launch::configure_presets(root);
    let preset = presets.iter().find(|(n, _)| n.to_lowercase().contains("debug")).or(presets.first());
    if let Some((name, dir)) = preset {
        let db = dir.join("compile_commands.json");
        return Some(format!(
            "cmake --preset \"{name}\" -DCMAKE_EXPORT_COMPILE_COMMANDS=ON && cmake -E make_directory build && cmake -E copy_if_different \"{}\" \"{}\"",
            db.display(),
            root.join("build").join("compile_commands.json").display()
        ));
    }
    if root.join("conanfile.py").is_file() || root.join("conanfile.txt").is_file() {
        return None;
    }
    Some(cmake_configure(root))
}

/// Whether `path` is part of the CMake build description (`CMakeLists.txt`, `*.cmake`, presets).
pub fn is_cmake_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name == "CMakeLists.txt" || name == "CMakePresets.json" || name.ends_with(".cmake")
}

/// Configure again after the build description changed (a library added with `find_package`,
/// `FetchContent`, `target_link_libraries`…): clangd then knows the new include paths.
pub fn reconfigure_command(root: &Path, changed: &Path) -> Option<String> {
    (is_cmake_file(changed) && !changed.starts_with(root.join("build"))).then(|| configure(root)).flatten()
}

// ---- templates ----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Template {
    C,
    Cpp,
    Rust,
    TypeScript,
    Web,
}

impl Template {
    pub const ALL: [Template; 5] = [Template::C, Template::Cpp, Template::Rust, Template::TypeScript, Template::Web];

    pub fn label(self) -> &'static str {
        match self {
            Template::C => "C | Makefile",
            Template::Cpp => "C++ | CMake",
            Template::Rust => "Rust | Cargo",
            Template::TypeScript => "TypeScript | Node",
            Template::Web => "Web | HTML/CSS/JS",
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) && !name.starts_with('.')
}

/// Create `parent/name` from a template, `git init` it, and return (project dir, file to open).
pub fn create(template: Template, parent: &Path, name: &str) -> Result<(PathBuf, PathBuf)> {
    if !valid_name(name) {
        bail!("invalid project name: use letters, digits, '-', '_' or '.'");
    }
    let dir = parent.join(name);
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    let files: Vec<(&str, String)> = match template {
        Template::C => vec![
            ("main.c", "#include <stdio.h>\n\nint main(void) {\n    printf(\"Hello, world!\\n\");\n    return 0;\n}\n".into()),
            ("Makefile", format!("CC ?= cc\nCFLAGS ?= -Wall -Wextra -g -O0\n\n{name}: main.c\n\t$(CC) $(CFLAGS) -o $@ $^\n\nclean:\n\trm -f {name}\n\n.PHONY: clean\n")),
            (".gitignore", format!("{name}\n*.o\n")),
        ],
        Template::Cpp => vec![
            ("src/main.cpp", "#include <iostream>\n\nint main() {\n    std::cout << \"Hello, world!\" << std::endl;\n    return 0;\n}\n".into()),
            ("CMakeLists.txt", format!("cmake_minimum_required(VERSION 3.20)\nproject({name} CXX)\n\nset(CMAKE_CXX_STANDARD 20)\nset(CMAKE_CXX_STANDARD_REQUIRED ON)\nset(CMAKE_EXPORT_COMPILE_COMMANDS ON)\n\nadd_executable({name} src/main.cpp)\n")),
            (".gitignore", "build/\n".into()),
        ],
        Template::Rust => vec![
            ("Cargo.toml", format!("[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n", name.replace('.', "_"))),
            ("src/main.rs", "fn main() {\n    println!(\"Hello, world!\");\n}\n".into()),
            (".gitignore", "/target\n".into()),
        ],
        Template::TypeScript => vec![
            ("src/index.ts", "const greet = (name: string): string => `Hello, ${name}!`;\n\nconsole.log(greet(\"world\"));\n".into()),
            ("package.json", format!("{{\n  \"name\": \"{}\",\n  \"version\": \"0.1.0\",\n  \"type\": \"module\",\n  \"scripts\": {{\n    \"start\": \"npx tsx src/index.ts\",\n    \"build\": \"tsc\"\n  }}\n}}\n", name.to_lowercase())),
            ("tsconfig.json", "{\n  \"compilerOptions\": {\n    \"target\": \"ES2022\",\n    \"module\": \"NodeNext\",\n    \"strict\": true,\n    \"outDir\": \"dist\"\n  },\n  \"include\": [\"src\"]\n}\n".into()),
            (".gitignore", "node_modules/\ndist/\n".into()),
        ],
        Template::Web => vec![
            ("index.html", format!("<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\">\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n  <title>{name}</title>\n  <link rel=\"stylesheet\" href=\"style.css\">\n</head>\n<body>\n  <h1>{name}</h1>\n  <script src=\"script.js\"></script>\n</body>\n</html>\n")),
            ("style.css", "body {\n  font-family: system-ui, sans-serif;\n  margin: 2rem;\n}\n".into()),
            ("script.js", "console.log(\"ready\");\n".into()),
        ],
    };
    for (rel, content) in &files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, content)?;
    }
    // A repository from the start, like `cargo new`; not fatal when git is missing.
    let _ = crate::util::command("git").arg("init").arg("-q").current_dir(&dir).status();
    Ok((dir.clone(), dir.join(files[0].0)))
}

#[cfg(test)]
mod cmake_tests {
    use super::*;

    #[test]
    fn reconfigures_when_the_build_description_changes() {
        let root = std::env::temp_dir().join(format!("forge-cmake-{}", std::process::id()));
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("CMakeLists.txt"), "project(x)").unwrap();
        assert!(reconfigure_command(&root, &root.join("CMakeLists.txt")).is_some());
        assert!(reconfigure_command(&root, &root.join("cmake/deps.cmake")).is_some());
        assert!(reconfigure_command(&root, &root.join("src/main.cpp")).is_none());
        assert!(reconfigure_command(&root, &root.join("build/x.cmake")).is_none(), "cmake's own output");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// compile_commands.json: an existing one deeper in build/ is used; a Conan project goes
    /// through its preset (never a bare configure, which can't find its dependencies).
    #[test]
    fn compile_commands_the_project_s_way() {
        if !has("cmake") {
            return eprintln!("cmake not installed, skipping");
        }
        let root = std::env::temp_dir().join(format!("forge-cc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("build/gcc-debug")).unwrap();
        std::fs::write(root.join("CMakeLists.txt"), "").unwrap();
        std::fs::write(root.join("conanfile.py"), "").unwrap();
        assert_eq!(configure_command(&root), None, "Conan not installed yet: nothing to configure");
        std::fs::write(root.join("CMakeUserPresets.json"), r#"{"version": 4, "configurePresets": [{"name": "conan-release", "binaryDir": "${sourceDir}/build/gcc-release"}, {"name": "conan-debug", "binaryDir": "${sourceDir}/build/gcc-debug"}]}"#).unwrap();
        let cmd = configure_command(&root).unwrap();
        assert!(cmd.starts_with("cmake --preset \"conan-debug\""), "{cmd}");
        std::fs::write(root.join("build/gcc-debug/compile_commands.json"), "[]").unwrap();
        assert_eq!(configure_command(&root), None, "the build's own database is adopted");
        assert!(root.join("build/compile_commands.json").is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn runs_tests_where_ctest_does() {
        let json = r#"{"kind": "ctestInfo", "tests": [
            {"name": "other", "command": ["/b/other"]},
            {"name": "Suite.Case", "command": ["/b/tests/app_test", "--gtest_filter=Suite.Case"], "properties": [
                {"name": "WORKING_DIRECTORY", "value": "/b/tests"}, {"name": "ENVIRONMENT", "value": ["DATA=/d"]}]}]}"#;
        assert_eq!(ctest_lookup(json, Path::new("/b/tests/app_test")), Some((PathBuf::from("/b/tests"), vec!["DATA=/d".to_string()])));
        assert_eq!(ctest_lookup(json, Path::new("/b/other")), Some((PathBuf::from("/b"), vec![])), "CTest's default: the program's folder");
        assert_eq!(ctest_lookup(json, Path::new("/b/app")), None);
    }
    use crate::plugins::Plugins;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("forgecode-proj-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn templates_create_buildable_layouts() {
        let parent = tmp("tpl");
        for (i, t) in Template::ALL.iter().enumerate() {
            let (dir, main) = create(*t, &parent, &format!("p{i}")).unwrap();
            assert!(main.is_file(), "{t:?}");
            assert!(dir.join(".git").is_dir(), "{t:?} is a git repo");
        }
        assert_eq!(detect(&parent.join("p0")), BuildSystem::Make);
        assert_eq!(detect(&parent.join("p1")), BuildSystem::CMake);
        assert_eq!(detect(&parent.join("p2")), BuildSystem::Cargo);
        assert!(create(Template::C, &parent, "p0").is_err(), "no overwrite");
        assert!(create(Template::C, &parent, "../x").is_err());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn run_uses_the_languages_interpreter_and_arguments() {
        let s = crate::settings::LangSettings { runtime: "/v/bin/python".into(), args: "--n 3".into(), ..Default::default() };
        assert_eq!(with_lang_settings("python3 \"a.py\"".into(), &s), "\"/v/bin/python\" \"a.py\" --n 3");
        assert_eq!(with_lang_settings("cargo run".into(), &Default::default()), "cargo run");
        assert_eq!(with_lang_settings("cargo run".into(), &s), "cargo run -- --n 3");
    }

    #[test]
    fn run_and_debug_follow_the_build_system() {
        let root = tmp("bs");
        let file = root.join("main.c");
        let lang = language::detect(&file, &Plugins::default());
        assert!(run_command(&root, &file, &lang).unwrap().starts_with("cc "), "single file without project");
        std::fs::write(root.join("Makefile"), "all:\n").unwrap();
        assert!(run_command(&root, &file, &lang).unwrap().starts_with("make && "));
        assert_eq!(crate::debug::adapters::plan(&root, &file, "c").unwrap().program, Program::Newest(root.clone()));
        std::fs::write(root.join("CMakeLists.txt"), "").unwrap();
        assert!(run_command(&root, &file, &lang).unwrap().starts_with("cmake -S . -B build"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn finds_the_newest_executable() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("exe");
        std::fs::create_dir_all(root.join("build/CMakeFiles")).unwrap();
        for p in ["build/app", "build/CMakeFiles/probe", "build/notes.txt"] {
            std::fs::write(root.join(p), "x").unwrap();
        }
        for p in ["build/app", "build/CMakeFiles/probe"] {
            std::fs::set_permissions(root.join(p), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(newest_executable(&root.join("build")), Some(root.join("build/app")));
        std::fs::remove_dir_all(root).unwrap();
    }
}
