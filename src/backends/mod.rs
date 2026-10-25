//! Where redlib gets Reddit data from.
//!
//! Every Reddit JSON request goes through [`json`]: it works out the request's
//! [`Kind`], then tries each backend in [`BACKENDS`] that is available and can
//! serve that kind, until one returns the page or Reddit gives a real answer
//! (not found, private, …). A backend is one file in this directory with a type
//! implementing [`Backend`], plus one line in [`BACKENDS`].

mod cookied_json;
pub mod gql;
mod json;
#[cfg(feature = "obscura")]
pub mod obscura;
pub mod scraped;

use crate::oauth::force_refresh_token;
use cached::proc_macro::cached;
use log::{debug, error, warn};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// A place redlib can get Reddit's JSON from.
pub trait Backend: Sync {
	/// The name used in settings and logs.
	fn name(&self) -> &'static str;

	/// Whether this instance can use the backend at all (a session is set, it isn't switched off).
	fn available(&self) -> bool {
		true
	}

	fn supports(&self, kind: Kind) -> bool {
		let _ = kind;
		true
	}

	/// Logged-in sessions see quarantined and gated subreddits as such even
	/// when the visitor opted in, so those pages need a logged-out backend.
	fn logged_in(&self) -> bool {
		false
	}

	/// `path` (a JSON API path like `/r/rust/hot.json?raw_json=1`) in the
	/// shape Reddit's JSON API returns it.
	fn fetch<'a>(&'a self, path: &'a str, quarantine: bool) -> Fetch<'a>;
}

pub type Fetch<'a> = Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send + 'a>>;

/// Every backend, in the order they are tried.
static BACKENDS: &[&dyn Backend] = &[
	#[cfg(feature = "obscura")]
	&obscura::ObscuraCookiedJson,
	&json::Json,
	&gql::Gql,
	&cookied_json::CookiedJson,
	&scraped::Scraped,
];

/// The kinds of Reddit data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
	/// Just a post (`/by_id/t3_<id>`).
	Post,
	/// A post page with its comments, "more comments" and comment permalinks.
	Comments,
	/// Posts of a subreddit, popular, all and the front page.
	Listing,
	/// Subreddit info (`/r/<sub>/about`).
	Subreddit,
	/// A user's profile, overview, posts and comments.
	User,
	/// Post and subreddit search.
	Search,
	Wiki,
	Duplicates,
}

impl Kind {
	pub fn name(self) -> &'static str {
		match self {
			Self::Post => "post",
			Self::Comments => "comments",
			Self::Listing => "listing",
			Self::Subreddit => "subreddit",
			Self::User => "user",
			Self::Search => "search",
			Self::Wiki => "wiki",
			Self::Duplicates => "duplicates",
		}
	}

	pub fn of(path: &str) -> Self {
		let route = path.split('?').next().unwrap_or_default().trim_end_matches(".json");
		let segments: Vec<&str> = route.split('/').filter(|s| !s.is_empty()).collect();
		match segments.as_slice() {
			["by_id", ..] => Self::Post,
			["duplicates", ..] => Self::Duplicates,
			["comments", _, ..] | [_, _, "comments", _, ..] => Self::Comments,
			["user" | "u", ..] => Self::User,
			["search", ..] | ["subreddits", "search", ..] | ["r", _, "search", ..] => Self::Search,
			["r", _, "wiki", ..] | ["wiki", ..] => Self::Wiki,
			["r", _, "about", ..] => Self::Subreddit,
			_ => Self::Listing,
		}
	}
}

/// Why a backend didn't return the page.
#[derive(Debug)]
pub enum Failure {
	/// The backend can't serve this particular path.
	Unsupported,
	/// Reddit refused the request ("blocked by network security", refused session).
	Blocked,
	RateLimited(String),
	/// The request or the response didn't make it through.
	Network(String),
	/// A response that can't be used.
	Invalid(String),
	/// Reddit's real answer (not found, private, banned, quarantined, …).
	Final(String),
}

/// Fetches `path` from Reddit's JSON API or whichever backend serves it.
pub async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	fetch(path, quarantine).await.map(|(json, _)| json)
}

