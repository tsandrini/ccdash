//! The report document: the chrome every HTML report shares (the
//! `ccdash:report` skill's `template.html`, embedded here too) around the
//! body a session writes, and what a report must satisfy to be published:
//! one self-contained file that fetches nothing from anywhere, so it reads
//! the same in the panel's sandboxed frame, as a file sent on, and offline.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::knowledge::{slugify, ymd};

/// The chrome, with `{{title}}`, `{{date}}`, `{{generator}}` and `{{body}}`.
pub const TEMPLATE: &str = include_str!("../claude-plugin/skills/report/template.html");
pub const GENERATOR: &str = concat!("ccdash report build ", env!("CARGO_PKG_VERSION"));
/// Past this a report is still taken, with a warning.
const HEAVY_BYTES: usize = 8 * 1024 * 1024;

/// What the chrome shows about the report.
#[derive(Debug, Clone, Default)]
pub struct Meta<'a> {
    pub title: &'a str,
    /// Shown next to the title, `YYYY-MM-DD`.
    pub date: &'a str,
}

/// A problem with a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Refused when published; otherwise a warning.
    pub fatal: bool,
    pub text: String,
}

/// The template around `body`, an HTML fragment (sections under `h2` and
/// `h3` headings, tables, figures, tiles). Headings without an `id` get
/// one from their text, so links to sections stay stable.
pub fn build(meta: &Meta<'_>, body: &str) -> String {
    let body = with_heading_ids(body);
    // NOTE: the body goes in last: its text may spell the other markers
    TEMPLATE
        .replace("{{title}}", &escape(meta.title))
        .replace("{{date}}", &escape(meta.date))
        .replace("{{generator}}", GENERATOR)
        .replace("{{body}}", body.trim_end())
}

/// A built report taken apart again: what `build` was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unwrapped {
    pub title: String,
    pub date: String,
    /// The article's content, the body fragment.
    pub body: String,
}

/// The body, title and date out of a report `build` made (the document
/// carries `data-ccdash-report`); None for any other HTML. `build` of the
/// result with the current template is the same report in today's chrome.
pub fn unwrap(html: &str) -> Option<Unwrapped> {
    let lower = html.to_ascii_lowercase();
    let head = &lower[..lower.len().min(4096)];
    if !head.contains("data-ccdash-report") {
        return None;
    }
    let main = lower.find("<main id=\"content\">")?;
    let (_, open_end) = tag_opens(&lower[main..], "article").first().copied()?;
    let from = main + open_end;
    let colophon = lower[from..].find("<footer class=\"colophon\">")?;
    let to = from + lower[from..colophon + from].rfind("</article>")?;
    let date = tag_opens(&lower, "meta")
        .into_iter()
        .find(|(s, e)| attr_value(&lower[*s..*e], "name") == Some("ccdash:date"))
        .and_then(|(s, e)| attr_value(&html[s..e], "content").map(str::to_owned))
        .unwrap_or_default();
    Some(Unwrapped {
        title: title_of(html).unwrap_or_default(),
        date,
        body: html[from..to].trim_end().to_owned(),
    })
}

/// Whether `bytes` start like an HTML document.
pub fn is_html(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with("<!doctype html") || head.starts_with("<html") || head.contains("<html")
}

/// The document's `<title>`.
pub fn title_of(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = start + lower[start..].find('>')? + 1;
    let close = open_end + lower[open_end..].find("</title>")?;
    let title = unescape(html[open_end..close].trim());
    (!title.is_empty()).then_some(title)
}

/// The `h2` headings of a report's `<article>`, in order: its contents.
pub fn sections(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let (from, to) = article(&lower);
    tag_opens(&lower[from..to], "h2")
        .into_iter()
        .filter_map(|(_, end)| {
            let end = from + end;
            let close = end + lower[end..to].find("</h2>")?;
            let text = strip_tags(&html[end..close]).trim().to_owned();
            (!text.is_empty()).then_some(unescape(&text))
        })
        .collect()
}

/// The span of the document's `<article>` (the report's own content);
/// the whole document when it has none.
fn article(lower: &str) -> (usize, usize) {
    // NOTE: the last one: a comment or a code sample may spell the tag
    let Some((_, open_end)) = tag_opens(lower, "article").last().copied() else {
        return (0, lower.len());
    };
    let close = lower[open_end..]
        .find("</article>")
        .map_or(lower.len(), |len| open_end + len);
    (open_end, close)
}

