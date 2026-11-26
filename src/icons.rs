//! File and folder icons with language colors, used by the explorer and the editor tabs.

use std::path::Path;

use std::sync::{Arc, OnceLock};

use gpui_kit::assets::IconName;
use gpui_kit::{Hsla, Image, ImageFormat, rgb};

/// The ForgeCode flame without background (assets/logo-mark.svg), for the interface.
/// `logo.svg` (with its background) is the app icon.
pub fn logo() -> Arc<Image> {
    static LOGO: OnceLock<Arc<Image>> = OnceLock::new();
    LOGO.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Svg, include_bytes!("../assets/logo-mark.svg").to_vec()))).clone()
}

/// A Lucide icon by its name (`git-branch`, `bug`), as modules write it.
pub fn by_name(name: &str) -> Option<IconName> {
    let path = format!("icons/{name}.svg");
    IconName::ALL.iter().copied().find(|i| i.path() == path.as_str())
}

/// A module's icon: a Lucide name, or an image file inside the module; `fallback` otherwise.
pub fn module_icon(spec: Option<&str>, dir: &Path, fallback: IconName, size: gpui_kit::Pixels) -> gpui_kit::AnyElement {
    use gpui_kit::{IntoElement as _, ParentElement as _, Styled as _};
    let Some(spec) = spec.filter(|s| !s.is_empty()) else {
        return gpui_kit::div().child(fallback).into_any_element();
    };
    if let Some(icon) = by_name(spec) {
        return gpui_kit::div().child(icon).into_any_element();
    }
    let file = dir.join(spec);
    if file.is_file() {
        return gpui_kit::img(file).size(size).into_any_element();
    }
    gpui_kit::div().child(fallback).into_any_element()
}

fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}

/// Icon and tint for a file, by name then extension (case-insensitive).
pub fn file_icon(path: &Path) -> (IconName, Hsla) {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let by_name = match name.as_str() {
        "cargo.toml" | "cargo.lock" => Some((IconName::Package, 0xdea584)),
        "package.json" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" => Some((IconName::Package, 0xcb3837)),
        "makefile" | "gnumakefile" => Some((IconName::Hammer, 0xe37933)),
        "cmakelists.txt" => Some((IconName::Hammer, 0x4fa3e0)),
        "license" | "license.md" | "license.txt" | "copying" => Some((IconName::Scale, 0xcbcb41)),
        "readme.md" | "readme" => Some((IconName::BookOpen, 0x519aba)),
        ".gitignore" | ".gitattributes" | ".gitmodules" => Some((IconName::GitBranch, 0xe8704a)),
        ".editorconfig" | ".clang-format" | ".prettierrc" | ".eslintrc" => Some((IconName::Settings2, 0x6d8086)),
        n if n == "dockerfile" || n.starts_with("dockerfile.") || n.starts_with("docker-compose") || n == ".dockerignore" || n.starts_with("compose.") => {
            Some((IconName::Container, 0x2496ed))
        }
        n if n == ".env" || n.starts_with(".env.") => Some((IconName::KeyRound, 0xe5c07b)),
        _ => None,
    };
    let (icon, hex) = by_name.unwrap_or(match ext.as_str() {
        "rs" => (IconName::FileCode, 0xdea584),
        "c" => (IconName::FileCode, 0x599eff),
        "h" | "hpp" | "hh" | "hxx" => (IconName::FileCode, 0xa074c4),
        "cpp" | "cc" | "cxx" | "c++" => (IconName::FileCode, 0xf34b7d),
        "js" | "mjs" | "cjs" | "jsx" => (IconName::FileCode, 0xcbcb41),
        "ts" | "mts" | "cts" | "tsx" => (IconName::FileCode, 0x519aba),
        "vue" => (IconName::FileCode, 0x41b883),
        "svelte" => (IconName::FileCode, 0xff3e00),
        "py" => (IconName::FileCode, 0x3572a5),
        "go" => (IconName::FileCode, 0x00add8),
        "java" | "kt" | "kts" | "scala" | "gradle" => (IconName::Coffee, 0xb07219),
        "swift" => (IconName::FileCode, 0xf05138),
        "rb" => (IconName::Gem, 0xcc342d),
        "php" => (IconName::FileCode, 0x8892bf),
        "lua" => (IconName::FileCode, 0x51a0cf),
        "zig" => (IconName::FileCode, 0xf7a41d),
        "cs" => (IconName::FileCode, 0x68217a),
        "html" | "htm" => (IconName::FileCode, 0xe44d26),
        "css" | "scss" | "sass" | "less" => (IconName::Palette, 0x42a5f5),
        "json" | "jsonc" | "json5" => (IconName::FileBraces, 0xcbcb41),
        "xml" | "plist" | "xaml" => (IconName::FileCode, 0xe37933),
        "sql" | "sqlite" | "db" | "prisma" => (IconName::Database, 0xdad8d8),
        "graphql" | "gql" => (IconName::Waypoints, 0xe535ab),
        "proto" => (IconName::FileBraces, 0x6d8086),
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => (IconName::FileCog, 0x6d8086),
        "mk" | "make" => (IconName::Hammer, 0xe37933),
        "cmake" => (IconName::Hammer, 0x4fa3e0),
        "md" | "mdx" | "rst" | "adoc" => (IconName::FileText, 0x519aba),
        "txt" | "text" => (IconName::FileText, 0x8b93a1),
        "log" => (IconName::ScrollText, 0x8b93a1),
        "csv" | "tsv" | "xlsx" | "xls" => (IconName::Sheet, 0x89e051),
        "pdf" => (IconName::FileText, 0xe5534b),
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" => (IconName::FileTerminal, 0x89e051),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" => (IconName::FileImage, 0xa074c4),
        "mp3" | "wav" | "flac" | "ogg" => (IconName::FileMusic, 0xa074c4),
        "mp4" | "mov" | "mkv" | "webm" => (IconName::FileVideoCamera, 0xa074c4),
        "ttf" | "otf" | "woff" | "woff2" => (IconName::Type, 0x8b93a1),
        "lock" => (IconName::FileLock, 0x6d8086),
        "zip" | "gz" | "tar" | "xz" | "7z" | "rar" => (IconName::FileArchive, 0xafb42b),
        "wasm" | "o" | "a" | "so" | "dylib" | "dll" | "exe" | "out" | "bin" => (IconName::Binary, 0x6d8086),
        "key" | "pem" | "crt" | "p12" => (IconName::KeyRound, 0xe5c07b),
        _ => (IconName::File, 0x8b93a1),
    });
    (icon, color(hex))
}

