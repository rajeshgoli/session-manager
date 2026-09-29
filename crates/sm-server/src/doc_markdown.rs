//! Compact agent-readable rendering shared by the CLI and doc server.
use htmd::{element_handler::Handlers, Element, HtmlToMarkdown, Node};
use markup5ever_rcdom::NodeData;

fn text(node: &Node) -> String {
    match &node.data {
        NodeData::Text { contents } => contents.borrow().to_string(),
        _ => node.children.borrow().iter().map(|n| text(n)).collect(),
    }
}
fn collapse(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn svg_parts(node: &Node, title: &mut Option<String>, labels: &mut Vec<String>) {
    if let NodeData::Element { name, .. } = &node.data {
        match name.local.as_ref() {
            "title" if title.is_none() => *title = Some(collapse(&text(node))),
            "text" => {
                let label = collapse(&text(node));
                if !label.is_empty() {
                    labels.push(label);
                }
                return;
            }
            _ => {}
        }
    }
    for child in node.children.borrow().iter() {
        svg_parts(child, title, labels);
    }
}

pub fn is_html(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("html") || s.eq_ignore_ascii_case("htm"))
}

pub fn convert(html: &str) -> std::io::Result<String> {
    let markdown = HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "head"])
        .add_handler(vec!["svg"], |_h: &dyn Handlers, el: Element| {
            let mut title = None;
            let mut labels = Vec::new();
            svg_parts(el.node, &mut title, &mut labels);
            let name = el
                .attrs
                .iter()
                .find(|a| a.name.local.as_ref() == "aria-label")
                .map(|a| collapse(&a.value))
                .or(title)
                .unwrap_or_else(|| "untitled".into());
            let labels = if labels.is_empty() {
                String::new()
            } else {
                format!(" Labels: {}", labels.join(" · "))
            };
            Some(format!("\n\n[figure: {name}]{labels}\n\n").into())
        })
        .add_handler(vec!["dt"], |h: &dyn Handlers, el: Element| {
            Some(format!("\n\n**{}**\n\n", h.walk_children(el.node).content.trim()).into())
        })
        .add_handler(vec!["dd"], |h: &dyn Handlers, el: Element| {
            Some(format!("\n\n{}\n\n", h.walk_children(el.node).content.trim()).into())
        })
        .build()
        .convert(html)?;
    Ok(compact_tables(&markdown))
}

fn compact_tables(markdown: &str) -> String {
    let mut fence: Option<(char, usize)> = None;
    markdown
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let first = trimmed.chars().next().unwrap_or(' ');
            let count = trimmed.chars().take_while(|c| *c == first).count();
            if matches!(first, '`' | '~') && count >= 3 {
                match fence {
                    None => fence = Some((first, count)),
                    Some((ch, n))
                        if ch == first && count >= n && trimmed[count..].trim().is_empty() =>
                    {
                        fence = None
                    }
                    _ => {}
                }
                return line.to_owned();
            }
            if fence.is_some() || !line.starts_with('|') {
                return line.to_owned();
            }
            let mut cells = Vec::new();
            let mut start = 1;
            let mut slashes = 0;
            for (i, ch) in line.char_indices().skip(1) {
                if ch == '|' && slashes % 2 == 0 {
                    cells.push(line[start..i].trim());
                    start = i + 1;
                }
                slashes = if ch == '\\' { slashes + 1 } else { 0 };
            }
            if start < line.len() && !line[start..].trim().is_empty() {
                cells.push(line[start..].trim());
            }
            if cells.is_empty() {
                return line.to_owned();
            }
            let separator = cells
                .iter()
                .all(|c| c.contains('-') && c.chars().all(|ch| matches!(ch, '-' | ':')));
            format!(
                "| {} |",
                cells
                    .iter()
                    .map(|c| if separator { "---" } else { c })
                    .collect::<Vec<_>>()
                    .join(" | ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memo_golden() {
        assert_eq!(
            convert(include_str!("../tests/fixtures/owner_docs/cat_memo.html")).unwrap(),
            include_str!("../tests/fixtures/owner_docs/cat_memo.md")
        );
    }
    #[test]
    fn drops_non_content_and_formats_definitions() {
        let md = convert("<head><title>hidden</title></head><style>hidden</style><script>hidden</script><dl><dt>Claim</dt><dd>A record.</dd></dl>").unwrap();
        assert_eq!(md, "**Claim**\n\nA record.");
    }
    #[test]
    fn figures_are_single_lines() {
        let md = convert(r#"<svg aria-label="Chart"><title>ignored</title><text> a <tspan>b</tspan> </text><text> </text><text>c</text></svg><svg><title>Title</title></svg><svg><text>label</text></svg><svg/>"#).unwrap();
        assert_eq!(md, "[figure: Chart] Labels: a b · c\n\n[figure: Title]\n\n[figure: untitled] Labels: label\n\n[figure: untitled]");
    }
    #[test]
    fn tables_preserve_pipes_and_fences() {
        assert_eq!(compact_tables("| wide     | x\\|y |\n| :---- | ---: |\n````text\n| keep    |\n```\n| keep    |\n````\n~~~\n| keep    |\n~~~"), "| wide | x\\|y |\n| --- | --- |\n````text\n| keep    |\n```\n| keep    |\n````\n~~~\n| keep    |\n~~~");
    }
}
