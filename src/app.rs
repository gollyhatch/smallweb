use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::Cursor;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use copypasta::{ClipboardContext, ClipboardProvider};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use opaline::Theme;
use opaline::widgets::{ThemeSelectorAction, ThemeSelectorState};
use ratatui::layout::Size;
use ratatui::text::Line;
use ratatui::widgets::ListState;
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::sliced::SlicedProtocol;
use reqwest::Url;

use crate::api::{Api, FALLBACK_TOPICS, LikeOutcome, Post, Topic};
use crate::article::{self, Article};
use crate::config::{Config, save_theme, save_topics};
use crate::history;

const QUEUE_TARGET: usize = 4;
const HISTORY_LIMIT: usize = 100;
const SEEN_LIMIT: usize = 200;
const MAX_IMAGES: usize = 12;
const MAX_IMAGE_WIDTH: u32 = 1200;
const MAX_IMAGE_HEIGHT: u32 = 1200;
const MAX_IMAGE_ROWS: u16 = 28;
const STATUS_TTL: Duration = Duration::from_secs(7);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Reader,
    Topics,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug)]
pub struct Status {
    pub message: String,
    pub kind: StatusKind,
    pub at: Instant,
}

pub enum ArticleState {
    Empty,
    Loading,
    Ready(Article),
    Error { url: String, message: String },
}

pub struct App {
    pub api: Api,
    picker: Picker,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pub config: Config,
    pub config_path: PathBuf,
    history_path: PathBuf,
    pub current: Option<Post>,
    pub queue: VecDeque<Post>,
    pub article: ArticleState,
    pub topics: Vec<Topic>,
    pub selected_topics: BTreeSet<String>,
    pub topic_draft: BTreeSet<String>,
    pub topic_list: ListState,
    pub view: View,
    pub theme_selector: Option<ThemeSelectorState>,
    pub status: Option<Status>,
    pub scroll: usize,
    pub article_lines: Vec<Line<'static>>,
    pub article_images: Vec<ArticleImageLayout>,
    pub lines_width: u16,
    pub lines_theme: Option<Arc<Theme>>,
    pub article_height: usize,
    pub liked: HashSet<String>,
    pub liking: HashSet<String>,
    flagged: HashSet<String>,
    flagging: HashSet<String>,
    history: Vec<Post>,
    pub seen_posts: Vec<Post>,
    seen: HashSet<String>,
    pub history_search: Option<HistorySearch>,
    pub search_input: Option<String>,
    pub article_search: Option<ArticleSearch>,
    clipboard: Option<ClipboardContext>,
    pub quit: bool,
    pub tick: u64,
    epoch: u64,
    topic_rotation: usize,
    fetching_deck: bool,
    pending_next: bool,
}

impl App {
    pub fn new(
        config: Config,
        config_path: PathBuf,
        selected_topics: Vec<String>,
        picker: Picker,
    ) -> Result<Self> {
        let api = Api::new()?;
        let (tx, rx) = mpsc::channel();
        let selected_topics: BTreeSet<String> = selected_topics
            .into_iter()
            .map(|topic| topic.trim().to_string())
            .filter(|topic| !topic.is_empty())
            .collect();
        let history_path = history::default_path();
        let stored = history::load(&history_path)
            .ok()
            .flatten()
            .or_else(|| {
                history::load(&history::legacy_path(&config_path))
                    .ok()
                    .flatten()
            })
            .unwrap_or_default();
        let seen_posts = history::normalize(stored.seen, SEEN_LIMIT);
        let mut back = history::normalize_back(stored.back, HISTORY_LIMIT);
        if let Some(last) = seen_posts.last() {
            if back.last().map(|post| post.url.as_str()) != Some(last.url.as_str()) {
                back.push(last.clone());
                if back.len() > HISTORY_LIMIT {
                    let overflow = back.len() - HISTORY_LIMIT;
                    back.drain(..overflow);
                }
            }
        }
        let seen: HashSet<String> = seen_posts
            .iter()
            .chain(back.iter())
            .map(|post| post.url.clone())
            .collect();
        let mut app = Self {
            api,
            picker,
            tx,
            rx,
            config,
            config_path,
            history_path,
            current: None,
            queue: VecDeque::new(),
            article: ArticleState::Empty,
            topics: fallback_topics(),
            selected_topics,
            topic_draft: BTreeSet::new(),
            topic_list: ListState::default(),
            view: View::Reader,
            theme_selector: None,
            status: None,
            scroll: 0,
            article_lines: Vec::new(),
            article_images: Vec::new(),
            lines_width: 0,
            lines_theme: None,
            article_height: 10,
            liked: HashSet::new(),
            liking: HashSet::new(),
            flagged: HashSet::new(),
            flagging: HashSet::new(),
            history: back,
            seen_posts,
            seen,
            history_search: None,
            search_input: None,
            article_search: None,
            clipboard: None,
            quit: false,
            tick: 0,
            epoch: 0,
            topic_rotation: 0,
            fetching_deck: false,
            pending_next: false,
        };
        app.spawn_topics();
        app.refresh_feed();
        Ok(app)
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if let Some(status) = &self.status {
            if status.kind != StatusKind::Error && status.at.elapsed() > STATUS_TTL {
                self.status = None;
            }
        }
    }

