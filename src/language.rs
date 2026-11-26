//! Per-language knowledge: highlighter name, language server and run command.

use std::path::Path;

use crate::plugins::Plugins;

#[derive(Clone, Debug, PartialEq)]
pub struct LanguageInfo {
    /// Tree-sitter language name used by the editor highlighter.
    pub highlight: String,
    /// LSP `languageId`.
    pub lsp_id: String,
    /// Language server command line; empty = none.
    pub server: Vec<String>,
    /// Command to run the current file. `${file}`, `${dir}`, `${stem}` are substituted.
    pub run: Option<String>,
}

fn info(highlight: &str, lsp_id: &str, server: &[&str], run: Option<&str>) -> LanguageInfo {
    LanguageInfo {
        highlight: highlight.into(),
        lsp_id: lsp_id.into(),
        server: server.iter().map(|s| s.to_string()).collect(),
        run: run.map(Into::into),
    }
}

pub fn detect(path: &Path, plugins: &Plugins) -> LanguageInfo {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if let Some(lang) = plugins.language_for_extension(&ext) {
        return lang;
    }
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let html_ls = ["vscode-html-language-server", "--stdio"];
    let css_ls = ["vscode-css-language-server", "--stdio"];
    let ts_ls = ["typescript-language-server", "--stdio"];
    match ext.as_str() {
        "rs" => info("rust", "rust", &["rust-analyzer"], Some("cargo run")),
        "c" => info("c", "c", &CLANGD, Some("cc ${includes} \"${file}\" -o \"${out}\" && \"${out}\"")),
        "h" => info("c", "c", &CLANGD, None),
        "cpp" | "cc" | "cxx" | "c++" => info(
            "cpp",
            "cpp",
            &CLANGD,
            Some("c++ -std=c++20 ${includes} \"${file}\" -o \"${out}\" && \"${out}\""),
        ),
        "hpp" | "hh" | "hxx" => info("cpp", "cpp", &CLANGD, None),
        "js" | "mjs" | "cjs" => info("javascript", "javascript", &ts_ls, Some("node \"${file}\"")),
        "jsx" => info("javascript", "javascriptreact", &ts_ls, None),
        "ts" | "mts" | "cts" => info("typescript", "typescript", &ts_ls, Some("npx tsx \"${file}\"")),
        "tsx" => info("tsx", "typescriptreact", &ts_ls, None),
        "html" | "htm" => info("html", "html", &html_ls, Some(OPEN_FILE)),
        "css" => info("css", "css", &css_ls, None),
        // shortcut: SCSS/Less use the CSS grammar (none bundled for them); the server knows both.
        "scss" | "sass" => info("css", "scss", &css_ls, None),
        "less" => info("css", "less", &css_ls, None),
        "php" | "phtml" => info("php", "php", &["intelephense", "--stdio"], Some("php \"${file}\"")),
        "json" => info("json", "json", &["vscode-json-language-server", "--stdio"], None),
        // JSON with comments: tsconfig, VS Code settings…
        "jsonc" | "json5" => info("json", "jsonc", &["vscode-json-language-server", "--stdio"], None),
        "py" | "pyw" => info("python", "python", &["pyright-langserver", "--stdio"], Some(PYTHON)),
        "yml" | "yaml" => info("yaml", "yaml", &["yaml-language-server", "--stdio"], None),
        "svelte" => info("svelte", "svelte", &["svelteserver", "--stdio"], None),
        "go" => info("go", "go", &["gopls"], Some("go run \"${file}\"")),
        "java" => info("java", "java", &["jdtls"], Some("java \"${file}\"")),
        "rb" => info("ruby", "ruby", &["ruby-lsp"], Some("ruby \"${file}\"")),
        "lua" => info("lua", "lua", &["lua-language-server"], Some("lua \"${file}\"")),
        "kt" | "kts" => info("kotlin", "kotlin", &["kotlin-lsp", "--stdio"], None),
        "cs" => info("csharp", "csharp", &["csharp-ls"], None),
        "ex" | "exs" => info("elixir", "elixir", &[], None),
        "gql" | "graphql" => info("graphql", "graphql", &[], None),
        "toml" => info("toml", "toml", &["taplo", "lsp", "stdio"], None),
        "md" => info("markdown", "markdown", &[], None),
        "sh" | "bash" | "zsh" => info("bash", "shellscript", &["bash-language-server", "start"], Some("bash \"${file}\"")),
        "mk" => info("make", "makefile", &[], Some("make")),
        "cmake" => info("cmake", "cmake", &[], None),
        _ if matches!(file_name, "Makefile" | "makefile" | "GNUmakefile") => info("make", "makefile", &[], Some("make")),
        _ if file_name == "CMakeLists.txt" => info("cmake", "cmake", &[], None),
        // Let any other tree-sitter grammar bundled with the editor pick it up by extension.
        other => info(other, other, &[], None),
    }
}

