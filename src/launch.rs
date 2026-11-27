//! The ways to start a project under the debugger, offered on F5 like VS Code's picker:
//! `.vscode/launch.json`, the modules' targets (Conan…), CMake presets, the build system, the file.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::debug::adapters::{self, Plan};
use crate::project::{Program, cmake_build, executables};

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub label: String,
    pub detail: String,
    pub plan: Plan,
}

/// A module able to describe its own targets: `run` prints them as JSON (see [`module_targets`]).
pub struct ModuleLaunch {
    pub name: String,
    pub dir: PathBuf,
    pub run: String,
}

/// Every target for the project, the most specific first. Blocking (reads files, runs modules).
pub fn targets(root: &Path, file: Option<(&Path, &str)>, modules: &[ModuleLaunch]) -> Vec<Target> {
    let mut all = vscode_targets(root, file.map(|f| f.0));
    for m in modules {
        all.extend(module_targets(root, m));
    }
    // Every build system of the project whose tool is installed (not only the first found).
    let build = |label: &str, detail: &str, cmd: &str, dir: PathBuf| Target { label: label.into(), detail: detail.into(), plan: Plan::native(Some(cmd.into()), Program::Newest(dir)) };
    if has("cmake") {
        all.extend(cmake_presets(root));
        if root.join("CMakeLists.txt").is_file() {
            all.push(build("CMake", "cmake -B build (Debug)", &cmake_build(root), root.join("build")));
        }
    }
    if let Some(make) = make_program().filter(|_| ["Makefile", "makefile", "GNUmakefile"].iter().any(|m| root.join(m).is_file())) {
        let targets = make_targets(root);
        if targets.is_empty() {
            all.push(build("Make", &make, &make, root.to_path_buf()));
        }
        for t in targets {
            let cmd = format!("{make} {t}");
            all.push(build(&format!("Make | {t}"), &cmd, &cmd, root.to_path_buf()));
        }
    }
    if root.join("Cargo.toml").is_file() && has("cargo") {
        all.push(Target { label: "Cargo".into(), detail: "cargo build".into(), plan: Plan::native(Some("cargo build --message-format=json-render-diagnostics".into()), Program::FromCargo) });
    }
    if let Some((file, lsp_id)) = file {
        all.extend(file_targets(root, file, lsp_id));
    }
    let mut unique: Vec<Target> = vec![];
    for t in all {
        if !unique.iter().any(|u| u.plan == t.plan) {
            unique.push(t);
        }
    }
    expand_programs(unique)
}

fn has(program: &str) -> bool {
    crate::lsp::install::which(program).is_some()
}

/// `make`, else MinGW's `mingw32-make`; none when neither is installed.
fn make_program() -> Option<String> {
    ["make", "mingw32-make"].into_iter().find(|m| has(m)).map(String::from)
}

/// The current file on its own: C / C++ with each installed compiler (gcc, clang, MSVC's cl),
/// other languages with their runtime when it is installed.
fn file_targets(root: &Path, file: &Path, lsp_id: &str) -> Vec<Target> {
    let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let here = crate::i18n::t("Current file");
    let compilers: &[&str] = match lsp_id {
        "c" => &["gcc", "clang", "cl"],
        "cpp" => &["g++", "clang++", "cl"],
        _ => {
            let runtime: &[&str] = match lsp_id {
                "python" => &["python3", "python", "py"],
                "javascript" | "javascriptreact" | "typescript" | "typescriptreact" => &["node"],
                "php" => &["php"],
                "go" => &["go"],
                "zig" => &["zig"],
                "swift" => &["swiftc"],
                "rust" => &["cargo"],
                _ => &[],
            };
            let Some(tool) = runtime.iter().find(|r| has(r)) else { return vec![] };
            return adapters::plan(root, file, lsp_id).map(|plan| Target { label: name, detail: format!("{here} | {tool}"), plan }).into_iter().collect();
        }
    };
    let stem = file.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = std::env::temp_dir();
    let includes = crate::language::include_flags(root);
    compilers
        .iter()
        .filter(|c| has(c))
        .map(|compiler| {
            let exe = tmp.join(format!("forgecode-debug-{stem}-{compiler}{}", std::env::consts::EXE_SUFFIX)).display().to_string();
            let std = if lsp_id == "cpp" { if *compiler == "cl" { "/std:c++20 /EHsc" } else { "-std=c++20" } } else { "" };
            let cmd = if *compiler == "cl" {
                // Objects and debug info next to the program, not in the project.
                format!("cl /nologo /Zi /Od {std} {includes} \"{}\" /Fe\"{exe}\" /Fo\"{}\\\\\" /Fd\"{}\\\\\"", file.display(), tmp.display(), tmp.display())
            } else {
                format!("{compiler} -g -O0 {std} {includes} \"{}\" -o \"{exe}\"", file.display())
            };
            Target { label: format!("{name} | {compiler}"), detail: here.to_string(), plan: Plan::native(Some(cmd), Program::Path(exe)) }
        })
        .collect()
}

