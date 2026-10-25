use crate::body::Body;
use crate::config::get_setting;
use crate::dbg_msg;
use crate::oauth::{force_refresh_token, token_daemon, Oauth, OauthBackendImpl};
use crate::server::RequestExt;
use crate::utils::{enable_gql, format_url, Post};
use arc_swap::ArcSwap;
use bytes::Buf;
use cached::proc_macro::cached;
use futures_lite::future::block_on;
use futures_lite::{future::Boxed, FutureExt};
use hyper::{header, Request as HyperRequest, Response as HyperResponse};
use log::{error, info, trace, warn};
use percent_encoding::{percent_encode, CONTROLS};
use serde_json::Value;
use std::result::Result;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize};
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};
use wreq::cookie::Jar;
use wreq::redirect::Policy;
use wreq::{header as wreq_header, Client as WreqClient, Method, Response as WreqResponse};
use wreq_util::{Emulation, Platform, Profile};

const REDDIT_URL_BASE: &str = "https://oauth.reddit.com";
const REDDIT_URL_BASE_HOST: &str = "oauth.reddit.com";

const REDDIT_SHORT_URL_BASE: &str = "https://redd.it";
const REDDIT_SHORT_URL_BASE_HOST: &str = "redd.it";

const ALTERNATIVE_REDDIT_URL_BASE: &str = "https://www.reddit.com";
const ALTERNATIVE_REDDIT_URL_BASE_HOST: &str = "www.reddit.com";

const SHREDDIT_PATH_PREFIX: &str = "/svc/shreddit/";
// Asks for the bare fragment instead of a full page shell, roughly halving the response.
const SHREDDIT_PARTIAL_ACCEPT: &str = "text/vnd.reddit.partial+html, text/html;q=0.9";
// Reddit answers rate-limited shreddit requests with this reCAPTCHA page and a 200 status.
const SHREDDIT_CHALLENGE_TITLE: &str = "<title>Reddit - Prove your humanity</title>";

// Logged-out reddit.com session shared by shreddit requests. Reddit challenges
// cookieless clients after about ten requests, but gives each session a budget
// of a couple hundred requests. It is kept apart from the OAuth API requests on purpose.
static SHREDDIT_SESSION: LazyLock<Arc<Jar>> = LazyLock::new(Arc::default);
// Leaves room for the requests already in flight when the session is rotated.
const SHREDDIT_RATELIMIT_ROTATE_BELOW: f32 = 20.0;

/// Configuration variable holding the `Cookie` header of a logged-in
/// reddit.com session.
const REDDIT_COOKIE_SETTING: &str = "REDLIB_REDDIT_COOKIE";

/// Upper bound for the configured cookie. A browser sends a few kilobytes at
/// most; anything larger is a configuration mistake.
const REDDIT_COOKIE_MAX_LEN: usize = 8192;

pub static CLIENT: LazyLock<ArcSwap<WreqClient>> = LazyLock::new(|| ArcSwap::from_pointee(build_client()));

pub static OAUTH_CLIENT: LazyLock<ArcSwap<Oauth>> = LazyLock::new(|| {
	let client = block_on(Oauth::new());
	tokio::spawn(token_daemon());
	ArcSwap::new(client.into())
});

pub static OAUTH_RATELIMIT_REMAINING: AtomicU16 = AtomicU16::new(99);

/// Lower the local quota estimate by one, without ever wrapping below zero.
pub fn burn_rate_limit_unit() {
	OAUTH_RATELIMIT_REMAINING
		.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(1)))
		.ok();
}

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);

const URL_PAIRS: [(&str, &str); 2] = [
	(ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST),
	(REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST),
];

/// Browser emulations the client picks from, each with the platforms that
/// browser ships on. Opera on Linux/Android is left out because wreq-util
/// sends a macOS user agent for it there.
const EMULATIONS: &[(Profile, &[Platform])] = {
	use Platform::*;
	use Profile::*;
	&[
		(Chrome149, &[Windows, MacOS, Linux, Android]),
		(Chrome148, &[Windows, MacOS, Linux, Android]),
		(Chrome147, &[Windows, MacOS, Linux, Android]),
		(Edge148, &[Windows, MacOS]),
		(Edge147, &[Windows, MacOS]),
		(Firefox151, &[Windows, MacOS, Linux, Android]),
		(Firefox150, &[Windows, MacOS, Linux, Android]),
		(Firefox149, &[Windows, MacOS, Linux]),
		(Safari26_4, &[MacOS]),
		(Safari26_3, &[MacOS]),
		(SafariIos26_2, &[IOS]),
		(SafariIpad26_2, &[IOS]),
		(Opera131, &[Windows, MacOS]),
		(Opera130, &[Windows, MacOS]),
	]
};