/// Like VS Code's C/C++ setup: index the whole project (cross-file errors and navigation),
/// clang-tidy (the linter) and `#include` added when a completion needs it.
const CLANGD: [&str; 5] = ["clangd", "--background-index", "--clang-tidy", "--header-insertion=iwyu", "--completion-style=detailed"];

#[cfg(windows)]
const PYTHON: &str = "python \"${file}\"";
#[cfg(not(windows))]
const PYTHON: &str = "python3 \"${file}\"";

/// The project's Python virtualenv (`.venv`, `venv`, `env`), the directory holding `bin/python`.
pub fn project_venv(root: &Path) -> Option<std::path::PathBuf> {
    project_venvs(root).into_iter().next()
}

/// Every virtualenv of the project: the usual names at the root first, then any folder holding a
/// `pyvenv.cfg` up to three levels down (`backend/.venv`, `api/env`, `myenv`…).
pub fn project_venvs(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found: Vec<std::path::PathBuf> = [".venv", "venv", "env"].iter().map(|d| root.join(d)).filter(|d| venv_python(d).is_file()).collect();
    let mut deeper = vec![];
    find_venvs(root, 3, &mut deeper);
    deeper.sort_by_key(|d| (d.components().count(), d.clone()));
    for d in deeper {
        if !found.contains(&d) {
            found.push(d);
        }
    }
    found
}

/// Folders never searched for project tools: dependencies and build output.
pub const SKIPPED_DIRS: &[&str] = &["node_modules", ".git", "target", "dist", "build", "__pycache__", ".idea", ".vscode", "vendor"];

fn find_venvs(dir: &Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if path.join("pyvenv.cfg").is_file() {
            if venv_python(&path).is_file() {
                out.push(path);
            }
            continue; // nothing to find inside an environment
        }
        if depth > 1 && !SKIPPED_DIRS.contains(&name.as_str()) {
            find_venvs(&path, depth - 1, out);
        }
    }
}

pub fn venv_python(venv: &Path) -> std::path::PathBuf {
    if cfg!(windows) { venv.join("Scripts").join("python.exe") } else { venv.join("bin").join("python") }
}

/// The Python a project runs with: the configured one, else its virtualenv's.
pub fn project_python(settings: &crate::settings::LangSettings, root: &Path) -> Option<std::path::PathBuf> {
    if !settings.runtime.is_empty() {
        return Some(settings.runtime.clone().into());
    }
    project_venv(root).map(|v| venv_python(&v))
}

/// Environment that activates the project's virtualenv, like `source .venv/bin/activate`.
pub fn venv_env(root: &Path) -> Vec<(String, String)> {
    let Some(venv) = project_venv(root) else { return vec![] };
    let bin = venv_python(&venv).parent().map(Path::to_path_buf).unwrap_or_default();
    vec![("VIRTUAL_ENV".into(), venv.display().to_string()), ("PATH".into(), crate::util::path_with(vec![bin]))]
}

