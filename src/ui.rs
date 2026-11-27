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
use ratatui_image::sliced::{SignedPosition, SlicedImage};
use textwrap::Options;

use crate::app::{App, ArticleImageLayout, ArticleState, StatusKind, View, history_matches};
use crate::article::{Article, ArticleImage, BlockKind};

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
        Constraint::Length(1),
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

    if app.history_search.is_some() {
        render_history(app, frame, &theme);
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
        if app.is_flagged(&post.url) {
            push_separator(&mut meta, muted);
            meta.push(Span::styled(
                "⚑ flagged",
                Style::default().fg(color(theme, "warning")),
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

    if app.config.images {
        if let ArticleState::Ready(article) = &app.article {
            for layout in &app.article_images {
                if layout.width == 0 || layout.height == 0 {
                    continue;
                }
                let relative_y = layout.line as isize - app.scroll as isize;
                if relative_y >= area.height as isize || relative_y + layout.height as isize <= 0 {
                    continue;
                }
                let Some(protocol) = article
                    .images
                    .get(layout.index)
                    .and_then(|image| image.protocol.as_ref())
                else {
                    continue;
                };
                let x = text_area.x.saturating_sub(area.x)
                    + text_area.width.saturating_sub(layout.width) / 2;
                let y = relative_y.clamp(i16::MIN as isize, i16::MAX as isize) as i16;
                let position = SignedPosition::from((x as i16, y));
                frame.render_widget(SlicedImage::new(protocol, position), area);
            }
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
        Span::styled(" next ", label),
        Span::styled("b", key),
        Span::styled(" back ", label),
        Span::styled("l", key),
        Span::styled(" like ", label),
        Span::styled("F", key),
        Span::styled(" flag ", label),
        Span::styled("y", key),
        Span::styled(" copy ", label),
        Span::styled("Y", key),
        Span::styled(" web ", label),
        Span::styled("o", key),
        Span::styled(" open ", label),
        Span::styled("S", key),
        Span::styled(" share ", label),
        Span::styled("/", key),
        Span::styled(" search ", label),
        Span::styled("i", key),
        Span::styled(" img ", label),
        Span::styled("H", key),
        Span::styled(" hist ", label),
        Span::styled("t", key),
        Span::styled(" topics ", label),
        Span::styled("?", key),
        Span::styled(" help ", label),
        Span::styled("q", key),
        Span::styled(" quit", label),
    ]);
    let line = if let Some(input) = &app.search_input {
        Line::from(vec![
            Span::styled(
                format!("/{input}▏"),
                Style::default().fg(color(theme, "accent.primary")),
            ),
            Span::styled("  enter search · esc cancel", muted_style(theme)),
        ])
    } else if let Some(search) = &app.article_search {
        let count = search.matches.len();
        let (message, style) = if count == 0 {
            (
                format!(
                    "search \"{}\" · no matches · / edit · esc clear",
                    search.query
                ),
                Style::default().fg(color(theme, "warning")),
            )
        } else {
            (
                format!(
                    "search \"{}\" · {}/{} · n next · N previous · esc clear",
                    search.query,
                    search.current + 1,
                    count
                ),
                Style::default().fg(color(theme, "accent.secondary")),
            )
        };
        Line::from(Span::styled(message, style))
    } else if let Some(status) = &app.status {
        let (prefix, token) = match status.kind {
            StatusKind::Info => ("· ", "info"),
            StatusKind::Success => ("✓ ", "success"),
            StatusKind::Warning => ("! ", "warning"),
            StatusKind::Error => ("✗ ", "error"),
        };
        Line::from(Span::styled(
            format!("{prefix}{}", status.message),
            Style::default().fg(color(theme, token)),
        ))
    } else {
        help
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_history(app: &mut App, frame: &mut Frame<'_>, theme: &Theme) {
    let area = frame.area();
    let popup = centered(area, area.width.min(100), area.height.min(30));
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" History ")
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

    let query = app
        .history_search
        .as_ref()
        .map(|search| search.query.clone())
        .unwrap_or_default();
    let selected = app
        .history_search
        .as_ref()
        .and_then(|search| search.list.selected())
        .unwrap_or(0);
    let results = history_matches(&app.seen_posts, &query);
    let count = results.len();
    let muted = muted_style(theme);
    let accent = Style::default().fg(color(theme, "accent.primary"));

    let search_line = Line::from(vec![
        Span::styled("search: ", muted),
        Span::styled(query, accent),
        Span::styled("▏", accent),
    ]);
    frame.render_widget(Paragraph::new(search_line), row(chunks[0], 0));
    let count_label = format!("{count} result{}", if count == 1 { "" } else { "s" });
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(count_label, muted))).alignment(Alignment::Right),
        row(chunks[0], 0),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Type to fuzzy-search by title, domain, author, URL or topic",
            Style::default().fg(color(theme, "text.dim")),
        ))),
        row(chunks[0], 1),
    );

    if count == 0 {
        frame.render_widget(Paragraph::new("No matching posts.").style(muted), chunks[1]);
    } else {
        let current_url = app.current.as_ref().map(|post| post.url.as_str());
        let items: Vec<ListItem<'_>> = results
            .iter()
            .map(|post| {
                let mut spans = vec![Span::styled(
                    one_line(&post.title),
                    Style::default().fg(color(theme, "text.primary")),
                )];
                if !post.domain.is_empty() {
                    spans.push(Span::styled(format!("  {}", post.domain), muted));
                }
                if current_url == Some(post.url.as_str()) {
                    spans.push(Span::styled(
                        "  (current)",
                        Style::default().fg(color(theme, "accent.secondary")),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let selected = selected.min(count - 1);
        let search = app.history_search.as_mut().expect("history search is open");
        search.list.select(Some(selected));
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::from(theme.style("active_selected")));
        frame.render_stateful_widget(list, chunks[1], &mut search.list);
    }

    let footer = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("type", key_style(theme)),
            Span::styled(" filter  ", muted),
            Span::styled("↑↓", key_style(theme)),
            Span::styled(" select  ", muted),
            Span::styled("enter", key_style(theme)),
            Span::styled(" open ", muted),
            Span::styled("ctrl+s", key_style(theme)),
            Span::styled(" share ", muted),
            Span::styled("esc", key_style(theme)),
            Span::styled(" close", muted),
        ]),
        Line::from(Span::styled(
            "History is saved across sessions.",
            Style::default().fg(color(theme, "text.dim")),
        )),
    ]);
    frame.render_widget(footer, chunks[2]);
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
            "Enter applies the draft and saves it to the config file.",
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
        ("b / ←", "go back to the previous post"),
        ("/", "search the article text"),
        ("n / N", "next / previous search match (while searching)"),
        ("l", "toggle like on the post (sent to Kagi)"),
        ("F", "flag the post as inappropriate (sent to Kagi)"),
        ("y", "copy the original URL"),
        ("Y", "copy the Small Web URL for the post"),
        ("o / enter", "open the original post in your browser"),
        ("S", "run the configured share command with the post URL"),
        ("i", "toggle inline images"),
        ("H", "fuzzy-search the history of seen posts"),
        ("t", "choose topics"),
        ("T", "choose and save a theme (live preview)"),
        ("j / k", "scroll one line"),
        ("d / u", "scroll half a page down / up"),
        ("space", "scroll to the next paragraph"),
        ("pgdn", "page down"),
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
    if app.config.images {
        app.prepare_image_protocols(width);
    }
    let (lines, images) = match &app.article {
        ArticleState::Ready(article) => {
            build_article_lines(article, width as usize, theme, app.config.images)
        }
        _ => (Vec::new(), Vec::new()),
    };
    app.article_lines = lines;
    app.article_images = images;
    app.lines_width = width;
    app.lines_theme = Some(theme.clone());
    app.refresh_article_search();
}

