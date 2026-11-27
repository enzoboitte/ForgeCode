//! Shader linter: Khronos' reference compiler (glslang) checks GLSL as it is typed, beyond the
//! syntax glsl_analyzer sees (undeclared names, wrong types, missing `main`…).

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;
use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// glslang's stage for a shader file (raylib's `.fs` / `.vs` included).
pub fn stage(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "vs" | "vert" => "vert",
        "geom" => "geom",
        "comp" => "comp",
        "tesc" => "tesc",
        "tese" => "tese",
        _ => "frag",
    }
}

/// `ERROR: 0:12: 'foo' : undeclared identifier` → a diagnostic on line 12.
pub fn parse(output: &str) -> Vec<Diagnostic> {
    output
        .lines()
        .filter_map(|line| {
            let (severity, rest) = if let Some(r) = line.strip_prefix("ERROR: ") {
                (DiagnosticSeverity::ERROR, r)
            } else if let Some(r) = line.strip_prefix("WARNING: ") {
                (DiagnosticSeverity::WARNING, r)
            } else {
                return None;
            };
            // `<string>:<line>: message`; summaries ("2 compilation errors…") have no line.
            let mut parts = rest.splitn(3, ':');
            let (_, line_no, message) = (parts.next()?, parts.next()?.trim().parse::<u32>().ok()?, parts.next()?.trim());
            // "compilation terminated" only repeats that an error came before.
            if message.contains("compilation terminated") {
                return None;
            }
            let at = line_no.saturating_sub(1);
            Some(Diagnostic {
                range: Range::new(Position::new(at, 0), Position::new(at, 1000)),
                severity: Some(severity),
                source: Some("glslang".into()),
                message: message.trim_start_matches("'' : ").to_string(),
                ..Default::default()
            })
        })
        .collect()
}

/// Check `text` (the editor's, saved or not) as the shader `path`.
pub fn lint(path: &Path, text: &str) -> Result<Vec<Diagnostic>> {
    let glslang = crate::lsp::install::ensure_glslang()?;
    let mut child = Command::new(glslang)
        .args(["-S", stage(path), "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(text.as_bytes())?;
    let out = child.wait_with_output()?;
    Ok(parse(&format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_glslang_messages() {
        let out = "stdin\nERROR: 0:4: 'foo' : undeclared identifier \nWARNING: 0:2: 'x' : unused\nERROR: 0:4: '' : compilation terminated \nERROR: 2 compilation errors.  No code generated.\n";
        let d = parse(out);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].range.start.line, d[0].severity, d[0].message.as_str()), (3, Some(DiagnosticSeverity::ERROR), "'foo' : undeclared identifier"));
        assert_eq!((d[1].range.start.line, d[1].severity), (1, Some(DiagnosticSeverity::WARNING)));
        assert_eq!(stage(Path::new("a/outline.fs")), "frag");
        assert_eq!(stage(Path::new("base.vs")), "vert");
    }
}
