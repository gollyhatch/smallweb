use std::sync::Arc;

use opaline::Theme;
use opaline::widgets::ThemeSelector;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use textwrap::Options;

use crate::app::{App, ArticleState, StatusKind, View};
use crate::article::{Article, BlockKind};

const MAX_TEXT_WIDTH: u16 = 100;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(app: &mut App, frame: &mut Frame<'_>) {
    let theme = opaline::current();
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(color(&theme, "bg.base"))),
        area,
    );

    if let Some(selector) = app.theme_selector.as_mut() {
        frame.render_widget(Clear, area);
        frame.render_stateful_widget(ThemeSelector::new().title("themes"), area, selector);
        return;
    }

    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(area);

    render_header(app, frame, chunks[0], &theme);
    render_separator(frame, chunks[1], &theme);
    render_body(app, frame, chunks[2], &theme);
    render_footer(app, frame, chunks[3], &theme);

    match app.view {
        View::Topics => render_topics(app, frame, &theme),
        View::Help => render_help(app, frame, &theme),
        View::Reader => {}
    }
}

fn render_header(app: &App, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let muted = Style::default().fg(color(theme, "text.muted"));
    let accent = Style::default()
        .fg(color(theme, "accent.primary"))
        .add_modifier(Modifier::BOLD);
    let secondary = Style::default().fg(color(theme, "accent.secondary"));
    let error = Style::default().fg(color(theme, "error"));

    let title = app
        .article_title()
        .map(one_line)
        .unwrap_or_else(|| "Loading posts…".to_string());
    let mut title_spans = theme.gradient_text("primary", "smallweb").spans;
    title_spans.push(Span::styled("  ", muted));
    title_spans.push(Span::styled(title, accent));
    frame.render_widget(Paragraph::new(Line::from(title_spans)), row(area, 0));

    let mut meta = Vec::new();
    if let Some(post) = &app.current {
        if !post.domain.is_empty() {
            meta.push(Span::styled(post.domain.clone(), secondary));
        }
        if !post.author.is_empty() {
            push_separator(&mut meta, muted);
            meta.push(Span::styled(one_line(&post.author), muted));
        }
        if let ArticleState::Ready(article) = &app.article {
            if let Some(published) = article
                .published
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                push_separator(&mut meta, muted);
                meta.push(Span::styled(one_line(published), muted));
            }
        }
        for slug in &post.topics {
            push_separator(&mut meta, muted);
            meta.push(Span::styled(app.topic_name(slug), muted));
        }
        if app.is_liked(&post.url) {
            push_separator(&mut meta, muted);
            meta.push(Span::styled("♥ liked", error));
        }
        if matches!(app.article, ArticleState::Loading) {
            push_separator(&mut meta, muted);
            meta.push(Span::styled(
                format!("{} loading article…", spinner(app.tick)),
                muted,
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(meta)), row(area, 1));

    if let Some(post) = &app.current {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(post.url.clone(), muted))),
            row(area, 2),
        );
    }

    let filter = format!("filter: {}", app.filter_label());
    let theme_label = format!("theme: {}", theme.meta.name);
    let line_area = row(area, 3);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(filter, muted))),
        line_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(theme_label, muted))).alignment(Alignment::Right),
        line_area,
    );
}

fn render_separator(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let style = Style::default().fg(color(theme, "border.unfocused"));
    let width = area.width as usize;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("─".repeat(width), style))),
        area,
    );
}

fn render_body(app: &mut App, frame: &mut Frame<'_>, area: Rect, theme: &Arc<Theme>) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width.saturating_sub(4).clamp(10, MAX_TEXT_WIDTH);
    let text_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y,
        width,
        area.height,
    );
    app.article_height = area.height as usize;

    match &app.article {
        ArticleState::Empty => {
            let message = if app
                .status
                .as_ref()
                .is_some_and(|status| status.kind == StatusKind::Error)
            {
                app.status
                    .as_ref()
                    .map(|status| status.message.clone())
                    .unwrap_or_else(|| "Nothing loaded yet. Press r to retry.".to_string())
            } else {
                format!("{} Loading posts…", spinner(app.tick))
            };
            render_centered_message(frame, text_area, &message, theme, false);
            return;
        }
        ArticleState::Error { url, message } => {
            render_centered_message(frame, text_area, message, theme, true);
            let hint = Rect::new(
                text_area.x,
                text_area.y + (text_area.height / 2).saturating_add(2),
                text_area.width,
                text_area.height.saturating_sub(text_area.height / 2 + 2),
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(url.clone(), muted_style(theme))),
                    Line::from(Span::styled(
                        "press r to retry",
                        Style::default().fg(color(theme, "text.dim")),
                    )),
                ])
                .alignment(Alignment::Center),
                hint,
            );
            return;
        }
        ArticleState::Loading => {
            render_centered_message(
                frame,
                text_area,
                &format!("{} Fetching and extracting the article…", spinner(app.tick)),
                theme,
                false,
            );
            return;
        }
        ArticleState::Ready(_) => {}
    }

    ensure_article_lines(app, width, theme);
    if app.scroll > app.max_scroll() {
        app.scroll = app.max_scroll();
    }

    {
        let buffer = frame.buffer_mut();
        let start = app.scroll.min(app.article_lines.len());
        for (offset, line) in app
            .article_lines
            .iter()
            .skip(start)
            .take(area.height as usize)
            .enumerate()
        {
            buffer.set_line(
                text_area.x,
                text_area.y + offset as u16,
                line,
                text_area.width,
            );
        }
    }

    if app.article_lines.len() > area.height as usize && area.width > 1 {
        let scrollbar_area = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
        let mut state = ScrollbarState::new(app.article_lines.len())
            .viewport_content_length(area.height as usize)
            .position(app.scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(color(theme, "border.unfocused")))
            .thumb_symbol("█")
            .track_symbol(Some("║"));
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }
}

