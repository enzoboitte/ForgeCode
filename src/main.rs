//! ForgeCode - a GPUI code editor for C/C++, Rust, JS/TS and the web,
//! with language servers, GitHub Copilot, a local-model agent, a terminal, a debugger and modules.

// A window app on Windows: no console window behind it in release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ai;
mod debug;
mod dnd;
mod dotforge;
mod git;
mod i18n;
mod icons;
mod language;
mod launch;
mod lsp;
mod plugins;
mod project;
mod recent;
mod rpc;
mod settings;
mod toolchain;
mod util;
mod workspace;

use std::path::PathBuf;

use gpui_kit::component::TitleBar;
use gpui_kit::component::theme::ThemeRegistry;
use gpui_kit::*;

use ai::copilot::CopilotState;
use plugins::Plugins;
use settings::Settings;
use workspace::{Quit, Workspace};

/// Load modules and the themes they ship.
pub fn load_modules(cx: &mut App) {
    if let Err(e) = ThemeRegistry::global_mut(cx).load_themes_from_str(include_str!("themes/forgecode.json")) {
        eprintln!("bundled themes: {e:#}");
    }
    let market = crate::plugins::marketplace::marketplace_dir(&crate::settings::Settings::get(cx).marketplace_dir);
    if let Err(e) = crate::plugins::marketplace::seed(&market) {
        eprintln!("marketplace {}: {e}", market.display());
    }
    crate::plugins::marketplace::update_bundled(&market);
    let plugins = Plugins::load();
    for dir in plugins.theme_dirs() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                if let Ok(json) = std::fs::read_to_string(entry.path()) {
                    if let Err(e) = ThemeRegistry::global_mut(cx).load_themes_from_str(&json) {
                        eprintln!("theme {}: {e:#}", entry.path().display());
                    }
                }
            }
        }
    }
    cx.set_global(plugins);
    settings::apply_theme(cx);
}

/// The folder to open when nothing else says: where `forgecode` was typed in a terminal, but not
/// the app's own folder (a double click on the .exe / binary starts there) nor `/` (macOS apps).
fn launch_dir() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).and_then(|e| e.parent().map(PathBuf::from));
    if Some(&cwd) == exe_dir.as_ref() || cwd.parent().is_none() {
        return dirs::home_dir();
    }
    Some(cwd)
}

fn main() {
    // `forgecode --mcp-web`: the web tools server the agent starts (no window).
    if std::env::args().nth(1).as_deref() == Some(ai::web_mcp::FLAG) {
        return ai::web_mcp::serve();
    }
    #[cfg(unix)]
    util::adopt_login_path();
    // `forge [folder | file]`; without an argument, reopen the last project.
    let arg = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| recent::list().into_iter().next())
        .or_else(launch_dir)
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let (root, file) = match arg.is_file() {
        true => (arg.parent().map(PathBuf::from).unwrap_or_default(), Some(arg)),
        false => (arg, None),
    };

    // Builds, Conan, CMake, the terminal and clangd use the chosen compiler (MSVC found anywhere).
    toolchain::adopt(&Settings::load());

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        Settings::init(cx);
        CopilotState::init(cx);
        language::fix_sql_highlighting();
        language::fix_tsx_highlighting();
        language::fix_c_family_highlighting();
        load_modules(cx);
        // User themes: drop theme JSON files in <config>/forgecode/themes.
        let themes_dir = settings::config_dir().join("themes");
        let _ = std::fs::create_dir_all(&themes_dir);
        let _ = ThemeRegistry::watch_dir(themes_dir, cx, settings::apply_theme);

        cx.bind_keys(workspace::key_bindings());
        cx.on_action(|_: &Quit, cx| cx.quit());
        workspace::menus::install(cx);
        cx.activate(true);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1280.), px(800.)), cx)),
            // Our own VS Code-like title bar draws over a transparent native one.
            titlebar: Some(TitlebarOptions { title: Some("ForgeCode".into()), ..TitleBar::title_bar_options() }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| {
                let mut ws = Workspace::new(root, window, cx);
                if let Some(file) = file {
                    ws.open_file(file, None, window, cx);
                }
                ws
            })
        })
            .expect("failed to open window");
    });
}