/// Index into the flattened `EMULATIONS` pairs of the emulation in use.
static EMULATION: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Unix time of the last emulation switch, so a burst of blocked requests
/// switches only once.
static LAST_REROLL: AtomicU64 = AtomicU64::new(0);
const REROLL_COOLDOWN_SECS: u64 = 60;

fn emulation_pairs() -> impl Iterator<Item = (Profile, Platform)> {
	EMULATIONS
		.iter()
		.flat_map(|(profile, platforms)| platforms.iter().map(move |platform| (*profile, *platform)))
}

/// Builds a client with a random browser emulation other than the current one.
pub fn build_client() -> WreqClient {
	let count = emulation_pairs().count();
	let current = EMULATION.load(Ordering::Relaxed);
	let mut index = fastrand::usize(..count);
	if index == current {
		index = (index + 1 + fastrand::usize(..count - 1)) % count;
	}
	EMULATION.store(index, Ordering::Relaxed);
	let (profile, platform) = emulation_pairs().nth(index).expect("index is within the emulation pairs");
	let emulation = Emulation::builder().profile(profile).platform(platform).build();

	info!("Building Wreq client with random emulation {profile:?} on {platform:?}");
	WreqClient::builder()
		.emulation(emulation)
		.redirect(Policy::none())
		.build()
		.expect("Should always be able to build a client")
}

/// Switches `CLIENT` to another browser emulation, at most once per
/// `REROLL_COOLDOWN_SECS`. Returns whether it switched.
fn reroll_client() -> bool {
	let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
	let last = LAST_REROLL.load(Ordering::Relaxed);
	if now.saturating_sub(last) < REROLL_COOLDOWN_SECS || LAST_REROLL.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
		return false;
	}
	warn!("Reddit refused every session, switching the browser emulation");
	CLIENT.store(Arc::new(build_client()));
	true
}

/// The `Cookie` headers of logged-in reddit.com sessions, from configuration.
///
/// Reddit stopped serving `.json` to logged-out clients: it answers with an
/// HTML block page and HTTP 403. A real session cookie makes the same request
/// return JSON, which is the only way to keep post and comment pages working.
/// Several sessions are separated by `|` and used in turn.
pub(crate) fn session_cookies() -> Vec<(usize, String)> {
	let Some(raw) = get_setting(REDDIT_COOKIE_SETTING) else {
		return Vec::new();
	};

	raw
		.split('|')
		.enumerate()
		.filter(|(_, part)| !part.trim().is_empty())
		.filter_map(|(i, part)| {
			let cookie = sanitize_cookie(part);
			if cookie.is_none() {
				// The value is a credential, so it must not be logged.
				warn!("Ignoring session #{} in {REDDIT_COOKIE_SETTING}: not a valid Cookie header", i + 1);
			}
			cookie.map(|cookie| (i + 1, cookie))
		})
		.collect()
}

/// Index of the session the next blocked request starts with.
static NEXT_SESSION: AtomicUsize = AtomicUsize::new(0);

/// Trims and validates a `Cookie` header from configuration.
///
/// The value goes into a single header line, so control characters (which could
/// end the line and inject another header) and non-ASCII bytes are rejected
/// rather than forwarded to Reddit.
fn sanitize_cookie(raw: &str) -> Option<String> {
	let cookie = raw.trim();

	if cookie.is_empty() || cookie.len() > REDDIT_COOKIE_MAX_LEN || !cookie.is_ascii() || cookie.chars().any(char::is_control) {
		return None;
	}

	Some(cookie.to_string())
}

/// Fetches `path` from Reddit's web host as the logged-in session in
/// `REDLIB_REDDIT_COOKIE`.
///
/// Only the cookie is overridden: the client's browser emulation already sends
/// the TLS, HTTP/2 and default headers Reddit expects next to a session cookie,
/// and a mismatch between them is what gets a cookie refused. Redirects are not
/// followed, because Reddit sends an expired session to its login page and that
/// must be reported rather than parsed as JSON.
async fn reddit_web_get(path: &str, cookie: &str) -> Result<WreqResponse, String> {
	let url = format!("{ALTERNATIVE_REDDIT_URL_BASE}{path}");

	CLIENT.load_full().get(&url).header(wreq_header::COOKIE, cookie).send().await.map_err(|e| e.to_string())
}

