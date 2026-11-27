use std::io::Read;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use reqwest::blocking::{Client, Response};
use scraper::{Html, Selector};
use serde::Deserialize;

const SMALLWEB_URL: &str = "https://kagi.com/smallweb/";
const DECK_URL: &str = "https://kagi.com/smallweb/api/deck";
const LIKE_URL: &str = "https://kagi.com/smallweb/api/like";
const MAX_HTML_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 12 * 1024 * 1024;
const USER_AGENT: &str = "smallweb-tui/0.1 (terminal reader)";

static CAT_ROW: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".cat-item-row").expect("valid selector"));
static CAT_NAME: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".cat-name").expect("valid selector"));
static CAT_DESC: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".cat-desc").expect("valid selector"));
static CAT_CHIP: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.cat-chip[href]").expect("valid selector"));

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post {
    pub title: String,
    pub url: String,
    pub domain: String,
    pub author: String,
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topic {
    pub slug: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LikeOutcome {
    pub likes_total: u64,
}

#[derive(Clone)]
pub struct Api {
    client: Client,
}

impl Api {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(25))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("building HTTP client")?;
        Ok(Self { client })
    }

    pub fn deck(&self, topic: Option<&str>) -> Result<Vec<Post>> {
        let mut url = Url::parse(DECK_URL).expect("valid deck URL");
        if let Some(topic) = topic.filter(|topic| !topic.is_empty()) {
            url.query_pairs_mut().append_pair("cat", topic);
        }
        let response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .send()
            .context("requesting the Small Web deck")?;
        let status = response.status();
        if !status.is_success() {
            bail!("deck request failed with HTTP {status}");
        }
        let deck: DeckResponse = response.json().context("decoding the deck response")?;
        Ok(deck
            .posts
            .into_iter()
            .filter(|post| !post.flagged)
            .map(Post::from)
            .filter(|post| !post.url.is_empty())
            .collect())
    }

    pub fn topics(&self) -> Result<Vec<Topic>> {
        let html = self.fetch_html(SMALLWEB_URL)?;
        let document = Html::parse_document(&html);
        let mut topics = Vec::new();
        for row in document.select(&CAT_ROW) {
            let Some(slug) = row.value().attr("data-slug").map(str::trim) else {
                continue;
            };
            if slug.is_empty() {
                continue;
            }
            let name = row
                .select(&CAT_NAME)
                .next()
                .map(|element| clean_topic_name(&element.text().collect::<String>()))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| slug.to_string());
            let description = row
                .select(&CAT_DESC)
                .next()
                .map(|element| element.text().collect::<String>().trim().to_string())
                .unwrap_or_default();
            topics.push(Topic {
                slug: slug.to_string(),
                name,
                description,
            });
        }
        if topics.is_empty() {
            bail!("no topics found on the Small Web page");
        }
        topics.sort_by_key(|a| a.name.to_lowercase());
        let mut seen = std::collections::HashSet::new();
        topics.retain(|topic| seen.insert(topic.slug.clone()));
        Ok(topics)
    }

    pub fn like(&self, url: &str) -> Result<LikeOutcome> {
        let response = self
            .client
            .post(LIKE_URL)
            .header("Accept", "application/json")
            .json(&serde_json::json!({ "url": url, "emoji": "👍" }))
            .send()
            .context("sending the like")?;
        let status = response.status();
        if !status.is_success() {
            bail!("like failed with HTTP {status}");
        }
        let result: LikeResponse = response.json().context("decoding the like response")?;
        if !result.ok {
            bail!("Small Web rejected the like");
        }
        Ok(LikeOutcome {
            likes_total: result.likes_total.unwrap_or_default(),
        })
    }

    pub fn fetch_html(&self, url: &str) -> Result<String> {
        let response = self
            .client
            .get(url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,text/plain;q=0.8,*/*;q=0.5",
            )
            .send()
            .with_context(|| format!("requesting {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("HTTP {status} from {url}");
        }
        let final_url = response.url().clone();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let media_type = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !media_type.is_empty()
            && !media_type.contains("html")
            && !media_type.starts_with("text/")
            && !media_type.contains("xml")
        {
            bail!("{final_url} is not an HTML article ({media_type})");
        }
        let bytes = read_limited(response, MAX_HTML_BYTES, "page")?;
        if bytes.is_empty() {
            bail!("{final_url} returned an empty page");
        }
        Ok(decode_html(&bytes, &content_type))
    }

    pub fn fetch_image(&self, url: &str, referer: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .header(
                "Accept",
                "image/webp,image/png,image/jpeg,image/gif,image/*;q=0.8,*/*;q=0.2",
            )
            .header("Referer", referer)
            .send()
            .with_context(|| format!("requesting image {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("HTTP {status} from {url}");
        }
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !media_type.is_empty()
            && !media_type.starts_with("image/")
            && media_type != "application/octet-stream"
        {
            bail!("{url} is not an image ({media_type})");
        }
        let bytes = read_limited(response, MAX_IMAGE_BYTES, "image")?;
        if bytes.is_empty() {
            bail!("{url} returned an empty image");
        }
        Ok(bytes)
    }
}

