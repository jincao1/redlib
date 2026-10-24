# Changes compared to upstream Redlib

This fork is based on [redlib-org/redlib](https://github.com/redlib-org/redlib) and tracks its `main` branch. It adds unmerged upstream pull requests, changes from [Sparronator9999/redlib](https://github.com/Sparronator9999/redlib), pull requests from contributors, and its own work.

## Container images

| | Upstream | This fork |
|---|---|---|
| Registry | `quay.io/redlib/redlib` | `ghcr.io/nachtalb/redlib` (forks publish to `ghcr.io/<owner>/<repo>`) |
| Platforms | `linux/amd64`, `linux/arm64`, `linux/arm/v7` | `linux/amd64`, `linux/386`, `linux/arm64`, `linux/arm/v7`, `linux/arm/v6` |
| Base | Alpine | Distroless `static:nonroot`: static musl binary, no shell or package manager, runs as uid 65532, ~16 MB |
| Tags | `latest` | `latest` (from `main`) and `sha-<short commit>`, plus `latest-obscura` and `sha-<short commit>-obscura` (amd64 only, ~76 MB) with the Obscura browser embedded |
| Health check | Docker `HEALTHCHECK` with `wget /settings` | `GET /healthz` for the orchestrator to probe (the image has no `wget`) |

All platforms are cross-compiled natively on the build host and published under one multi-arch tag, so `docker pull` picks the right one.

```bash
docker run -d --name redlib -p 8080:8080 ghcr.io/nachtalb/redlib:latest
```

No release binaries are published. Build from source with `cargo build --release`.

## Fixes

Bugs present in upstream Redlib that are fixed here:

- **Rate-limit counter wrapped around**: requests near the end of the quota could push it below zero to 65535, which skipped the token refresh ([redlib-org#586](https://github.com/redlib-org/redlib/pull/586)).
- **`/commits.atom` and `/instances.json` crashed the request** when GitHub was unreachable. They now serve the last good copy, or a 502 ([redlib-org#396](https://github.com/redlib-org/redlib/pull/396), ported).
- **Error pages called fork builds outdated**: the update check compared the instance's commit with upstream's commit list, so every fork build was "at least 20 commits old". It now checks the repository in `REDLIB_SOURCE_URL`.
- **Post pages failed** with `Failed to parse page JSON data`: Reddit blocks `/r/<sub>/comments/...` for anonymous clients, so the slash before `comments` is encoded. A block is now reported as a block instead of a JSON error. Short `/comments/<id>` links stay blocked ([redlib-org#592](https://github.com/redlib-org/redlib/pull/592)).
- **Subscriptions and filters were wiped** when saving a single setting ([redlib-org#562](https://github.com/redlib-org/redlib/issues/562)).
- **Instance default post sort ignored** until the user saved their settings once ([redlib-org#521](https://github.com/redlib-org/redlib/pull/521)).
- **Crash on a deleted or invalid comment link**: out-of-bounds access when a thread has no comments ([redlib-org#542](https://github.com/redlib-org/redlib/pull/542)).
- **Share links to posts on a user's profile** (`/u/<name>/s/<id>`) showed "Nothing here" ([redlib-org#560](https://github.com/redlib-org/redlib/pull/560)).
- **Videos transcoded by Reddit** (`rich:video`, e.g. external GIF hosts) had no player; CMAF video format is supported ([redlib-org#549](https://github.com/redlib-org/redlib/pull/549)).
- **Inconsistent margins** inside posts and comments, and headings without margins; empty zero-width-space paragraphs no longer add gaps ([redlib-org#413](https://github.com/redlib-org/redlib/pull/413)).
- **Autoplaying videos played with sound**: with "Autoplay videos" on, every video on a listing could start with audio at once. Autoplay is now muted.
- **Pausing an HLS video while it loads** was ignored and the video started playing anyway.
- **Settings codes broke on every new setting**: codes exported before a new field was added failed to restore. See [Settings codes](#settings-codes).
- **`/info.txt` showed the wrong values** next to its labels (e.g. "Pushshift frontend" showed the RSS setting).
- **Comment search had no submit button.**
- **Large numbers** show as `10K` instead of `10.0K`.
- **Comment collapse arrow** has a bigger touch area on mobile ([redlib-org#520](https://github.com/redlib-org/redlib/pull/520)).
- **LibreJS blocked scripts** without license tags (copy settings code, update check, highlighted comment). Every script now carries one ([redlib-org#410](https://github.com/redlib-org/redlib/pull/410), extended to all scripts).
- **Search box focus outline** covers the whole search form, not just the input ([redlib-org#524](https://github.com/redlib-org/redlib/pull/524)).
- **`REDLIB_ENABLE_RSS=off` still served feeds**: any value turned feeds on, while the RSS buttons only showed for `on`. Feeds now need `on` ([#16](https://github.com/Nachtalb/redlib/pull/16)).
- **RSS feeds for `r/popular` and `r/all` failed** with a Reddit 404: they have no subreddit metadata to fetch ([#16](https://github.com/Nachtalb/redlib/pull/16)).
- **RSS text was escaped or decoded wrongly**: subreddit feed titles showed `&amp;` for `&`, and user feeds turned escaped text like `Vec&lt;T&gt;` into markup that readers hid ([#16](https://github.com/Nachtalb/redlib/pull/16)).
- **Shutdown cut off running downloads**: on SIGTERM or Ctrl+C, the server now finishes in-flight requests before exiting.

## Features

From upstream pull requests that are not merged upstream:

- **RedGifs videos** play inline, proxied through the instance ([redlib-org#507](https://github.com/redlib-org/redlib/pull/507)). Can be turned off with `REDLIB_ENABLE_REDGIFS=off`.
- **GIPHY GIFs in comments** are proxied and embedded ([redlib-org#561](https://github.com/redlib-org/redlib/pull/561)). Can be turned off with `REDLIB_ENABLE_GIPHY=off`.
- **Keyboard navigation** on post pages ([redlib-org#422](https://github.com/redlib-org/redlib/pull/422)): `j` / `k` next / previous comment, `t` / `p` next / previous thread, `Enter` open the post link, `Shift+Enter` open it in a new tab.
- **Collapse a comment** by clicking its indent line ([redlib-org#291](https://github.com/redlib-org/redlib/pull/291)).
- **Lazy-loaded post images** ([redlib-org#539](https://github.com/redlib-org/redlib/pull/539)).
- **Geo filter** as a user setting instead of a URL parameter ([redlib-org#566](https://github.com/redlib-org/redlib/pull/566)), with country flags and names in the dropdown.
- **Configurable source code link** in the footer ([redlib-org#568](https://github.com/redlib-org/redlib/pull/568)).
- **Better RSS media**: images, galleries and videos render in RSS readers, and content is HTML-escaped ([redlib-org#572](https://github.com/redlib-org/redlib/pull/572)).
- **Catppuccin themes**: Latte, Frappé, Macchiato and Mocha ([redlib-org#538](https://github.com/redlib-org/redlib/pull/538)).
- **Download all images of a gallery** from the post page as individual files ([redlib-org#546](https://github.com/redlib-org/redlib/pull/546)). Needs JavaScript.
- **Post pages without a Reddit session**: when Reddit blocks a thread and no session cookie gets through, the post comes from `/by_id` and the comments are scraped from the HTML Reddit serves logged-out visitors ([redlib-org#599](https://github.com/redlib-org/redlib/pull/599), used only as a fallback here). Turn it off with `REDLIB_ENABLE_SCRAPE_FALLBACK=off`. See [Scrape fallback](README.md#scrape-fallback).

From [Sparronator9999/redlib](https://github.com/Sparronator9999/redlib):

- **Clean URLs**: optional setting that strips tracking parameters (`utm_*`, `si=`, `fbclid`, …) from external links (upstream [redlib-org#460](https://github.com/redlib-org/redlib/pull/460)).
- **Posts per page** (1–100) setting.
- **Maximum comment thread depth** setting.
- **Return to the previous page** after saving settings.

From [VictorVow1](https://github.com/VictorVow1) ([#16](https://github.com/Nachtalb/redlib/pull/16)):

- **RSS feeds for every listing Reddit has feeds for**, using the same URLs as Reddit. See [RSS feeds](#rss-feeds).

From [0dragosh](https://github.com/0dragosh) ([patch](https://gist.github.com/0dragosh/6afe28f5e4f8659f5ae04ac2b418402b), [redlib-org#446](https://github.com/redlib-org/redlib/issues/446)):

- **Logged-in Reddit sessions**: Reddit blocks post pages for logged-out clients. When a request is blocked, it is retried on `www.reddit.com` with the session cookie in `REDLIB_REDDIT_COOKIE`. The cookie never shows on `/info` and is never logged. See [Reddit session](README.md#reddit-session).

Own features:

- **Several Reddit sessions**: `REDLIB_REDDIT_COOKIE` takes several cookies separated by `|`. Blocked requests use them in turn, and a refused session hands the request to the next.
- **Requests go through a headless browser**: with a session set, every Reddit API request is fetched from `www.reddit.com` through the embedded [Obscura](https://github.com/h4ckf0r0day/obscura) browser in stealth mode: one request per page instead of an anonymous attempt followed by session retries. It is in the `latest-obscura` image (amd64 only) and in builds with `--features obscura`; `latest` and default builds keep the plain HTTP client.
- **The block message doesn't reveal whether a session is set**: visitors see the same 403 text either way, telling the instance host to set or renew `REDLIB_REDDIT_COOKIE`.
- **Scraped comments load on demand**: a scraped post page shows Reddit's first batch of comments. "More comments" and "More replies" open a page with the next batch, one request to Reddit per click, with a link back to all comments. `REDLIB_SCRAPE_COMMENT_PREFETCH` loads more up front.
- **"More replies" and "More comments" load in place**: the comments appear where the link was instead of opening another page. Without JavaScript the link opens the page as before.

- **GIF posts play like GIFs**: muted, looping, no controls, independent of the autoplay setting.
- **One video at a time**: with autoplay on, only the video most in view plays. It pauses when scrolled away and the next one takes over.
- **Instance info shows the values in use**: `/info` and `/info.txt` show the default for settings that aren't configured, marked `(default)`, instead of `Unset` or `None`.
- **`/healthz` endpoint**: returns `200 ok` without contacting Reddit.

## Configuration

### New instance settings

| Variable | Default | Description |
|---|---|---|
| `REDLIB_ENABLE_REDGIFS` | `on` | Proxy RedGifs videos and play them inline. `off` leaves RedGifs as plain links. |
| `REDLIB_ENABLE_GIPHY` | `on` | Proxy GIPHY GIFs in comments and play them inline. `off` leaves them as plain links. |
| `REDLIB_ENABLE_SCRAPE_FALLBACK` | `on` | Scrape post pages from Reddit's HTML when Reddit blocks the thread JSON. `off` shows the block error instead. |
| `REDLIB_SCRAPE_COMMENT_PREFETCH` | `0` | Extra requests a scraped post page makes to load more comments up front. `0` loads them only when asked. |
| `REDLIB_REDDIT_COOKIE` | (empty) | `Cookie` header of a logged-in reddit.com session, several separated by `\|`. Needed for post pages. **Secret.** |
| `REDLIB_SOURCE_URL` | `https://github.com/redlib-org/redlib` | Source code link in the footer, and the GitHub repository whose `main` branch the update check on error pages compares against. |

### New user settings

Each has an instance default via `REDLIB_DEFAULT_<NAME>`:

| Setting | Variable | Default |
|---|---|---|
| Geo filter | `REDLIB_DEFAULT_GEO_FILTER` | `GLOBAL` |
| Clean URLs | `REDLIB_DEFAULT_CLEAN_URLS` | `off` |
| Posts per page | `REDLIB_DEFAULT_POSTS_PER_PAGE` | `25` |
| Max comment thread depth | `REDLIB_DEFAULT_MAX_COMMENT_THREAD_DEPTH` | `0` (unlimited) |

All of them are documented in the README, `.env.example` and `app.json`.

### Settings codes

Settings codes (Settings → export / restore) now carry a format revision:

- **New codes** start with a marker byte and are revisioned. A code exported before a setting existed restores that setting to its default.
- **Old codes** from upstream Redlib still restore. Settings they don't contain get their defaults.
- All settings added in this fork belong to revision 2.

Codes exported from this fork can't be restored on upstream Redlib.

### RSS feeds

With `REDLIB_ENABLE_RSS=on`, append `.rss` to a page URL, as on Reddit. Both `/r/rust/top.rss` and `/r/rust/top/.rss` work. Query parameters are passed through, so `?t=year`, `?sort=top` and `?limit=100` work as on Reddit.

| Feed | Example |
|---|---|
| Front page | `/.rss`, `/top.rss?t=day` |
| Subreddit or multireddit | `/r/rust.rss`, `/r/SkincareAddiction/top.rss?t=year`, `/r/rust+linux/new.rss` |
| Subreddit comments | `/r/rust/comments.rss` |
| Post and its comments | `/r/rust/comments/abc123.rss`, `/comments/abc123/title/.rss?sort=top` |
| User | `/user/spez.rss`, `/u/spez/submitted.rss?sort=top&t=all`, `/user/spez/comments.rss` |
| Search | `/search.rss?q=rust&sort=new`, `/r/rust/search.rss?q=async&restrict_sr=on` |

- Feeds are built from Reddit's JSON API, not proxied from Reddit's `.rss` feeds, so they keep working if Reddit drops those.
- Post feeds list comments newest first by default.
- The RSS button on subreddit and user pages links to the feed for the current sort, timeframe and listing. Every page with a feed advertises it with `<link rel="alternate">`, so feed readers find it from the page URL.
- Feed readers don't send cookies, so feeds use the instance defaults. On the front page, the advertised feed points at the user's subscriptions or chosen front page directly.
- With `REDLIB_FULL_URL` set, relative links and images in post and comment bodies are made absolute so they load in feed readers.
- On SFW-only instances, NSFW posts and comments are left out of feeds.

## Dependencies and networking

Upgraded in the hope of reducing Reddit's TLS/SSL and fingerprinting errors (rate limits, failed token refreshes, broken media):

- **wreq** `6.0.0-rc.28` → `0.16.1` and **wreq-util** `3.0.0-rc.10` → `0.2.0`, the stable line of the HTTP client that emulates browser TLS and HTTP/2 fingerprints. The BoringSSL bindings move from `boring2 5.0.0-alpha.13` to `btls 0.5.6`. 0.16.1 also applies the total timeout to the response body, uses the configured timer for connect timeouts, and no longer compresses range requests.
- The emulated browsers are Chrome 149 and Firefox 151 (the newest profiles in wreq-util 0.2.0, up from Chrome 145 and Firefox 147), each on Android or Windows, chosen at random. Their JA4, Akamai HTTP/2 and peetprint fingerprints were checked against tls.peet.ws after every upgrade.
- **hyper** `0.14` → `1.x` for the server, with graceful shutdown.
- **hls.js** `1.5.1` → `1.7.3`.
- All other Rust dependencies are updated (askama 0.16, cached 4, revision 0.30, toml 1.0, brotli 9, base64 0.23, …). The minimum Rust version is 1.98.

## Removals

- **quay.io image** and the **Alpine image** (`Dockerfile.alpine`) are replaced by the multi-arch distroless image on GHCR.
- **Docker `HEALTHCHECK`** in `compose.yaml` and `compose.dev.yaml`: it used `wget`, which the image doesn't have. Probe `/healthz` instead.
- **Release binary, Rust test and pull-request workflows** are disabled (`*.disabled`). Only the container build and lint workflows run.

## Development

- Formatting is enforced with pre-commit and a Lint workflow: `cargo fmt`, [djhtml](https://github.com/rtts/djhtml) for templates, Prettier for CSS and JS, plus whitespace and line-ending checks. Run `pre-commit install` once.
- `.editorconfig`: tabs for Rust, 4 spaces for templates, CSS and JS, 2 spaces for YAML and JSON.
- CI builds each platform in its own job, with the Cargo registry and `target/` cached between runs.
- The Docker build context is an allowlist (`.dockerignore`); the commit hash comes in as the `GIT_HASH` build argument.