fn build_article_lines(
    article: &Article,
    width: usize,
    theme: &Theme,
    images_enabled: bool,
) -> (Vec<Line<'static>>, Vec<ArticleImageLayout>) {
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
    let mut placed_images = Vec::new();
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
            BlockKind::Image(index) => {
                if !images_enabled {
                    let alt = article
                        .images
                        .get(index)
                        .map(|image| image.alt.as_str())
                        .unwrap_or(&block.text);
                    let message = if alt.is_empty() {
                        "[image]".to_string()
                    } else {
                        format!("[image: {alt}]")
                    };
                    wrap_lines(&mut sink, &message, width, "", "", caption);
                } else if let Some(image) = article.images.get(index) {
                    if let Some(protocol) = &image.protocol {
                        let size = protocol.size();
                        let line = sink.lines.len();
                        for _ in 0..size.height {
                            sink.lines.push(Line::from(""));
                        }
                        sink.last_blank = false;
                        placed_images.push(ArticleImageLayout {
                            index,
                            line,
                            width: size.width,
                            height: size.height,
                        });
                    } else {
                        render_image_placeholder(&mut sink, image, width, theme);
                    }
                } else if !block.text.is_empty() {
                    wrap_lines(&mut sink, &block.text, width, "  ", "  ", caption);
                }
            }
            BlockKind::Rule => unreachable!("rules are handled above"),
        }
        previous = Some(block.kind);
    }
    (sink.lines, placed_images)
}

