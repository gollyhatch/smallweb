mod api;
mod app;
mod article;
mod config;
mod history;
mod ui;

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{self, Event};
use ratatui::DefaultTerminal;
use ratatui_image::picker::Picker;

use crate::app::App;

const DEFAULT_THEME: &str = "silkcircuit-neon";

fn main() {
    if let Err(error) = run() {
        eprintln!("smallweb: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let options = parse_args()?;
    if options.help {
        print_help();
        return Ok(());
    }
    if options.list_themes {
        list_themes();
        return Ok(());
    }

    let loaded = config::load(options.config)?;
    let mut config = loaded.config;
    if let Some(browser) = options.browser {
        config.browser = Some(browser);
    }
    if let Some(share) = options.share {
        config.share = Some(share);
    }
    let topics = if options.topics.is_empty() {
        config.topics.clone()
    } else {
        options.topics.clone()
    };

    let theme = options.theme.as_deref().unwrap_or_else(|| {
        config
            .theme
            .as_deref()
            .filter(|theme| !theme.trim().is_empty())
            .unwrap_or(DEFAULT_THEME)
    });
    if let Err(error) = apply_theme(theme) {
        eprintln!("smallweb: {error:#}");
        eprintln!("smallweb: falling back to {DEFAULT_THEME}");
        apply_theme(DEFAULT_THEME)?;
    }

    let mut terminal = ratatui::init();
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let mut app = match App::new(config, loaded.path, topics, picker) {
        Ok(app) => app,
        Err(error) => {
            ratatui::restore();
            return Err(error);
        }
    };
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        // Draw once per iteration so input is rendered before the next poll
        // timeout, instead of waiting up to 100 ms after every key press.
        terminal
            .draw(|frame| ui::render(app, frame))
            .context("drawing the terminal UI")?;

        if event::poll(Duration::from_millis(100)).context("polling terminal events")? {
            match event::read().context("reading terminal events")? {
                Event::Key(key) => app.handle_key(key),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        app.poll_messages();
        app.tick();
        if app.quit {
            return Ok(());
        }
    }
}

fn apply_theme(theme: &str) -> Result<()> {
    let path = Path::new(theme);
    let theme = if path.is_file() {
        opaline::load_from_file(path)
            .map_err(|error| anyhow!("loading theme {}: {error}", path.display()))?
    } else {
        opaline::load_by_name(theme).ok_or_else(|| {
            anyhow!("unknown theme '{theme}' (run `smallweb --list-themes` for built-in themes)")
        })?
    };
    opaline::set_theme(theme);
    Ok(())
}

fn list_themes() {
    let mut themes = opaline::list_available_themes();
    themes.sort_by(|a, b| a.name.cmp(&b.name));
    for theme in themes {
        println!("{:<28} {}", theme.name, theme.display_name);
    }
}

#[derive(Default)]
struct Options {
    config: Option<PathBuf>,
    theme: Option<String>,
    browser: Option<String>,
    share: Option<String>,
    topics: Vec<String>,
    list_themes: bool,
    help: bool,
}

fn parse_args() -> Result<Options> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some((flag, value)) = arg.split_once('=') {
            match flag {
                "--config" => options.config = Some(PathBuf::from(value)),
                "--theme" => options.theme = Some(value.to_string()),
                "--browser" => options.browser = Some(value.to_string()),
                "--share" => options.share = Some(value.to_string()),
                "--topic" | "--topics" => options.topics.extend(split_topics(value)),
                _ => bail!("unknown option '{flag}' (try --help)"),
            }
            index += 1;
            continue;
        }

        match arg.as_str() {
            "--config" => {
                index += 1;
                let value = args
                    .get(index)
                    .filter(|value| !value.starts_with('-'))
                    .ok_or_else(|| anyhow!("--config needs a path"))?;
                options.config = Some(PathBuf::from(value));
            }
            "--theme" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow!("--theme needs a theme id or path"))?;
                options.theme = Some(value.clone());
            }
            "--browser" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow!("--browser needs a command"))?;
                options.browser = Some(value.clone());
            }
            "--share" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow!("--share needs a command"))?;
                options.share = Some(value.clone());
            }
            "--topic" | "--topics" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow!("--topic needs a slug"))?;
                options.topics.extend(split_topics(value));
            }
            "--list-themes" => options.list_themes = true,
            "-h" | "--help" => options.help = true,
            "-V" | "--version" => {
                println!("smallweb {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => bail!("unknown argument '{arg}' (try --help)"),
        }
        index += 1;
    }
    Ok(options)
}

fn split_topics(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
        .map(str::to_string)
}

fn print_help() {
    println!(
        "\
smallweb — Kagi Small Web in your terminal

Usage: smallweb [options]

Options:
  --config <path>   Config file (default: ~/.config/smallweb/config.toml)
  --theme <id>      Opaline theme id or path to a theme TOML
  --browser <cmd>   Browser command. The URL is appended, or use {{}} / %s.
  --share <cmd>     Share command. The URL is appended, or use {{}} / %s.
  --topic <slug>    Start with a topic filter (repeatable, comma separated)
  --list-themes     List built-in Opaline theme ids
  -h, --help        Print this help
  -V, --version     Print the version

Config example:
  theme = \"catppuccin-mocha\"
  browser = \"firefox --new-tab\"   # or \"xdg-open\", \"open\", ...
  share = \"toot post\"             # optional; receives the post URL
  images = false                   # default off; toggle at runtime with \"i\"
  topics = [\"programming\", \"science\"]

Keys: n next · b back · l like · F flag · y copy URL · Y Small Web URL · o open · S share · i images · H history · t topics · T theme · ? help · q quit"
    );
}