    pub fn poll_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.handle_message(message);
        }
    }

    fn handle_message(&mut self, message: Msg) {
        match message {
            Msg::Deck {
                epoch,
                topic,
                result,
            } => {
                if epoch != self.epoch {
                    return;
                }
                self.fetching_deck = false;
                match result {
                    Ok(posts) => {
                        let mut appended = 0;
                        for post in posts {
                            if self.seen.contains(&post.url)
                                || self.queue.iter().any(|queued| queued.url == post.url)
                            {
                                continue;
                            }
                            self.queue.push_back(post);
                            appended += 1;
                        }
                        if self.current.is_none() {
                            if let Some(post) = self.queue.pop_front() {
                                self.show_post(post);
                            }
                        } else if self.pending_next {
                            if let Some(post) = self.queue.pop_front() {
                                self.pending_next = false;
                                self.remember_current();
                                self.show_post(post);
                            }
                        }
                        if appended > 0 && self.queue.len() < QUEUE_TARGET {
                            self.ensure_queue();
                        } else if self.current.is_none() || self.pending_next {
                            let retry_key = if self.current.is_some() { "n" } else { "r" };
                            self.pending_next = false;
                            let suffix = topic
                                .as_deref()
                                .map(|topic| format!(" for topic '{topic}'"))
                                .unwrap_or_default();
                            self.set_status(
                                StatusKind::Warning,
                                format!("No posts{suffix}; press {retry_key} to retry"),
                            );
                        }
                    }
                    Err(error) => {
                        self.set_status(
                            StatusKind::Error,
                            format!("Could not load posts: {error}"),
                        );
                    }
                }
            }
            Msg::Article { url, result } => {
                if !self.current.as_ref().is_some_and(|post| post.url == url) {
                    return;
                }
                match result {
                    Ok(mut article) => {
                        let fallback = article.used_fallback;
                        for (index, image) in article.images.iter_mut().enumerate() {
                            if index >= MAX_IMAGES && image.error.is_none() {
                                image.error = Some("image limit reached".to_string());
                            }
                        }
                        let mut jobs = Vec::new();
                        if self.config.images {
                            for (index, image) in
                                article.images.iter_mut().enumerate().take(MAX_IMAGES)
                            {
                                if !image.url.is_empty()
                                    && !image.loading
                                    && image.data.is_none()
                                    && image.error.is_none()
                                {
                                    image.loading = true;
                                    jobs.push((index, image.url.clone()));
                                }
                            }
                        }
                        let referer = article.url.clone();
                        self.article = ArticleState::Ready(article);
                        self.reset_article_view();
                        self.spawn_images(url.clone(), referer, jobs);
                        if fallback {
                            self.set_status(
                                StatusKind::Warning,
                                "Readability failed; showing the page's raw text",
                            );
                        } else {
                            self.status = None;
                        }
                    }
                    Err(error) => {
                        self.article = ArticleState::Error {
                            url,
                            message: error.clone(),
                        };
                        self.reset_article_view();
                        self.set_status(
                            StatusKind::Error,
                            format!("Could not extract article: {error}"),
                        );
                    }
                }
            }
            Msg::Image {
                article_url,
                index,
                result,
            } => {
                if !self
                    .current
                    .as_ref()
                    .is_some_and(|post| post.url == article_url)
                {
                    return;
                }
                if let ArticleState::Ready(article) = &mut self.article {
                    if let Some(image) = article.images.get_mut(index) {
                        image.loading = false;
                        match result {
                            Ok(data) => {
                                image.data = Some(data);
                                image.error = None;
                            }
                            Err(error) => image.error = Some(error),
                        }
                        self.lines_width = 0;
                    }
                }
            }
            Msg::Flag { url, result } => {
                self.flagging.remove(&url);
                match result {
                    Ok(()) => {
                        self.flagged.insert(url);
                        self.set_status(StatusKind::Success, "Post flagged");
                    }
                    Err(error) => {
                        self.set_status(StatusKind::Error, format!("Could not flag post: {error}"));
                    }
                }
            }
            Msg::Like {
                url,
                action,
                result,
            } => {
                self.liking.remove(&url);
                match result {
                    Ok(outcome) => {
                        let label = match action {
                            LikeAction::Like => {
                                self.liked.insert(url);
                                "♥ liked"
                            }
                            LikeAction::Unlike => {
                                self.liked.remove(&url);
                                "♡ unliked"
                            }
                        };
                        let message = if outcome.likes_total > 0 {
                            format!("{label} · {} total", outcome.likes_total)
                        } else {
                            label.to_string()
                        };
                        self.set_status(StatusKind::Success, message);
                    }
                    Err(error) => {
                        let action = match action {
                            LikeAction::Like => "like",
                            LikeAction::Unlike => "unlike",
                        };
                        self.set_status(
                            StatusKind::Error,
                            format!("Could not {action} post: {error}"),
                        );
                    }
                }
            }
            Msg::Topics(result) => match result {
                Ok(topics) if !topics.is_empty() => {
                    self.topics = topics;
                    if self.topic_list.selected().is_none() {
                        self.topic_list.select(Some(0));
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    self.set_status(
                        StatusKind::Warning,
                        format!("Could not refresh topics: {error}"),
                    );
                }
            },
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }

        if self.theme_selector.is_some() {
            self.handle_theme_key(key);
            return;
        }

        if self.history_search.is_some() {
            self.handle_history_key(key);
            return;
        }

        if self.search_input.is_some() {
            self.handle_search_input(key);
            return;
        }

        match self.view {
            View::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?')
                ) {
                    self.view = View::Reader;
                }
            }
            View::Topics => self.handle_topic_key(key),
            View::Reader => self.handle_reader_key(key),
        }
    }

    fn handle_theme_key(&mut self, key: KeyEvent) {
        let (action, selected) = {
            let selector = self
                .theme_selector
                .as_mut()
                .expect("theme selector is open");
            (selector.handle_key(key), selector.selected_theme().cloned())
        };
        match action {
            ThemeSelectorAction::Select(id) => {
                self.theme_selector = None;
                let save_result = save_theme(&self.config_path, &id);
                self.config.theme = Some(id);
                let name = selected
                    .map(|theme| theme.display_name)
                    .unwrap_or_else(|| opaline::current().meta.name.clone());
                match save_result {
                    Ok(()) => self.set_status(StatusKind::Success, format!("Theme saved: {name}")),
                    Err(error) => self.set_status(
                        StatusKind::Error,
                        format!("Theme: {name}; could not save config: {error}"),
                    ),
                }
            }
            ThemeSelectorAction::Cancel => {
                self.theme_selector = None;
                self.set_status(StatusKind::Info, "Theme unchanged");
            }
            _ => {}
        }
    }

    fn handle_topic_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.view = View::Reader;
            }
            KeyCode::Enter => {
                let topics: Vec<String> = self.topic_draft.iter().cloned().collect();
                let save_result = save_topics(&self.config_path, &topics);
                self.selected_topics = self.topic_draft.clone();
                self.config.topics = topics;
                self.view = View::Reader;
                let label = self.filter_label();
                self.refresh_feed();
                match save_result {
                    Ok(()) => {
                        self.set_status(StatusKind::Success, format!("Filter saved: {label}"))
                    }
                    Err(error) => self.set_status(
                        StatusKind::Error,
                        format!("Filter: {label}; could not save config: {error}"),
                    ),
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let current = self.topic_list.selected().unwrap_or(0);
                if current + 1 < self.topics.len() {
                    self.topic_list.select(Some(current + 1));
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let current = self.topic_list.selected().unwrap_or(0);
                self.topic_list.select(Some(current.saturating_sub(1)));
            }
            KeyCode::Home => {
                self.topic_list.select(Some(0));
            }
            KeyCode::End => {
                self.topic_list
                    .select(Some(self.topics.len().saturating_sub(1)));
            }
            KeyCode::Char(' ') => {
                if let Some(topic) = self.topics.get(self.topic_list.selected().unwrap_or(0)) {
                    let slug = topic.slug.clone();
                    if !self.topic_draft.remove(&slug) {
                        self.topic_draft.insert(slug);
                    }
                }
            }
            KeyCode::Char('a') => {
                self.topic_draft = self.topics.iter().map(|topic| topic.slug.clone()).collect();
            }
            KeyCode::Char('c') => {
                self.topic_draft.clear();
            }
            _ => {}
        }
    }

    fn handle_reader_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('n') if self.article_search.is_some() => self.next_match(),
            KeyCode::Char('N') if self.article_search.is_some() => self.previous_match(),
            KeyCode::Char('n') | KeyCode::Right | KeyCode::Char(']') => self.next_post(),
            KeyCode::Char('b') | KeyCode::Left => self.previous_post(),
            KeyCode::Char('/') => self.start_article_search(),
            KeyCode::Esc if self.article_search.is_some() => {
                self.article_search = None;
            }
            KeyCode::Char('l') => self.toggle_like_current(),
            KeyCode::Char('F') => self.flag_current(),
            KeyCode::Char('y') => self.copy_current_url(),
            KeyCode::Char('Y') => self.copy_smallweb_url(),
            KeyCode::Char('o') | KeyCode::Enter => self.open_current(),
            KeyCode::Char('S') => self.share_current(),
            KeyCode::Char('i') => self.toggle_images(),
            KeyCode::Char('H') => self.open_history(),
            KeyCode::Char('t') => self.open_topics(),
            KeyCode::Char('T') => self.open_themes(),
            KeyCode::Char('?') | KeyCode::F(1) => self.view = View::Help,
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1),
            KeyCode::PageDown => self.scroll_page(1),
            KeyCode::PageUp => self.scroll_page(-1),
            KeyCode::Char(' ') => self.scroll_to_next_paragraph(),
            KeyCode::Char('d') => self.scroll_half_page(1),
            KeyCode::Char('u') => self.scroll_half_page(-1),
            KeyCode::Char('g') | KeyCode::Home => self.scroll_to_top(),
            KeyCode::Char('G') | KeyCode::End => self.scroll_to_bottom(),
            _ => {}
        }
    }

    fn start_article_search(&mut self) {
        self.search_input = Some(
            self.article_search
                .as_ref()
                .map(|search| search.query.clone())
                .unwrap_or_default(),
        );
    }

    fn handle_search_input(&mut self, key: KeyEvent) {
        let Some(mut input) = self.search_input.take() else {
            return;
        };
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                let query = input.trim().to_string();
                if query.is_empty() {
                    self.article_search = None;
                    return;
                }
                let matches = self.article_matches(&query);
                let first = matches.first().copied();
                self.article_search = Some(ArticleSearch {
                    query,
                    matches,
                    current: 0,
                });
                if let Some(line) = first {
                    self.reveal_line(line);
                } else {
                    self.set_status(StatusKind::Warning, "No matches in article");
                }
                return;
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.clear();
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                input.push(character);
            }
            _ => {}
        }
        self.search_input = Some(input);
    }

    fn article_matches(&self, query: &str) -> Vec<usize> {
        find_matches(&self.article_lines, query)
    }

    pub fn refresh_article_search(&mut self) {
        let Some(query) = self
            .article_search
            .as_ref()
            .map(|search| search.query.clone())
        else {
            return;
        };
        let matches = self.article_matches(&query);
        if let Some(search) = self.article_search.as_mut() {
            search.current = search.current.min(matches.len().saturating_sub(1));
            search.matches = matches;
        }
    }

    fn next_match(&mut self) {
        self.cycle_match(true);
    }

    fn previous_match(&mut self) {
        self.cycle_match(false);
    }

    fn cycle_match(&mut self, forward: bool) {
        let Some(search) = self.article_search.as_mut() else {
            return;
        };
        let count = search.matches.len();
        if count == 0 {
            let query = search.query.clone();
            self.set_status(StatusKind::Warning, format!("No matches for \"{query}\""));
            return;
        }
        search.current = if forward {
            (search.current + 1) % count
        } else {
            (search.current + count - 1) % count
        };
        let line = search.matches[search.current];
        self.reveal_line(line);
    }

    fn reveal_line(&mut self, line: usize) {
        let height = self.article_height.max(1);
        let top = line.saturating_sub(height / 2);
        self.scroll = top.min(self.max_scroll());
    }

    fn open_history(&mut self) {
        if self.seen_posts.is_empty() {
            self.set_status(StatusKind::Warning, "No seen posts yet");
            return;
        }
        let mut list = ListState::default();
        list.select(Some(0));
        self.history_search = Some(HistorySearch {
            query: String::new(),
            list,
        });
    }

    fn handle_history_key(&mut self, key: KeyEvent) {
        let query = self
            .history_search
            .as_ref()
            .map(|search| search.query.clone())
            .unwrap_or_default();
        let count = history_matches(&self.seen_posts, &query).len();
        let mut view = false;
        let mut open_external = false;
        let mut share = false;
        let mut close = false;

        {
            let search = self
                .history_search
                .as_mut()
                .expect("history search is open");
            match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Enter => view = true,
                KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    open_external = true;
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    share = true;
                }
                KeyCode::Up => move_selection(&mut search.list, -1, count),
                KeyCode::Down => move_selection(&mut search.list, 1, count),
                KeyCode::PageUp => move_selection(&mut search.list, -8, count),
                KeyCode::PageDown => move_selection(&mut search.list, 8, count),
                KeyCode::Home => search.list.select(Some(0)),
                KeyCode::End => search.list.select(Some(count.saturating_sub(1))),
                KeyCode::Backspace => {
                    if search.query.pop().is_some() {
                        search.list = ListState::default();
                        search.list.select(Some(0));
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    search.query.clear();
                    search.list = ListState::default();
                    search.list.select(Some(0));
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    search.query.push(character);
                    search.list = ListState::default();
                    search.list.select(Some(0));
                }
                _ => {}
            }
        }

        if close {
            self.history_search = None;
            return;
        }
        if view || open_external || share {
            let Some(post) = self.selected_history_post() else {
                self.set_status(StatusKind::Warning, "No matching post");
                return;
            };
            self.history_search = None;
            if view {
                if self
                    .current
                    .as_ref()
                    .is_some_and(|current| current.url == post.url)
                {
                    return;
                }
                self.remember_current();
                self.show_post(post);
                self.ensure_queue();
            } else if open_external {
                self.open_url(&post.url);
            } else {
                self.share_url(&post.url);
            }
        }
    }

    fn selected_history_post(&self) -> Option<Post> {
        let search = self.history_search.as_ref()?;
        let selected = search.list.selected().unwrap_or(0);
        history_matches(&self.seen_posts, &search.query)
            .get(selected)
            .map(|post| (*post).clone())
    }

    fn open_topics(&mut self) {
        self.topic_draft = self.selected_topics.clone();
        if self.topics.is_empty() {
            self.set_status(StatusKind::Warning, "No topics available yet");
            return;
        }
        if self.topic_list.selected().is_none() {
            self.topic_list.select(Some(0));
        }
        self.view = View::Topics;
    }

    fn open_themes(&mut self) {
        let selector = ThemeSelectorState::with_current_selected();
        self.theme_selector = Some(selector);
    }

    fn next_post(&mut self) {
        if let Some(post) = self.queue.pop_front() {
            self.pending_next = false;
            self.remember_current();
            self.show_post(post);
            self.ensure_queue();
            return;
        }
        self.pending_next = true;
        self.set_status(StatusKind::Info, "Loading the next post…");
        self.ensure_queue();
    }

    fn previous_post(&mut self) {
        let Some(post) = self.history.pop() else {
            self.set_status(StatusKind::Info, "No previous post");
            return;
        };
        self.pending_next = false;
        self.show_post(post);
        self.ensure_queue();
    }

    fn remember_current(&mut self) {
        let Some(post) = self.current.as_ref() else {
            return;
        };
        if self.history.last().is_some_and(|last| last.url == post.url) {
            return;
        }
        self.history.push(post.clone());
        if self.history.len() > HISTORY_LIMIT {
            let overflow = self.history.len() - HISTORY_LIMIT;
            self.history.drain(..overflow);
        }
    }

    fn show_post(&mut self, post: Post) {
        let url = post.url.clone();
        let title = post.title.clone();
        self.article_search = None;
        self.search_input = None;
        self.seen.insert(url.clone());
        self.seen_posts.retain(|seen| seen.url != url);
        self.seen_posts.push(post.clone());
        if self.seen_posts.len() > SEEN_LIMIT {
            let overflow = self.seen_posts.len() - SEEN_LIMIT;
            self.seen_posts.drain(..overflow);
        }
        self.current = Some(post);
        self.pending_next = false;
        self.status = None;
        self.article = ArticleState::Loading;
        self.reset_article_view();
        self.spawn_article(url, title);
        self.persist_history();
    }

    fn persist_history(&mut self) {
        if self.seen_posts.is_empty() && self.history.is_empty() {
            return;
        }
        if let Err(error) = history::save(&self.history_path, &self.seen_posts, &self.history) {
            self.set_status(
                StatusKind::Error,
                format!("Could not save history: {error}"),
            );
        }
    }

    fn reset_article_view(&mut self) {
        self.scroll = 0;
        self.article_lines.clear();
        self.article_images.clear();
        self.lines_width = 0;
        self.lines_theme = None;
    }

    fn reload(&mut self) {
        self.article_search = None;
        self.search_input = None;
        if let Some(post) = self.current.clone() {
            self.article = ArticleState::Loading;
            self.reset_article_view();
            self.spawn_article(post.url, post.title);
        } else {
            self.refresh_feed();
        }
    }

    fn refresh_feed(&mut self) {
        self.remember_current();
        self.epoch = self.epoch.wrapping_add(1);
        self.fetching_deck = false;
        self.pending_next = false;
        self.queue.clear();
        self.current = None;
        self.article = ArticleState::Empty;
        self.article_search = None;
        self.search_input = None;
        self.topic_rotation = 0;
        self.reset_article_view();
        self.ensure_queue();
        self.persist_history();
    }

    fn ensure_queue(&mut self) {
        if self.fetching_deck || self.queue.len() >= QUEUE_TARGET {
            return;
        }
        self.start_deck_fetch();
    }

    fn start_deck_fetch(&mut self) {
        if self.fetching_deck {
            return;
        }
        let topic = self.next_topic_slug();
        let epoch = self.epoch;
        let api = self.api.clone();
        let tx = self.tx.clone();
        self.fetching_deck = true;
        thread::spawn(move || {
            let result = api.deck(topic.as_deref()).map_err(format_error);
            let _ = tx.send(Msg::Deck {
                epoch,
                topic,
                result,
            });
        });
    }

    fn next_topic_slug(&mut self) -> Option<String> {
        if self.selected_topics.is_empty() {
            return None;
        }
        let index = self.topic_rotation % self.selected_topics.len();
        self.topic_rotation = self.topic_rotation.wrapping_add(1);
        self.selected_topics.iter().nth(index).cloned()
    }

    fn spawn_article(&self, url: String, fallback_title: String) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = api
                .fetch_html(&url)
                .and_then(|html| article::extract(&html, &url, &fallback_title))
                .map_err(format_error);
            let _ = tx.send(Msg::Article { url, result });
        });
    }

    fn spawn_images(&self, article_url: String, referer: String, jobs: Vec<(usize, String)>) {
        if jobs.is_empty() {
            return;
        }
        let api = self.api.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            for (index, url) in jobs {
                let result = api
                    .fetch_image(&url, &referer)
                    .and_then(decode_image)
                    .map_err(format_error);
                if tx
                    .send(Msg::Image {
                        article_url: article_url.clone(),
                        index,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    fn spawn_topics(&self) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::Topics(api.topics().map_err(format_error)));
        });
    }

    fn toggle_like_current(&mut self) {
        let Some(url) = self.current.as_ref().map(|post| post.url.clone()) else {
            self.set_status(StatusKind::Warning, "No post to react to");
            return;
        };
        if !self.liking.insert(url.clone()) {
            self.set_status(StatusKind::Info, "Reaction already in flight");
            return;
        }
        let action = if self.liked.contains(&url) {
            LikeAction::Unlike
        } else {
            LikeAction::Like
        };
        let api = self.api.clone();
        let tx = self.tx.clone();
        let reaction_url = url.clone();
        thread::spawn(move || {
            let result = match action {
                LikeAction::Like => api.like(&reaction_url),
                LikeAction::Unlike => api.unlike(&reaction_url),
            }
            .map_err(format_error);
            let _ = tx.send(Msg::Like {
                url: reaction_url,
                action,
                result,
            });
        });
        self.set_status(
            StatusKind::Info,
            match action {
                LikeAction::Like => "Liking…",
                LikeAction::Unlike => "Unliking…",
            },
        );
    }

    fn flag_current(&mut self) {
        let Some(url) = self.current.as_ref().map(|post| post.url.clone()) else {
            self.set_status(StatusKind::Warning, "No post to flag");
            return;
        };
        if self.flagged.contains(&url) {
            self.set_status(StatusKind::Info, "You already flagged this post");
            return;
        }
        if !self.flagging.insert(url.clone()) {
            self.set_status(StatusKind::Info, "Flag already in flight");
            return;
        }
        let api = self.api.clone();
        let tx = self.tx.clone();
        let flag_url = url.clone();
        thread::spawn(move || {
            let result = api.flag(&flag_url).map_err(format_error);
            let _ = tx.send(Msg::Flag {
                url: flag_url,
                result,
            });
        });
        self.set_status(StatusKind::Info, "Flagging post…");
    }

    fn copy_current_url(&mut self) {
        let Some(url) = self.current.as_ref().map(|post| post.url.clone()) else {
            self.set_status(StatusKind::Warning, "No post to copy");
            return;
        };
        self.copy_url(&url, "Original URL copied");
    }

    fn copy_smallweb_url(&mut self) {
        let Some(url) = self.current.as_ref().map(|post| post.url.clone()) else {
            self.set_status(StatusKind::Warning, "No post to copy");
            return;
        };
        self.copy_url(&smallweb_url(&url), "Smallweb URL copied");
    }

    fn copy_url(&mut self, url: &str, success: &str) {
        match copy_to_clipboard(&mut self.clipboard, url) {
            Ok(()) => self.set_status(StatusKind::Success, success),
            Err(error) => self.set_status(StatusKind::Error, format!("Copy failed: {error}")),
        }
    }

    fn open_current(&mut self) {
        let Some(url) = self.current.as_ref().map(|post| post.url.clone()) else {
            self.set_status(StatusKind::Warning, "No post to open");
            return;
        };
        self.open_url(&url);
    }

    fn open_url(&mut self, url: &str) {
        let command = self.config.browser_command();
        match run_command_with_url(&command, url) {
            Ok(()) => self.set_status(StatusKind::Success, format!("Opened with {command}")),
            Err(error) => self.set_status(StatusKind::Error, format!("Open failed: {error}")),
        }
    }

    fn share_current(&mut self) {
        let Some(url) = self.current.as_ref().map(|post| post.url.clone()) else {
            self.set_status(StatusKind::Warning, "No post to share");
            return;
        };
        self.share_url(&url);
    }

    fn share_url(&mut self, url: &str) {
        let Some(command) = self.config.share_command() else {
            self.set_status(
                StatusKind::Warning,
                "No share command configured; set `share` in config.toml",
            );
            return;
        };
        match run_command_with_url(&command, url) {
            Ok(()) => self.set_status(StatusKind::Success, format!("Shared with {command}")),
            Err(error) => self.set_status(StatusKind::Error, format!("Share failed: {error}")),
        }
    }

    pub fn prepare_image_protocols(&mut self, width: u16) {
        if !self.config.images {
            return;
        }
        let picker = self.picker.clone();
        let ArticleState::Ready(article) = &mut self.article else {
            return;
        };
        let target = Size::new(width.max(1), MAX_IMAGE_ROWS);
        for image in &mut article.images {
            if image.protocol_width == Some(width) {
                continue;
            }
            let Some(data) = image.data.as_deref() else {
                image.protocol = None;
                image.protocol_width = None;
                continue;
            };
            let source = image::DynamicImage::ImageRgba8((*data).clone());
            match SlicedProtocol::new_with_resize(&picker, source, target, Resize::Fit(None)) {
                Ok(protocol) => {
                    image.protocol = Some(protocol);
                    image.protocol_width = Some(width);
                    image.error = None;
                }
                Err(error) => {
                    image.protocol = None;
                    image.protocol_width = Some(width);
                    image.error = Some(error.to_string());
                }
            }
        }
    }

    fn toggle_images(&mut self) {
        self.config.images = !self.config.images;
        self.lines_width = 0;
        if self.config.images {
            self.start_current_images();
            self.set_status(StatusKind::Info, "Images on");
        } else {
            self.set_status(StatusKind::Info, "Images off");
        }
    }

    fn start_current_images(&mut self) {
        if !self.config.images {
            return;
        }
        let Some(article_url) = self.current.as_ref().map(|post| post.url.clone()) else {
            return;
        };
        let ArticleState::Ready(article) = &mut self.article else {
            return;
        };
        let referer = article.url.clone();
        let mut jobs = Vec::new();
        for (index, image) in article.images.iter_mut().enumerate().take(MAX_IMAGES) {
            if !image.url.is_empty()
                && !image.loading
                && image.data.is_none()
                && image.error.is_none()
            {
                image.loading = true;
                jobs.push((index, image.url.clone()));
            }
        }
        if !jobs.is_empty() {
            self.spawn_images(article_url, referer, jobs);
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.max_scroll() as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    pub fn scroll_page(&mut self, direction: i32) {
        let step = self.article_height.saturating_sub(2).max(1) as isize;
        self.scroll_by(step * direction as isize);
    }

    pub fn scroll_half_page(&mut self, direction: i32) {
        let step = (self.article_height / 2).max(1) as isize;
        self.scroll_by(step * direction as isize);
    }

    pub fn scroll_to_next_paragraph(&mut self) {
        let target = next_scroll_stop(&self.article_lines, &self.article_images, self.scroll)
            .unwrap_or(self.article_lines.len());
        self.scroll = target.min(self.max_scroll());
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.max_scroll();
    }

    pub fn max_scroll(&self) -> usize {
        self.article_lines
            .len()
            .saturating_sub(self.article_height.max(1))
    }

    pub fn set_status(&mut self, kind: StatusKind, message: impl Into<String>) {
        self.status = Some(Status {
            message: message.into(),
            kind,
            at: Instant::now(),
        });
    }

    pub fn filter_label(&self) -> String {
        if self.selected_topics.is_empty() {
            return "all topics".to_string();
        }
        self.selected_topics
            .iter()
            .map(|slug| self.topic_name(slug))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn topic_name(&self, slug: &str) -> String {
        self.topics
            .iter()
            .find(|topic| topic.slug == slug)
            .map(|topic| topic.name.clone())
            .unwrap_or_else(|| slug.to_string())
    }

    pub fn article_title(&self) -> Option<&str> {
        match &self.article {
            ArticleState::Ready(article) => Some(&article.title),
            _ => self.current.as_ref().map(|post| post.title.as_str()),
        }
    }

    pub fn is_liked(&self, url: &str) -> bool {
        self.liked.contains(url)
    }

    pub fn is_flagged(&self, url: &str) -> bool {
        self.flagged.contains(url)
    }
}

pub struct ArticleImageLayout {
    pub index: usize,
    pub line: usize,
    pub width: u16,
    pub height: u16,
}

pub struct ArticleSearch {
    pub query: String,
    pub matches: Vec<usize>,
    pub current: usize,
}

fn find_matches(lines: &[Line<'_>], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            line.to_string()
                .to_lowercase()
                .contains(&query)
                .then_some(index)
        })
        .collect()
}

pub struct HistorySearch {
    pub query: String,
    pub list: ListState,
}

pub fn history_matches<'a>(posts: &'a [Post], query: &str) -> Vec<&'a Post> {
    let terms: Vec<&str> = query.split_whitespace().collect();
    if terms.is_empty() {
        return posts.iter().rev().collect();
    }

    let matcher = SkimMatcherV2::default().ignore_case();
    let mut scored: Vec<(i64, usize, &Post)> = posts
        .iter()
        .enumerate()
        .filter_map(|(index, post)| {
            let topics = post.topics.join(" ");
            let fields = [
                (post.title.as_str(), 4),
                (post.domain.as_str(), 2),
                (post.author.as_str(), 2),
                (topics.as_str(), 2),
                (post.url.as_str(), 1),
            ];
            let mut total = 0;
            for term in &terms {
                let score = fields
                    .iter()
                    .filter_map(|(value, weight)| {
                        matcher.fuzzy_match(value, term).map(|score| score * weight)
                    })
                    .max()?;
                total += score;
            }
            Some((total, index, post))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    scored.into_iter().map(|(_, _, post)| post).collect()
}

fn move_selection(list: &mut ListState, delta: i32, count: usize) {
    if count == 0 {
        list.select(None);
        return;
    }
    let current = list.selected().unwrap_or(0) as i32;
    let next = (current + delta).clamp(0, count.saturating_sub(1) as i32) as usize;
    list.select(Some(next));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LikeAction {
    Like,
    Unlike,
}

enum Msg {
    Deck {
        epoch: u64,
        topic: Option<String>,
        result: Result<Vec<Post>, String>,
    },
    Article {
        url: String,
        result: Result<Article, String>,
    },
    Like {
        url: String,
        action: LikeAction,
        result: Result<LikeOutcome, String>,
    },
    Image {
        article_url: String,
        index: usize,
        result: Result<Arc<image::RgbaImage>, String>,
    },
    Flag {
        url: String,
        result: Result<(), String>,
    },
    Topics(Result<Vec<Topic>, String>),
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn smallweb_url(post_url: &str) -> String {
    let mut url = Url::parse("https://kagi.com/smallweb/").expect("valid Smallweb URL");
    url.query_pairs_mut().append_pair("url", post_url);
    url.to_string()
}

fn decode_image(bytes: Vec<u8>) -> Result<Arc<image::RgbaImage>> {
    let mut reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .context("detecting image format")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().context("decoding image")?;
    let decoded = if decoded.width() > MAX_IMAGE_WIDTH || decoded.height() > MAX_IMAGE_HEIGHT {
        decoded.thumbnail(MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT)
    } else {
        decoded
    };
    Ok(Arc::new(decoded.to_rgba8()))
}

fn fallback_topics() -> Vec<Topic> {
    let mut topics: Vec<Topic> = FALLBACK_TOPICS
        .iter()
        .map(|(slug, name)| Topic {
            slug: (*slug).to_string(),
            name: (*name).to_string(),
            description: String::new(),
        })
        .collect();
    topics.sort_by_key(|a| a.name.to_lowercase());
    topics
}

fn copy_to_clipboard(clipboard: &mut Option<ClipboardContext>, text: &str) -> Result<()> {
    if clipboard.is_none() {
        *clipboard = ClipboardContext::new().ok();
    }
    let result = {
        let Some(clipboard) = clipboard.as_mut() else {
            bail!("could not initialize the system clipboard");
        };
        clipboard.set_contents(text.to_string())
    };
    if let Err(error) = result {
        *clipboard = None;
        return Err(anyhow!("copying to the system clipboard: {error}"));
    }
    Ok(())
}

#[cfg(unix)]
fn run_command_with_url(command: &str, url: &str) -> Result<()> {
    let quoted = shell_quote(url);
    let script = if command.contains("{}") {
        command.replace("{}", &quoted)
    } else if command.contains("%s") {
        command.replace("%s", &quoted)
    } else {
        format!("{command} {quoted}")
    };
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning command '{command}'"))?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(windows)]
fn run_command_with_url(command: &str, url: &str) -> Result<()> {
    let mut child = Command::new("cmd")
        .args(["/C", &format!("{command} \"{url}\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning command '{command}'"))?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn next_scroll_stop(
    lines: &[Line<'_>],
    images: &[ArticleImageLayout],
    current: usize,
) -> Option<usize> {
    let text = next_paragraph_start(lines, current);
    let image = images
        .iter()
        .map(|image| image.line)
        .filter(|line| *line > current)
        .min();
    match (text, image) {
        (Some(text), Some(image)) => Some(text.min(image)),
        (text, image) => text.or(image),
    }
}

fn next_paragraph_start(lines: &[Line<'_>], current: usize) -> Option<usize> {
    let mut index = current.min(lines.len());
    while index < lines.len() && !is_blank_line(&lines[index]) {
        index += 1;
    }
    while index < lines.len() && is_blank_line(&lines[index]) {
        index += 1;
    }
    (index < lines.len()).then_some(index)
}

fn is_blank_line(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn finds_the_next_paragraph_start() {
        let lines = vec![
            Line::from("first"),
            Line::from("continues"),
            Line::from(""),
            Line::from("second"),
            Line::from("continues"),
            Line::from(""),
            Line::from(""),
            Line::from("third"),
        ];
        assert_eq!(next_paragraph_start(&lines, 0), Some(3));
        assert_eq!(next_paragraph_start(&lines, 2), Some(3));
        assert_eq!(next_paragraph_start(&lines, 3), Some(7));
        assert_eq!(next_paragraph_start(&lines, 7), None);
    }

    #[test]
    fn stops_at_image_tops_as_well_as_paragraphs() {
        let lines = vec![
            Line::from("first"),
            Line::from(""),
            Line::from(""),
            Line::from(""),
            Line::from("second"),
        ];
        let images = vec![ArticleImageLayout {
            index: 0,
            line: 2,
            width: 10,
            height: 2,
        }];
        assert_eq!(next_scroll_stop(&lines, &images, 0), Some(2));
        assert_eq!(next_scroll_stop(&lines, &images, 1), Some(2));
        assert_eq!(next_scroll_stop(&lines, &images, 2), Some(4));
    }

    #[test]
    fn treats_whitespace_only_lines_as_blank() {
        let lines = vec![
            Line::from("first"),
            Line::from(ratatui::text::Span::raw("   ")),
            Line::from("second"),
        ];
        assert_eq!(next_paragraph_start(&lines, 0), Some(2));
    }

    #[test]
    fn fuzzy_searches_history_and_ranks_matches() {
        let post = |title: &str, domain: &str, url: &str| Post {
            title: title.to_string(),
            url: url.to_string(),
            domain: domain.to_string(),
            author: String::new(),
            topics: Vec::new(),
        };
        let posts = vec![
            post(
                "Rust async runtimes",
                "example.com",
                "https://example.com/a",
            ),
            post("Garden notes", "rust.garden", "https://rust.garden/b"),
            post("Async patterns", "patterns.dev", "https://patterns.dev/c"),
        ];

        let results = history_matches(&posts, "async rust");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust async runtimes");

        let results = history_matches(&posts, "rst");
        assert_eq!(results[0].title, "Rust async runtimes");

        let results = history_matches(&posts, "gardn");
        assert_eq!(results[0].title, "Garden notes");

        assert!(history_matches(&posts, "zzzzz").is_empty());
    }

    #[test]
    fn empty_history_query_keeps_recency_order() {
        let post = |title: &str| Post {
            title: title.to_string(),
            url: format!("https://example.com/{title}"),
            domain: "example.com".to_string(),
            author: String::new(),
            topics: Vec::new(),
        };
        let posts = vec![post("old"), post("new")];
        let results = history_matches(&posts, "");
        assert_eq!(results[0].title, "new");
        assert_eq!(results[1].title, "old");
    }

    #[test]
    fn finds_article_search_matches_case_insensitively() {
        let lines = vec![
            Line::from("Rust and more Rust"),
            Line::from("nothing here"),
            Line::from("another rust line"),
        ];
        assert_eq!(find_matches(&lines, "rust"), [0, 2]);
        assert_eq!(find_matches(&lines, "RUST"), [0, 2]);
        assert!(find_matches(&lines, "  ").is_empty());
        assert!(find_matches(&lines, "missing").is_empty());
    }

    #[test]
    fn builds_smallweb_url_with_encoded_post_url() {
        assert_eq!(
            smallweb_url("https://example.com/a?x=1&y=2"),
            "https://kagi.com/smallweb/?url=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1%26y%3D2"
        );
    }

    #[test]
    fn decodes_and_limits_image_size() {
        let source = image::RgbaImage::from_pixel(400, 200, image::Rgba([1, 2, 3, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let decoded = decode_image(encoded.into_inner()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (400, 200));
    }

    #[test]
    fn shell_quotes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn fallback_topics_are_unique_and_sorted() {
        let topics = fallback_topics();
        let mut slugs: Vec<_> = topics.iter().map(|topic| topic.slug.as_str()).collect();
        let unique = slugs.len();
        slugs.sort();
        slugs.dedup();
        assert_eq!(slugs.len(), unique);
        assert!(
            topics
                .windows(2)
                .all(|pair| { pair[0].name.to_lowercase() <= pair[1].name.to_lowercase() })
        );
    }
}