/// Gets the canonical path for a resource on Reddit. This is accomplished by
/// making a `HEAD` request to Reddit at the path given in `path`.
///
/// This function returns `Ok(Some(path))`, where `path`'s value is identical
/// to that of the value of the argument `path`, if Reddit responds to our
/// `HEAD` request with a 2xx-family HTTP code. It will also return an
/// `Ok(Some(String))` if Reddit responds to our `HEAD` request with a
/// `Location` header in the response, and the HTTP code is in the 3xx-family;
/// the `String` will contain the path as reported in `Location`. The return
/// value is `Ok(None)` if Reddit responded with a 3xx, but did not provide a
/// `Location` header. An `Err(String)` is returned if Reddit responds with a
/// 429, or if we were unable to decode the value in the `Location` header.
#[cached(max_size = 1024, ttl_secs = 600)]
#[async_recursion::async_recursion]
pub async fn canonical_path(path: String, tries: i8) -> Result<Option<String>, String> {
	if tries == 0 {
		return Ok(None);
	}

	// for each URL pair, try the HEAD request
	let res = {
		// for url base and host in URL_PAIRS, try reddit_short_head(path.clone(), true, url_base, url_base_host) and if it succeeds, set res. else, res = None
		let mut res = None;
		for (url_base, url_base_host) in URL_PAIRS {
			res = reddit_short_head(path.clone(), true, url_base, url_base_host).await.ok();
			if let Some(res) = &res {
				if !res.status().is_client_error() {
					break;
				}
			}
		}
		res
	};

	let res = res.ok_or_else(|| "Unable to make HEAD request to Reddit.".to_string())?;
	let status = res.status().as_u16();
	let policy_error = res.headers().get(wreq_header::RETRY_AFTER).is_some();

	match status {
		// If Reddit responds with a 2xx, then the path is already canonical.
		200..=299 => Ok(Some(path)),

		// If Reddit responds with a 301, then the path is redirected.
		301 => match res.headers().get(wreq_header::LOCATION) {
			Some(val) => {
				let Ok(original) = val.to_str() else {
					return Err("Unable to decode Location header.".to_string());
				};

				// We need to strip the .json suffix from the original path.
				// In addition, we want to remove share parameters.
				// Cut it off here instead of letting it propagate all the way
				// to main.rs
				let stripped_uri = original.strip_suffix(".json").unwrap_or(original).split('?').next().unwrap_or_default();

				// The reason why we now have to format_url, is because the new OAuth
				// endpoints seem to return full paths, instead of relative paths.
				// So we need to strip the .json suffix from the original path, and
				// also remove all Reddit domain parts with format_url.
				// Otherwise, it will literally redirect to Reddit.com.
				let uri = format_url(stripped_uri);

				// Decrement tries and try again
				canonical_path(uri, tries - 1).await
			}
			None => Ok(None),
		},

		// If Reddit responds with anything other than 3xx (except for the 2xx and 301
		// as above), return a None.
		300..=399 => Ok(None),

		// Rate limiting
		429 => Err("Too many requests.".to_string()),

		// Special condition rate limiting - https://github.com/redlib-org/redlib/issues/229
		403 if policy_error => Err("Too many requests.".to_string()),

		_ => Ok(
			res
				.headers()
				.get(wreq_header::LOCATION)
				.map(|val| percent_encode(val.as_bytes(), CONTROLS).to_string().trim_start_matches(REDDIT_URL_BASE).to_string()),
		),
	}
}

pub async fn proxy(req: HyperRequest<Body>, format: &str) -> Result<HyperResponse<Body>, String> {
	let mut url = format!("{format}?{}", req.uri().query().unwrap_or_default());

	// For each parameter in request
	for (name, value) in &req.params() {
		// Fill the parameter value in the url
		url = url.replace(&format!("{{{name}}}"), value);
	}

	// First parameter is target URL (mandatory).
	let wreq_uri = wreq::Uri::try_from(url).map_err(|_| "Couldn't parse URL".to_string())?;

	let mut builder = CLIENT.load_full().get(wreq_uri);

	// Copy useful headers from original request
	for &key in &["Range", "If-Modified-Since", "Cache-Control"] {
		if let Some(value) = req.headers().get(key) {
			builder = builder.header(key, value.as_bytes());
		}
	}

	// Add User-Agent header of the currently spoofed device
	{
		let client = OAUTH_CLIENT.load_full();
		builder = builder.header("User-Agent", client.user_agent());
	}

	// This is needed or Reddit will redirect us to a /media landing page that just renders the image.
	builder = builder.header(wreq_header::ACCEPT, "*/*");

	builder
		.send()
		.await
		.map(|mut res| {
			let headers = res.headers_mut();

			let mut rm = |key: &str| headers.remove(key);

			rm("access-control-expose-headers");
			rm("server");
			rm("vary");
			rm("etag");
			rm("x-cdn");
			rm("x-cdn-client-region");
			rm("x-cdn-name");
			rm("x-cdn-server-region");
			rm("x-reddit-cdn");
			rm("x-reddit-video-features");
			rm("Nel");
			rm("Report-To");

			res.into_hyper_response()
		})
		.map_err(|e| e.to_string())
}

