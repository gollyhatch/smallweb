use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::{Cursor, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use opaline::Theme;
use opaline::widgets::{ThemeSelectorAction, ThemeSelectorState};
use ratatui::text::Line;
use ratatui::widgets::ListState;

use crate::api::{Api, FALLBACK_TOPICS, LikeOutcome, Post, Topic};
use crate::article::{self, Article};
use crate::config::{Config, save_theme, save_topics};

const QUEUE_TARGET: usize = 4;
const HISTORY_LIMIT: usize = 100;
const SEEN_LIMIT: usize = 200;
const MAX_IMAGES: usize = 12;
const MAX_IMAGE_WIDTH: u32 = 200;
const MAX_IMAGE_HEIGHT: u32 = 400;
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

#[derive(Debug)]
pub enum ArticleState {
    Empty,
    Loading,
    Ready(Article),
    Error { url: String, message: String },
}

pub struct App {
    pub api: Api,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pub config: Config,
    pub config_path: PathBuf,
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
    pub lines_width: u16,
    pub lines_theme: Option<Arc<Theme>>,
    pub article_height: usize,
    pub liked: HashSet<String>,
    pub liking: HashSet<String>,
    history: Vec<Post>,
    pub seen_posts: Vec<Post>,
    seen: HashSet<String>,
    pub history_search: Option<HistorySearch>,
    pub clipboard: Option<arboard::Clipboard>,
    pub quit: bool,
    pub tick: u64,
    epoch: u64,
    topic_rotation: usize,
    fetching_deck: bool,
    pending_next: bool,
}

impl App {
    pub fn new(config: Config, config_path: PathBuf, selected_topics: Vec<String>) -> Result<Self> {
        let api = Api::new()?;
        let (tx, rx) = mpsc::channel();
        let selected_topics: BTreeSet<String> = selected_topics
            .into_iter()
            .map(|topic| topic.trim().to_string())
            .filter(|topic| !topic.is_empty())
            .collect();
        let mut app = Self {
            api,
            tx,
            rx,
            config,
            config_path,
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
            lines_width: 0,
            lines_theme: None,
            article_height: 10,
            liked: HashSet::new(),
            liking: HashSet::new(),
            history: Vec::new(),
            seen_posts: Vec::new(),
            seen: HashSet::new(),
            history_search: None,
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
            KeyCode::Char('n') | KeyCode::Right | KeyCode::Char(']') => self.next_post(),
            KeyCode::Char('b') | KeyCode::Left => self.previous_post(),
            KeyCode::Char('l') => self.toggle_like_current(),
            KeyCode::Char('y') => self.copy_current_url(),
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
        let mut open = false;
        let mut share = false;
        let mut close = false;

        {
            let search = self
                .history_search
                .as_mut()
                .expect("history search is open");
            match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Enter => open = true,
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
        if open || share {
            let Some(post) = self.selected_history_post() else {
                self.set_status(StatusKind::Warning, "No matching post");
                return;
            };
            self.history_search = None;
            if open {
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
    }

    fn reset_article_view(&mut self) {
        self.scroll = 0;
        self.article_lines.clear();
        self.lines_width = 0;
        self.lines_theme = None;
    }

    fn reload(&mut self) {
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
        self.topic_rotation = 0;
        self.reset_article_view();
        self.ensure_queue();
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

    fn copy_current_url(&mut self) {
        let Some(url) = self.current.as_ref().map(|post| post.url.clone()) else {
            self.set_status(StatusKind::Warning, "No post to copy");
            return;
        };
        match copy_to_clipboard(&mut self.clipboard, &url) {
            Ok(()) => self.set_status(StatusKind::Success, "Original URL copied"),
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
        let target = next_paragraph_start(&self.article_lines, self.scroll)
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

#[derive(Debug)]
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
    Topics(Result<Vec<Topic>, String>),
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
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
    let thumbnail = decoded
        .thumbnail(MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT)
        .to_rgba8();
    Ok(Arc::new(thumbnail))
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

fn copy_to_clipboard(clipboard: &mut Option<arboard::Clipboard>, text: &str) -> Result<()> {
    if try_external_clipboard(text) {
        return Ok(());
    }

    if clipboard.is_none() {
        *clipboard = arboard::Clipboard::new().ok();
    }
    if let Some(cb) = clipboard.as_mut() {
        if cb.set_text(text.to_string()).is_ok() {
            return Ok(());
        }
    }
    *clipboard = None;
    bail!("no clipboard backend available (tried wl-copy, xclip, xsel, pbcopy, arboard)")
}

fn try_external_clipboard(text: &str) -> bool {
    let mut commands: Vec<(&str, &[&str])> = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        commands.push(("wl-copy", &[]));
    }
    if std::env::var_os("DISPLAY").is_some() {
        commands.push(("xclip", &["-selection", "clipboard"]));
        commands.push(("xsel", &["--clipboard", "--input"]));
    }
    #[cfg(target_os = "macos")]
    commands.push(("pbcopy", &[]));
    if commands.is_empty() {
        commands.extend([
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
            ("pbcopy", &[][..]),
        ]);
    }

    for (command, args) in commands {
        let Ok(mut child) = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(stdin) = child.stdin.as_mut() {
            if stdin.write_all(text.as_bytes()).is_err() {
                let _ = child.wait();
                continue;
            }
        }
        if child.wait().is_ok_and(|status| status.success()) {
            return true;
        }
    }
    false
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
    fn decodes_and_limits_image_size() {
        let source = image::RgbaImage::from_pixel(400, 200, image::Rgba([1, 2, 3, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let decoded = decode_image(encoded.into_inner()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (200, 100));
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