fn render_centered_message(
    frame: &mut Frame<'_>,
    area: Rect,
    message: &str,
    theme: &Theme,
    error: bool,
) {
    if area.height == 0 {
        return;
    }
    let style = if error {
        Style::default().fg(color(theme, "error"))
    } else {
        Style::default().fg(color(theme, "text.muted"))
    };
    let height = area.height.min(4);
    let message_area = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height) / 2,
        area.width,
        height,
    );
    frame.render_widget(
        Paragraph::new(message.to_string())
            .style(style)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        message_area,
    );
}

fn render_footer(app: &App, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let key = Style::default()
        .fg(color(theme, "accent.secondary"))
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(color(theme, "text.muted"));

    let help = Line::from(vec![
        Span::styled("n", key),
        Span::styled(" next  ", label),
        Span::styled("l", key),
        Span::styled(" like  ", label),
        Span::styled("y", key),
        Span::styled(" copy  ", label),
        Span::styled("o", key),
        Span::styled(" open  ", label),
        Span::styled("S", key),
        Span::styled(" share  ", label),
        Span::styled("t", key),
        Span::styled(" topics  ", label),
        Span::styled("T", key),
        Span::styled(" theme  ", label),
        Span::styled("?", key),
        Span::styled(" help  ", label),
        Span::styled("q", key),
        Span::styled(" quit", label),
    ]);
    frame.render_widget(Paragraph::new(help), row(area, 0));

    let (text, style) = if let Some(status) = &app.status {
        let (prefix, token) = match status.kind {
            StatusKind::Info => ("· ", "info"),
            StatusKind::Success => ("✓ ", "success"),
            StatusKind::Warning => ("! ", "warning"),
            StatusKind::Error => ("✗ ", "error"),
        };
        (
            format!("{prefix}{}", status.message),
            Style::default().fg(color(theme, token)),
        )
    } else {
        (
            "j/k scroll · space page · g/G top/bottom · r reload".to_string(),
            Style::default().fg(color(theme, "text.dim")),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, style))),
        row(area, 1),
    );
}

fn render_topics(app: &mut App, frame: &mut Frame<'_>, theme: &Theme) {
    let area = frame.area();
    let popup = centered(area, area.width.min(88), area.height.min(26));
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" Topics — {} selected ", app.topic_draft.len()))
        .border_style(Style::from(theme.style("focused_border")))
        .style(Style::default().bg(color(theme, "bg.panel")));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 3 {
        return;
    }

    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(inner);

    let hint = Paragraph::new(vec![
        Line::from(Span::styled(
            "Filter random posts by topic. No selection means all topics.",
            muted_style(theme),
        )),
        Line::from(Span::styled(
            "Applied filters stay until changed; Enter applies the draft.",
            Style::default().fg(color(theme, "text.dim")),
        )),
    ]);
    frame.render_widget(hint, chunks[0]);

    if app.topics.is_empty() {
        frame.render_widget(
            Paragraph::new("Loading topics…").style(muted_style(theme)),
            chunks[1],
        );
    } else {
        if let Some(selected) = app.topic_list.selected() {
            if selected >= app.topics.len() {
                app.topic_list.select(Some(app.topics.len() - 1));
            }
        } else {
            app.topic_list.select(Some(0));
        }
        let items: Vec<ListItem<'_>> = app
            .topics
            .iter()
            .map(|topic| {
                let checked = app.topic_draft.contains(&topic.slug);
                let mut spans = vec![
                    Span::styled(
                        if checked { "[x] " } else { "[ ] " },
                        Style::default()
                            .fg(color(theme, if checked { "success" } else { "text.dim" })),
                    ),
                    Span::styled(
                        topic.name.clone(),
                        Style::default().fg(color(theme, "text.primary")),
                    ),
                ];
                if !topic.description.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", topic.description),
                        Style::default().fg(color(theme, "text.dim")),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::from(theme.style("active_selected")));
        frame.render_stateful_widget(list, chunks[1], &mut app.topic_list);
    }

    let footer = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("space", key_style(theme)),
            Span::styled(" toggle  ", muted_style(theme)),
            Span::styled("a", key_style(theme)),
            Span::styled(" all  ", muted_style(theme)),
            Span::styled("c", key_style(theme)),
            Span::styled(" clear  ", muted_style(theme)),
            Span::styled("enter", key_style(theme)),
            Span::styled(" apply  ", muted_style(theme)),
            Span::styled("esc", key_style(theme)),
            Span::styled(" cancel", muted_style(theme)),
        ]),
        Line::from(Span::styled(
            "Selected topics are ORed: every next post comes from one of them.",
            Style::default().fg(color(theme, "text.dim")),
        )),
    ]);
    frame.render_widget(footer, chunks[2]);
}