impl Post {
    fn from(post: ApiPost) -> Self {
        let url = if post.url.trim().is_empty() {
            post.source_url
        } else {
            post.url
        };
        let domain = if post.domain.trim().is_empty() {
            Url::parse(&url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .unwrap_or_default()
        } else {
            post.domain
        };
        let topics = post
            .slots
            .as_ref()
            .and_then(|slots| slots.get("post-cats"))
            .map(|html| topics_from_slot(html))
            .unwrap_or_default();
        Self {
            title: if post.title.trim().is_empty() {
                url.clone()
            } else {
                post.title.trim().to_string()
            },
            url,
            domain,
            author: post.author.trim().to_string(),
            topics,
        }
    }
}

fn topics_from_slot(html: &str) -> Vec<String> {
    let fragment = Html::parse_fragment(html);
    let mut topics: Vec<String> = fragment
        .select(&CAT_CHIP)
        .filter_map(|anchor| {
            let href = anchor.value().attr("href")?;
            let url = Url::parse(href)
                .or_else(|_| Url::parse(&format!("https://kagi.com{href}")))
                .ok()?;
            url.query_pairs()
                .find(|(key, _)| key == "cat")
                .map(|(_, value)| value.into_owned())
        })
        .collect();
    topics.sort();
    topics.dedup();
    topics
}

fn clean_topic_name(raw: &str) -> String {
    let raw = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(index) = raw.rfind(" (") {
        let count = raw[index + 2..].trim_end_matches(')');
        if !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()) {
            return raw[..index].trim().to_string();
        }
    }
    raw
}

fn read_limited(response: Response, limit: u64, label: &str) -> Result<Vec<u8>> {
    let mut limited = response.take(limit + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .context("reading the response body")?;
    if bytes.len() as u64 > limit {
        bail!("{label} is larger than {} MiB", limit / 1024 / 1024);
    }
    Ok(bytes)
}

fn decode_html(bytes: &[u8], content_type: &str) -> String {
    let charset = content_type
        .split(';')
        .find_map(|part| {
            let part = part.trim();
            part.get(..8)
                .filter(|prefix| prefix.eq_ignore_ascii_case("charset="))
                .map(|_| part[8..].trim_matches(['"', '\'']).to_string())
        })
        .or_else(|| sniff_meta_charset(bytes));

    if let Some(charset) = charset {
        if let Some(encoding) = encoding_rs::Encoding::for_label(charset.as_bytes()) {
            let (text, _, _) = encoding.decode(bytes);
            return text.into_owned();
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn sniff_meta_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(4096)];
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    let index = head.find("charset=")?;
    let rest = &head[index + "charset=".len()..];
    let label: String = rest
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    (!label.is_empty()).then_some(label)
}

#[derive(Deserialize)]
struct DeckResponse {
    #[serde(default)]
    posts: Vec<ApiPost>,
}

#[derive(Deserialize)]
struct ApiPost {
    #[serde(default)]
    author: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    flagged: bool,
    #[serde(default)]
    source_url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    slots: Option<std::collections::HashMap<String, String>>,
}

#[derive(Deserialize)]
struct LikeResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    likes_total: Option<u64>,
}

pub const FALLBACK_TOPICS: &[(&str, &str)] = &[
    ("ai", "✨ AI"),
    ("science", "⚛️ Science"),
    ("programming", "🧩 Programming"),
    ("diy", "🛠️ DIY & Making"),
    ("tech", "📱 Technology"),
    ("hardware", "🎛️ Hardware"),
    ("infra", "🛡️ Sysadmin & Security"),
    ("web", "🕸️ Web & Internet"),
    ("art", "🌊 Art"),
    ("design", "📐 Design & UX"),
    ("essays", "🪶 Essays"),
    ("humanities", "🏛️ Humanities"),
    ("culture", "🎭 Culture"),
    ("life", "🌱 Life"),
    ("society", "👥 Society"),
    ("politics", "🏛️ Politics"),
    ("economy", "💸 Economy"),
    ("work", "💼 Work"),
    ("education", "🎓 Education"),
    ("health", "🩺 Health"),
    ("food", "🍲 Food"),
    ("travel", "✈️ Travel"),
    ("photography", "📷 Photography"),
    ("gaming", "🎮 Gaming"),
    ("retro", "🕹️ Retro"),
    ("sports", "🏅 Sports"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_post_counts_from_topic_names() {
        assert_eq!(clean_topic_name("✨  AI (505)"), "✨ AI");
        assert_eq!(clean_topic_name("🛠️ DIY & Making (86)"), "🛠️ DIY & Making");
        assert_eq!(clean_topic_name("Science"), "Science");
    }

    #[test]
    fn reads_topics_from_deck_slot_html() {
        let topics = topics_from_slot(
            r#"<a href="/smallweb/?cat=programming" class="cat-chip">a</a>
               <a href="/smallweb/?cat=web" class="cat-chip">b</a>"#,
        );
        assert_eq!(topics, ["programming", "web"]);
    }

    #[test]
    fn decodes_a_declared_latin1_page() {
        let bytes = b"caf\xe9";
        let decoded = decode_html(bytes, "text/html; charset=iso-8859-1");
        assert_eq!(decoded, "café");
    }

    #[test]
    fn sniffs_meta_charset() {
        let bytes = br#"<meta charset="windows-1252">caf\xe9"#;
        assert_eq!(sniff_meta_charset(bytes).as_deref(), Some("windows-1252"));
    }
}