/// Fetches `path` from the first backend that serves it, and names that backend.
#[cached(max_size = 100, ttl_secs = 30)]
pub async fn fetch(path: String, quarantine: bool) -> Result<(Value, &'static str), String> {
	let kind = Kind::of(&path);
	let mut blocked = false;
	let mut last_error = None;

	for backend in BACKENDS.iter().filter(|backend| backend.available() && backend.supports(kind)) {
		let error = match backend.fetch(&path, quarantine).await {
			Ok(json) => return Ok((json, backend.name())),
			Err(Failure::Unsupported) => {
				debug!("{} can't serve {path}", backend.name());
				continue;
			}
			Err(Failure::Final(msg)) if quarantine && backend.logged_in() && (msg == "quarantined" || msg == "gated") => msg,
			Err(Failure::Final(msg)) => return Err(msg),
			Err(Failure::Blocked) => {
				blocked = true;
				"blocked".to_string()
			}
			Err(Failure::RateLimited(msg) | Failure::Network(msg) | Failure::Invalid(msg)) => msg,
		};
		warn!("{} {} failed for {path}: {error}", kind.name(), backend.name());
		last_error = Some(error);
	}

	Err(match last_error {
		Some(error) if !blocked => error,
		_ => blocked_error(&path),
	})
}

/// Turns the errors Reddit reports inside a JSON body into failures.
async fn check_json(json: Value, path: &str) -> Result<Value, Failure> {
	// If user is suspended
	if let Some(data) = json.get("data") {
		if let Some(is_suspended) = data.get("is_suspended").and_then(Value::as_bool) {
			if is_suspended {
				return Err(Failure::Final("suspended".into()));
			}
		}
	}

	// If Reddit returned an error
	if json["error"].is_i64() {
		// OAuth token has expired; http status 401
		if json["message"] == "Unauthorized" {
			error!("Forcing a token refresh");
			let () = force_refresh_token().await;
			return Err(Failure::Invalid("OAuth token has expired. Please refresh the page!".to_string()));
		}

		for reason in ["quarantined", "gated", "private", "banned"] {
			if json["reason"] == reason {
				return Err(Failure::Final(reason.into()));
			}
		}

		Err(Failure::Final(format!(
			"Reddit error {} \"{}\": {} | {path}",
			json["error"], json["reason"], json["message"]
		)))
	} else {
		Ok(json)
	}
}

/// The failure for a response that Reddit did not send as JSON.
fn non_json_failure(status: u16, parse_error: &str, path: &str) -> Failure {
	let msg = non_json_response_error(status, parse_error, path);
	match status {
		403 => Failure::Blocked,
		429 => Failure::RateLimited(msg),
		500..=599 => Failure::Network(msg),
		_ => Failure::Invalid(msg),
	}
}

/// Builds the user-facing error for a response that Reddit did not send as JSON.
///
/// Reddit answers logged-out clients - for every `.json` endpoint, not just the
/// thread/comment ones - with an HTML "You've been blocked by network security"
/// page rather than JSON. Reporting that as a JSON parse error sends users
/// looking for a bug in Redlib, so name the real cause and the setting that
/// fixes it instead.
///
/// The text is the same whether or not a session is configured, so visitors
/// can't tell if the instance has one.
///
/// See <https://github.com/redlib-org/redlib/issues/591>.
fn non_json_response_error(status: u16, parse_error: &str, path: &str) -> String {
	if status == 403 {
		return blocked_error(path);
	}

	if status == 429 {
		return format!("Reddit is rate limiting this instance (429 Too Many Requests). Try again in a minute. | {path}");
	}

	if (500..=599).contains(&status) {
		return "Reddit is having issues, check if there's an outage".to_string();
	}

	format!("Failed to parse page JSON data: {parse_error} | {path}")
}

const BLOCKED_ERROR_PREFIX: &str = "Reddit blocked this request (403 Forbidden)";

fn blocked_error(path: &str) -> String {
	format!("{BLOCKED_ERROR_PREFIX}: Reddit only serves this page to logged-in sessions. | {path}")
}