fn render_help(app: &App, frame: &mut Frame<'_>, theme: &Theme) {
    let area = frame.area();
    let popup = centered(area, area.width.min(76), area.height.min(24));
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Keys ")
        .border_style(Style::from(theme.style("focused_border")))
        .style(Style::default().bg(color(theme, "bg.panel")));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let key_style = key_style(theme);
    let label_style = muted_style(theme);
    let rows = [
        ("n / → / ]", "next random post"),
        ("l", "like the post (sent to Kagi)"),
        ("y", "copy the original URL"),
        ("o / enter", "open the original post in your browser"),
        ("S", "run the configured share command with the post URL"),
        ("t", "choose topics"),
        ("T", "choose a theme (live preview)"),
        ("j / k", "scroll one line"),
        ("space / pgdn", "page down"),
        ("pgup", "page up"),
        ("g / G", "jump to top / bottom"),
        ("r", "reload article, or retry loading posts"),
        ("?", "toggle help"),
        ("q", "quit"),
    ];
    let mut lines = Vec::new();
    for (key, description) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("{key:<14}"), key_style),
            Span::styled(description, label_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press esc, ? or q to close.",
        Style::default().fg(color(theme, "text.dim")),
    )));
    lines.push(Line::from(Span::styled(
        format!("config: {}", app.config_path.display()),
        Style::default().fg(color(theme, "text.dim")),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn ensure_article_lines(app: &mut App, width: u16, theme: &Arc<Theme>) {
    let same_theme = app
        .lines_theme
        .as_ref()
        .is_some_and(|cached| Arc::ptr_eq(cached, theme));
    if same_theme && app.lines_width == width {
        return;
    }
    app.article_lines = match &app.article {
        ArticleState::Ready(article) => build_article_lines(article, width as usize, theme),
        _ => Vec::new(),
    };
    app.lines_width = width;
    app.lines_theme = Some(theme.clone());
}

fn build_article_lines(article: &Article, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(10);
    let body = Style::default().fg(color(theme, "text.primary"));
    let heading = Style::default()
        .fg(color(theme, "accent.primary"))
        .add_modifier(Modifier::BOLD);
    let subheading = Style::default()
        .fg(color(theme, "accent.secondary"))
        .add_modifier(Modifier::BOLD);
    let quote = Style::default()
        .fg(color(theme, "text.secondary"))
        .add_modifier(Modifier::ITALIC);
    let code = Style::default()
        .fg(color(theme, "code.string"))
        .bg(color(theme, "bg.code"));
    let caption = Style::default()
        .fg(color(theme, "text.muted"))
        .add_modifier(Modifier::ITALIC);
    let rule = Style::default().fg(color(theme, "border.unfocused"));

    let mut sink = LineSink::default();
    let mut previous: Option<BlockKind> = None;
    for block in &article.blocks {
        if matches!(block.kind, BlockKind::Rule) {
            sink.blank();
            sink.push(Line::from(Span::styled("─".repeat(width), rule)));
            previous = Some(block.kind);
            continue;
        }

        if needs_gap(previous, block.kind) {
            sink.blank();
        }

        match block.kind {
            BlockKind::Heading(level) => {
                let style = if level <= 2 { heading } else { subheading };
                wrap_lines(&mut sink, &block.text, width, "▌ ", "  ", style);
            }
            BlockKind::Paragraph => {
                if block.quote {
                    wrap_lines(&mut sink, &block.text, width, "│ ", "│ ", quote);
                } else {
                    wrap_lines(&mut sink, &block.text, width, "", "", body);
                }
            }
            BlockKind::ListItem(depth) => {
                let indent = "  ".repeat(depth);
                let bullet = format!("{indent}• ");
                let subsequent = " ".repeat(indent.len() + 2);
                wrap_lines(&mut sink, &block.text, width, &bullet, &subsequent, body);
            }
            BlockKind::Code => {
                for line in block.text.lines() {
                    sink.push(Line::from(Span::styled(format!("  {line}"), code)));
                }
            }
            BlockKind::Caption => {
                let style = if block.quote { quote } else { caption };
                wrap_lines(&mut sink, &block.text, width, "  ", "  ", style);
            }
            BlockKind::Rule => unreachable!("rules are handled above"),
        }
        previous = Some(block.kind);
    }
    while sink.lines.last().is_some_and(|line| line.spans.is_empty()) {
        sink.lines.pop();
    }
    sink.lines
}

fn needs_gap(previous: Option<BlockKind>, current: BlockKind) -> bool {
    !matches!(
        (previous, current),
        (None, _)
            | (Some(BlockKind::ListItem(_)), BlockKind::ListItem(_))
            | (Some(BlockKind::Caption), BlockKind::Caption)
    )
}

#[derive(Default)]
struct LineSink {
    lines: Vec<Line<'static>>,
    last_blank: bool,
}

impl LineSink {
    fn blank(&mut self) {
        if !self.lines.is_empty() && !self.last_blank {
            self.lines.push(Line::from(""));
            self.last_blank = true;
        }
    }

    fn push(&mut self, line: Line<'static>) {
        if line.spans.is_empty() {
            self.blank();
            return;
        }
        self.lines.push(line);
        self.last_blank = false;
    }
}

fn wrap_lines(
    sink: &mut LineSink,
    text: &str,
    width: usize,
    initial: &str,
    subsequent: &str,
    style: Style,
) {
    for (index, segment) in text.split('\n').enumerate() {
        if segment.trim().is_empty() {
            sink.blank();
            continue;
        }
        let first = if index == 0 { initial } else { subsequent };
        let options = Options::new(width.max(1))
            .initial_indent(first)
            .subsequent_indent(subsequent);
        for line in textwrap::wrap(segment, options) {
            sink.push(Line::from(Span::styled(line.into_owned(), style)));
        }
    }
}

fn row(area: Rect, index: u16) -> Rect {
    Rect::new(
        area.x,
        area.y + index.min(area.height.saturating_sub(1)),
        area.width,
        1,
    )
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn push_separator(spans: &mut Vec<Span<'static>>, style: Style) {
    if !spans.is_empty() {
        spans.push(Span::styled(" · ", style));
    }
}

fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick as usize) % SPINNER.len()]
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn color(theme: &Theme, token: &str) -> Color {
    theme.color(token).into()
}

fn muted_style(theme: &Theme) -> Style {
    Style::default().fg(color(theme, "text.muted"))
}

fn key_style(theme: &Theme) -> Style {
    Style::default()
        .fg(color(theme, "accent.secondary"))
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::article::Block;

    fn article() -> Article {
        Article {
            title: "Test".to_string(),
            byline: None,
            site_name: None,
            published: None,
            excerpt: None,
            url: "https://example.com".to_string(),
            blocks: vec![
                Block {
                    kind: BlockKind::Heading(1),
                    text: "A heading that is long enough to wrap around a narrow terminal width"
                        .to_string(),
                    quote: false,
                },
                Block {
                    kind: BlockKind::Paragraph,
                    text: "Hello world. ".repeat(20),
                    quote: false,
                },
                Block {
                    kind: BlockKind::ListItem(0),
                    text: "First item".to_string(),
                    quote: false,
                },
                Block {
                    kind: BlockKind::ListItem(1),
                    text: "Nested item".to_string(),
                    quote: false,
                },
                Block {
                    kind: BlockKind::Rule,
                    text: String::new(),
                    quote: false,
                },
                Block {
                    kind: BlockKind::Code,
                    text: "fn main() {\n    println!(\"hi\");\n}".to_string(),
                    quote: false,
                },
            ],
            used_fallback: false,
        }
    }

    #[test]
    fn builds_wrapped_lines_with_structure() {
        let theme = Theme::default();
        let lines = build_article_lines(&article(), 32, &theme);
        assert!(lines.len() > 8);
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("▌ A heading"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("• First item"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("  • Nested item"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("fn main()"))
        );
        assert!(lines.iter().any(|line| line.to_string() == "─".repeat(32)));
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>()
                <= 32
        }));
    }

    #[test]
    fn keeps_hard_line_breaks() {
        let mut sink = LineSink::default();
        wrap_lines(&mut sink, "one\ntwo", 40, "", "", Style::default());
        assert_eq!(sink.lines.len(), 2);
        assert_eq!(sink.lines[0].to_string(), "one");
        assert_eq!(sink.lines[1].to_string(), "two");
    }
}
