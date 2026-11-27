//! The ways to start a project under the debugger, offered on F5 like VS Code's picker:
//! `.vscode/launch.json`, the modules' targets (Conan…), CMake presets, the build system, the file.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::debug::adapters::{self, Plan};
use crate::project::{BuildSystem, CMAKE_BUILD, Program, detect, executables};

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
    all.extend(cmake_presets(root));
    let build = |label: &str, detail: &str, cmd: &str, dir: PathBuf| Target { label: label.into(), detail: detail.into(), plan: Plan::native(Some(cmd.into()), Program::Newest(dir)) };
    match detect(root) {
        BuildSystem::CMake => all.push(build("CMake", "cmake -B build (Debug)", CMAKE_BUILD, root.join("build"))),
        BuildSystem::Make => all.push(build("Make", "make", "make", root.to_path_buf())),
        BuildSystem::Cargo => all.push(Target { label: "Cargo".into(), detail: "cargo build".into(), plan: Plan::native(Some("cargo build --message-format=json-render-diagnostics".into()), Program::FromCargo) }),
        _ => {}
    }
    if let Some((file, lsp_id)) = file {
        if let Some(plan) = adapters::plan(root, file, lsp_id) {
            let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            all.push(Target { label: name, detail: crate::i18n::t("Current file").into(), plan });
        }
    }
    let mut unique: Vec<Target> = vec![];
    for t in all {
        if !unique.iter().any(|u| u.plan == t.plan) {
            unique.push(t);
        }
    }
    expand_programs(unique)
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
                    let name = exe.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
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
        return Some(CMAKE_BUILD.into());
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
            let program = match (t["program"].as_str(), t["program_dir"].as_str()) {
                (Some(p), _) => Program::Path(absolute(root, p)),
                (None, Some(d)) => Program::Newest(root.join(d)),
                (None, None) => Program::Newest(root.join("build")),
            };
            let plan = Plan {
                args: t["args"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()),
                cwd: t["cwd"].as_str().map(|c| PathBuf::from(absolute(root, c))),
                env: env.clone(),
                ..Plan::native(t["build"].as_str().map(String::from), program)
            };
            Some(Target { label: t["label"].as_str()?.to_string(), detail: t["detail"].as_str().map_or_else(|| m.name.clone(), |d| format!("{} | {d}", m.name)), plan })
        })
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
        let dir = std::env::temp_dir().join(format!("forge-launch-{}-{}", std::process::id(), files.len()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
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
        assert_eq!(labels, ["Make > app", "Make > gtest_app"]);
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
        let out = "building\n@forge {\"action\":\"notify\"}\n@forge {\"action\":\"launch\",\"env\":{\"PATH\":\"/deps\"}}\n";
        assert_eq!(launch_override(out).unwrap()["env"]["PATH"], "/deps");
        let _ = std::fs::remove_dir_all(root);
    }
}
