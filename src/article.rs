use anyhow::{Result, bail};
use dom_smoothie::Readability;
use reqwest::Url;
use scraper::node::Node;
use scraper::{ElementRef, Html, Selector};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article {
    pub title: String,
    pub byline: Option<String>,
    pub site_name: Option<String>,
    pub published: Option<String>,
    pub excerpt: Option<String>,
    pub url: String,
    pub blocks: Vec<Block>,
    pub used_fallback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
    pub quote: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    ListItem(usize),
    Code,
    Caption,
    Rule,
}

pub fn extract(html: &str, url: &str, fallback_title: &str) -> Result<Article> {
    match Readability::new(html, Some(url), None).and_then(|mut readability| readability.parse()) {
        Ok(article) => {
            let blocks = blocks_from_fragment(&article.content);
            if blocks.is_empty() {
                return fallback(html, url, fallback_title, true);
            }
            Ok(Article {
                title: non_empty(clean_text(&article.title))
                    .unwrap_or_else(|| fallback_title.to_string()),
                byline: article.byline.map(|value| clean_text(&value)),
                site_name: article
                    .site_name
                    .map(|value| clean_text(&value))
                    .or_else(|| host(url)),
                published: article.published_time.map(|value| clean_text(&value)),
                excerpt: article.excerpt.map(|value| clean_text(&value)),
                url: clean_text(article.url.as_deref().unwrap_or(url)),
                blocks,
                used_fallback: false,
            })
        }
        Err(_) => fallback(html, url, fallback_title, true),
    }
}

fn fallback(html: &str, url: &str, fallback_title: &str, used_fallback: bool) -> Result<Article> {
    let document = Html::parse_document(html);
    let title = document
        .select(&selector("title"))
        .next()
        .map(|element| normalize_inline(&element.text().collect::<String>()))
        .and_then(non_empty)
        .unwrap_or_else(|| fallback_title.to_string());

    let mut blocks = Vec::new();
    if let Some(body) = document.select(&selector("body")).next() {
        walk_children(&body, &mut blocks, 0, false);
    }
    if blocks.is_empty() {
        bail!("readability found no article text");
    }
    Ok(Article {
        title,
        byline: None,
        site_name: host(url),
        published: None,
        excerpt: None,
        url: url.to_string(),
        blocks,
        used_fallback,
    })
}

fn blocks_from_fragment(fragment: &str) -> Vec<Block> {
    let document = Html::parse_fragment(fragment);
    let mut blocks = Vec::new();
    for child in document.tree.root().children() {
        if let Some(element) = ElementRef::wrap(child) {
            walk_element(&element, &mut blocks, 0, false);
        }
    }
    blocks
}

fn walk_children(element: &ElementRef<'_>, blocks: &mut Vec<Block>, depth: usize, quote: bool) {
    let mut inline = String::new();
    for child in element.children() {
        if let Node::Text(text) = child.value() {
            inline.push_str(text);
            continue;
        }
        let Some(child) = ElementRef::wrap(child) else {
            continue;
        };
        if is_inline_tag(child.value().name()) {
            collect_inline(&child, &mut inline, false);
        } else {
            flush_inline(&mut inline, blocks, quote);
            walk_element(&child, blocks, depth, quote);
        }
    }
    flush_inline(&mut inline, blocks, quote);
}

fn flush_inline(inline: &mut String, blocks: &mut Vec<Block>, quote: bool) {
    let text = normalize_inline(inline);
    inline.clear();
    if !text.is_empty() {
        push(blocks, BlockKind::Paragraph, text, quote);
    }
}

fn is_inline_tag(tag: &str) -> bool {
    matches!(
        tag,
        "a" | "abbr"
            | "b"
            | "bdi"
            | "bdo"
            | "br"
            | "cite"
            | "code"
            | "del"
            | "em"
            | "i"
            | "ins"
            | "kbd"
            | "mark"
            | "q"
            | "rp"
            | "rt"
            | "ruby"
            | "s"
            | "samp"
            | "small"
            | "span"
            | "strong"
            | "sub"
            | "sup"
            | "time"
            | "u"
            | "var"
            | "wbr"
    )
}

