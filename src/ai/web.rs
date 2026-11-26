//! Research tools: web search (DuckDuckGo HTML, no API key) and page fetching as plain text.

use anyhow::{Result, bail};

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) ForgeCode";

pub fn search(query: &str) -> Result<String> {
    let html = ureq::get("https://html.duckduckgo.com/html/")
        .query("q", query)
        .set("User-Agent", UA)
        .timeout(std::time::Duration::from_secs(20))
        .call()?
        .into_string()?;
    let results = parse_results(&html);
    if results.is_empty() {
        return Ok("No results".into());
    }
    Ok(results.iter().take(8).map(|(title, url, snippet)| format!("{title}\n{url}\n{snippet}\n")).collect::<Vec<_>>().join("\n"))
}

pub fn fetch(url: &str) -> Result<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("only http(s) URLs can be fetched");
    }
    let body = ureq::get(url).set("User-Agent", UA).timeout(std::time::Duration::from_secs(30)).call()?.into_string()?;
    Ok(html_to_text(&body))
}

/// (title, url, snippet) from DuckDuckGo's HTML results page.
pub fn parse_results(html: &str) -> Vec<(String, String, String)> {
    let link = regex::Regex::new(r#"class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let snippet = regex::Regex::new(r#"class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();
    let snippets: Vec<String> = snippet.captures_iter(html).map(|c| html_to_text(&c[1])).collect();
    link.captures_iter(html)
        .enumerate()
        .map(|(i, c)| {
            let href = c[1].replace("&amp;", "&");
            // Results go through a redirect: //duckduckgo.com/l/?uddg=<encoded target>&...
            let url = href
                .split("uddg=")
                .nth(1)
                .map(|u| percent_decode(u.split('&').next().unwrap_or(u)))
                .unwrap_or(href);
            (html_to_text(&c[2]), url, snippets.get(i).cloned().unwrap_or_default())
        })
        .collect()
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
                continue;
            }
            (b'+', _) => out.push(b' '),
            (b, _) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Readable text from HTML: drops scripts/styles/tags, decodes common entities, collapses blank space.
pub fn html_to_text(html: &str) -> String {
    let blocks = regex::Regex::new(r"(?is)<(script|style|noscript|svg|head)[^>]*>.*?</(script|style|noscript|svg|head)>").unwrap();
    let breaks = regex::Regex::new(r"(?i)<(br|/p|/div|/li|/h[1-6]|/tr)[^>]*>").unwrap();
    let tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let text = blocks.replace_all(html, " ");
    let text = breaks.replace_all(&text, "\n");
    let text = tags.replace_all(&text, "");
    let text = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    text.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duckduckgo_results() {
        let html = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fbook%2F&amp;rut=x">The <b>Rust</b> Book</a>
            <a class="result__snippet" href="x">Learn <b>Rust</b> &amp; more</a>"#;
        let r = parse_results(html);
        assert_eq!(r[0], ("The Rust Book".into(), "https://doc.rust-lang.org/book/".into(), "Learn Rust & more".into()));
    }

    #[test]
    fn html_becomes_text() {
        let t = html_to_text("<html><head><title>x</title></head><body><script>var a=1;</script><h1>Title</h1><p>Hello&nbsp;<b>world</b></p></body></html>");
        assert_eq!(t, "Title\nHello world");
        assert_eq!(percent_decode("a%20b%2Fc+d"), "a b/c d");
    }
}
