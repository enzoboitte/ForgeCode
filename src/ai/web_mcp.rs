//! Web access for the agent: `forgecode --mcp-web` is a small MCP server (stdio, one JSON-RPC
//! message per line) offering web search, page reading and a headless browser (Chrome, Edge,
//! Brave or Chromium already installed) to look at pages, with no API key. The agent panel
//! hands it to every conversation, and [`GUIDANCE`] tells the model to use it rather than guess.

use std::io::{BufRead as _, Write as _};

use serde_json::{Value, json};

/// The argument that makes the ForgeCode binary run as this server.
pub const FLAG: &str = "--mcp-web";

/// Standing instruction given to the agent (written in its persona file).
pub const GUIDANCE: &str = "You have Internet access: the `web_search` and `web_fetch` tools of the `forgecode-web` server (and any other web tool you have). \
Whenever you need information you do not know for certain — a library's or API's documentation, a version, an error message, how a tool is installed or configured, anything recent — you MUST search the web and read the relevant page BEFORE answering or writing code. \
Never guess, never invent an API, an option or a URL. When the search finds nothing, say so. Give the URLs you relied on. \
After changing a web page or app, look at it: `browser_screenshot` shows you the page as rendered (with its console messages), `browser_dom` gives the HTML after its scripts ran. Check the layout and the console before saying it works.";

/// The `mcpServers` entry for `session/new`: this very binary, as a web server.
pub fn server_config() -> Option<Value> {
    let exe = std::env::current_exe().ok()?;
    Some(json!({"name": "forgecode-web", "command": exe.display().to_string(), "args": [FLAG], "env": []}))
}

fn tools() -> Value {
    json!([
        {
            "name": "web_search",
            "description": "Search the web. Returns the first results (title, URL, excerpt). Use it whenever you are not certain of a fact, an API, a version or an error's cause; then read the best result with web_fetch.",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string", "description": "The search query"}}, "required": ["query"]}
        },
        {
            "name": "web_fetch",
            "description": "Read a web page (http or https) as plain text: documentation, an issue, an article.",
            "inputSchema": {"type": "object", "properties": {"url": {"type": "string", "description": "The page's URL"}}, "required": ["url"]}
        },
        {
            "name": "browser_screenshot",
            "description": "Open a page in a headless browser (scripts run) and return a screenshot plus the console messages. Use it to check visually a site or app you are building (e.g. http://localhost:3000) or any web page.",
            "inputSchema": {"type": "object", "properties": {
                "url": {"type": "string", "description": "http(s):// or file:// URL"},
                "width": {"type": "integer", "description": "Viewport width in pixels (default 1280; 390 for a phone)"},
                "height": {"type": "integer", "description": "Viewport height in pixels (default 800)"}
            }, "required": ["url"]}
        },
        {
            "name": "browser_dom",
            "description": "Open a page in a headless browser and return its HTML after the scripts ran, plus the console messages.",
            "inputSchema": {"type": "object", "properties": {"url": {"type": "string", "description": "http(s):// or file:// URL"}}, "required": ["url"]}
        }
    ])
}

/// Longest text handed back for one page (characters).
const PAGE_LIMIT: usize = 24_000;

fn cut(text: String) -> String {
    match text.char_indices().nth(PAGE_LIMIT) {
        Some((cut, _)) => format!("{}\n… (page cut at {PAGE_LIMIT} characters)", &text[..cut]),
        None => text,
    }
}

fn text(t: String) -> Vec<Value> {
    vec![json!({"type": "text", "text": t})]
}

/// A tool's result: MCP content blocks (text, image).
fn call(name: &str, args: &Value) -> Result<Vec<Value>, String> {
    match name {
        "web_search" => {
            let query = args["query"].as_str().filter(|q| !q.trim().is_empty()).ok_or("missing `query`")?;
            super::web::search(query).map(text).map_err(|e| e.to_string())
        }
        "web_fetch" => {
            let url = args["url"].as_str().ok_or("missing `url`")?;
            super::web::fetch(url).map(|t| text(cut(t))).map_err(|e| e.to_string())
        }
        "browser_screenshot" => {
            let url = args["url"].as_str().ok_or("missing `url`")?;
            let size = |k: &str, d: u64| args[k].as_u64().unwrap_or(d).clamp(200, 4000);
            let (w, h) = (size("width", 1280), size("height", 800));
            let out = std::env::temp_dir().join(format!("forgecode-shot-{}.png", std::process::id()));
            let (_, console) = browser(url, &[format!("--window-size={w},{h}"), format!("--screenshot={}", out.display())])?;
            let png = std::fs::read(&out).map_err(|e| format!("no screenshot: {e}\n{console}"))?;
            let _ = std::fs::remove_file(&out);
            Ok(vec![
                json!({"type": "image", "mimeType": "image/png", "data": crate::util::base64(&png)}),
                json!({"type": "text", "text": format!("{url} at {w}×{h}.\n{console}")}),
            ])
        }
        "browser_dom" => {
            let url = args["url"].as_str().ok_or("missing `url`")?;
            let (dom, console) = browser(url, &["--dump-dom".into()])?;
            Ok(text(format!("{}\n\n{console}", cut(dom))))
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

/// An installed Chromium-based browser.
fn chrome() -> Option<std::path::PathBuf> {
    let apps = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ];
    let names = ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge", "brave-browser", "chrome"];
    apps.iter().map(std::path::PathBuf::from).find(|p| p.exists()).or_else(|| {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
    }).or_else(playwright)
}

/// The headless Chromium Playwright downloads (`npx playwright install chromium`), newest first.
fn playwright() -> Option<std::path::PathBuf> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
    let caches = [home.join("Library/Caches/ms-playwright"), home.join(".cache/ms-playwright")];
    let mut found: Vec<_> = caches
        .iter()
        .filter_map(|c| std::fs::read_dir(c).ok())
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("chromium"))
        .flat_map(|e| std::fs::read_dir(e.path()).into_iter().flatten().flatten())
        .flat_map(|d| ["chrome-headless-shell", "chrome", "Chromium.app/Contents/MacOS/Chromium"].map(|n| d.path().join(n)))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found.pop()
}