fn walk_element(element: &ElementRef<'_>, blocks: &mut Vec<Block>, depth: usize, quote: bool) {
    let tag = element.value().name();
    match tag {
        "script" | "style" | "noscript" | "svg" | "iframe" | "canvas" | "form" | "nav"
        | "header" | "footer" | "aside" | "button" | "input" | "select" | "textarea" => {}
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = tag.as_bytes()[1] - b'0';
            let text = inline_text(element, false);
            if !text.is_empty() {
                push(blocks, BlockKind::Heading(level), text, quote);
            }
        }
        "p" => {
            let text = inline_text(element, false);
            if !text.is_empty() {
                push(blocks, BlockKind::Paragraph, text, quote);
            }
        }
        "li" => {
            let text = inline_text(element, true);
            if !text.is_empty() {
                push(blocks, BlockKind::ListItem(depth), text, quote);
            }
            for child in element.children() {
                if let Some(child) = ElementRef::wrap(child) {
                    if matches!(child.value().name(), "ul" | "ol") {
                        walk_children(&child, blocks, depth + 1, quote);
                    }
                }
            }
        }
        "ul" | "ol" => walk_children(element, blocks, depth, quote),
        "blockquote" => {
            let before = blocks.len();
            walk_children(element, blocks, depth, true);
            if blocks.len() == before {
                let text = inline_text(element, false);
                if !text.is_empty() {
                    push(blocks, BlockKind::Paragraph, text, true);
                }
            }
        }
        "pre" => {
            let text = normalize_pre(&element.text().collect::<String>());
            if !text.is_empty() {
                push(blocks, BlockKind::Code, text, quote);
            }
        }
        "hr" => blocks.push(Block {
            kind: BlockKind::Rule,
            text: String::new(),
            quote,
        }),
        "table" => walk_children(element, blocks, depth, quote),
        "tr" => {
            let cells: Vec<String> = element
                .children()
                .filter_map(ElementRef::wrap)
                .filter(|cell| matches!(cell.value().name(), "td" | "th"))
                .map(|cell| inline_text(&cell, false))
                .filter(|text| !text.is_empty())
                .collect();
            if !cells.is_empty() {
                push(blocks, BlockKind::Paragraph, cells.join(" · "), quote);
            }
        }
        "dt" | "summary" => {
            let text = inline_text(element, false);
            if !text.is_empty() {
                push(blocks, BlockKind::Heading(3), text, quote);
            }
        }
        "dd" => {
            let text = inline_text(element, false);
            if !text.is_empty() {
                push(blocks, BlockKind::Paragraph, text, quote);
            }
        }
        "figcaption" => {
            let text = inline_text(element, false);
            if !text.is_empty() {
                push(blocks, BlockKind::Caption, text, quote);
            }
        }
        "img" => {
            if let Some(alt) = element
                .value()
                .attr("alt")
                .map(str::trim)
                .filter(|alt| !alt.is_empty())
            {
                push(blocks, BlockKind::Caption, alt.to_string(), quote);
            }
        }
        _ => walk_children(element, blocks, depth, quote),
    }
}

fn push(blocks: &mut Vec<Block>, kind: BlockKind, text: String, quote: bool) {
    blocks.push(Block { kind, text, quote });
}

fn inline_text(element: &ElementRef<'_>, skip_lists: bool) -> String {
    let mut raw = String::new();
    collect_inline(element, &mut raw, skip_lists);
    normalize_inline(&raw)
}

fn collect_inline(node: &ElementRef<'_>, out: &mut String, skip_lists: bool) {
    for child in node.children() {
        if let Node::Text(text) = child.value() {
            out.push_str(text);
            continue;
        }
        let Some(element) = ElementRef::wrap(child) else {
            continue;
        };
        let tag = element.value().name();
        if skip_lists && matches!(tag, "ul" | "ol") {
            continue;
        }
        match tag {
            "br" => out.push('\n'),
            "img" => {
                if let Some(alt) = element.value().attr("alt") {
                    out.push(' ');
                    out.push_str(alt);
                    out.push(' ');
                }
            }
            "script" | "style" | "noscript" | "svg" => {}
            _ => collect_inline(&element, out, skip_lists),
        }
    }
}

fn clean_text(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn normalize_inline(raw: &str) -> String {
    let raw = clean_text(raw);
    raw.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn normalize_pre(raw: &str) -> String {
    clean_text(raw)
        .replace('\t', "    ")
        .trim_matches('\n')
        .to_string()
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn host(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

fn selector(value: &str) -> Selector {
    Selector::parse(value).expect("valid built-in selector")
}

#[cfg(test)]
mod tests {
    use super::*;

    const POST: &str = r#"
        <html><head><title>Fallback title</title></head><body>
        <nav>Navigation noise</nav>
        <article>
          <h1>Readable heading</h1>
          <p>Hello <strong>world</strong> &amp; friends.<br>Second line.</p>
          <blockquote><p>Quoted wisdom.</p></blockquote>
          <ul>
            <li>First item
              <ul><li>Nested item</li></ul>
            </li>
            <li>Second item</li>
          </ul>
          <pre>fn main() {
    println!("hi");
}</pre>
          <hr>
          <figure><img alt="A diagram"><figcaption>Figure caption</figcaption></figure>
          <div>Stray text<br>second line</div>
        </article>
        </body></html>
    "#;

    #[test]
    fn extracts_structured_blocks() {
        let article = fallback(POST, "https://example.com/post", "Fallback", true).unwrap();
        assert_eq!(article.title, "Fallback title");
        assert!(
            !article
                .blocks
                .iter()
                .any(|block| block.text.contains("Navigation"))
        );
        assert!(article.blocks.iter().any(|block| {
            block.kind == BlockKind::Heading(1) && block.text == "Readable heading"
        }));
        assert!(article.blocks.iter().any(|block| {
            block.kind == BlockKind::Paragraph
                && block.text == "Hello world & friends.\nSecond line."
        }));
        assert!(article.blocks.iter().any(|block| {
            block.kind == BlockKind::Paragraph && block.quote && block.text == "Quoted wisdom."
        }));
        assert!(
            article.blocks.iter().any(|block| {
                block.kind == BlockKind::ListItem(0) && block.text == "First item"
            })
        );
        assert!(
            article.blocks.iter().any(|block| {
                block.kind == BlockKind::ListItem(1) && block.text == "Nested item"
            })
        );
        assert!(
            article
                .blocks
                .iter()
                .any(|block| { block.kind == BlockKind::Code && block.text.contains("fn main()") })
        );
        assert!(
            article.blocks.iter().any(|block| {
                block.kind == BlockKind::Caption && block.text == "Figure caption"
            })
        );
    }

    #[test]
    fn normalizes_preformatted_text() {
        assert_eq!(normalize_pre("\n\tx\n\n"), "    x");
    }

    #[test]
    fn strips_terminal_control_characters() {
        assert_eq!(normalize_inline("hello\u{1b}[31mred"), "hello [31mred");
        assert_eq!(normalize_pre("a\u{0}b\n\tc"), "a b\n    c");
    }
}