/// Whether `json()` failed because Reddit only serves the page to logged-in sessions.
pub fn is_blocked_error(msg: &str) -> bool {
	msg.starts_with(BLOCKED_ERROR_PREFIX)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_kind_of_path() {
		for (path, kind) in [
			("/by_id/t3_abc.json?raw_json=1", Kind::Post),
			("/r/rust/comments/abc.json?sort=top&raw_json=1", Kind::Comments),
			("/r/rust/comments/abc/some_title/def.json?context=3", Kind::Comments),
			("/comments/abc.json", Kind::Comments),
			("/user/spez/comments/abc/title.json", Kind::Comments),
			("/user/spez/comments.json?sort=new", Kind::User),
			("/user/spez/about.json?raw_json=1", Kind::User),
			("/r/rust.json", Kind::Listing),
			("/r/rust+linux/new.json", Kind::Listing),
			("/.json?raw_json=1", Kind::Listing),
			("/r/rust/about.json?raw_json=1", Kind::Subreddit),
			("/search.json?q=rust", Kind::Search),
			("/r/rust/search.json?q=async&restrict_sr=on", Kind::Search),
			("/subreddits/search.json?q=rust&limit=3", Kind::Search),
			("/r/rust/wiki/index.json?raw_json=1", Kind::Wiki),
			("/duplicates/abc.json", Kind::Duplicates),
		] {
			assert_eq!(Kind::of(path), kind, "{path}");
		}
	}

	#[test]
	fn test_backend_names_are_unique() {
		let mut names: Vec<&str> = BACKENDS.iter().map(|backend| backend.name()).collect();
		names.sort_unstable();
		names.dedup();
		assert_eq!(names.len(), BACKENDS.len());
	}

	#[test]
	fn test_blocked_response_reports_the_block_not_a_json_error() {
		// Reddit's block page is HTML, so serde fails on it. The message the
		// user sees must not blame JSON parsing.
		let msg = non_json_response_error(403, "expected value at line 1 column 1", "/r/politics/comments/1x9xx9e/.json?&sort=confidence&raw_json=1");
		assert!(msg.contains("403"), "{msg}");
		assert!(!msg.contains("Failed to parse page JSON data"), "{msg}");

		// The error page links the setting that makes the page work again.
		assert!(is_blocked_error(&msg), "{msg}");
		assert!(include_str!("../../templates/error.html").contains("REDLIB_REDDIT_COOKIE"));
		assert!(msg.contains("/r/politics/comments/1x9xx9e/.json"), "{msg}");
		assert_eq!(msg, blocked_error("/r/politics/comments/1x9xx9e/.json?&sort=confidence&raw_json=1"));
	}

	#[test]
	fn test_rate_limited_html_response_says_so() {
		assert_eq!(
			non_json_response_error(429, "expected value at line 1 column 1", "/r/x/hot.json"),
			"Reddit is rate limiting this instance (429 Too Many Requests). Try again in a minute. | /r/x/hot.json"
		);
	}

	#[test]
	fn test_non_json_server_error_keeps_the_outage_message() {
		assert_eq!(
			non_json_response_error(502, "expected value at line 1 column 1", "/r/x/hot.json"),
			"Reddit is having issues, check if there's an outage"
		);
	}

	#[test]
	fn test_other_non_json_responses_still_report_a_parse_error() {
		assert_eq!(
			non_json_response_error(200, "expected value at line 1 column 1", "/r/x/hot.json"),
			"Failed to parse page JSON data: expected value at line 1 column 1 | /r/x/hot.json"
		);
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "live: needs reddit.com"]
	async fn test_localization_popular() {
		let val = json("/r/popular/hot.json?&raw_json=1&geo_filter=GLOBAL".to_string(), false).await.unwrap();
		assert_eq!("GLOBAL", val["data"]["geo_filter"].as_str().unwrap());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "live: needs reddit.com"]
	async fn test_private_sub() {
		let link = json("/r/suicide/about.json?raw_json=1".into(), true).await;
		assert_eq!(link, Err("private".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "live: needs reddit.com"]
	async fn test_banned_sub() {
		let link = json("/r/aaa/about.json?raw_json=1".into(), true).await;
		assert_eq!(link, Err("banned".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "live: needs reddit.com"]
	async fn test_gated_sub() {
		// quarantine to false to specifically catch when we _don't_ catch it
		let link = json("/r/drugs/about.json?raw_json=1".into(), false).await;
		assert_eq!(link, Err("gated".into()));
	}
}