/// Header directories of a C/C++ project without a compilation database: its own `include`,
/// `src`, vendored libraries' `include`, vcpkg's, and the package managers' system prefixes.
pub fn c_include_dirs(root: &Path) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![];
    for name in ["include", "inc", "src", "lib", "libs", "third_party", "thirdparty", "external", "extern", "vendor", "deps"] {
        let dir = root.join(name);
        if !dir.is_dir() {
            continue;
        }
        dirs.push(dir.clone());
        // Vendored libraries: third_party/<lib>/include.
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let include = entry.path().join("include");
            if include.is_dir() {
                dirs.push(include);
            }
        }
    }
    for entry in std::fs::read_dir(root.join("vcpkg_installed")).into_iter().flatten().flatten() {
        let include = entry.path().join("include");
        if include.is_dir() {
            dirs.push(include);
        }
    }
    dirs.extend(["/opt/homebrew/include", "/usr/local/include"].iter().map(std::path::PathBuf::from).filter(|d| d.is_dir()));
    dirs
}

/// `-I` flags for [`c_include_dirs`], quoted for a shell command line.
pub fn include_flags(root: &Path) -> String {
    c_include_dirs(root).iter().map(|d| format!("-I\"{}\"", d.display())).collect::<Vec<_>>().join(" ")
}

/// What language servers read through `workspace/configuration` (and `initializationOptions`):
/// the type checking level of each language in each server's own terms, the project's Python
/// interpreter, and clangd's flags for projects without `compile_commands.json`.
pub fn server_settings(settings: &crate::settings::Settings, root: &Path) -> serde_json::Value {
    let level = |lang: &str| settings.lang(lang).type_checking;
    let python = project_python(&settings.lang("python"), root).map(|p| p.display().to_string()).unwrap_or_default();
    let fallback: Vec<String> = c_include_dirs(root).iter().map(|d| format!("-I{}", d.display())).collect();
    let php = level("php");
    let php_types = !matches!(php.as_str(), "off" | "basic");
    let (js, ts) = (level("javascript"), level("typescript"));
    serde_json::json!({
        // pyright: off | basic | standard | strict.
        "python": {
            "analysis": {
                "typeCheckingMode": level("python"),
                "autoSearchPaths": true,
                "useLibraryCodeForTypes": true,
                // Like Pylance: suggest names from other project files and libraries, adding their import.
                "autoImportCompletions": true,
                "indexing": true,
                "diagnosticMode": "workspace",
            },
            // pyright resolves imports from this interpreter: the project's virtualenv libraries.
            "pythonPath": python,
        },
        "intelephense": {"diagnostics": {
            "typeErrors": php_types,
            "undefinedTypes": php != "off",
            "undefinedFunctions": php != "off",
            "undefinedMethods": php_types,
            "undefinedProperties": php_types,
            "undefinedVariables": php != "off",
            "argumentCount": php != "off",
        }},
        // rust-analyzer: clippy's lints on save, like VS Code's usual setup.
        "rust-analyzer": {"check": {"command": if crate::lsp::install::which("cargo-clippy").is_some() { "clippy" } else { "check" }}},
        // gopls: staticcheck's extra analyses.
        "gopls": {"staticcheck": true},
        // clangd: include paths for files no compile_commands.json describes.
        "fallbackFlags": fallback,
        // vscode-json-language-server: formatting, plus validation against SchemaStore schemas.
        "provideFormatter": true,
        "json": {
            "validate": {"enable": true},
            "format": {"enable": true},
            "schemas": [
                {"fileMatch": ["package.json"], "url": "https://json.schemastore.org/package.json"},
                {"fileMatch": ["tsconfig.json", "tsconfig.*.json"], "url": "https://json.schemastore.org/tsconfig.json"},
                {"fileMatch": ["jsconfig.json"], "url": "https://json.schemastore.org/jsconfig.json"},
                {"fileMatch": ["composer.json"], "url": "https://getcomposer.org/schema.json"},
                {"fileMatch": [".eslintrc", ".eslintrc.json"], "url": "https://json.schemastore.org/eslintrc.json"},
                {"fileMatch": [".prettierrc", ".prettierrc.json"], "url": "https://json.schemastore.org/prettierrc.json"},
                {"fileMatch": [".babelrc", "babel.config.json"], "url": "https://json.schemastore.org/babelrc.json"},
                {"fileMatch": ["*.code-workspace", ".vscode/settings.json"], "url": "https://json.schemastore.org/vscode-settings.json"},
            ],
        },
        // typescript-language-server, for JS/TS files outside a tsconfig/jsconfig project.
        "implicitProjectConfiguration": {
            "checkJs": js != "off",
            "strictNullChecks": matches!(ts.as_str(), "standard" | "strict"),
            "strictFunctionTypes": ts == "strict",
        },
    })
}