/// Fetch a server-rendered HTML fragment from Reddit's web frontend.
///
/// These are the fragments reddit.com loads for logged-out visitors, so they
/// are sent without the spoofed OAuth client's headers. A `cursor` turns the
/// request into the form POST used to expand a "more comments" placeholder.
pub async fn shreddit_fragment(path: String, cursor: Option<String>) -> Result<String, String> {
	if !path.starts_with(SHREDDIT_PATH_PREFIX) {
		return Err(format!("Refusing to fetch non-shreddit path | {path}"));
	}

	if let Some(body) = shreddit_request(&path, cursor.as_deref()).await? {
		return Ok(body);
	}

	warn!("Reddit rate limited the shreddit session, starting a new one");
	SHREDDIT_SESSION.clear();
	shreddit_request(&path, cursor.as_deref())
		.await?
		.ok_or_else(|| format!("Reddit is rate limiting comment requests. Try refreshing in a few seconds. | {path}"))
}

/// Returns `Ok(None)` when Reddit rate limits or challenges the session.
async fn shreddit_request(path: &str, cursor: Option<&str>) -> Result<Option<String>, String> {
	let url = format!("{ALTERNATIVE_REDDIT_URL_BASE}{path}");

	let builder = match cursor {
		Some(cursor) => CLIENT
			.load_full()
			.post(&url)
			.header(wreq_header::CONTENT_TYPE, "application/x-www-form-urlencoded")
			.body(url::form_urlencoded::Serializer::new(String::new()).append_pair("cursor", cursor).finish()),
		None => CLIENT.load_full().get(&url),
	};

	let response = builder
		.header(wreq_header::ACCEPT, SHREDDIT_PARTIAL_ACCEPT)
		.cookie_provider(Arc::clone(&SHREDDIT_SESSION))
		.send()
		.await
		.map_err(|e| format!("Couldn't send request to Reddit: {e} | {path}"))?;

	let status = response.status();
	if status == wreq::StatusCode::TOO_MANY_REQUESTS {
		return Ok(None);
	}
	if !status.is_success() {
		return Err(format!("Reddit returned {status} | {path}"));
	}

	let remaining = response
		.headers()
		.get("x-ratelimit-remaining")
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<f32>().ok());
	if remaining.is_some_and(|remaining| remaining < SHREDDIT_RATELIMIT_ROTATE_BELOW) {
		trace!("Shreddit session rate limit is low ({remaining:?} remaining), starting a new one");
		SHREDDIT_SESSION.clear();
	}

	let body = response.text().await.map_err(|e| format!("Failed receiving body from Reddit: {e} | {path}"))?;
	Ok((!body.contains(SHREDDIT_CHALLENGE_TITLE)).then_some(body))
}

/// Makes a GET request to Reddit at `path`. By default, this will honor HTTP
/// 3xx codes Reddit returns and will automatically redirect.
fn reddit_get(path: String, quarantine: bool) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::GET, path, true, quarantine, REDDIT_URL_BASE, REDDIT_URL_BASE_HOST)
}

/// Makes a HEAD request to Reddit at `path, using the short URL base. This will not follow redirects.
fn reddit_short_head(path: String, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::HEAD, path, false, quarantine, base_path, host)
}

// /// Makes a HEAD request to Reddit at `path`. This will not follow redirects.
// fn reddit_head(path: String, quarantine: bool) -> Boxed<Result<Response<Body>, String>> {
// 	request(&Method::HEAD, path, false, quarantine, false)
// }
// Unused - reddit_head is only ever called in the context of a short URL