/// What keeps `html` from being self-contained, and what makes it a poor
/// report: a script, style, image, font, frame or medium fetched from
/// anywhere (fatal: the panel shows reports under a policy that loads
/// nothing, and a file sent on would depend on the network too), a very
/// large file or no sections (warnings).
pub fn check(html: &str) -> Vec<Problem> {
    let mut out = Vec::new();
    let fatal = |text: String| Problem { fatal: true, text };
    let lower = html.to_ascii_lowercase();
    for tag in ["iframe", "object", "embed", "frame"] {
        if !tag_opens(&lower, tag).is_empty() {
            out.push(fatal(format!("<{tag}> is not allowed in a report")));
        }
    }
    for (tag, attr) in [
        ("script", "src"),
        ("img", "src"),
        ("source", "src"),
        ("video", "src"),
        ("video", "poster"),
        ("audio", "src"),
        ("track", "src"),
        ("link", "href"),
        ("use", "href"),
        ("image", "href"),
    ] {
        for (start, end) in tag_opens(&lower, tag) {
            let attrs = &lower[start..end];
            let Some(value) = attr_value(attrs, attr) else {
                continue;
            };
            if embedded(value) {
                continue;
            }
            out.push(fatal(format!(
                "<{tag} {attr}=\"{}\"> fetches a resource: embed it (a data: URI) or drop it",
                shorten(value)
            )));
        }
    }
    let mut from = 0;
    while let Some(at) = lower[from..].find("url(") {
        let at = from + at + 4;
        let rest = lower[at..].trim_start().trim_start_matches(['"', '\'']);
        if rest.starts_with("http:") || rest.starts_with("https:") || rest.starts_with("//") {
            let end = rest.find([')', '"', '\'']).unwrap_or(rest.len().min(80));
            out.push(fatal(format!(
                "a stylesheet loads url({}): embed it or drop it",
                shorten(&rest[..end])
            )));
        }
        from = at;
    }
    if lower.contains("@import") {
        out.push(fatal(
            "a stylesheet has an @import: inline the rules".into(),
        ));
    }
    if html.len() > HEAVY_BYTES {
        out.push(Problem {
            fatal: false,
            text: format!(
                "the report is {:.1} MB: keep data behind it in the hub, not in the page",
                html.len() as f64 / 1_048_576.0
            ),
        });
    }
    let (from, to) = article(&lower);
    if tag_opens(&lower[from..to], "h2").is_empty() {
        out.push(Problem {
            fatal: false,
            text: "no <h2> sections: the table of contents will be empty".into(),
        });
    }
    out
}

/// Directories `find_html` never looks into.
const SKIPPED_DIRS: [&str; 8] = [
    "node_modules",
    "target",
    "build",
    "dist",
    ".git",
    ".svelte-kit",
    ".direnv",
    "result",
];

/// Every `.html`/`.htm` file under `paths`, sorted; a path that is a file
/// counts as is.
pub fn find_html(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if !SKIPPED_DIRS.contains(&name) {
                    walk(&path, out)?;
                }
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"))
            {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    for path in paths {
        let path = std::path::absolute(path)?;
        if path.is_dir() {
            walk(&path, &mut out)?;
        } else if path.is_file() {
            out.push(path);
        } else {
            anyhow::bail!("{} does not exist", path.display());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// What an older report file would be published as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub title: String,
    pub slug: String,
    pub summary: String,
    pub body: String,
    /// `YYYY-MM-DD` of the file's modification time.
    pub date: String,
    pub bytes: usize,
    /// Fatal problems; empty when it can be published.
    pub problems: Vec<String>,
}

/// Reads `file` as a report to import; None when it is no HTML document.
pub fn inspect(file: &Path) -> Result<Option<Found>> {
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    if !is_html(&bytes) {
        return Ok(None);
    }
    let html = String::from_utf8_lossy(&bytes);
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("report")
        .to_owned();
    let title = title_of(&html).unwrap_or_else(|| stem.replace(['-', '_'], " "));
    let modified = std::fs::metadata(file)?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    let date = ymd(modified);
    let base = slugify(&title);
    // NOTE: a title that dates itself ("… 2026-11-02") needs no second date
    let dated = base
        .as_bytes()
        .windows(10)
        .any(|w| crate::hub::reports::valid_date(std::str::from_utf8(w).unwrap_or("")));
    let slug = if dated {
        base
    } else {
        slugify(&format!("{base} {date}"))
    };
    let sections = sections(&html);
    let mut body = format!(
        "# {title}\n\nImported from `{}` (made {date}).\n",
        file.display()
    );
    if !sections.is_empty() {
        body.push_str("\n## Sections\n\n");
        for s in &sections {
            body.push_str(&format!("- {s}\n"));
        }
    }
    Ok(Some(Found {
        summary: format!("Imported {date}: {}", file.display()),
        problems: check(&html)
            .into_iter()
            .filter(|p| p.fatal)
            .map(|p| p.text)
            .collect(),
        title,
        slug,
        body,
        date,
        bytes: bytes.len(),
    }))
}

fn embedded(value: &str) -> bool {
    value.starts_with("data:") || value.starts_with("blob:") || value.starts_with('#')
}

fn shorten(s: &str) -> String {
    crate::util::truncate(s, 60)
}

/// `(attrs_start, tag_end)` of every `<tag …>` in `lower`: the attribute
/// text starts after the name and ends at the `>`.
fn tag_opens(lower: &str, tag: &str) -> Vec<(usize, usize)> {
    let needle = format!("<{tag}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find(&needle) {
        let start = from + at + needle.len();
        let next = lower[start..].chars().next();
        from = start;
        if !matches!(next, Some(c) if c.is_whitespace() || c == '>' || c == '/') {
            continue;
        }
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        out.push((start, start + len + 1));
        from = start + len + 1;
    }
    out
}

/// The value of `attr` in a tag's attribute text (`attrs` ends with `>`).
fn attr_value<'a>(attrs: &'a str, attr: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(at) = attrs[from..].find(attr) {
        let at = from + at;
        let before = attrs[..at].chars().next_back();
        let after = attrs[at + attr.len()..].trim_start();
        from = at + attr.len();
        if !matches!(before, Some(c) if c.is_whitespace()) || !after.starts_with('=') {
            continue;
        }
        let value = after[1..].trim_start();
        return Some(match value.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let inner = &value[1..];
                &inner[..inner.find(q).unwrap_or(inner.len())]
            }
            _ => &value[..value.find([' ', '>', '\n', '\t']).unwrap_or(value.len())],
        });
    }
    None
}