/// Language tag for a Markdown code fence.
pub fn fence_language(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "py" => "python",
        "html" => "html",
        "css" => "css",
        "json" => "json",
        "toml" => "toml",
        "md" => "markdown",
        "sh" | "bash" | "zsh" => "bash",
        _ => "",
    }
}

/// Icon and tint for a folder; a few well-known names get their own.
pub fn folder_icon(path: &Path, open: bool) -> (IconName, Hsla) {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let tint = color(0x90a4ae);
    match name {
        ".git" => (IconName::FolderGit2, color(0xe8704a)),
        "src" | "lib" => (IconName::FolderCode, color(0x4fa3e0)),
        "test" | "tests" | "__tests__" => (IconName::FolderCheck, color(0x89e051)),
        "target" | "build" | "dist" | "out" | "node_modules" => (IconName::FolderCog, color(0x6d8086)),
        ".vscode" | ".github" | ".idea" => (IconName::FolderCog, tint),
        _ if open => (IconName::FolderOpen, tint),
        _ => (IconName::Folder, tint),
    }
}

#[cfg(test)]
mod name_tests {
    #[test]
    fn finds_lucide_icons_by_name() {
        assert_eq!(super::by_name("git-branch"), Some(gpui_kit::assets::IconName::GitBranch));
        assert_eq!(super::by_name("not-an-icon"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_icons_by_name_then_extension() {
        assert!(matches!(file_icon(Path::new("a/Cargo.toml")).0, IconName::Package));
        assert!(matches!(file_icon(Path::new("schema.SQL")).0, IconName::Database));
        assert!(matches!(file_icon(Path::new("Dockerfile.dev")).0, IconName::Container));
        assert!(matches!(file_icon(Path::new("docker-compose.yml")).0, IconName::Container));
        assert!(matches!(file_icon(Path::new("MAKEFILE")).0, IconName::Hammer));
        assert!(matches!(file_icon(Path::new("CMakeLists.txt")).0, IconName::Hammer));
        assert!(matches!(file_icon(Path::new("rules.mk")).0, IconName::Hammer));
        assert!(matches!(file_icon(Path::new(".env.local")).0, IconName::KeyRound));
        assert!(matches!(file_icon(Path::new("main.rs")).0, IconName::FileCode));
        assert!(matches!(file_icon(Path::new("README.md")).0, IconName::BookOpen));
        assert!(matches!(file_icon(Path::new("notes.md")).0, IconName::FileText));
        assert!(matches!(folder_icon(Path::new("p/.git"), false).0, IconName::FolderGit2));
        assert!(matches!(folder_icon(Path::new("p/other"), true).0, IconName::FolderOpen));
    }
}
