#![allow(clippy::cmp_owned)]
use crate::body::Body;
use crate::client::{is_blocked_error, json};
use crate::config::get_setting;
use crate::reddit_health::{self, Outcome};
use crate::server::RequestExt;
use crate::shreddit::{self, More};
use crate::subreddit::{can_access_quarantine, quarantine};
use crate::utils::{
	build_rss_comment_item, build_rss_item, clean_url, enable_rss, enable_scrape_fallback, error, format_num, get_filters, nsfw_landing, param, parse_post, rewrite_emotes,
	rss_page_link, rss_response, setting, sfw_only, template, time, val, Author, Awards, Comment, Flair, FlairPart, Post, Preferences,
};
use askama::Template;
use hyper::{Request, Response};
use regex::Regex;
use rss::ChannelBuilder;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

// STRUCTS
#[derive(Template)]
#[template(path = "post.html")]
struct PostTemplate {
	comments: Vec<Comment>,
	post: Post,
	sort: String,
	prefs: Preferences,
	single_thread: bool,
	url: String,
	url_without_query: String,
	comment_query: String,
	/// The page shows one batch of scraped comments loaded from a "more" link.
	more_page: bool,
}

static COMMENT_SEARCH_CAPTURE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\?q=(.*)&type=comment").unwrap());

pub async fn item(req: Request<Body>) -> Result<Response<Body>, String> {
	// Build Reddit API path
	let mut params = req.uri().query().unwrap_or_default().to_string();
	let sub = req.param("sub").unwrap_or_default();
	let quarantined = can_access_quarantine(&req, &sub);
	let url = req.uri().to_string();

	// Set sort to sort query parameter
	let sort = param(&format!("?{params}"), "sort").unwrap_or_else(|| {
		// Grab default comment sort method from Cookies
		let default_sort = setting(&req, "comment_sort");

		// If there's no sort query but there's a default sort, set sort to default_sort
		if default_sort.is_empty() {
			String::new()
		} else {
			params.push_str(&format!("&sort={default_sort}"));
			default_sort
		}
	});

	let _depth = param(&format!("?{params}"), "depth").unwrap_or_else(|| {
		let default_depth: u32 = setting(&req, "max_comment_thread_depth").parse().unwrap_or(0);
		if default_depth < 1 {
			String::new()
		} else {
			params.push_str(&format!("&depth={default_depth}"));
			default_depth.to_string()
		}
	});

	// Log the post ID being fetched in debug mode
	#[cfg(debug_assertions)]
	req.param("id").unwrap_or_default();

	let single_thread = req.param("comment_id").is_some();
	let highlighted_comment = &req.param("comment_id").unwrap_or_default();

	let more = param(&format!("?{params}"), "more_cursor").map(|cursor| More {
		cursor,
		query: param(&format!("?{params}"), "more_query").unwrap_or_default(),
	});
	let more_page = more.is_some();

	// Send a request to the url, receive JSON in response
	let path: String = format!("{}.json?{}&raw_json=1", req.uri().path().trim_end_matches("/"), params);
	let response = match json(path, quarantined).await {
		Ok(response) => {
			reddit_health::record(Outcome::Ok);
			response
		}
		Err(msg) if is_blocked_error(&msg) && enable_scrape_fallback() => {
			match scraped_thread(
				req.param("id").unwrap_or_default(),
				req.param("comment_id"),
				param(&format!("?{params}"), "context"),
				sort.clone(),
				more,
				quarantined,
			)
			.await
			{
				Ok(thread) => {
					reddit_health::record(Outcome::Scraped);
					thread
				}
				Err(msg) => {
					reddit_health::record(Outcome::Blocked);
					return error(req, &msg).await;
				}
			}
		}
		Err(msg) if msg == "quarantined" || msg == "gated" => {
			let sub = req.param("sub").unwrap_or_default();
			return Ok(quarantine(&req, sub, &msg));
		}
		Err(msg) => {
			if is_blocked_error(&msg) {
				reddit_health::record(Outcome::Blocked);
			}
			return error(req, &msg).await;
		}
	};

	// Parse the JSON into Post and Comment structs
	let mut post = parse_post(&response[0]["data"]["children"][0]).await;

	let clean_urls = setting(&req, "clean_urls");
	if clean_urls == "on".to_owned() {
		post.media.url = clean_url(post.media.url);
	}

	let req_url = req.uri().to_string();
	// Return landing page if this post if this Reddit deems this post
	// NSFW, but we have also disabled the display of NSFW content
	// or if the instance is SFW-only.
	if post.nsfw && crate::utils::should_be_nsfw_gated(&req, &req_url) {
		return Ok(nsfw_landing(req, req_url).await.unwrap_or_default());
	}

	let query_body = match COMMENT_SEARCH_CAPTURE.captures(&url) {
		Some(captures) => captures.get(1).unwrap().as_str().replace("%20", " ").replace('+', " "),
		None => String::new(),
	};

	let query_string = format!("q={query_body}&type=comment");
	let form = url::form_urlencoded::parse(query_string.as_bytes()).collect::<HashMap<_, _>>();
	let query = form.get("q").unwrap().clone().to_string();

	let comments = match query.as_str() {
		"" => parse_comments(&response[1], &post.permalink, &post.author.name, highlighted_comment, &get_filters(&req), &req),
		_ => query_comments(&response[1], &post.permalink, &post.author.name, highlighted_comment, &get_filters(&req), &query, &req),
	};

	// Use the Post and Comment structs to generate a website to show users
	Ok(template(&PostTemplate {
		comments,
		post,
		url_without_query: url.clone().trim_end_matches(&format!("?q={query}&type=comment")).to_string(),
		sort,
		prefs: Preferences::new(&req),
		single_thread,
		url: req_url,
		comment_query: query,
		more_page,
	}))
}