fn render_image_placeholder(
    sink: &mut LineSink,
    image: &ArticleImage,
    width: usize,
    theme: &Theme,
) {
    let caption = Style::default()
        .fg(color(theme, "text.muted"))
        .add_modifier(Modifier::ITALIC);
    let alt = if image.alt.is_empty() {
        "image"
    } else {
        image.alt.as_str()
    };
    let (message, style) = match (image.error.as_deref(), image.data.is_some()) {
        (Some(error), _) => (
            format!("[image: {alt} — {error}]"),
            Style::default().fg(color(theme, "warning")),
        ),
        (None, true) => (format!("[image: {alt} — preparing…]"), caption),
        (None, false) => (format!("[image: {alt} — loading…]"), caption),
    };
    wrap_lines(sink, &message, width, "", "", style);
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
            published: None,
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
            images: Vec::new(),
            used_fallback: false,
        }
    }

    #[test]
    fn builds_wrapped_lines_with_structure() {
        let theme = Theme::default();
        let (lines, images) = build_article_lines(&article(), 32, &theme, false);
        assert!(images.is_empty());
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
    fn lays_out_images_from_ratatui_image_protocols() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            64,
            32,
            image::Rgba([255, 0, 0, 255]),
        ));
        let protocol = ratatui_image::sliced::SlicedProtocol::new_with_resize(
            &picker,
            source,
            ratatui::layout::Size::new(20, 10),
            ratatui_image::Resize::Fit(None),
        )
        .unwrap();
        let size = protocol.size();
        let image = ArticleImage {
            url: "https://example.com/image.png".to_string(),
            alt: "Test image".to_string(),
            data: None,
            protocol: Some(protocol),
            protocol_width: Some(20),
            error: None,
            loading: false,
        };
        let mut article = article();
        article.blocks = vec![Block {
            kind: BlockKind::Image(0),
            text: "Test image".to_string(),
            quote: false,
        }];
        article.images = vec![image];

        let (lines, images) = build_article_lines(&article, 20, &Theme::default(), true);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].width, size.width);
        assert_eq!(images[0].height, size.height);
        assert!(lines.len() >= size.height as usize);
    }

    #[test]
    fn renders_image_placeholders() {
        let mut article = article();
        article.blocks = vec![Block {
            kind: BlockKind::Image(0),
            text: "Missing".to_string(),
            quote: false,
        }];
        article.images = vec![ArticleImage {
            url: String::new(),
            alt: "Missing".to_string(),
            data: None,
            protocol: None,
            protocol_width: None,
            error: Some("HTTP 404".to_string()),
            loading: false,
        }];
        let (lines, images) = build_article_lines(&article, 40, &Theme::default(), true);
        assert!(images.is_empty());
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("HTTP 404"))
        );
    }

    #[test]
    fn hides_images_with_alt_placeholders_when_disabled() {
        let mut article = article();
        article.blocks = vec![Block {
            kind: BlockKind::Image(0),
            text: "Test image".to_string(),
            quote: false,
        }];
        article.images = vec![ArticleImage {
            url: "https://example.com/image.png".to_string(),
            alt: "Test image".to_string(),
            data: Some(Arc::new(image::RgbaImage::from_pixel(
                4,
                4,
                image::Rgba([255, 0, 0, 255]),
            ))),
            protocol: None,
            protocol_width: None,
            error: None,
            loading: false,
        }];
        let (lines, images) = build_article_lines(&article, 40, &Theme::default(), false);
        assert!(images.is_empty());
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("[image: Test image]"))
        );
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