/// Opens `${file}` with the system's default app.
#[cfg(target_os = "macos")]
const OPEN_FILE: &str = "open \"${file}\"";
#[cfg(windows)]
const OPEN_FILE: &str = "start \"\" \"${file}\"";
#[cfg(not(any(target_os = "macos", windows)))]
const OPEN_FILE: &str = "xdg-open \"${file}\"";

/// Expand `${file}`, `${dir}`, `${stem}`, `${workspace}` and `${out}` (an executable named after
/// the file in the temp dir) placeholders.
pub fn expand(template: &str, file: Option<&Path>, workspace: &Path) -> String {
    let file_s = file.map(|f| f.display().to_string()).unwrap_or_default();
    let dir = file.and_then(|f| f.parent()).map(|d| d.display().to_string()).unwrap_or_default();
    let stem = file.and_then(|f| f.file_stem()).map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let out = std::env::temp_dir().join(format!("{stem}{}", std::env::consts::EXE_SUFFIX));
    template
        .replace("${out}", &out.display().to_string())
        .replace("${file}", &file_s)
        .replace("${dir}", &dir)
        .replace("${stem}", &stem)
        .replace("${workspace}", &workspace.display().to_string())
        .replace("${includes}", &if template.contains("${includes}") { include_flags(workspace) } else { String::new() })
}