/// The thread's API response (`[post listing, comment listing]`), rebuilt
/// from Reddit's web frontend. Used when Reddit only serves the thread's JSON
/// to logged-in sessions: the post still comes from `/by_id`, the comments
/// are scraped from the HTML Reddit serves logged-out visitors.
async fn scraped_thread(id: String, comment_id: Option<String>, context: Option<String>, sort: String, more: Option<More>, quarantined: bool) -> Result<Value, String> {
	let post = json(format!("/by_id/t3_{id}.json?raw_json=1"), quarantined).await?;
	let Some(sub) = post["data"]["children"][0]["data"]["subreddit"].as_str() else {
		return Err("Post not found".to_string());
	};

	let comments = shreddit::comments(sub.to_string(), id, comment_id, sort, context, more).await?;
	Ok(json!([post, comments]))
}

/// Feed for a post and its comments, built from the JSON API.
/// Mirrors Reddit's feed URLs: `/r/rust/comments/abc123.rss`, `/comments/abc123/title/.rss`.
/// Comments are flattened and fetched newest first unless `?sort=` says otherwise.
pub async fn rss(req: Request<Body>) -> Result<Response<Body>, String> {
	if !enable_rss() {
		return Ok(error(req, "RSS is disabled on this instance.").await.unwrap_or_default());
	}

	let id = req.param("id").unwrap_or_default();
	let mut query = req.uri().query().unwrap_or_default().to_string();
	if param(&format!("?{query}"), "sort").is_none() {
		query.push_str("&sort=new");
	}
	// Reddit blocks `/comments/<id>.json`, so threads are fetched through their subreddit
	let sub = match req.param("sub") {
		Some(sub) => sub,
		None => {
			let info = json(format!("/by_id/t3_{id}.json?raw_json=1"), false).await?;
			info["data"]["children"][0]["data"]["subreddit"].as_str().unwrap_or_default().to_string()
		}
	};
	let response = json(format!("/r/{sub}/comments/{id}.json?{query}&raw_json=1"), false).await?;

	let post = parse_post(&response[0]["data"]["children"][0]).await;
	if post.nsfw && sfw_only() {
		return error(req, "NSFW content is disabled on this instance.").await;
	}

	let mut items = vec![(post.created_ts, build_rss_item(&post))];
	collect_rss_comments(&response[1], &post.title, &mut items);
	items.sort_by_key(|(created_ts, _)| std::cmp::Reverse(*created_ts));

	let channel = ChannelBuilder::default()
		.title(format!("{} - r/{}", post.title, post.community))
		.link(rss_page_link(&req))
		.items(items.into_iter().map(|(_, item)| item).collect::<Vec<_>>())
		.build();

	Ok(rss_response(&channel))
}

/// Flattens a comment tree into feed items, paired with their creation time for sorting.
fn collect_rss_comments(listing: &serde_json::Value, link_title: &str, items: &mut Vec<(u64, rss::Item)>) {
	for comment in listing["data"]["children"].as_array().into_iter().flatten() {
		// Skip "load more comments" stubs
		if comment["kind"] != "t1" {
			continue;
		}
		let data = &comment["data"];
		let created_ts = data["created_utc"].as_f64().unwrap_or_default().round() as u64;
		let body = rewrite_emotes(&data["media_metadata"], val(comment, "body_html"));
		items.push((
			created_ts,
			build_rss_comment_item(&val(comment, "author"), link_title, &val(comment, "permalink"), &body, created_ts),
		));
		if data["replies"].is_object() {
			collect_rss_comments(&data["replies"], link_title, items);
		}
	}
}

// COMMENTS

fn parse_comments(json: &serde_json::Value, post_link: &str, post_author: &str, highlighted_comment: &str, filters: &HashSet<String>, req: &Request<Body>) -> Vec<Comment> {
	// Parse the comment JSON into a Vector of Comments
	let comments = json["data"]["children"].as_array().map_or(Vec::new(), std::borrow::ToOwned::to_owned);

	// For each comment, retrieve the values to build a Comment object
	comments
		.into_iter()
		.map(|comment| {
			let data = &comment["data"];
			let replies: Vec<Comment> = if data["replies"].is_object() {
				parse_comments(&data["replies"], post_link, post_author, highlighted_comment, filters, req)
			} else {
				Vec::new()
			};
			build_comment(&comment, data, replies, post_link, post_author, highlighted_comment, filters, req)
		})
		.collect()
}