/// Makes a request to Reddit. If `redirect` is `true`, `request_with_redirect`
/// will recurse on the URL that Reddit provides in the Location HTTP header
/// in its response.
fn request(method: &'static Method, path: String, redirect: bool, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
	// Build Reddit URL from path.
	let url = format!("{base_path}{path}");

	let mut headers: Vec<(String, String)> = vec![
		("Host".into(), host.into()),
		(
			"Cookie".into(),
			if quarantine {
				"_options=%7B%22pref_quarantine_optin%22%3A%20true%2C%20%22pref_gated_sr_optin%22%3A%20true%7D".into()
			} else {
				"".into()
			},
		),
	];

	{
		let client = OAUTH_CLIENT.load_full();
		for (key, value) in client.headers_map.clone() {
			headers.push((key, value));
		}
	}

	// shuffle headers: https://github.com/redlib-org/redlib/issues/324
	fastrand::shuffle(&mut headers);

	let mut builder = CLIENT.load_full().request(method.clone(), &url);

	for (key, value) in headers {
		builder = builder.header(key, value);
	}

	async move {
		match builder.send().await {
			Ok(response) => {
				// Reddit may respond with a 3xx. Decide whether or not to
				// redirect based on caller params.
				if response.status().is_redirection() {
					if !redirect {
						return Ok(response);
					};
					let location_header = response.headers().get(wreq::header::LOCATION);
					if location_header.and_then(|h| h.to_str().ok()) == Some(ALTERNATIVE_REDDIT_URL_BASE) {
						return Err("Reddit response was invalid".to_string());
					}
					return request(
						method,
						location_header
							.map(|val| {
								// We need to make adjustments to the URI
								// we get back from Reddit. Namely, we
								// must:
								//
								//     1. Remove the authority (e.g.
								//     https://www.reddit.com) that may be
								//     present, so that we recurse on the
								//     path (and query parameters) as
								//     required.
								//
								//     2. Percent-encode the path.
								let new_path = percent_encode(val.as_bytes(), CONTROLS)
									.to_string()
									.trim_start_matches(REDDIT_URL_BASE)
									.trim_start_matches(ALTERNATIVE_REDDIT_URL_BASE)
									.to_string();
								format!("{new_path}{}raw_json=1", if new_path.contains('?') { "&" } else { "?" })
							})
							.unwrap_or_default()
							.to_string(),
						true,
						quarantine,
						base_path,
						host,
					)
					.await;
				};

				Ok(response)
			}
			Err(e) => {
				dbg_msg!("{method} {REDDIT_URL_BASE}{path}: {}", e);

				Err(e.to_string())
			}
		}
	}
	.boxed()
}

/// Make a request to a Reddit API and parse the JSON response
#[cached(max_size = 100, ttl_secs = 30)]
pub async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	// Closure to quickly build errors
	let err = |msg: &str, e: String, path: String| -> Result<Value, String> {
		// eprintln!("{} - {}: {}", url, msg, e);
		Err(format!("{msg}: {e} | {path}"))
	};

	// Reddit ignores the quarantine opt-in cookie for logged-in sessions, so
	// subreddits the visitor opted into are fetched the anonymous way below.
	#[cfg(feature = "obscura")]
	if crate::obscura_client::enabled() {
		match web_json(&path).await {
			Err(e) if quarantine && (e == "quarantined" || e == "gated") => {}
			Err(e) if is_blocked_error(&e) => return gql_fallback(&path, quarantine).await.ok_or(e),
			result => return result,
		}
	}

	// First, handle rolling over the OAUTH_CLIENT if need be.
	let current_rate_limit = OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst);
	let is_rolling_over = OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst);
	if current_rate_limit < 10 && !is_rolling_over {
		warn!("Rate limit {current_rate_limit} is low. Spawning force_refresh_token()");
		tokio::spawn(force_refresh_token());
	}
	burn_rate_limit_unit();

	// A logged-in web session is used when Reddit blocks an endpoint for
	// logged-out clients (see the 403 handling below).
	let sessions = session_cookies();

	match reddit_get(path.clone(), quarantine).await {
		Ok(response) => {
			let mut response = response;
			let mut from_web_session = false;

			// Reddit's network security block is an HTML page; private and
			// quarantined subreddits answer with JSON 403s.
			let blocked =
				response.status() == wreq::StatusCode::FORBIDDEN && response.headers().get(wreq_header::CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"text/html"));
			if blocked {
				if let Some(json) = gql_fallback(&path, quarantine).await {
					return Ok(json);
				}
			}

			// Reddit answers logged-out `.json` requests with an HTML block
			// page and HTTP 403. The same path on the web host returns JSON
			// once it carries a user session, so retry it there. Sessions take
			// turns; a refused one passes the request on to the next.
			// When every session is refused, Reddit may have flagged the
			// browser emulation, so switch to another one and try once more.
			if response.status() == wreq::StatusCode::FORBIDDEN && !sessions.is_empty() {
				let mut web_response = web_session_get(&path, &sessions).await;
				if web_response.is_none() && reroll_client() {
					web_response = web_session_get(&path, &sessions).await;
				}
				if let Some(web_response) = web_response {
					response = web_response;
					from_web_session = true;
				}
			}

			let status = response.status();

			// Only the OAuth endpoint reports the instance's rate limit; the
			// web endpoint has its own limits and must not overwrite it.
			let reset: Option<String> = if !from_web_session {
				if let (Some(remaining), Some(reset), Some(used)) = (
					response.headers().get("x-ratelimit-remaining").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
					response.headers().get("x-ratelimit-reset").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
					response.headers().get("x-ratelimit-used").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
				) {
					trace!(
						"Ratelimit remaining: Header says {remaining}, we have {current_rate_limit}. Resets in {reset}. Rollover: {}. Ratelimit used: {used}",
						if is_rolling_over { "yes" } else { "no" },
					);

					// If can parse remaining as a float, round to a u16 and save
					if let Ok(val) = remaining.parse::<f32>() {
						OAUTH_RATELIMIT_REMAINING.store(val.round() as u16, Ordering::SeqCst);
					}

					Some(reset)
				} else {
					None
				}
			} else {
				None
			};

			// asynchronously aggregate the chunks of the body
			match response.into_hyper_response().into_body().collect_bytes().await {
				Ok(body) => {
					let has_remaining = body.has_remaining();

					if !has_remaining {
						// An empty body from the web host is not the OAuth
						// rate limit.
						if from_web_session {
							return Err(blocked_error(&path));
						}

						// Rate limited, so spawn a force_refresh_token()
						tokio::spawn(force_refresh_token());
						return match reset {
							Some(val) => Err(format!(
								"Reddit rate limit exceeded. Try refreshing in a few seconds.\
								 Rate limit will reset in: {val}"
							)),
							None => Err("Reddit rate limit exceeded".to_string()),
						};
					}

					// Parse the response from Reddit as JSON
					match serde_json::from_reader(body.reader()) {
						Ok(json) => check_json(json, &path).await,
						Err(e) => {
							error!("Got an invalid response from reddit {e}. Status code: {status}");
							if from_web_session {
								return Err(blocked_error(&path));
							}
							Err(non_json_response_error(status.as_u16(), &e.to_string(), &path))
						}
					}
				}
				Err(e) => err("Failed receiving body from Reddit", e.to_string(), path),
			}
		}
		Err(e) => err("Couldn't send request to Reddit", e, path),
	}
}

