//! Owner doc rendering for the review reader (sm#1447 / #1451): source-line
//! annotation and review-client injection.
//!
//! HTML is edited in place: a single forward scan inserts `data-sm-line="N"`
//! into block start tags and leaves every other byte of the owner's document
//! untouched. Markdown gets the same attribute from `pulldown-cmark` source
//! offsets. See `specs/1447_owner_docs.md` "Rendering".

/// Block-level start tags that carry a source line.
const ANNOTATED_TAGS: &[&str] = &[
    "p",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "pre",
    "blockquote",
    "td",
    "th",
    "dt",
    "dd",
    "figcaption",
    "div",
    "section",
    "tr",
];

/// Raw-text and RCDATA elements: their content is text up to the matching
/// end tag, so tag-like text inside them is never markup.
const RAW_TEXT_TAGS: &[&str] = &[
    "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript",
];

/// The attribute every annotated block carries.
pub const LINE_ATTRIBUTE: &str = "data-sm-line";

/// Result of scanning an HTML document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotatedHtml {
    pub bytes: Vec<u8>,
    /// Offset in `bytes` of the first real `</body` end tag, if any.
    pub body_end: Option<usize>,
}

/// Adds `data-sm-line="N"` (the 1-based source line of the tag's `<`) right
/// after the tag name of every block start tag in [`ANNOTATED_TAGS`].
///
/// One forward pass over the bytes. It skips comments, doctype and other
/// `<!`/`<?` constructs, attribute values (quoted values may hold `>` and
/// newlines), and the contents of raw-text/RCDATA elements up to their
/// matching end tag. `<plaintext>` makes the rest of the document text.
pub fn annotate_html_lines(source: &[u8]) -> AnnotatedHtml {
    scan_html(source, true)
}

/// Offset of the first real `</body` end tag, found by the same scan
/// without changing anything.
pub fn find_body_end(source: &[u8]) -> Option<usize> {
    scan_html(source, false).body_end
}

fn scan_html(source: &[u8], annotate: bool) -> AnnotatedHtml {
    let mut output = Vec::with_capacity(source.len() + source.len() / 16);
    let mut body_end = None;
    let mut copied = 0;
    let mut line = 1usize;
    let mut index = 0;
    // Counts newlines in `source[from..to]`.
    let newlines =
        |from: usize, to: usize| source[from..to].iter().filter(|b| **b == b'\n').count();

    while index < source.len() {
        let byte = source[index];
        if byte != b'<' {
            if byte == b'\n' {
                line += 1;
            }
            index += 1;
            continue;
        }
        let rest = &source[index..];
        if rest.starts_with(b"<!--") {
            let end = find(source, index + 4, b"-->").map_or(source.len(), |end| end + 3);
            line += newlines(index, end);
            index = end;
            continue;
        }
        if rest.starts_with(b"<!") || rest.starts_with(b"<?") {
            let end = find(source, index + 2, b">").map_or(source.len(), |end| end + 1);
            line += newlines(index, end);
            index = end;
            continue;
        }
        let is_end_tag = rest.starts_with(b"</");
        let name_start = index + if is_end_tag { 2 } else { 1 };
        if !source.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            // A bare `<` in text.
            index += 1;
            continue;
        }
        let name_end = source[name_start..]
            .iter()
            .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b':' | b'_')))
            .map_or(source.len(), |offset| name_start + offset);
        let name = source[name_start..name_end].to_ascii_lowercase();
        let tag_line = line;
        let tag_end = tag_end(source, name_end);
        line += newlines(index, tag_end);

        if is_end_tag {
            if body_end.is_none() && name == b"body" {
                body_end = Some(output.len() + (index - copied));
            }
            index = tag_end;
            continue;
        }

        if annotate && ANNOTATED_TAGS.iter().any(|tag| tag.as_bytes() == name) {
            output.extend_from_slice(&source[copied..name_end]);
            output.extend_from_slice(format!(" {LINE_ATTRIBUTE}=\"{tag_line}\"").as_bytes());
            copied = name_end;
        }
        index = tag_end;

        if name == b"plaintext" {
            break;
        }
        let self_closing = tag_end >= 2 && source[tag_end - 2] == b'/';
        if !self_closing && RAW_TEXT_TAGS.iter().any(|tag| tag.as_bytes() == name) {
            let close = find_end_tag(source, index, &name).unwrap_or(source.len());
            line += newlines(index, close);
            index = close;
        }
    }
    output.extend_from_slice(&source[copied..]);
    AnnotatedHtml {
        bytes: output,
        body_end,
    }
}

fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// Index just past the `>` that closes a tag whose name ends at `from`,
/// skipping quoted attribute values.
fn tag_end(source: &[u8], from: usize) -> usize {
    let mut index = from;
    let mut after_equals = false;
    while index < source.len() {
        match source[index] {
            b'>' => return index + 1,
            quote @ (b'"' | b'\'') if after_equals => {
                index = source[index + 1..]
                    .iter()
                    .position(|b| *b == quote)
                    .map_or(source.len(), |offset| index + 1 + offset + 1);
                after_equals = false;
                continue;
            }
            b'=' => after_equals = true,
            byte if byte.is_ascii_whitespace() => {}
            _ => after_equals = false,
        }
        index += 1;
    }
    source.len()
}

/// Offset of the `<` of `</name` (case-insensitive) followed by whitespace,
/// `/` or `>`, searching from `from`.
fn find_end_tag(source: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let mut index = from;
    while let Some(open) = find(source, index, b"</") {
        let name_start = open + 2;
        let name_end = name_start + name.len();
        if source
            .get(name_start..name_end)
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
            && source
                .get(name_end)
                .is_none_or(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'>'))
        {
            return Some(open);
        }
        index = open + 2;
    }
    None
}

/// Markdown rendered to HTML with `data-sm-line` on block start tags, from
/// `pulldown-cmark` source offsets.
///
/// Each block start event is preceded by a sentinel naming the tag and line;
/// after `push_html` renders, each sentinel is removed and its attribute is
/// added to the next start tag of that name. The sentinel carries a random
/// nonce and a NUL, so document text cannot forge one.
pub fn render_markdown_with_lines(source: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag};

    let nonce = {
        use rand_core::{OsRng, RngCore};
        format!("{:016x}", OsRng.next_u64())
    };
    let sentinel_open = format!("\u{0}smline{nonce}:");
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(
            source
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(index, _)| index + 1),
        )
        .collect();
    let line_of = |offset: usize| line_starts.partition_point(|start| *start <= offset);

    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut events = Vec::new();
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        if let Event::Start(tag) = &event {
            let target = match tag {
                Tag::Paragraph => Some("p"),
                Tag::Heading { level, .. } => Some(match *level as u8 {
                    1 => "h1",
                    2 => "h2",
                    3 => "h3",
                    4 => "h4",
                    5 => "h5",
                    _ => "h6",
                }),
                Tag::BlockQuote(_) => Some("blockquote"),
                Tag::CodeBlock(_) => Some("pre"),
                Tag::Item => Some("li"),
                Tag::TableHead | Tag::TableRow => Some("tr"),
                Tag::TableCell => Some("td|th"),
                _ => None,
            };
            if let Some(target) = target {
                events.push(Event::Html(
                    format!("{sentinel_open}{target}:{}\u{0}", line_of(range.start)).into(),
                ));
            }
        }
        events.push(event);
    }
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events.into_iter());
    apply_line_sentinels(&html, &sentinel_open)
}

fn apply_line_sentinels(html: &str, sentinel_open: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut pending: Vec<(Vec<&str>, String)> = Vec::new();
    let mut rest = html;
    loop {
        let next_sentinel = rest.find(sentinel_open);
        // Apply pending attributes to start tags before the next sentinel.
        let segment_end = next_sentinel.unwrap_or(rest.len());
        let mut segment = &rest[..segment_end];
        while !pending.is_empty() {
            let Some(open) = segment.find('<') else { break };
            let after = &segment[open + 1..];
            let name_len = after
                .find(|ch: char| !ch.is_ascii_alphanumeric())
                .unwrap_or(after.len());
            let name = &after[..name_len];
            output.push_str(&segment[..open + 1 + name_len]);
            segment = &after[name_len..];
            if let Some(position) = pending.iter().position(|(names, _)| names.contains(&name)) {
                let (_, line) = pending.remove(position);
                output.push_str(&format!(" {LINE_ATTRIBUTE}=\"{line}\""));
            }
        }
        output.push_str(segment);
        let Some(start) = next_sentinel else { break };
        let body = &rest[start + sentinel_open.len()..];
        let Some(end) = body.find('\u{0}') else {
            output.push_str(&rest[start..]);
            break;
        };
        if let Some((target, line)) = body[..end].split_once(':') {
            pending.push((target.split('|').collect(), line.to_owned()));
        }
        rest = &body[end + 1..];
    }
    output
}

