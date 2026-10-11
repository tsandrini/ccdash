//! Markdown to HTML for prompts and replies. Raw HTML is escaped, images are
//! reduced to their alt text and only http(s)/mailto links survive, so the
//! output can be inserted as markup.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};

pub fn render(text: &str) -> String {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut in_unsafe_link = false;
    let events = Parser::new_ext(text, options).filter_map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Some(Event::Text(raw)),
        Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,
        Event::Start(Tag::Link { ref dest_url, .. }) if !safe_url(dest_url) => {
            in_unsafe_link = true;
            None
        }
        Event::End(TagEnd::Link) if in_unsafe_link => {
            in_unsafe_link = false;
            None
        }
        other => Some(other),
    });
    let mut out = String::with_capacity(text.len() * 3 / 2);
    html::push_html(&mut out, events);
    out
}

fn safe_url(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_raw_html() {
        let out = render("hi <script>alert(1)</script> <b>x</b>");
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
        assert!(!out.contains("<b>"));
    }

    #[test]
    fn keeps_only_safe_links() {
        let out = render("[ok](https://example.com) [bad](javascript:alert(1)) [rel](src/main.rs)");
        assert!(out.contains(r#"<a href="https://example.com">ok</a>"#));
        assert!(!out.contains("javascript:"));
        assert!(out.contains("bad") && out.contains("rel"));
        assert_eq!(out.matches("<a ").count(), 1);
    }

    #[test]
    fn images_become_alt_text() {
        let out = render("![a chart](https://tracker.example/x.png)");
        assert!(!out.contains("<img"));
        assert!(out.contains("a chart"));
    }

    #[test]
    fn renders_lists_code_and_tables() {
        let out = render("- one\n- `two`\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(out.contains("<li>one</li>"));
        assert!(out.contains("<code>two</code>"));
        assert!(out.contains("<table>"));
    }
}
