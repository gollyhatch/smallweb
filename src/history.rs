use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::api::Post;

#[derive(Debug, Default, Deserialize)]
pub struct StoredHistory {
    #[serde(default)]
    pub seen: Vec<Post>,
    #[serde(default)]
    pub back: Vec<Post>,
}

#[derive(Serialize)]
struct HistoryFile<'a> {
    seen: &'a [Post],
    back: &'a [Post],
}

pub fn path_for_config(config_path: &Path) -> PathBuf {
    config_path.with_extension("history.json")
}

pub fn load(config_path: &Path) -> Result<StoredHistory> {
    let path = path_for_config(config_path);
    if !path.exists() {
        return Ok(StoredHistory::default());
    }
    let data = fs::read(&path).with_context(|| format!("reading history {}", path.display()))?;
    serde_json::from_slice(&data).with_context(|| format!("parsing history {}", path.display()))
}

pub fn save(config_path: &Path, seen: &[Post], back: &[Post]) -> Result<()> {
    let path = path_for_config(config_path);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating state directory {}", parent.display()))?;
    }

    let data = serde_json::to_vec(&HistoryFile { seen, back }).context("encoding history")?;
    let temp = path.with_extension("json.tmp");
    let _ = fs::remove_file(&temp);
    write_private(&temp, &data).with_context(|| format!("writing history {}", temp.display()))?;

    if let Err(error) = fs::rename(&temp, &path) {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("replacing history {}", path.display()))?;
            fs::rename(&temp, &path)
                .with_context(|| format!("writing history {}", path.display()))?;
        } else {
            return Err(error).with_context(|| format!("writing history {}", path.display()));
        }
    }
    Ok(())
}

pub fn normalize(posts: Vec<Post>, limit: usize) -> Vec<Post> {
    let mut seen = HashSet::new();
    let mut posts: Vec<Post> = posts
        .into_iter()
        .rev()
        .filter(|post| !post.url.is_empty() && seen.insert(post.url.clone()))
        .collect();
    posts.reverse();
    cap(posts, limit)
}

pub fn normalize_back(posts: Vec<Post>, limit: usize) -> Vec<Post> {
    let posts = posts
        .into_iter()
        .filter(|post| !post.url.is_empty())
        .collect();
    cap(posts, limit)
}

fn cap(mut posts: Vec<Post>, limit: usize) -> Vec<Post> {
    if posts.len() > limit {
        let overflow = posts.len() - limit;
        posts.drain(..overflow);
    }
    posts
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    fs::write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn post(title: &str, url: &str) -> Post {
        Post {
            title: title.to_string(),
            url: url.to_string(),
            domain: "example.com".to_string(),
            author: String::new(),
            topics: Vec::new(),
        }
    }

    #[test]
    fn history_file_sits_next_to_the_config() {
        assert_eq!(
            path_for_config(Path::new("/tmp/smallweb/config.toml")),
            PathBuf::from("/tmp/smallweb/config.history.json")
        );
    }

    #[test]
    fn normalizes_and_caps_posts() {
        let posts = vec![
            post("old", "https://example.com/old"),
            post("dup", "https://example.com/dup"),
            post("dup newer", "https://example.com/dup"),
            post("", ""),
            post("new", "https://example.com/new"),
        ];
        let normalized = normalize(posts, 2);
        let titles: Vec<_> = normalized.iter().map(|post| post.title.as_str()).collect();
        assert_eq!(titles, ["dup newer", "new"]);
    }

    #[test]
    fn saves_and_loads_history() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "smallweb-history-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("config.toml");
        let seen = vec![post("seen", "https://example.com/seen")];
        let back = vec![post("back", "https://example.com/back")];

        save(&config_path, &seen, &back).unwrap();
        let loaded = load(&config_path).unwrap();
        assert_eq!(loaded.seen, seen);
        assert_eq!(loaded.back, back);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path_for_config(&config_path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        fs::remove_dir_all(&directory).unwrap();
    }
}
