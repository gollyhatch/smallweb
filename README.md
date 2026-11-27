# smallweb

A terminal reader for [Kagi Small Web](https://kagi.com/smallweb). It pulls random posts from
Small Web's deck, extracts the article with [Readability](https://crates.io/crates/dom_smoothie),
and renders it as readable text in a [ratatui](https://ratatui.rs) interface themed by
[opaline](https://crates.io/crates/opaline).

## Build

```sh
cargo build --release
./target/release/smallweb
```

or install it:

```sh
cargo install --path .
```

## Keys

| Key | Action |
| --- | --- |
| `n`, `→`, `]` | Next random post |
| `l` | Like the current post (sent to Kagi) |
| `y` | Copy the original URL |
| `o`, `Enter` | Open the original post in the configured browser |
| `S` | Run the configured share command with the post URL |
| `t` | Select topics to filter random posts |
| `T` | Theme picker with live preview |
| `j`/`k`, `↑`/`↓` | Scroll one line |
| `d`/`u` | Scroll half a page down / up |
| `Space`, `PgDn` | Page down |
| `PgUp` | Page up |
| `g`, `G` | Top / bottom |
| `r` | Reload the article, or retry loading posts |
| `?` | Help |
| `q` | Quit (also `Ctrl+C`) |

In the topic picker: `Space` toggles, `a` selects all, `c` clears, `Enter` applies and saves the
selection to `config.toml`, and `Esc` cancels. Selected topics are ORed; an empty selection means
all topics.

## Configuration

Config lives at `~/.config/smallweb/config.toml` (respects `XDG_CONFIG_HOME`), or at the path in
`SMALLWEB_CONFIG` / `--config`.

```toml
# Opaline theme id or a path to a theme TOML.
theme = "catppuccin-mocha"

# The URL is appended to this command. Use {} or %s as an explicit placeholder.
browser = "firefox --new-tab"

# Optional share command. Receives the post URL as its argument.
share = "toot post"

# Initial topic filter. Applying a selection with `t` writes it back here.
# Empty or omitted means all topics.
topics = ["programming", "science"]
```

`browser` falls back to `$BROWSER`, then `xdg-open` (Linux), `open` (macOS), or
`cmd /C start ""` (Windows). `share` has no default; pressing `S` without it shows a warning.
Both commands receive the post URL as their final argument, unless they contain `{}` or `%s`.
The theme picker (`T`) changes the theme for the current session; set `theme` in the config to keep
it across launches.

Run `smallweb --list-themes` to list the built-in opaline theme ids.

## Notes

- This uses Kagi's public Small Web deck and like endpoints, which are not a documented public API.
  Likes are real reactions sent to Kagi.
- Article extraction uses `dom_smoothie`, a Rust port of Mozilla Readability. Pages whose content
  is mostly JavaScript may produce little readable text; press `o` to read those in the browser.
- Only the Small Web blogs deck is used. Videos, code, comics, search, and river views are not
  implemented.