/// Inserts `injection` just before `</body>` (at `body_end`), or appends it.
pub fn inject_before_body_end(
    mut html: Vec<u8>,
    body_end: Option<usize>,
    injection: &str,
) -> Vec<u8> {
    match body_end {
        Some(offset) if offset <= html.len() => {
            html.splice(offset..offset, injection.bytes());
        }
        _ => html.extend_from_slice(injection.as_bytes()),
    }
    html
}

/// JSON for an inline `<script>`: `<` is escaped so the config can never
/// close the script element or open a comment.
pub fn inline_json(value: &serde_json::Value) -> String {
    value.to_string().replace('<', "\\u003c")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annotate(source: &str) -> String {
        String::from_utf8(annotate_html_lines(source.as_bytes()).bytes).unwrap()
    }

    /// Output minus the inserted attributes is the input, byte for byte.
    fn strip_annotations(annotated: &str) -> String {
        let mut output = String::new();
        let mut rest = annotated;
        while let Some(start) = rest.find(" data-sm-line=\"") {
            output.push_str(&rest[..start]);
            let after = &rest[start + " data-sm-line=\"".len()..];
            rest = &after[after.find('"').unwrap() + 1..];
        }
        output.push_str(rest);
        output
    }

    #[test]
    fn annotates_block_start_tags_with_their_source_line() {
        let source = "<!doctype html>\n<html><body>\n<h1 class=\"t\">Title</h1>\n<p>One\ntwo</p>\n<ul>\n  <li>A</li><LI>B</LI>\n</ul>\n<span>x</span><div\n  id=\"d\">y</div>\n</body></html>\n";
        let annotated = annotate(source);
        assert!(
            annotated.contains("<h1 data-sm-line=\"3\" class=\"t\">"),
            "{annotated}"
        );
        assert!(
            annotated.contains("<p data-sm-line=\"4\">One"),
            "{annotated}"
        );
        assert!(annotated.contains("<li data-sm-line=\"7\">A</li><LI data-sm-line=\"7\">B"));
        assert!(annotated.contains("<div data-sm-line=\"9\"\n  id=\"d\">"));
        assert!(annotated.contains("<span>x</span>"));
        assert!(!annotated.contains("<ul data-sm-line"));
        assert!(!annotated.contains("<html data-sm-line"));
        assert_eq!(strip_annotations(&annotated), source);
    }

    #[test]
    fn skips_comments_attribute_values_and_raw_text() {
        let source = concat!(
            "<!-- <p>commented\n</p> -->\n",                               // 1-2
            "<a title=\"x > <p>\n y\">link</a>\n",                         // 3-4
            "<textarea><p>example</p>\n</textarea>\n",                     // 5-6
            "<script>if (a <p) { s = '</scriptx><p>'; }\n</SCRIPT >\n",    // 7-8
            "<style>p > li { }\n<p></style>\n",                            // 9-10
            "<title><p>t</p></title><xmp><p></xmp><iframe><p></iframe>\n", // 11
            "<noembed><p></noembed><noframes><p></noframes><noscript><p></noscript>\n", // 12
            "<p a='1>2'\n b=c>after</p>\n",                                // 13-14
        );
        let annotated = annotate(source);
        assert_eq!(annotated.matches("data-sm-line").count(), 1, "{annotated}");
        assert!(
            annotated.contains("<p data-sm-line=\"13\" a='1>2'"),
            "{annotated}"
        );
        assert_eq!(strip_annotations(&annotated), source);
    }

    #[test]
    fn plaintext_ends_markup_and_body_end_is_found_outside_raw_text() {
        let annotated = annotate_html_lines(b"<p>a</p><plaintext><p>b</body>");
        assert_eq!(
            String::from_utf8(annotated.bytes).unwrap(),
            "<p data-sm-line=\"1\">a</p><plaintext><p>b</body>"
        );
        assert_eq!(annotated.body_end, None);

        let source = "<body><script>\"</body>\"</script><!-- </body> --><p>x</p>\n</BODY></html>";
        let annotated = annotate_html_lines(source.as_bytes());
        let html = String::from_utf8(annotated.bytes.clone()).unwrap();
        let at = annotated.body_end.unwrap();
        assert!(html[at..].starts_with("</BODY>"), "{}", &html[at..]);
        let injected = inject_before_body_end(annotated.bytes, annotated.body_end, "<i></i>");
        assert!(String::from_utf8(injected)
            .unwrap()
            .ends_with("<i></i></BODY></html>"));
        assert_eq!(
            inject_before_body_end(b"<p>x</p>".to_vec(), None, "<i></i>"),
            b"<p>x</p><i></i>"
        );
    }

    #[test]
    fn unterminated_constructs_do_not_panic() {
        for source in [
            "<p",
            "<p a=\"",
            "<!--",
            "<script>x",
            "<",
            "</",
            "<!doctype",
            "<p a='x",
        ] {
            let annotated = annotate_html_lines(source.as_bytes());
            assert!(annotated
                .bytes
                .starts_with(source.as_bytes().get(..2).unwrap_or(b"")));
        }
    }

    #[test]
    fn real_memo_is_annotated_without_other_changes() {
        let source = include_str!("../tests/fixtures/owner_docs/1612_decision_memo.html");
        let annotated = annotate(source);
        assert_eq!(strip_annotations(&annotated), source);
        // Every `<p>` in the memo starts on its own line, so each anchor is exact.
        let paragraphs = source.matches("<p>").count() + source.matches("<p ").count();
        assert!(paragraphs > 0);
        for (index, line) in source.lines().enumerate() {
            let annotated_line = annotated.lines().nth(index).unwrap();
            if line.trim_start().starts_with("<p>") {
                assert!(
                    annotated_line.contains(&format!("<p data-sm-line=\"{}\">", index + 1)),
                    "line {}: {annotated_line}",
                    index + 1
                );
            }
        }
    }

    #[test]
    fn markdown_blocks_carry_their_source_line() {
        let source = "# Title\n\nFirst paragraph\nstill first.\n\n- one\n- two\n\n> quoted\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n<div>raw <p>html</p></div>\n\nLast & text\n";
        let html = render_markdown_with_lines(source);
        assert!(html.contains("<h1 data-sm-line=\"1\">Title</h1>"), "{html}");
        assert!(
            html.contains("<p data-sm-line=\"3\">First paragraph"),
            "{html}"
        );
        assert!(html.contains("<li data-sm-line=\"6\">one</li>"), "{html}");
        assert!(html.contains("<li data-sm-line=\"7\">two</li>"), "{html}");
        assert!(html.contains("<blockquote data-sm-line=\"9\">"), "{html}");
        assert!(html.contains("<p data-sm-line=\"9\">quoted</p>"), "{html}");
        assert!(
            html.contains("<pre data-sm-line=\"11\"><code class=\"language-rust\">"),
            "{html}"
        );
        assert!(
            html.contains("<tr data-sm-line=\"15\"><th data-sm-line=\"15\">a</th>"),
            "{html}"
        );
        assert!(
            html.contains("<tr data-sm-line=\"17\"><td data-sm-line=\"17\">1</td>"),
            "{html}"
        );
        // Raw HTML in the markdown is passed through, not annotated.
        assert!(html.contains("<div>raw <p>html</p></div>"), "{html}");
        assert!(
            html.contains("<p data-sm-line=\"21\">Last &amp; text</p>"),
            "{html}"
        );
        assert!(!html.contains('\u{0}'));
    }

    #[test]
    fn markdown_text_cannot_forge_a_sentinel() {
        let html = render_markdown_with_lines("a \u{0}smline0000000000000000:p:99\u{0} b\n");
        assert!(!html.contains("data-sm-line=\"99\""), "{html}");
        assert!(html.contains("<p data-sm-line=\"1\">"), "{html}");
    }

    #[test]
    fn inline_json_cannot_close_the_script() {
        let json = inline_json(&serde_json::json!({"title": "</script><!--"}));
        assert!(!json.contains('<'), "{json}");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["title"], "</script><!--");
    }
}