/// `body` with an `id` on every `h2` and `h3` that has none, from its
/// text; a taken id gets `-2`, `-3`….
fn with_heading_ids(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let mut taken: HashSet<String> = HashSet::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find(" id=") {
        let at = from + at + 4;
        if let Some(value) = attr_value(&lower[at - 4..], "id") {
            taken.insert(value.to_owned());
        }
        from = at;
    }
    let mut opens: Vec<(usize, usize, &str)> = Vec::new();
    for tag in ["h2", "h3"] {
        opens.extend(tag_opens(&lower, tag).into_iter().map(|(s, e)| (s, e, tag)));
    }
    opens.sort_unstable();
    let mut out = String::with_capacity(body.len() + opens.len() * 24);
    let mut last = 0;
    for (start, end, tag) in opens {
        if attr_value(&lower[start..end], "id").is_some() {
            continue;
        }
        let close = format!("</{tag}>");
        let text = lower[end..]
            .find(&close)
            .map_or("", |len| &body[end..end + len]);
        let base = slugify(&unescape(&strip_tags(text)));
        let base = if base.is_empty() { "section" } else { &base };
        let mut id = base.to_owned();
        let mut n = 2;
        while taken.contains(&id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        taken.insert(id.clone());
        out.push_str(&body[last..start]);
        out.push_str(&format!(" id=\"{id}\""));
        last = start;
    }
    out.push_str(&body[last..]);
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut inside = false;
    for c in s.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "<p class=\"lead\">Hi</p>\n<h2>Findings &amp; more</h2><p>a</p>\n<h3>Detail</h3>\n<h2 id=\"own\">Kept</h2>\n<h2>Findings &amp; more</h2>";

    #[test]
    fn the_chrome_wraps_the_body_and_headings_get_ids() {
        let html = build(
            &Meta {
                title: "A <b>report</b>",
                date: "2026-11-04",
            },
            BODY,
        );
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<title>A &lt;b&gt;report&lt;/b&gt;</title>"));
        assert!(html.contains("<h2 id=\"findings-more\">Findings &amp; more</h2>"));
        assert!(html.contains("<h3 id=\"detail\">Detail</h3>"));
        assert!(html.contains("<h2 id=\"own\">Kept</h2>"));
        assert!(html.contains("<h2 id=\"findings-more-2\">Findings &amp; more</h2>"));
        assert!(html.contains(GENERATOR));
        assert!(!html.contains("{{"));
        assert_eq!(title_of(&html).as_deref(), Some("A <b>report</b>"));
        assert_eq!(
            sections(&html),
            ["Findings & more", "Kept", "Findings & more"]
        );
        assert!(is_html(html.as_bytes()));
        assert!(!is_html(b"# not html"));
        assert!(check(&html).is_empty(), "{:?}", check(&html));
    }

    #[test]
    fn a_built_report_unwraps_to_what_built_it() {
        let meta = Meta {
            title: "A <b>report</b> & more",
            date: "2026-11-04",
        };
        let body = concat!(
            "<p class=\"lead\">Hi</p>\n<h2>One</h2><p>a</p>\n",
            "<pre><code>&lt;article&gt;&lt;/article&gt;</code></pre>\n",
            "<article class=\"inner\"><h3>Nested</h3></article>\n"
        );
        let html = build(&meta, body);
        let un = unwrap(&html).unwrap();
        assert_eq!(un.title, meta.title);
        assert_eq!(un.date, meta.date);
        assert!(
            un.body
                .contains("<article class=\"inner\"><h3 id=\"nested\">Nested</h3></article>")
        );
        // the round trip is stable: rebuilt, the report is the same file
        assert_eq!(
            build(
                &Meta {
                    title: &un.title,
                    date: &un.date
                },
                &un.body
            ),
            html
        );
        assert!(unwrap("<!doctype html><html><head><title>x</title></head><body><article>y</article></body></html>").is_none());
    }

    #[test]
    fn fetched_resources_are_refused_and_embedded_ones_pass() {
        let bad = build(
            &Meta::default(),
            concat!(
                "<h2>x</h2><script src=\"https://cdn.example/x.js\"></script>",
                "<img src=\"pic.png\"><link rel=stylesheet href='//f.example/a.css'>",
                "<style>body{background:url(\"https://x/y.png\")}</style><iframe src=\"a\"></iframe>"
            ),
        );
        let problems: Vec<String> = check(&bad)
            .into_iter()
            .filter(|p| p.fatal)
            .map(|p| p.text)
            .collect();
        assert_eq!(problems.len(), 5, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("cdn.example/x.js")));
        assert!(problems.iter().any(|p| p.contains("pic.png")));
        assert!(problems.iter().any(|p| p.contains("<iframe>")));
        assert!(problems.iter().any(|p| p.contains("url(")));
        let good = build(
            &Meta::default(),
            concat!(
                "<h2>x</h2><img src=\"data:image/png;base64,AAAA\" alt=\"\"><svg><use href=\"#icon\"/></svg>",
                "<script>const url = 'https://example.org'; fetch(url)</script><a href=\"https://example.org\">link</a>"
            ),
        );
        assert!(check(&good).iter().all(|p| !p.fatal), "{:?}", check(&good));
        let empty = check(&build(&Meta::default(), "<p>nothing</p>"));
        assert_eq!(empty.len(), 1);
        assert!(!empty[0].fatal);
    }

    #[test]
    fn older_files_are_found_and_sized_up_for_an_import() {
        let dir = std::env::temp_dir().join(format!("ccdash-report-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("node_modules/x.html"), "<html></html>").unwrap();
        std::fs::write(dir.join("notes.md"), "# md").unwrap();
        std::fs::write(
            dir.join("sub/good.html"),
            build(
                &Meta {
                    title: "Good report",
                    date: "2026-10-01",
                },
                "<h2>A</h2><h2>B</h2>",
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("bad.html"),
            "<!doctype html><html><head><title>Bad</title></head><body><script src=\"https://x/y.js\"></script></body></html>",
        )
        .unwrap();
        std::fs::write(dir.join("plain.html"), "not html at all").unwrap();
        let files = find_html(std::slice::from_ref(&dir)).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.strip_prefix(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names, ["bad.html", "plain.html", "sub/good.html"]);
        let good = inspect(&dir.join("sub/good.html")).unwrap().unwrap();
        assert_eq!(good.title, "Good report");
        assert!(good.slug.starts_with("good-report-20"), "{}", good.slug);
        std::fs::write(
            dir.join("sub/dated.html"),
            build(
                &Meta {
                    title: "Dated 2026-11-02",
                    date: "2026-11-02",
                },
                "<h2>A</h2>",
            ),
        )
        .unwrap();
        let dated = inspect(&dir.join("sub/dated.html")).unwrap().unwrap();
        assert_eq!(dated.slug, "dated-2026-11-02");
        assert!(good.problems.is_empty());
        assert!(good.body.contains("- A\n- B\n"));
        assert_eq!(good.date.len(), 10);
        let bad = inspect(&dir.join("bad.html")).unwrap().unwrap();
        assert_eq!(bad.title, "Bad");
        assert_eq!(bad.problems.len(), 1);
        assert!(inspect(&dir.join("plain.html")).unwrap().is_none());
        assert!(find_html(&[dir.join("missing")]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attributes_are_read_loosely() {
        assert_eq!(attr_value(" src=\"a b\">", "src"), Some("a b"));
        assert_eq!(attr_value(" data-src=\"x\" src=y>", "src"), Some("y"));
        assert_eq!(attr_value(" srcset=\"x\">", "src"), None);
        assert_eq!(tag_opens("<h2>a</h2><h20>b</h20><h2 id=x>", "h2").len(), 2);
    }
}
