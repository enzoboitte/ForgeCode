//! `Content-Length` framed JSON messages over a child process' stdio.
//! Shared by the LSP client and the DAP (debugger) client.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};

use anyhow::{Context as _, Result, anyhow};
use serde_json::Value;

pub fn write_message(w: &mut impl Write, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one framed message; `Ok(None)` at end of stream.
pub fn read_message(r: &mut impl BufRead) -> Result<Option<Value>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = Some(v.trim().parse::<usize>()?);
        }
    }
    let len = len.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

/// Spawn `argv` and call `on_message` for each message it prints, on a reader thread.
pub fn spawn(argv: &[String], cwd: &std::path::Path, on_message: impl FnMut(Value) + Send + 'static) -> Result<(Child, ChildStdin)> {
    spawn_with_exit(argv, cwd, on_message, |_| {})
}

/// Like [`spawn`], then `on_exit` with the end of the process's stderr once its output closes
/// (it exited or crashed), so callers can fail what is still waiting and say why.
pub fn spawn_with_exit(
    argv: &[String],
    cwd: &std::path::Path,
    mut on_message: impl FnMut(Value) + Send + 'static,
    on_exit: impl FnOnce(String) + Send + 'static,
) -> Result<(Child, ChildStdin)> {
    let (program, args) = argv.split_first().ok_or_else(|| anyhow!("empty command"))?;
    let mut child = crate::util::command(program)
        .args(args)
        .current_dir(cwd)
        // npm-installed servers start with `#!/usr/bin/env node`: they need ForgeCode's own Node.
        .env("PATH", crate::lsp::install::search_path())
        // .NET tools (csharp-ls) find ForgeCode's own SDK.
        .envs(crate::lsp::install::dotnet_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start `{program}`"))?;
    let stdin = child.stdin.take().context("no stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("no stdout")?);
    // Keep the last stderr lines (servers log there; a crash explains itself there).
    let tail = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::<String>::new()));
    if let Some(stderr) = child.stderr.take() {
        let tail = tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut t = tail.lock().unwrap();
                t.push_back(line);
                if t.len() > 20 {
                    t.pop_front();
                }
            }
        });
    }
    std::thread::spawn(move || {
        while let Ok(Some(msg)) = read_message(&mut stdout) {
            on_message(msg);
        }
        // Let the stderr reader catch the last lines of a crash.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let tail = tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
        on_exit(tail);
    });
    Ok((child, stdin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        write_message(&mut buf, &serde_json::json!({"a": "é"})).unwrap();
        write_message(&mut buf, &serde_json::json!(1)).unwrap();
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(read_message(&mut r).unwrap(), Some(serde_json::json!({"a": "é"})));
        assert_eq!(read_message(&mut r).unwrap(), Some(serde_json::json!(1)));
        assert_eq!(read_message(&mut r).unwrap(), None);
    }
}