fn query_comments(
	json: &serde_json::Value,
	post_link: &str,
	post_author: &str,
	highlighted_comment: &str,
	filters: &HashSet<String>,
	query: &str,
	req: &Request<Body>,
) -> Vec<Comment> {
	let comments = json["data"]["children"].as_array().map_or(Vec::new(), std::borrow::ToOwned::to_owned);
	let mut results = Vec::new();

	for comment in comments {
		let data = &comment["data"];

		// If this comment contains replies, handle those too
		if data["replies"].is_object() {
			results.append(&mut query_comments(&data["replies"], post_link, post_author, highlighted_comment, filters, query, req));
		}

		let c = build_comment(&comment, data, Vec::new(), post_link, post_author, highlighted_comment, filters, req);
		if c.body.to_lowercase().contains(&query.to_lowercase()) {
			results.push(c);
		}
	}

	results
}
#[allow(clippy::too_many_arguments)]
fn build_comment(
	comment: &serde_json::Value,
	data: &serde_json::Value,
	replies: Vec<Comment>,
	post_link: &str,
	post_author: &str,
	highlighted_comment: &str,
	filters: &HashSet<String>,
	req: &Request<Body>,
) -> Comment {
	let id = val(comment, "id");

	let body = if (val(comment, "author") == "[deleted]" && val(comment, "body") == "[removed]") || val(comment, "body") == "[ Removed by Reddit ]" {
		format!(
			"<div class=\"md\"><p>[removed] — <a href=\"https://{}{post_link}{id}\">view removed comment</a></p></div>",
			get_setting("REDLIB_PUSHSHIFT_FRONTEND").unwrap_or_else(|| String::from(crate::config::DEFAULT_PUSHSHIFT_FRONTEND)),
		)
	} else {
		rewrite_emotes(&data["media_metadata"], val(comment, "body_html"))
	};
	let kind = comment["kind"].as_str().unwrap_or_default().to_string();

	let unix_time = data["created_utc"].as_f64().unwrap_or_default();
	let (rel_time, created) = time(unix_time);

	let edited = data["edited"].as_f64().map_or((String::new(), String::new()), time);

	let score = data["score"].as_i64().unwrap_or(0);

	// The JSON API only provides comments up to some threshold.
	// Further comments have to be loaded by subsequent requests.
	// The "kind" value will be "more" and the "count"
	// shows how many more (sub-)comments exist in the respective nesting level.
	// Note that in certain (seemingly random) cases, the count is simply wrong.
	let more_count = data["count"].as_i64().unwrap_or_default();

	// Scraped comments carry what loads them: the query string of a post
	// link that loads just this placeholder.
	let more_href = data["cursor"].as_str().filter(|cursor| !cursor.is_empty()).map_or(String::new(), |cursor| {
		url::form_urlencoded::Serializer::new(String::new())
			.append_pair("more_cursor", cursor)
			.append_pair("more_query", data["more_query"].as_str().unwrap_or_default())
			.finish()
	});

	let awards: Awards = Awards::parse(&data["all_awardings"]);

	let parent_kind_and_id = val(comment, "parent_id");
	let parent_info = parent_kind_and_id.split('_').collect::<Vec<&str>>();

	let highlighted = id == highlighted_comment;

	let author = Author {
		name: val(comment, "author"),
		flair: Flair {
			flair_parts: FlairPart::parse(
				data["author_flair_type"].as_str().unwrap_or_default(),
				data["author_flair_richtext"].as_array(),
				data["author_flair_text"].as_str(),
			),
			text: val(comment, "link_flair_text"),
			background_color: val(comment, "author_flair_background_color"),
			foreground_color: val(comment, "author_flair_text_color"),
		},
		distinguished: val(comment, "distinguished"),
	};
	let is_filtered = filters.contains(&["u_", author.name.as_str()].concat());

	// Many subreddits have a default comment posted about the sub's rules etc.
	// Many Redlib users do not wish to see this kind of comment by default.
	// Reddit does not tell us which users are "bots", so a good heuristic is to
	// collapse stickied moderator comments.
	let is_moderator_comment = data["distinguished"].as_str().unwrap_or_default() == "moderator";
	let is_stickied = data["stickied"].as_bool().unwrap_or_default();
	let collapsed = (is_moderator_comment && is_stickied) || is_filtered;

	Comment {
		id,
		kind,
		parent_id: parent_info[1].to_string(),
		parent_kind: parent_info[0].to_string(),
		post_link: post_link.to_string(),
		post_author: post_author.to_string(),
		body,
		author,
		score: if data["score_hidden"].as_bool().unwrap_or_default() {
			("\u{2022}".to_string(), "Hidden".to_string())
		} else {
			format_num(score)
		},
		rel_time,
		created,
		edited,
		replies,
		highlighted,
		awards,
		collapsed,
		is_filtered,
		more_count,
		more_href,
		prefs: Preferences::new(req),
	}
}
