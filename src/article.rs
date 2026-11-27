use std::sync::{Arc, LazyLock};

use anyhow::{Result, bail};
use dom_smoothie::Readability;
use reqwest::Url;
use scraper::node::Node;
use scraper::{ElementRef, Html, Selector};

#[derive(Debug, Clone)]
pub struct Article {
    pub title: String,
    pub published: Option<String>,
    pub url: String,
    pub blocks: Vec<Block>,
    pub images: Vec<ArticleImage>,
    pub used_fallback: bool,
}

#[derive(Debug, Clone)]
pub struct ArticleImage {
    pub url: String,
    pub alt: String,
    pub data: Option<Arc<image::RgbaImage>>,
    pub error: Option<String>,
    pub loading: bool,
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
    Image(usize),
}

pub fn extract(html: &str, url: &str, fallback_title: &str) -> Result<Article> {
    match Readability::new(html, Some(url), None).and_then(|mut readability| readability.parse()) {
        Ok(article) => {
            let base_url = article.url.as_deref().unwrap_or(url);
            let (blocks, mut images) = blocks_from_fragment(&article.content);
            if blocks.is_empty() {
                return fallback(html, url, fallback_title, true);
            }
            resolve_images(base_url, &mut images);
            Ok(Article {
                title: non_empty(clean_text(&article.title))
                    .unwrap_or_else(|| fallback_title.to_string()),
                published: article.published_time.map(|value| clean_text(&value)),
                url: clean_text(base_url),
                blocks,
                images,
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
    let mut images = Vec::new();
    if let Some(body) = document.select(&selector("body")).next() {
        walk_children(&body, &mut blocks, &mut images, 0, false);
    }
    if blocks.is_empty() {
        bail!("readability found no article text");
    }
    resolve_images(url, &mut images);
    Ok(Article {
        title,
        published: None,
        url: url.to_string(),
        blocks,
        images,
        used_fallback,
    })
}

fn blocks_from_fragment(fragment: &str) -> (Vec<Block>, Vec<ArticleImage>) {
    let document = Html::parse_fragment(fragment);
    let mut blocks = Vec::new();
    let mut images = Vec::new();
    for child in document.tree.root().children() {
        if let Some(element) = ElementRef::wrap(child) {
            walk_element(&element, &mut blocks, &mut images, 0, false);
        }
    }
    (blocks, images)
}

fn walk_children(
    element: &ElementRef<'_>,
    blocks: &mut Vec<Block>,
    images: &mut Vec<ArticleImage>,
    depth: usize,
    quote: bool,
) {
    let mut inline = String::new();
    for child in element.children() {
        if let Node::Text(text) = child.value() {
            inline.push_str(text);
            continue;
        }
        let Some(child) = ElementRef::wrap(child) else {
            continue;
        };
        if is_inline_tag(child.value().name()) && !contains_image(&child) {
            collect_inline(&child, &mut inline, false);
        } else {
            flush_inline(&mut inline, blocks, quote);
            walk_element(&child, blocks, images, depth, quote);
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

static IMG_SELECTOR: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("img").expect("valid img selector"));

fn contains_image(element: &ElementRef<'_>) -> bool {
    element.select(&IMG_SELECTOR).next().is_some()
}

fn image_source(element: &ElementRef<'_>) -> Option<String> {
    let attrs = element.value();
    let srcsets = ["srcset", "data-srcset", "data-lazy-srcset"];
    let lazy_attrs = ["data-src", "data-lazy-src", "data-original"];
    let has_lazy = lazy_attrs
        .iter()
        .any(|attr| attrs.attr(attr).is_some_and(is_usable_image_url))
        || srcsets.iter().any(|attr| {
            attrs
                .attr(attr)
                .is_some_and(|value| !value.trim().is_empty())
        });
    let tiny = ["width", "height"].iter().all(|attr| {
        attrs
            .attr(attr)
            .and_then(|value| value.parse::<u32>().ok())
            .is_some_and(|value| value <= 1)
    });
    if tiny && !has_lazy {
        return None;
    }

    for attr in lazy_attrs {
        if let Some(value) = attrs
            .attr(attr)
            .map(str::trim)
            .filter(|value| is_usable_image_url(value))
        {
            return Some(value.to_string());
        }
    }
    for attr in srcsets {
        if let Some(srcset) = attrs.attr(attr) {
            for candidate in srcset.split(',').rev() {
                if let Some(value) = candidate
                    .split_whitespace()
                    .next()
                    .filter(|value| is_usable_image_url(value))
                {
                    return Some(value.to_string());
                }
            }
        }
    }
    attrs
        .attr("src")
        .map(str::trim)
        .filter(|value| is_usable_image_url(value))
        .map(str::to_string)
}

fn is_usable_image_url(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with("data:")
        && !value.starts_with("about:")
        && !value.starts_with("blob:")
        && !value.starts_with("javascript:")
}

fn resolve_images(base_url: &str, images: &mut [ArticleImage]) {
    let base = Url::parse(base_url).ok();
    for image in images {
        let resolved = base
            .as_ref()
            .and_then(|base| base.join(&image.url).ok())
            .filter(|url| matches!(url.scheme(), "http" | "https"));
        match resolved {
            Some(url) => image.url = url.to_string(),
            None => {
                image.url.clear();
                image.error = Some("unsupported image URL".to_string());
            }
        }
    }
}

fn walk_element(
    element: &ElementRef<'_>,
    blocks: &mut Vec<Block>,
    images: &mut Vec<ArticleImage>,
    depth: usize,
    quote: bool,
) {
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
            if contains_image(element) {
                walk_children(element, blocks, images, depth, quote);
            } else {
                let text = inline_text(element, false);
                if !text.is_empty() {
                    push(blocks, BlockKind::Paragraph, text, quote);
                }
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
                        walk_children(&child, blocks, images, depth + 1, quote);
                    }
                }
            }
        }
        "ul" | "ol" => walk_children(element, blocks, images, depth, quote),
        "blockquote" => {
            let before = blocks.len();
            walk_children(element, blocks, images, depth, true);
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
        "table" => walk_children(element, blocks, images, depth, quote),
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
            let alt = element
                .value()
                .attr("alt")
                .map(normalize_inline)
                .unwrap_or_default();
            if let Some(src) = image_source(element) {
                let index = images.len();
                images.push(ArticleImage {
                    url: src,
                    alt: alt.clone(),
                    data: None,
                    error: None,
                    loading: false,
                });
                push(blocks, BlockKind::Image(index), alt, quote);
            } else if !alt.is_empty() {
                push(blocks, BlockKind::Caption, alt, quote);
            }
        }
        _ => walk_children(element, blocks, images, depth, quote),
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
          <figure><img src="/pic.png" alt="A diagram"><figcaption>Figure caption</figcaption></figure>
          <p><img data-src="//cdn.example.com/lazy.jpg" alt="Lazy"></p>
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
        assert!(
            article
                .blocks
                .iter()
                .any(|block| block.kind == BlockKind::Image(0))
        );
        assert_eq!(article.images.len(), 2);
        assert_eq!(article.images[0].url, "https://example.com/pic.png");
        assert_eq!(article.images[0].alt, "A diagram");
        assert_eq!(article.images[1].url, "https://cdn.example.com/lazy.jpg");
        assert_eq!(article.images[1].alt, "Lazy");
    }

    #[test]
    fn picks_lazy_and_srcset_image_urls() {
        let html = r#"<html><body>
          <p><img srcset="small.png 1x, large.png 2x" alt="Large"></p>
          <p><img width="1" height="1" src="/pixel.gif"></p>
        </body></html>"#;
        let article = fallback(html, "https://example.com/post", "Fallback", true).unwrap();
        assert_eq!(article.images.len(), 1);
        assert_eq!(article.images[0].url, "https://example.com/large.png");
        assert_eq!(article.images[0].alt, "Large");
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
