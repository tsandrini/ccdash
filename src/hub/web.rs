//! The panel's files, served from a directory (`http.web_dir`): the built
//! app at `/`, with `index.html` for every path that names no file (the
//! app routes on the client; only the hashed assets under `_app/` 404).
//! Inline scripts of a page are hashed into its CSP.

use std::path::{Component, Path, PathBuf};

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use ring::digest;

use super::http::App;

const CSP: &str = "default-src 'self'; script-src 'self'{hashes}; \
    style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
    manifest-src 'self'; worker-src 'self'; base-uri 'none'; frame-ancestors 'none'";

pub async fn serve(State(app): State<App>, uri: Uri) -> Response {
    let root = app.web_dir.clone();
    let path = uri.path().to_owned();
    tokio::task::spawn_blocking(move || file(&root, &path))
        .await
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn file(root: &Path, path: &str) -> Response {
    let Some(rel) = relative(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut file = root.join(&rel);
    // NOTE: a note slug may contain dots (`/notes/ws/a.b`), so a missing
    // file is a client route unless it is a built asset
    if !file.is_file() && !rel.starts_with("_app") {
        file = root.join("index.html");
    }
    let Ok(bytes) = std::fs::read(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let immutable = rel.starts_with("_app/immutable/");
    let csp = CSP.replace("{hashes}", &inline_script_hashes(&file, &bytes));
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(mime(&file))),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(if immutable {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                }),
            ),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_str(&csp).unwrap_or_else(|_| HeaderValue::from_static("")),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// The request path as a relative path under the web root; `..` and the
/// like are refused. Empty for the root.
fn relative(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        let part = percent_decode(part)?;
        match Path::new(&part).components().next() {
            Some(Component::Normal(_)) if !part.contains('/') && !part.contains('\\') => {
                out.push(&part);
            }
            _ => return None,
        }
    }
    Some(out)
}

fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_owned());
    }
    let mut bytes = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some(&b) = rest.first() {
        if b == b'%' {
            let hex = rest.get(1..3)?;
            bytes.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            rest = &rest[3..];
        } else {
            bytes.push(b);
            rest = &rest[1..];
        }
    }
    String::from_utf8(bytes).ok()
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}

/// `'sha256-…'` of every inline `<script>` of an HTML file, for its CSP
/// (the app's start-up script is inline).
fn inline_script_hashes(path: &Path, bytes: &[u8]) -> String {
    if path.extension().is_none_or(|e| e != "html") {
        return String::new();
    }
    let Ok(html) = std::str::from_utf8(bytes) else {
        return String::new();
    };
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let after = &rest[start + "<script".len()..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let attrs = &after[..open_end];
        let body = &after[open_end + 1..];
        let Some(close) = body.find("</script>") else {
            break;
        };
        if !attrs.contains("src=") {
            let hash = digest::digest(&digest::SHA256, &body.as_bytes()[..close]);
            out.push_str(&format!(" 'sha256-{}'", B64.encode(hash.as_ref())));
        }
        rest = &body[close..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_under_the_root() {
        assert_eq!(relative("/"), Some(PathBuf::new()));
        assert_eq!(relative("/_app/x.js"), Some(PathBuf::from("_app/x.js")));
        assert_eq!(relative("/a%20b"), Some(PathBuf::from("a b")));
        assert!(relative("/../x").is_none());
        assert!(relative("/a/../../x").is_none());
        assert!(relative("/%2e%2e/x").is_none());
        assert!(relative("/a%2fb").is_none());
    }

    #[test]
    fn inline_scripts_are_hashed() {
        let html = br#"<html><script src="a.js"></script><script>{ x = 1 }</script></html>"#;
        let hashes = inline_script_hashes(Path::new("index.html"), html);
        let want = B64.encode(digest::digest(&digest::SHA256, b"{ x = 1 }").as_ref());
        assert_eq!(hashes, format!(" 'sha256-{want}'"));
        assert_eq!(inline_script_hashes(Path::new("a.js"), html), "");
    }

    #[tokio::test]
    async fn serves_the_app() {
        // NOTE: a dotted directory name, like a store path's
        let dir = std::env::temp_dir().join(format!("ccdash-web-0.1.0-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("_app/immutable")).unwrap();
        std::fs::write(dir.join("index.html"), "<p>new</p>").unwrap();
        std::fs::write(dir.join("_app/immutable/x.js"), "1").unwrap();
        let body = async |path: &str| {
            let r = file(&dir, path);
            let status = r.status();
            let headers = r.headers().clone();
            let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
        };
        assert_eq!(body("/").await.2, "<p>new</p>");
        assert_eq!(body("/sessions/abc").await.2, "<p>new</p>");
        assert_eq!(body("/notes/ws/a.b.c").await.2, "<p>new</p>");
        assert_eq!(body("/_app/immutable/x.js").await.2, "1");
        assert_eq!(body("/_app/immutable/y.js").await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            body("/_app/immutable/x.js").await.1[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(body("/missing.js").await.2, "<p>new</p>");
        // the retired panel's address is a client route like any other
        assert_eq!(body("/v1/").await.2, "<p>new</p>");
        assert_eq!(body("/../etc/passwd").await.0, StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
