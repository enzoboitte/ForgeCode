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

pub const CMAKE_BUILD: &str = "cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug -DCMAKE_EXPORT_COMPILE_COMMANDS=ON && cmake --build build";

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
        BuildSystem::CMake if c_like => format!("{CMAKE_BUILD} && {}", run_newest("build")),
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

/// CMake projects need `compile_commands.json` for clangd: configure once if missing.
const CMAKE_CONFIGURE: &str = "cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug -DCMAKE_EXPORT_COMPILE_COMMANDS=ON";

pub fn configure_command(root: &Path) -> Option<&'static str> {
    (detect(root) == BuildSystem::CMake && !root.join("build/compile_commands.json").exists()).then_some(CMAKE_CONFIGURE)
}

/// Whether `path` is part of the CMake build description (`CMakeLists.txt`, `*.cmake`, presets).
pub fn is_cmake_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name == "CMakeLists.txt" || name == "CMakePresets.json" || name.ends_with(".cmake")
}

/// Configure again after the build description changed (a library added with `find_package`,
/// `FetchContent`, `target_link_libraries`…): clangd then knows the new include paths.
pub fn reconfigure_command(root: &Path, changed: &Path) -> Option<&'static str> {
    (detect(root) == BuildSystem::CMake && is_cmake_file(changed) && !changed.starts_with(root.join("build"))).then_some(CMAKE_CONFIGURE)
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