/// Load `url` headless with `extra` flags: (stdout, console messages).
fn browser(url: &str, extra: &[String]) -> Result<(String, String), String> {
    if !["http://", "https://", "file://"].iter().any(|s| url.starts_with(s)) {
        return Err("the URL must start with http://, https:// or file://".into());
    }
    let exe = chrome().ok_or("no Chrome, Edge, Brave or Chromium found on this computer")?;
    let profile = std::env::temp_dir().join(format!("forgecode-browser-{}", std::process::id()));
    let mut child = std::process::Command::new(exe)
        .args(["--headless=new", "--disable-gpu", "--hide-scrollbars", "--no-first-run", "--no-default-browser-check", "--virtual-time-budget=8000", "--enable-logging=stderr", "--v=0"])
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(extra)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let read = |r: &mut dyn std::io::Read| {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    };
    let stdout = std::thread::spawn(move || read(&mut out));
    let stderr = std::thread::spawn(move || read(&mut err));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = std::fs::remove_dir_all(&profile);
    let (stdout, stderr) = (stdout.join().unwrap_or_default(), stderr.join().unwrap_or_default());
    Ok((stdout, console(&stderr)))
}

/// The page's console messages out of the browser's log.
fn console(log: &str) -> String {
    let lines: Vec<&str> = log.lines().filter(|l| l.contains(":CONSOLE")).map(|l| l.split_once("] ").map_or(l, |(_, m)| m)).take(60).collect();
    if lines.is_empty() { "Console: no messages.".into() } else { format!("Console:\n{}", lines.join("\n")) }
}

/// Answer to one message, or `None` for a notification.
pub fn handle(msg: &Value, run: &dyn Fn(&str, &Value) -> Result<Vec<Value>, String>) -> Option<Value> {
    let id = msg.get("id").filter(|id| !id.is_null())?.clone();
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "forgecode-web", "version": env!("CARGO_PKG_VERSION")},
            "instructions": GUIDANCE,
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let (content, failed) = match run(msg["params"]["name"].as_str().unwrap_or(""), &msg["params"]["arguments"]) {
                Ok(content) => (content, false),
                Err(e) => (text(e), true),
            };
            json!({"content": content, "isError": failed})
        }
        other => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {other}")}})),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serve on stdin/stdout until the agent closes the pipe.
pub fn serve() {
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(answer) = handle(&msg, &call) {
            let mut out = stdout.lock();
            if writeln!(out, "{answer}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{console, handle, text};
    use serde_json::json;

    #[test]
    fn speaks_mcp() {
        let run = |name: &str, args: &serde_json::Value| match name {
            "web_search" => Ok(text(format!("results for {}", args["query"].as_str().unwrap_or("")))),
            _ => Err("boom".to_string()),
        };
        let init = handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}), &run).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(init["result"]["instructions"].as_str().unwrap().contains("MUST search the web"));
        // Notifications get no answer.
        assert!(handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), &run).is_none());
        let list = handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}), &run).unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["web_search", "web_fetch", "browser_screenshot", "browser_dom"]);
        let found = handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "web_search", "arguments": {"query": "gpui list"}}}), &run).unwrap();
        assert_eq!(found["result"]["content"][0]["text"], "results for gpui list");
        assert_eq!(found["result"]["isError"], false);
        let failed = handle(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "web_fetch", "arguments": {}}}), &run).unwrap();
        assert_eq!(failed["result"]["isError"], true);
        assert_eq!(handle(&json!({"jsonrpc": "2.0", "id": 5, "method": "nope"}), &run).unwrap()["error"]["code"], -32601);
    }

    #[test]
    fn keeps_only_console_messages() {
        let log = "[1113/101010.1:INFO:CONSOLE(3)] \"Uncaught TypeError: x is null\", source: http://localhost:3000/app.js (3)\n[1113/101010.2:WARNING:gpu_init.cc(1)] noise";
        assert_eq!(console(log), "Console:\n\"Uncaught TypeError: x is null\", source: http://localhost:3000/app.js (3)");
        assert_eq!(console("noise"), "Console: no messages.");
    }

    /// Needs a Chromium browser installed: `cargo test real_browser -- --ignored`.
    #[test]
    #[ignore]
    fn real_browser_takes_a_screenshot() {
        let out = super::call("browser_screenshot", &json!({"url": "data:text/html,<h1>hi</h1><script>console.log('ok')</script>"}));
        assert!(out.is_err(), "data: URLs are refused");
        let dir = std::env::temp_dir().join("forgecode-browser-test.html");
        std::fs::write(&dir, "<h1>hi</h1><script>console.log('ready')</script>").unwrap();
        let out = super::call("browser_screenshot", &json!({"url": format!("file://{}", dir.display())})).unwrap();
        assert_eq!(out[0]["type"], "image");
        assert!(out[1]["text"].as_str().unwrap().contains("ready"), "{}", out[1]);
    }
}