/// The Makefile's own targets (`build`, `debug`, `test`…), in file order; not patterns, special
/// targets (`.PHONY`) or variable assignments (`X := …`).
fn make_targets(root: &Path) -> Vec<String> {
    let Some(text) = ["Makefile", "makefile", "GNUmakefile"].iter().find_map(|m| std::fs::read_to_string(root.join(m)).ok()) else { return vec![] };
    let mut targets: Vec<String> = vec![];
    for line in text.lines() {
        let Some((head, rest)) = line.split_once(':') else { continue };
        if line.starts_with(['\t', ' ', '#', '.']) || rest.starts_with('=') || head.contains(['=', '%', '$', '(']) {
            continue;
        }
        for name in head.split_whitespace() {
            if !targets.iter().any(|t| t == name) {
                targets.push(name.to_string());
            }
        }
    }
    targets
}

/// A target whose program is "the newest executable of a folder" that already holds several
/// (an app and its tests): one entry per executable, so the right one is debugged.
fn expand_programs(targets: Vec<Target>) -> Vec<Target> {
    targets
        .into_iter()
        .flat_map(|t| {
            let Program::Newest(dir) = &t.plan.program else { return vec![t] };
            let exes = executables(dir);
            if exes.len() < 2 {
                return vec![t];
            }
            exes.into_iter()
                .map(|exe| {
                    // `windows-gcc-16-debug/app.exe`: the Debug and Release builds tell apart.
                    let name = exe.strip_prefix(dir).unwrap_or(&exe).display().to_string().replace('\\', "/");
                    Target { label: format!("{} > {name}", t.label), detail: t.detail.clone(), plan: Plan { program: Program::Path(exe.display().to_string()), ..t.plan.clone() } }
                })
                .collect()
        })
        .collect()
}

// ---- .vscode/launch.json -------------------------------------------------------------------------

/// VS Code's native configurations (cppdbg, cppvsdbg, lldb…), with their `preLaunchTask`.
fn vscode_targets(root: &Path, file: Option<&Path>) -> Vec<Target> {
    let Some(launch) = read_jsonc(&root.join(".vscode/launch.json")) else { return vec![] };
    let tasks = read_jsonc(&root.join(".vscode/tasks.json"));
    let native = ["cppdbg", "cppvsdbg", "lldb", "lldb-dap", "codelldb", "gdb"];
    launch["configurations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["request"] == "launch" && native.contains(&c["type"].as_str().unwrap_or("")))
        .filter_map(|c| {
            let vars = |s: &str| expand_vars(s, root, file);
            let program = c["program"].as_str().map(vars)?;
            // `${command:cmake.launchTargetPath}` and the like: the newest program of the build.
            let program = if program.contains("${") { Program::Newest(root.join("build")) } else { Program::Path(absolute(root, &program)) };
            let args = c["args"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).map(vars).collect());
            let cwd = c["cwd"].as_str().map(|s| PathBuf::from(absolute(root, &vars(s))));
            let env = match &c["env"] {
                Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), vars(v.as_str().unwrap_or("")))).collect(),
                Value::Array(a) => a.iter().filter_map(|e| Some((e["name"].as_str()?.to_string(), vars(e["value"].as_str()?)))).collect(),
                _ => vec![],
            };
            let build = c["preLaunchTask"].as_str().and_then(|label| task_command(tasks.as_ref()?, label, root, file));
            let label = c["name"].as_str().unwrap_or("launch.json").to_string();
            Some(Target { label, detail: ".vscode/launch.json".into(), plan: Plan { args, cwd, env, ..Plan::native(build, program) } })
        })
        .collect()
}