/// The GraphQL equivalent of a blocked `path`, if there is one and it answered.
async fn gql_fallback(path: &str, quarantine: bool) -> Option<Value> {
	if !enable_gql() {
		return None;
	}
	match Box::pin(crate::gql::json(path, quarantine)).await? {
		Ok(json) => Some(json),
		Err(e) => {
			warn!("GraphQL fallback failed for {path}: {e}");
			None
		}
	}
}

/// Tries `path` on Reddit's web host with each session in turn, starting at a
/// rotating index, and returns the first successful response.
async fn web_session_get(path: &str, sessions: &[(usize, String)]) -> Option<WreqResponse> {
	let start = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
	for i in 0..sessions.len() {
		let (number, cookie) = &sessions[(start + i) % sessions.len()];
		match reddit_web_get(path, cookie).await {
			Ok(web_response) if web_response.status().is_success() => {
				trace!("Retried {path} with session #{number}: status {}", web_response.status());
				return Some(web_response);
			}
			Ok(web_response) => warn!("Session #{number} was refused for {path}: status {}", web_response.status()),
			Err(e) => warn!("Couldn't reach Reddit's web host for {path}: {e}"),
		}
	}
	None
}

/// Fetches `path` from Reddit's web host through Obscura as a logged-in session.
#[cfg(feature = "obscura")]
async fn web_json(path: &str) -> Result<Value, String> {
	let (status, body) = crate::obscura_client::get(path)
		.await
		.map_err(|e| format!("Couldn't send request to Reddit: {e} | {path}"))?;
	match serde_json::from_slice(&body) {
		Ok(json) => check_json(json, path).await,
		Err(e) => Err(match status {
			429 | 500..=599 => non_json_response_error(status, &e.to_string(), path),
			_ => blocked_error(path),
		}),
	}
}

