use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Opaline theme id, e.g. `catppuccin-mocha`, or a path to a theme TOML.
    pub theme: Option<String>,
    /// Command used to open the original post. The URL is appended unless the
    /// command contains `{}` or `%s` as a placeholder.
    pub browser: Option<String>,
    /// Command used to share the original post. The URL is appended unless the
    /// command contains `{}` or `%s` as a placeholder. Optional.
    pub share: Option<String>,
    /// Topic slugs selected at startup. Empty means "all topics".
    pub topics: Vec<String>,
}

pub fn save_topics(path: &Path, topics: &[String]) -> Result<()> {
    let existing = if path.exists() {
        Some(
            fs::read_to_string(path)
                .with_context(|| format!("reading config {}", path.display()))?,
        )
    } else {
        None
    };
    let rendered = render_topics(existing.as_deref(), topics)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }
    fs::write(path, rendered).with_context(|| format!("writing config {}", path.display()))?;
    Ok(())
}

fn render_topics(existing: Option<&str>, topics: &[String]) -> Result<String> {
    let mut document = existing
        .unwrap_or_default()
        .parse::<toml_edit::DocumentMut>()
        .context("parsing existing config")?;
    let mut array = toml_edit::Array::new();
    for topic in topics {
        array.push(topic.as_str());
    }
    document["topics"] = toml_edit::value(array);
    let mut rendered = document.to_string();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    pub path: PathBuf,
}

pub fn load(path_override: Option<PathBuf>) -> Result<LoadedConfig> {
    let explicit = path_override.is_some();
    let path = path_override.unwrap_or_else(default_path);

    if !path.exists() {
        if explicit {
            bail!("config file {} does not exist", path.display());
        }
        return Ok(LoadedConfig {
            config: Config::default(),
            path,
        });
    }

    let raw =
        fs::read_to_string(&path).with_context(|| format!("reading config {}", path.display()))?;
    let config =
        toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
    Ok(LoadedConfig { config, path })
}

pub fn default_path() -> PathBuf {
    if let Some(path) = env::var_os("SMALLWEB_CONFIG") {
        return PathBuf::from(path);
    }
    config_dir().join("smallweb").join("config.toml")
}

fn config_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir);
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from(".")
}

impl Config {
    pub fn browser_command(&self) -> String {
        if let Some(browser) = self
            .browser
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            return browser.to_string();
        }
        if let Ok(browser) = env::var("BROWSER") {
            let browser = browser.trim();
            if !browser.is_empty() {
                return browser.to_string();
            }
        }
        default_browser().to_string()
    }

    pub fn share_command(&self) -> Option<String> {
        self.share
            .as_deref()
            .map(str::trim)
            .filter(|share| !share.is_empty())
            .map(str::to_string)
    }
}

#[cfg(target_os = "macos")]
fn default_browser() -> &'static str {
    "open"
}

#[cfg(target_os = "windows")]
fn default_browser() -> &'static str {
    "cmd /C start \"\""
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn default_browser() -> &'static str {
    "xdg-open"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_topics_without_dropping_comments_or_other_keys() {
        let existing = r#"# keep this comment
theme = "nord"
topics = ["old"]
browser = "firefox"
"#;
        let rendered = render_topics(Some(existing), &["science".into(), "web".into()]).unwrap();
        assert!(rendered.contains("# keep this comment"));
        assert!(rendered.contains("theme = \"nord\""));
        assert!(rendered.contains("browser = \"firefox\""));
        assert!(rendered.contains("topics = [\"science\", \"web\"]"));
        let config: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(config.topics, ["science", "web"]);
    }

    #[test]
    fn creates_topics_in_an_empty_document() {
        let rendered = render_topics(None, &[]).unwrap();
        let config: Config = toml::from_str(&rendered).unwrap();
        assert!(config.topics.is_empty());
    }

    #[test]
    fn parses_minimal_config() {
        let config: Config = toml::from_str(
            r#"
theme = "nord"
browser = "firefox --new-tab"
share = "toot post"
topics = ["programming", "science"]
"#,
        )
        .unwrap();
        assert_eq!(config.theme.as_deref(), Some("nord"));
        assert_eq!(config.share.as_deref(), Some("toot post"));
        assert_eq!(config.share_command().as_deref(), Some("toot post"));
        assert_eq!(config.topics, ["programming", "science"]);
    }
}