/// The shell command of the task `label` in tasks.json (CMake Tools' tasks build the project).
fn task_command(tasks: &Value, label: &str, root: &Path, file: Option<&Path>) -> Option<String> {
    let task = tasks["tasks"].as_array()?.iter().find(|t| t["label"] == label)?;
    if task["type"] == "cmake" {
        return Some(cmake_build(root));
    }
    let quote = |s: String| if s.contains(' ') && !s.starts_with('"') { format!("\"{s}\"") } else { s };
    let command = expand_vars(task["command"].as_str()?, root, file);
    let args = task["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str().or_else(|| a["value"].as_str())).map(|a| quote(expand_vars(a, root, file)));
    Some(std::iter::once(quote(command)).chain(args).collect::<Vec<_>>().join(" "))
}

fn absolute(root: &Path, path: &str) -> String {
    if Path::new(path).is_absolute() { path.to_string() } else { root.join(path).display().to_string() }
}

/// VS Code's `${workspaceFolder}`, `${file}`, `${env:NAME}`… Unknown ones are left as they are.
fn expand_vars(s: &str, root: &Path, file: Option<&Path>) -> String {
    let f = |p: Option<&Path>| p.map(|p| p.display().to_string()).unwrap_or_default();
    let mut out = s
        .replace("${workspaceFolder}", &root.display().to_string())
        .replace("${workspaceRoot}", &root.display().to_string())
        .replace("${workspaceFolderBasename}", &root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
        .replace("${fileDirname}", &f(file.and_then(Path::parent)))
        .replace("${fileBasenameNoExtension}", &file.and_then(|p| p.file_stem()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
        .replace("${fileBasename}", &file.and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
        .replace("${file}", &f(file))
        .replace("${pathSeparator}", std::path::MAIN_SEPARATOR_STR);
    while let Some(start) = out.find("${env:") {
        let Some(len) = out[start..].find('}') else { break };
        let value = std::env::var(&out[start + 6..start + len]).unwrap_or_default();
        out.replace_range(start..start + len + 1, &value);
    }
    out
}

/// JSON with comments and trailing commas, as VS Code and CMake presets allow.
fn read_jsonc(path: &Path) -> Option<Value> {
    serde_json::from_str(&strip_jsonc(&std::fs::read_to_string(path).ok()?)).ok()
}

fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                out.extend(chars.next());
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                while chars.peek().is_some_and(|&c| c != '\n') {
                    chars.next();
                }
            }
            ('/', Some('*')) => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
            }
            (']' | '}', _) => {
                // A trailing comma before the closing bracket.
                let end = out.trim_end().len();
                if out[..end].ends_with(',') {
                    out.truncate(end - 1);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

// ---- modules -------------------------------------------------------------------------------------

/// A module's targets: its `launch` command prints, on its last line, a JSON list of
/// `{label, detail, build, program | program_dir, args, cwd}` (paths relative to the project).
/// The build runs with the module's command environment; it may print
/// `@forge {"action":"launch","program","env","cwd","args"}` to set the program up.
fn module_targets(root: &Path, m: &ModuleLaunch) -> Vec<Target> {
    let mut env = crate::plugins::command_env(&m.dir);
    env.push(("FORGE_WORKSPACE".into(), root.display().to_string()));
    let Ok(out) = crate::util::shell_command(&m.run).current_dir(root).envs(env.clone()).output() else { return vec![] };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(list) = text.lines().rev().find(|l| l.trim_start().starts_with('[')).and_then(|l| serde_json::from_str::<Value>(l).ok()) else { return vec![] };
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let (dir, programs) = match (t["program"].as_str(), t["program_dir"].as_str()) {
                (Some(p), _) => (None, vec![Program::Path(absolute(root, p))]),
                (None, d) => {
                    let dir = root.join(d.unwrap_or("build"));
                    // Only the programs of this build (`build/…-debug/` for Debug), once built.
                    let wanted = t["program_match"].as_str().unwrap_or("").to_lowercase();
                    let mine: Vec<Program> = executables(&dir)
                        .into_iter()
                        .filter(|e| !wanted.is_empty() && e.strip_prefix(&dir).unwrap_or(e).to_string_lossy().to_lowercase().contains(&wanted))
                        .map(|e| Program::Path(e.display().to_string()))
                        .collect();
                    (Some(dir.clone()), if mine.is_empty() { vec![Program::Newest(dir)] } else { mine })
                }
            };
            let label = t["label"].as_str()?.to_string();
            let detail = t["detail"].as_str().map_or_else(|| m.name.clone(), |d| format!("{} | {d}", m.name));
            let several = programs.len() > 1;
            let env = env.clone();
            Some(programs.into_iter().map(move |program| {
                let label = match (&program, &dir) {
                    (Program::Path(p), Some(dir)) if several => format!("{label} > {}", Path::new(p).strip_prefix(dir).unwrap_or(Path::new(p)).display().to_string().replace('\\', "/")),
                    _ => label.clone(),
                };
                let plan = Plan {
                    args: t["args"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()),
                    cwd: t["cwd"].as_str().map(|c| PathBuf::from(absolute(root, c))),
                    env: env.clone(),
                    ..Plan::native(t["build"].as_str().map(String::from), program)
                };
                Target { label, detail: detail.clone(), plan }
            }).collect::<Vec<_>>())
        })
        .flatten()
        .collect()
}

/// `@forge {"action":"launch", …}` printed by a module's build: the program, its environment
/// (the libraries' folders), its directory and arguments.
pub fn launch_override(output: &str) -> Option<Value> {
    output.lines().rev().filter_map(|l| l.strip_prefix("@forge ")).filter_map(|l| serde_json::from_str::<Value>(l).ok()).find(|v| v["action"] == "launch")
}

// ---- CMake presets -------------------------------------------------------------------------------

/// The configure presets of CMakePresets.json / CMakeUserPresets.json (and the files they include),
/// each built with its build preset when there is one.
/// The visible configure presets: (name, build folder).
pub fn configure_presets(root: &Path) -> Vec<(String, PathBuf)> {
    cmake_presets(root)
        .into_iter()
        .filter_map(|t| match t.plan.program {
            Program::Newest(dir) => Some((t.detail, dir)),
            _ => None,
        })
        .collect()
}

fn cmake_presets(root: &Path) -> Vec<Target> {
    let mut configure: Vec<Value> = vec![];
    let mut build: Vec<Value> = vec![];
    let mut files: Vec<PathBuf> = vec![root.join("CMakePresets.json"), root.join("CMakeUserPresets.json")];
    let mut seen = 0;
    while seen < files.len() && seen < 20 {
        let path = files[seen].clone();
        seen += 1;
        let Some(v) = read_jsonc(&path) else { continue };
        let dir = path.parent().unwrap_or(root).to_path_buf();
        files.extend(v["include"].as_array().into_iter().flatten().filter_map(|i| i.as_str()).map(|i| dir.join(i)));
        configure.extend(v["configurePresets"].as_array().cloned().unwrap_or_default());
        build.extend(v["buildPresets"].as_array().cloned().unwrap_or_default());
    }
    configure
        .iter()
        .filter(|p| p["hidden"] != true)
        .filter_map(|p| {
            let name = p["name"].as_str()?;
            let binary = p["binaryDir"].as_str().map(|d| d.replace("${sourceDir}", &root.display().to_string()).replace("${presetName}", name));
            let dir = binary.map(|b| PathBuf::from(absolute(root, &b))).unwrap_or_else(|| root.join("build"));
            let build_cmd = match build.iter().find(|b| b["configurePreset"] == name && b["hidden"] != true).and_then(|b| b["name"].as_str()) {
                Some(b) => format!("cmake --preset \"{name}\" && cmake --build --preset \"{b}\""),
                None => format!("cmake --preset \"{name}\" && cmake --build \"{}\"", dir.display()),
            };
            let label = format!("CMake preset | {}", p["displayName"].as_str().unwrap_or(name));
            Some(Target { label, detail: name.to_string(), plan: Plan::native(Some(build_cmd), Program::Newest(dir)) })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> PathBuf {
        // One folder per test: they run in parallel.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("forge-launch-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    #[test]
    fn lists_makefile_targets() {
        let root = project(&[("Makefile", "MINIFORGE := C:/Bib\nCONDA := \"$(MINIFORGE)/conda.exe\"\n\n.PHONY: build debug\n\nsetup:\n# conan create x\n\t$(CONDA) run conan install .\n\nbuild: setup\n\t$(MAKE) setup\ndebug test:\n\techo\n%.o: %.c\n\tcc\n")]);
        assert_eq!(make_targets(&root), ["setup", "build", "debug", "test"]);
        let labels: Vec<_> = targets(&root, None, &[]).into_iter().map(|t| t.label).collect();
        assert!(labels.contains(&"Make | debug".to_string()), "{labels:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The current C file: one entry per installed compiler, none for a missing one.
    #[test]
    fn current_file_with_each_installed_compiler() {
        let root = project(&[("main.c", "int main(void) { return 0; }\n")]);
        let labels: Vec<String> = file_targets(&root, &root.join("main.c"), "c").into_iter().map(|t| t.label).collect();
        let installed: Vec<String> = ["gcc", "clang", "cl"].iter().filter(|c| has(c)).map(|c| format!("main.c | {c}")).collect();
        assert_eq!(labels, installed);
        assert!(file_targets(&root, &root.join("a.xyz"), "plaintext").is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn strips_comments_and_trailing_commas() {
        let v: Value = serde_json::from_str(&strip_jsonc("{\n // c\n \"a\": \"x // not a comment\", /* b */ \"l\": [1, 2,],\n}")).unwrap();
        assert_eq!(v["a"], "x // not a comment");
        assert_eq!(v["l"][1], 2);
    }

    #[test]
    fn reads_launch_json_and_its_task() {
        let root = project(&[
            (".vscode/launch.json", r#"{ "configurations": [
                // the app
                {"name": "App", "type": "cppvsdbg", "request": "launch", "program": "${workspaceFolder}/build/app.exe", "args": ["-v"], "cwd": "${workspaceFolder}", "preLaunchTask": "build", "env": {"MODE": "x"}},
                {"name": "Attach", "type": "cppdbg", "request": "attach", "program": "x"},
                {"name": "Py", "type": "python", "request": "launch", "program": "a.py"},
            ]}"#),
            (".vscode/tasks.json", r#"{"tasks": [{"label": "build", "type": "shell", "command": "cmake", "args": ["--build", "build dir"]}]}"#),
        ]);
        let t = vscode_targets(&root, None);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].label, "App");
        assert_eq!(t[0].plan.build.as_deref(), Some("cmake --build \"build dir\""));
        assert_eq!(t[0].plan.program, Program::Path(root.join("build/app.exe").display().to_string()));
        assert_eq!((t[0].plan.args.clone(), t[0].plan.env.clone()), (Some(vec!["-v".into()]), vec![("MODE".into(), "x".into())]));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reads_cmake_presets_through_includes() {
        let root = project(&[
            ("CMakeLists.txt", ""),
            ("CMakeUserPresets.json", r#"{"version": 4, "include": ["build/gen/CMakePresets.json"]}"#),
            ("build/gen/CMakePresets.json", r#"{"version": 4, "configurePresets": [{"name": "conan-debug", "binaryDir": "${sourceDir}/build/Debug"}, {"name": "base", "hidden": true}], "buildPresets": [{"name": "conan-debug", "configurePreset": "conan-debug"}]}"#),
        ]);
        let t = targets(&root, None, &[]);
        let labels: Vec<_> = t.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["CMake preset | conan-debug", "CMake"]);
        assert_eq!(t[0].plan.build.as_deref(), Some("cmake --preset \"conan-debug\" && cmake --build --preset \"conan-debug\""));
        assert_eq!(t[0].plan.program, Program::Newest(root.join("build/Debug")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn one_entry_per_built_program() {
        use std::os::unix::fs::PermissionsExt;
        let root = project(&[("Makefile", "all:\n"), ("app", ""), ("tests/gtest_app", "")]);
        for exe in ["app", "tests/gtest_app"] {
            std::fs::set_permissions(root.join(exe), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut labels: Vec<_> = targets(&root, None, &[]).into_iter().map(|t| t.label).collect();
        labels.sort();
        assert_eq!(labels, ["Make | all > app", "Make | all > tests/gtest_app"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn asks_modules_for_their_targets() {
        let root = project(&[("conanfile.txt", "")]);
        let run = r#"echo noise; echo '[{"label": "Conan | Debug", "build": "conan-forge build debug", "program_dir": "build/Debug"}]'"#;
        let t = module_targets(&root, &ModuleLaunch { name: "Conan".into(), dir: root.clone(), run: run.into() });
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].label.as_str(), t[0].detail.as_str()), ("Conan | Debug", "Conan"));
        assert_eq!(t[0].plan.program, Program::Newest(root.join("build/Debug")));
        assert!(t[0].plan.env.iter().any(|(k, _)| k == "FORGE_WORKSPACE"));
        // `program_match`: only this build type's programs.
        use std::os::unix::fs::PermissionsExt;
        for exe in ["build/gcc-debug/app", "build/gcc-release/app"] {
            std::fs::create_dir_all(root.join(exe).parent().unwrap()).unwrap();
            std::fs::write(root.join(exe), "").unwrap();
            std::fs::set_permissions(root.join(exe), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let run = r#"echo '[{"label": "Conan | Debug", "build": "b", "program_dir": "build", "program_match": "debug"}]'"#;
        let t = module_targets(&root, &ModuleLaunch { name: "Conan".into(), dir: root.clone(), run: run.into() });
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].plan.program, Program::Path(root.join("build/gcc-debug/app").display().to_string()));
        let out = "building\n@forge {\"action\":\"notify\"}\n@forge {\"action\":\"launch\",\"env\":{\"PATH\":\"/deps\"}}\n";
        assert_eq!(launch_override(out).unwrap()["env"]["PATH"], "/deps");
        let _ = std::fs::remove_dir_all(root);
    }
}