/// Turns the errors Reddit reports inside a JSON body into redlib errors.
async fn check_json(json: Value, path: &str) -> Result<Value, String> {
	// If user is suspended
	if let Some(data) = json.get("data") {
		if let Some(is_suspended) = data.get("is_suspended").and_then(Value::as_bool) {
			if is_suspended {
				return Err("suspended".into());
			}
		}
	}

	// If Reddit returned an error
	if json["error"].is_i64() {
		// OAuth token has expired; http status 401
		if json["message"] == "Unauthorized" {
			error!("Forcing a token refresh");
			let () = force_refresh_token().await;
			return Err("OAuth token has expired. Please refresh the page!".to_string());
		}

		// Handle quarantined
		if json["reason"] == "quarantined" {
			return Err("quarantined".into());
		}
		// Handle gated
		if json["reason"] == "gated" {
			return Err("gated".into());
		}
		// Handle private subs
		if json["reason"] == "private" {
			return Err("private".into());
		}
		// Handle banned subs
		if json["reason"] == "banned" {
			return Err("banned".into());
		}

		Err(format!("Reddit error {} \"{}\": {} | {path}", json["error"], json["reason"], json["message"]))
	} else {
		Ok(json)
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

async fn self_check(sub: &str) -> Result<(), String> {
	let query = format!("/r/{sub}/hot.json?&raw_json=1");

	match Post::fetch(&query, true).await {
		Ok(_) => Ok(()),
		Err(e) => Err(e),
	}
}

pub async fn rate_limit_check() -> Result<(), String> {
	// First, test the Oauth client: we can perform a rate limit check if the OAuth backend is MobileSpoof; if GenericWeb, we skip the check.
	if matches!(OAUTH_CLIENT.load().backend, OauthBackendImpl::GenericWeb(_)) {
		warn!("[⚠️] Cannot perform rate limit check, running as GenericWeb. Skipping check.");
		return Ok(());
	}

	// First, check a subreddit.
	self_check("reddit").await?;
	// This will reduce the rate limit to 99. Assert this check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 1 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}
	// Now, we switch out the OAuth client.
	// This checks for the IP rate limit association.
	force_refresh_token().await;
	// Now, check a new sub to break cache.
	self_check("rust").await?;
	// Again, assert the rate limit check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 2 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}

	Ok(())
}

trait IntoHyperResponse {
	fn into_hyper_response(self) -> HyperResponse<Body>;
}

impl IntoHyperResponse for WreqResponse {
	fn into_hyper_response(self) -> HyperResponse<Body> {
		let status = self.status();
		let version = self.version();

		let mut builder = HyperResponse::builder().status(status.as_u16()).version(match version {
			wreq::Version::HTTP_09 => hyper::Version::HTTP_09,
			wreq::Version::HTTP_10 => hyper::Version::HTTP_10,
			wreq::Version::HTTP_11 => hyper::Version::HTTP_11,
			wreq::Version::HTTP_2 => hyper::Version::HTTP_2,
			wreq::Version::HTTP_3 => hyper::Version::HTTP_3,
			_ => hyper::Version::HTTP_11,
		});

		for (name, value) in self.headers() {
			builder = builder.header(
				header::HeaderName::from_bytes(name.as_str().as_bytes()).unwrap(),
				header::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
			);
		}

		builder.body(Body::from_stream(self.bytes_stream())).unwrap()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use {crate::config::get_setting, sealed_test::prelude::*};

	const POPULAR_URL: &str = "/r/popular/hot.json?&raw_json=1&geo_filter=GLOBAL";

	#[tokio::test]
	async fn test_shreddit_fragment_rejects_other_paths() {
		for path in ["/api/v1/me", "https://evil.example/svc/shreddit/x", "/svc/shreddit"] {
			let error = shreddit_fragment(path.to_string(), None).await.unwrap_err();
			assert!(error.starts_with("Refusing to fetch non-shreddit path"), "{path}: {error}");
		}
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
		assert!(include_str!("../templates/error.html").contains("REDLIB_REDDIT_COOKIE"));
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
	fn test_cookie_is_trimmed_and_kept_intact() {
		assert_eq!(
			sanitize_cookie("  reddit_session=abc; token_v2=def; over18=1\n").as_deref(),
			Some("reddit_session=abc; token_v2=def; over18=1")
		);
	}

	#[test]
	fn test_empty_cookie_is_not_a_cookie() {
		assert_eq!(sanitize_cookie(""), None);
		assert_eq!(sanitize_cookie("   \n\t "), None);
	}

	#[test]
	fn test_cookie_cannot_inject_headers() {
		// A newline in the value would let it add headers to the request. A
		// trailing newline alone is harmless and trimmed, so test a second line.
		assert_eq!(sanitize_cookie("reddit_session=abc\r\nX-Injected: yes"), None);
		assert_eq!(sanitize_cookie("reddit_session=abc\nSet-Cookie: evil=1"), None);
		assert_eq!(sanitize_cookie("reddit_session=abc\u{0}"), None);
		// Header values are ASCII: non-ASCII bytes are not valid there.
		assert_eq!(sanitize_cookie("reddit_session=ábc"), None);
	}

	#[test]
	fn test_overlong_cookie_is_rejected() {
		let cookie = format!("reddit_session={}", "a".repeat(REDDIT_COOKIE_MAX_LEN));
		assert_eq!(sanitize_cookie(&cookie), None);
	}

	#[test]
	#[sealed_test(env = [("REDLIB_REDDIT_COOKIE", "reddit_session=abc; token_v2=def")])]
	fn test_session_cookie_comes_from_the_environment() {
		assert_eq!(session_cookies(), vec![(1, "reddit_session=abc; token_v2=def".to_string())]);
	}

	#[test]
	#[sealed_test(env = [("REDLIB_REDDIT_COOKIE", "reddit_session=a; token_v2=b | reddit_session=c\u{1}| |reddit_session=d")])]
	fn test_several_session_cookies_are_split_and_numbered() {
		// The invalid second session is dropped, the empty one skipped, and the
		// numbers keep pointing at the position in the setting.
		assert_eq!(
			session_cookies(),
			vec![(1, "reddit_session=a; token_v2=b".to_string()), (4, "reddit_session=d".to_string())]
		);
	}

	#[test]
	#[sealed_test]
	fn test_session_cookie_is_optional() {
		assert!(session_cookies().is_empty());
	}

	#[test]
	#[sealed_test]
	fn test_new_client_never_reuses_the_current_emulation() {
		for _ in 0..200 {
			let before = EMULATION.load(Ordering::Relaxed);
			build_client();
			assert_ne!(EMULATION.load(Ordering::Relaxed), before);
		}
	}

	#[test]
	#[sealed_test]
	fn test_reroll_waits_for_the_cooldown() {
		assert!(reroll_client());
		assert!(!reroll_client());
		LAST_REROLL.store(LAST_REROLL.load(Ordering::Relaxed) - REROLL_COOLDOWN_SECS, Ordering::Relaxed);
		assert!(reroll_client());
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
	async fn test_rate_limit_check() {
		rate_limit_check().await.unwrap();
	}

	#[test]
	#[sealed_test(env = [("REDLIB_DEFAULT_SUBSCRIPTIONS", "rust")])]
	#[ignore = "live: needs reddit.com"]
	fn test_default_subscriptions() {
		tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
			let subscriptions = get_setting("REDLIB_DEFAULT_SUBSCRIPTIONS");
			assert!(subscriptions.is_some());

			// check rate limit
			rate_limit_check().await.unwrap();
		});
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "live: needs reddit.com"]
	async fn test_localization_popular() {
		let val = json(POPULAR_URL.to_string(), false).await.unwrap();
		assert_eq!("GLOBAL", val["data"]["geo_filter"].as_str().unwrap());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "live: needs reddit.com"]
	async fn test_obfuscated_share_link() {
		let share_link = "/r/rust/s/kPgq8WNHRK".into();
		// Correct link without share parameters
		let canonical_link = "/r/rust/comments/18t5968/why_use_tuple_struct_over_standard_struct/kfbqlbc/".into();
		assert_eq!(canonical_path(share_link, 3).await, Ok(Some(canonical_link)));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "live: needs reddit.com"]
	async fn test_private_sub() {
		let link = json("/r/suicide/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("private".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "live: needs reddit.com"]
	async fn test_banned_sub() {
		let link = json("/r/aaa/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("banned".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "live: needs reddit.com"]
	async fn test_gated_sub() {
		// quarantine to false to specifically catch when we _don't_ catch it
		let link = json("/r/drugs/about.json?raw_json=1".into(), false).await;
		assert!(link.is_err());
		assert_eq!(link, Err("gated".into()));
	}

	#[test]
	#[sealed_test]
	fn test_rate_limit_counter_decrements_one_unit() {
		OAUTH_RATELIMIT_REMAINING.store(50, Ordering::SeqCst);
		burn_rate_limit_unit();
		assert_eq!(OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst), 49);
	}

	#[test]
	#[sealed_test]
	fn test_rate_limit_counter_saturates_at_zero() {
		OAUTH_RATELIMIT_REMAINING.store(0, Ordering::SeqCst);
		for _ in 0..100 {
			burn_rate_limit_unit();
		}
		assert_eq!(OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst), 0);
	}

	#[test]
	#[sealed_test]
	fn test_rate_limit_burst_across_zero_arms_rollover_guard() {
		// json() triggers a token rollover when it reads `current_rate_limit < 10`.
		// A burst crossing zero must leave the counter below that threshold.
		OAUTH_RATELIMIT_REMAINING.store(5, Ordering::SeqCst);
		for _ in 0..10 {
			burn_rate_limit_unit();
		}
		assert!(OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) < 10);
	}

	#[test]
	#[sealed_test]
	fn test_rate_limit_concurrent_bursts_never_wrap() {
		OAUTH_RATELIMIT_REMAINING.store(2, Ordering::SeqCst);
		let handles: Vec<_> = (0..8)
			.map(|_| {
				std::thread::spawn(|| {
					let mut max_seen = 0u16;
					for _ in 0..500 {
						burn_rate_limit_unit();
						max_seen = max_seen.max(OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst));
					}
					max_seen
				})
			})
			.collect();
		let max_seen = handles.into_iter().map(|h| h.join().unwrap()).max().unwrap();
		assert!(max_seen <= 2, "counter wrapped: observed {max_seen}");
		assert_eq!(OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst), 0);
	}
}