/// Patterns that must win over the bundled SQL query: the highlighter keeps the first capture of a
/// range, and upstream lists `@type` before function calls and `@string` before numbers (whose
/// Lua-style `%d` patterns never match anyway).
const SQL_OVERRIDES: &str = r#"
(invocation (object_reference name: (identifier) @function))
((literal) @number (#match? @number "^[-+]?[0-9]+(\\.[0-9]*)?$"))
"#;

/// The SQL highlight query with its captures mapped to the names the editor themes know.
pub fn sql_highlights(upstream: &str) -> String {
    let renamed = [
        ("@function.call", "@function"),
        ("@field", "@property"),
        ("@parameter", "@variable"),
        ("@conditional", "@keyword"),
        ("@storageclass", "@keyword"),
        ("@type.qualifier", "@keyword"),
        ("@float", "@number"),
        (" @spell", ""),
    ]
    .iter()
    .fold(upstream.to_string(), |q, (from, to)| q.replace(from, to));
    format!("{SQL_OVERRIDES}{renamed}")
}

/// Re-register the bundled SQL grammar with the corrected query.
pub fn fix_sql_highlighting() {
    use gpui_kit::component::highlighter::LanguageRegistry;
    let registry = LanguageRegistry::singleton();
    if let Some(mut config) = registry.language("sql") {
        config.highlights = sql_highlights(&config.highlights).into();
        registry.register("sql", &config);
    }
}

/// JSX elements, on top of the JavaScript/TypeScript queries (which don't cover them).
const JSX_HIGHLIGHTS: &str = r#"
(jsx_opening_element name: (identifier) @tag)
(jsx_closing_element name: (identifier) @tag)
(jsx_self_closing_element name: (identifier) @tag)
(jsx_opening_element name: (member_expression) @tag)
(jsx_closing_element name: (member_expression) @tag)
(jsx_self_closing_element name: (member_expression) @tag)
(jsx_attribute (property_identifier) @attribute)
(jsx_text) @text
(jsx_opening_element ["<" ">"] @punctuation.bracket)
(jsx_expression ["{" "}"] @punctuation.special)
"#;

/// The bundled TSX grammar only gets tree-sitter-typescript's partial query (TS additions meant
/// to be layered over JavaScript): almost nothing is colored. Use the full TypeScript query + JSX.
pub fn fix_tsx_highlighting() {
    use gpui_kit::component::highlighter::LanguageRegistry;
    let registry = LanguageRegistry::singleton();
    let (Some(ts), Some(mut tsx)) = (registry.language("typescript"), registry.language("tsx")) else { return };
    tsx.highlights = format!("{}{JSX_HIGHLIGHTS}", ts.highlights).into();
    tsx.locals = ts.locals.clone();
    registry.register("tsx", &tsx);
    // `.jsx` files use the JavaScript grammar, which parses JSX but doesn't color it either.
    if let Some(mut js) = registry.language("javascript").filter(|js| !js.highlights.contains("jsx_opening_element")) {
        js.highlights = format!("{}{JSX_HIGHLIGHTS}", js.highlights).into();
        registry.register("javascript", &js);
    }
}

/// The bundled CMake grammar has no query at all, and upstream's relies on Neovim-only predicates.
const CMAKE_HIGHLIGHTS: &str = r#"
[(line_comment) (bracket_comment)] @comment
(variable_ref) @variable.special
[(quoted_argument) (bracket_argument)] @string
[(if) (elseif) (else) (endif) (foreach) (endforeach) (while) (endwhile)
 (function) (endfunction) (macro) (endmacro) (block) (endblock)] @keyword
(normal_command (identifier) @function)
((unquoted_argument) @number (#match? @number "^[0-9][0-9.]*$"))
((unquoted_argument) @constant (#match? @constant "^[A-Z@][A-Z0-9_]+$"))
["(" ")"] @punctuation.bracket
"#;

/// The bundled C++ grammar only gets tree-sitter-cpp's partial query (C++ additions meant to be
/// layered over C): no strings, comments, numbers or #include. Add the C query after it; CMake
/// gets a query of its own.
pub fn fix_c_family_highlighting() {
    use gpui_kit::component::highlighter::LanguageRegistry;
    let registry = LanguageRegistry::singleton();
    // `int`, `char`… are keywords to VS Code (blue), not types (teal): first, so it wins.
    const PRIMITIVES: &str = "(primitive_type) @type.builtin\n";
    if let Some(mut c) = registry.language("c").filter(|c| !c.highlights.starts_with(PRIMITIVES)) {
        c.highlights = format!("{PRIMITIVES}{}", c.highlights).into();
        registry.register("c", &c);
    }
    if let (Some(c), Some(mut cpp)) = (registry.language("c"), registry.language("cpp")) {
        if !cpp.highlights.contains(c.highlights.as_ref()) {
            cpp.highlights = format!("{PRIMITIVES}{}\n{}", cpp.highlights, c.highlights).into();
            registry.register("cpp", &cpp);
        }
    }
    if let Some(mut cmake) = registry.language("cmake") {
        cmake.highlights = CMAKE_HIGHLIGHTS.into();
        registry.register("cmake", &cmake);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (capture, text) pairs the registered query finds in `src`.
    fn captures(name: &str, src: &str) -> std::collections::BTreeSet<(String, String)> {
        use gpui_kit::component::highlighter::LanguageRegistry;
        use tree_sitter::StreamingIterator as _;
        let config = LanguageRegistry::singleton().language(name).unwrap();
        let lang = config.language.unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let query = tree_sitter::Query::new(&lang, &config.highlights).unwrap();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut found = std::collections::BTreeSet::new();
        let mut it = cursor.captures(&query, tree.root_node(), src.as_bytes());
        while let Some((m, i)) = it.next() {
            let c = m.captures[*i];
            found.insert((query.capture_names()[c.index as usize].to_string(), c.node.utf8_text(src.as_bytes()).unwrap().to_string()));
        }
        found
    }

    /// The default dark theme colors like VS Code's Dark Modern.
    #[test]
    fn languages_get_vs_code_colors() {
        use gpui_kit::component::highlighter::{HighlightTheme, SyntaxHighlighter};
        fix_c_family_highlighting();
        let themes: serde_json::Value = serde_json::from_str(include_str!("themes/forgecode.json")).unwrap();
        let dark = themes["themes"].as_array().unwrap().iter().find(|t| t["mode"] == "dark").unwrap();
        let theme: HighlightTheme = serde_json::from_value(serde_json::json!({"name": "d", "style": {"syntax": dark["highlight"]["syntax"]}})).unwrap();
        let check = |lang: &str, code: &str, want: &[(&str, &str)]| {
            let rope = gpui_kit::component::input::Rope::from(code);
            let mut h = SyntaxHighlighter::new(lang);
            h.update(None, &rope, None);
            let styles = h.styles(&(0..code.len()), &theme);
            for (word, want) in want {
                let at = code.find(word).unwrap();
                let c = styles.iter().find(|(r, _)| r.start <= at && at < r.end).and_then(|(_, s)| s.color).map(|c| c.to_rgb());
                let hex = c.map(|c| format!("#{:02X}{:02X}{:02X}", (c.r * 255.).round() as u8, (c.g * 255.).round() as u8, (c.b * 255.).round() as u8));
                assert_eq!(hex.as_deref(), Some(*want), "{lang}: {word}");
            }
        };
        check("cpp", "#include <map>\nint main() { if (x) return 1; float y = f(2); }\n",
            &[("#include", "#C586C0"), ("<map>", "#CE9178"), ("int", "#569CD6"), ("if", "#C586C0"), ("return", "#C586C0"), ("main", "#DCDCAA"), ("1;", "#B5CEA8")]);
        check("python", "import os\ndef go(a):\n    return 'x'  # c\n",
            &[("import", "#C586C0"), ("def", "#569CD6"), ("go", "#DCDCAA"), ("return", "#C586C0"), ("'x'", "#CE9178"), ("# c", "#6A9955")]);
        check("typescript", "import { a } from 'b';\nconst n: number = 2;\nclass K {}\n",
            &[("import", "#C586C0"), ("const", "#569CD6"), ("'b'", "#CE9178"), ("number", "#569CD6"), ("K", "#4EC9B0"), ("2", "#B5CEA8")]);
        check("rust", "fn main() { let s: String = f(); if s.is_empty() { return; } }\n",
            &[("fn", "#569CD6"), ("main", "#DCDCAA"), ("String", "#4EC9B0"), ("if", "#C586C0"), ("return", "#C586C0")]);
    }

    #[test]
    fn cpp_and_cmake_are_colored() {
        fix_c_family_highlighting();
        let cpp = captures("cpp", "#include <SFML/Graphics.hpp>\n// hi\nconstexpr float k = 32.f;\nint main() { auto s = \"x\"; }\n");
        let cmake = captures("cmake", "# Box2D\nset(CMAKE_CXX_STANDARD 17)\nFetchContent_Declare(box2d GIT_TAG v2.4.1 \"q\" ${CMAKE_COMMAND})\n");
        for (found, capture, text) in [
            (&cpp, "keyword", "constexpr"), (&cpp, "string", "<SFML/Graphics.hpp>"), (&cpp, "comment", "// hi"),
            (&cpp, "number", "32.f"), (&cpp, "string", "\"x\""), (&cpp, "keyword", "#include"),
            (&cmake, "comment", "# Box2D"), (&cmake, "function", "set"), (&cmake, "constant", "CMAKE_CXX_STANDARD"),
            (&cmake, "number", "17"), (&cmake, "string", "\"q\""), (&cmake, "variable.special", "${CMAKE_COMMAND}"),
        ] {
            assert!(found.iter().any(|(c, t)| c.starts_with(capture) && t == text), "{capture} {text}: {found:?}");
        }
    }

    #[test]
    fn tsx_highlight_query_is_valid() {
        use gpui_kit::component::highlighter::LanguageRegistry;
        fix_tsx_highlighting();
        let registry = LanguageRegistry::singleton();
        for name in ["tsx", "javascript"] {
            let config = registry.language(name).unwrap();
            let query = tree_sitter::Query::new(config.language.as_ref().unwrap(), &config.highlights);
            assert!(query.is_ok(), "{name}: {:?}", query.err());
            assert!(config.highlights.contains("@keyword") && config.highlights.contains("jsx_opening_element"));
        }
        // A real component gets keywords, strings and tags.
        let config = registry.language("tsx").unwrap();
        let lang = config.language.unwrap();
        let src = "import React from 'react';\nexport const App = (): JSX.Element => <div className=\"x\">hi</div>;\n";
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let query = tree_sitter::Query::new(&lang, &config.highlights).unwrap();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut found = std::collections::BTreeSet::new();
        use tree_sitter::StreamingIterator as _;
        let mut it = cursor.captures(&query, tree.root_node(), src.as_bytes());
        while let Some((m, i)) = it.next() {
            let c = m.captures[*i];
            found.insert((query.capture_names()[c.index as usize].to_string(), c.node.utf8_text(src.as_bytes()).unwrap().to_string()));
        }
        for (capture, text) in [("keyword", "import"), ("string", "'react'"), ("tag", "div"), ("attribute", "className")] {
            assert!(found.iter().any(|(c, t)| c.starts_with(capture) && t == text), "{capture} {text}: {found:?}");
        }
    }

    #[test]
    fn detects_core_languages() {
        let p = Plugins::default();
        assert_eq!(detect(Path::new("a/main.rs"), &p).server, vec!["rust-analyzer"]);
        assert_eq!(detect(Path::new("x.hpp"), &p).highlight, "cpp");
        assert_eq!(detect(Path::new("x.tsx"), &p).lsp_id, "typescriptreact");
        assert_eq!(detect(Path::new("x.php"), &p).server, vec!["intelephense", "--stdio"]);
        assert_eq!((detect(Path::new("x.scss"), &p).highlight.as_str(), detect(Path::new("x.scss"), &p).lsp_id.as_str()), ("css", "scss"));
        assert_eq!(detect(Path::new("x.py"), &p).highlight, "python");
    }

    #[test]
    fn sql_query_colors_functions_and_numbers() {
        let lang: tree_sitter::Language = tree_sitter_sequel::LANGUAGE.into();
        let query = tree_sitter::Query::new(&lang, &sql_highlights(tree_sitter_sequel::HIGHLIGHTS_QUERY)).unwrap();
        let src = "SELECT count(id), 'a' FROM users WHERE age > 18;";
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut first = std::collections::HashMap::new();
        use tree_sitter::StreamingIterator as _;
        let mut caps = cursor.captures(&query, tree.root_node(), src.as_bytes());
        while let Some((m, i)) = caps.next() {
            let c = m.captures[*i];
            first.entry(&src[c.node.byte_range()]).or_insert(query.capture_names()[c.index as usize]);
        }
        assert_eq!(first["count"], "function");
        assert_eq!(first["18"], "number");
        assert_eq!(first["'a'"], "string");
        assert_eq!(first["SELECT"], "keyword");
        assert!(query.capture_names().iter().all(|n| !n.contains("spell") && !n.contains("field")));
    }

    #[test]
    fn finds_project_venv_and_c_includes() {
        let root = std::env::temp_dir().join(format!("forgecode-libs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(project_venv(&root).is_none());
        let python = venv_python(&root.join(".venv"));
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, "").unwrap();
        assert_eq!(project_venv(&root), Some(root.join(".venv")));
        assert!(venv_env(&root)[1].1.starts_with(&python.parent().unwrap().display().to_string()), "venv bin first on PATH");
        let s = crate::settings::Settings::default();
        assert_eq!(server_settings(&s, &root)["python"]["pythonPath"], python.display().to_string());

        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::create_dir_all(root.join("third_party/json/include")).unwrap();
        let dirs = c_include_dirs(&root);
        assert!(dirs.contains(&root.join("include")) && dirs.contains(&root.join("third_party/json/include")));
        let flags = server_settings(&s, &root)["fallbackFlags"].to_string();
        assert!(flags.contains("third_party/json/include"));
        assert!(expand("cc ${includes} x.c", None, &root).contains("-I"));
        // An environment in a sub-folder is found too.
        let nested = root.join("backend").join(".venv");
        std::fs::create_dir_all(venv_python(&nested).parent().unwrap()).unwrap();
        std::fs::write(venv_python(&nested), "").unwrap();
        std::fs::write(nested.join("pyvenv.cfg"), "").unwrap();
        assert!(project_venvs(&root).contains(&nested));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_placeholders() {
        let s = expand("cc ${file} -o ${stem} ${out}", Some(Path::new("/w/a.c")), Path::new("/w"));
        let out = std::env::temp_dir().join(format!("a{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(s, format!("cc /w/a.c -o a {}", out.display()));
    }
}

