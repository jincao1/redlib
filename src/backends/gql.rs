// Fallback for JSON API requests Reddit blocks for logged-out clients: the same
// data from Reddit's GraphQL API (gql-fed.reddit.com), which the Android app
// uses and which answers the app-only OAuth token.
//
// Each response is converted into the shape of the JSON API response it
// replaces, so the parsers and templates work unchanged.

use super::{json as reddit_json, Backend, Failure, Fetch, Kind};
use crate::client::{CLIENT, OAUTH_CLIENT};
use crate::utils::{enable_gql, format_url, param};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::DateTime;
use log::debug;
use serde_json::{json as json_value, Value};

const GQL_URL: &str = "https://gql-fed.reddit.com/";

// Persisted query ID from the Android app; Reddit rejects raw query text.
// Source: github.com/Amqx/reddit-gql `src/operations.ts`, or the APK's Apollo classes.
const POST_COMMENTS_HASH: &str = "314f29c2494fd78c7844ef2c6b09acb4bef5b37ab50b4ddc01691d6d5c8433f8";
// Reddit returns at most 200 comments per request, filled breadth-first
const COMMENT_COUNT: u32 = 200;
const MAX_DEPTH: u32 = 10;
/// `more_query` of the "more comments" links built here; their `more_cursor` is a GraphQL cursor.
pub const MORE_QUERY: &str = "gql";

pub struct Gql;

impl Backend for Gql {
	fn name(&self) -> &'static str {
		"gql"
	}

	fn available(&self) -> bool {
		enable_gql()
	}

	fn supports(&self, kind: Kind) -> bool {
		!matches!(kind, Kind::Wiki | Kind::Duplicates)
	}

	fn fetch<'a>(&'a self, path: &'a str, quarantine: bool) -> Fetch<'a> {
		Box::pin(async move {
			match json(path, quarantine).await {
				None => Err(Failure::Unsupported),
				// Quarantined subreddits stay unavailable here even after opting
				// in, so let the next backend try.
				Some(Err(reason)) if quarantine && (reason == "quarantined" || reason == "gated") => Err(Failure::Unsupported),
				Some(Err(reason)) if UNAVAILABLE_REASONS.contains(&reason.as_str()) => Err(Failure::Final(reason)),
				Some(result) => result.map_err(Failure::Invalid),
			}
		})
	}
}

/// The JSON API response for `path`, rebuilt from GraphQL. `None` if no query covers `path`.
pub async fn json(path: &str, quarantine: bool) -> Option<Result<Value, String>> {
	let route = path.split('?').next().unwrap_or_default();
	if route.contains("/comments/") {
		return Some(thread(path, quarantine).await);
	}
	let segments: Vec<&str> = route.trim_end_matches(".json").split('/').filter(|s| !s.is_empty()).collect();
	Some(match segments.as_slice() {
		["by_id", ids] => posts_by_ids(ids.split(',').collect()).await,
		["r", sub, "about"] => about(sub, quarantine).await,
		["search"] => search(path, None).await,
		["r", sub, "search"] => search(path, Some(sub)).await,
		["subreddits", "search"] => search_subreddits(path).await,
		["user", name, "about"] => user_about(name).await,
		["user", name, "submitted"] => user_posts(path, name, param(path, "after").as_deref()).await,
		["user", name, "comments"] => user_comments(path, name, param(path, "after").as_deref()).await,
		["user", name] | ["user", name, "overview"] => user_overview(path, name).await,
		["r", "all", rest @ ..] => all(path, sort(rest)?).await,
		["r", "popular", rest @ ..] => feed("PopularFeedElements", POPULAR_FEED_HASH, feed_variables(path, sort(rest)?), "/popular/elements").await,
		["r", sub, rest @ ..] if !sub.contains('+') && !sub.contains("%2B") => {
			let mut variables = feed_variables(path, sort(rest)?);
			variables["subredditName"] = (*sub).into();
			variables["includeSubredditInPosts"] = true.into();
			feed("SubredditFeedElements", SUBREDDIT_FEED_HASH, variables, "/postFeed/elements").await
		}
		_ => return None,
	})
}

/// Builds a `/comments/` JSON response for `path` (a `/comments/` API path, as
/// built by `post::item`). Supports the `sort` and `more_cursor` query
/// parameters and single comment threads.
async fn thread(path: &str, quarantine: bool) -> Result<Value, String> {
	let (id, comment_id) = parse_path(path).ok_or_else(|| format!("Couldn't parse post path | {path}"))?;

	let (post, comments) = futures_lite::future::zip(reddit_json(format!("/by_id/t3_{id}.json?raw_json=1"), quarantine), comments(&id, comment_id, path)).await;
	Ok(json_value!([post?, listing(comments?)]))
}

fn parse_path(path: &str) -> Option<(String, Option<String>)> {
	let path = path.split('?').next()?.trim_end_matches(".json");
	let mut segments = path.split('/').skip_while(|s| *s != "comments").skip(1);
	let id = segments.next().filter(|s| !s.is_empty())?.to_string();
	let comment_id = segments.nth(1).filter(|s| !s.is_empty()).map(str::to_string);
	Some((id, comment_id))
}

fn listing(children: Vec<Value>) -> Value {
	json_value!({ "kind": "Listing", "data": { "children": children } })
}

/// Escaped URL attribute value, proxied through Redlib for Reddit media. Empty for `javascript:` URLs.
fn url_attr(value: Option<&str>) -> String {
	let value = value.unwrap_or_default().trim();
	if value.to_lowercase().starts_with("javascript:") {
		return String::new();
	}
	htmlescape::encode_minimal(&format_url(value))
}

async fn comments(id: &str, comment_id: Option<String>, path: &str) -> Result<Vec<Value>, String> {
	let sort = param(path, "sort").filter(|s| !s.is_empty()).unwrap_or_else(|| "confidence".to_string());
	let mut variables = json_value!({
		"id": format!("t3_{id}"),
		"sortType": sort.to_uppercase(),
		"count": COMMENT_COUNT,
		"maxDepth": MAX_DEPTH,
		"includeConsistencyToken": false,
		"includePremiumAvatarTreatment": false,
	});
	if let Some(comment_id) = comment_id {
		variables["focusedCommentContext"] = json_value!({ "commentId": format!("t1_{comment_id}"), "isFocusedThreadOnly": true });
	}
	if let Some(cursor) = param(path, "more_cursor").filter(|_| param(path, "more_query").as_deref() == Some(MORE_QUERY)) {
		variables["after"] = cursor.into();
	}

	let response = query("PostComments", POST_COMMENTS_HASH, &variables).await?;
	comment_tree(&response, id)
}

fn comment_tree(response: &Value, post_id: &str) -> Result<Vec<Value>, String> {
	let trees = response["data"]["postInfoById"]["commentForest"]["trees"]
		.as_array()
		.ok_or_else(|| format!("No comments in response: {}", response["errors"]))?;
	Ok(nest(trees.iter().filter_map(|tree| item(tree, post_id)).collect()))
}

/// Runs a persisted query. Fields that time out upstream come back null with an
/// error next to the data; one retry usually fills them in.
async fn query(operation: &str, hash: &str, variables: &Value) -> Result<Value, String> {
	let body = json_value!({
		"operationName": operation,
		"variables": variables,
		"extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } },
	});

	let mut response = Value::Null;
	for _ in 0..2 {
		let mut builder = CLIENT.load_full().post(GQL_URL);
		for (key, value) in &OAUTH_CLIENT.load_full().headers_map {
			// `json()` sets its own; a duplicate Content-Type fails every query
			if !key.eq_ignore_ascii_case("content-type") {
				builder = builder.header(key, value);
			}
		}
		let res = builder.json(&body).send().await.map_err(|e| e.to_string())?;
		let status = res.status();
		response = res.json().await.map_err(|e| format!("{operation} returned {status}: {e}"))?;

		if response["data"].is_null() {
			return Err(format!("{operation} returned {status}: {}", response["errors"]));
		}
		if response["errors"].is_null() {
			break;
		}
		debug!("{operation} returned partial data, retrying: {}", response["errors"]);
	}
	Ok(response)
}

/// Converts one entry of the comment forest to an item of the JSON API, with its depth.
fn item(tree: &Value, post_id: &str) -> Option<(u64, Value)> {
	let depth = tree["depth"].as_u64()?;
	let post_fullname = format!("t3_{post_id}");
	let parent = tree["parentId"].as_str().unwrap_or(&post_fullname).to_string();

	let more = &tree["more"];
	if !more.is_null() {
		return Some((
			depth,
			json_value!({ "kind": "more", "data": {
				"id": "",
				"count": more["count"].as_i64().unwrap_or_default(),
				"parent_id": parent,
				"cursor": more["cursor"],
				"more_query": MORE_QUERY,
			} }),
		));
	}

	let node = &tree["node"];
	let fullname = node["id"].as_str().unwrap_or_default();
	let author = node["authorInfo"]["name"].as_str().unwrap_or("[unknown]");
	let content = &node["content"];

	// The JSON API's representation of deleted and removed comments, which post.rs looks for
	let (author, body, body_html) = if node["__typename"] == "DeletedComment" {
		("[deleted]", "[deleted]", "<p>[deleted]</p>".to_string())
	} else if node["isAdminTakedown"] == true {
		(author, "[ Removed by Reddit ]", String::new())
	} else if node["isRemoved"] == true {
		("[deleted]", "[removed]", String::new())
	} else {
		let richtext: Value = content["richtext"].as_str().and_then(|r| serde_json::from_str(r).ok()).unwrap_or_default();
		(
			author,
			content["markdown"].as_str().unwrap_or_default(),
			render(&richtext["document"], &content["richtextMedia"]),
		)
	};

	let flair = &node["authorFlair"];
	let (flair_type, flair_richtext) = flair_parts(flair);

	Some((
		depth,
		json_value!({
			"kind": "t1",
			"data": {
				"id": fullname.strip_prefix("t1_").unwrap_or(fullname),
				"name": fullname,
				"author": author,
				"body": body,
				"body_html": format!("<div class=\"md\">{body_html}</div>"),
				"created_utc": timestamp(&node["createdAt"]).unwrap_or_default(),
				"edited": timestamp(&node["editedAt"]).map_or(Value::Bool(false), Value::from),
				"score": node["score"].as_i64().unwrap_or_default(),
				"score_hidden": node["isScoreHidden"] == true,
				"stickied": node["isStickied"] == true,
				"distinguished": node["distinguishedAs"].as_str().map(str::to_lowercase),
				"author_flair_type": flair_type,
				"author_flair_richtext": flair_richtext,
				"author_flair_text": flair["text"],
				"author_flair_background_color": flair["template"]["backgroundColor"],
				"author_flair_text_color": flair["textColor"].as_str().map(str::to_lowercase),
				"all_awardings": awards(node),
				"parent_id": parent,
				"replies": "",
			}
		}),
	))
}

/// The JSON API's `*_flair_type` and `*_flair_richtext` of a flair.
fn flair_parts(flair: &Value) -> (&'static str, Value) {
	let richtext: Value = flair["richtext"].as_str().and_then(|r| serde_json::from_str(r).ok()).unwrap_or_default();
	let kind = if richtext.as_array().is_some_and(|r| !r.is_empty()) {
		"richtext"
	} else if flair["text"].as_str().is_some_and(|t| !t.is_empty()) {
		"text"
	} else {
		""
	};
	(kind, richtext)
}

fn awards(node: &Value) -> Vec<Value> {
	node["awardings"]
		.as_array()
		.into_iter()
		.flatten()
		.map(|a| json_value!({ "name": a["award"]["name"], "count": a["total"], "resized_icons": [{ "url": a["award"]["static_icon_16"]["url"] }] }))
		.collect()
}

const POSTS_BY_IDS_HASH: &str = "0e471495bcba33554b6c540b3cf1cae23dd5432a8b741dddc7fd6717dc2520a7";
const SUBREDDIT_FEED_HASH: &str = "97046e2a48050f10a507db354a8da60955ffccb586dcf63163bdfc6610d5cf7a";
const POPULAR_FEED_HASH: &str = "728c0f9a4d12c17b8cffe07669bb1a8770b0f2ec0c84e040485f8fad2d9c417f";
const ALL_FEED_HASH: &str = "0e9c0045dd244e934efd0ea4fefc00cb0b4d91523e2f09c8af075856cb59b937";
const SORTS: [&str; 6] = ["hot", "new", "top", "rising", "controversial", "best"];

/// The listing sort of the path segments after `/r/<sub>`, if they are one.
fn sort<'a>(rest: &[&'a str]) -> Option<&'a str> {
	match rest {
		[] => Some("hot"),
		[sort] if SORTS.contains(sort) => Some(sort),
		_ => None,
	}
}

/// Variables of a post feed query for the listing `path`.
fn feed_variables(path: &str, sort: &str) -> Value {
	let sort = sort.to_uppercase();
	let mut variables = json_value!({
		"sort": sort,
		"pageSize": param(path, "limit").and_then(|l| l.parse::<u32>().ok()).unwrap_or(25),
	});
	if let Some(range) = param(path, "t").filter(|t| !t.is_empty() && (sort == "TOP" || sort == "CONTROVERSIAL")) {
		variables["range"] = range.to_uppercase().into();
	}
	// Feed cursors are the base64 of the JSON API's `after`
	if let Some(after) = param(path, "after").filter(|a| !a.is_empty()) {
		variables["after"] = STANDARD.encode(after).into();
	}
	variables
}

/// A JSON API listing of the posts in a GraphQL connection (`edges` + `pageInfo`).
fn post_listing(connection: &Value) -> Value {
	let children: Vec<Value> = connection["edges"].as_array().into_iter().flatten().filter_map(|edge| post(&edge["node"])).collect();
	json_value!({ "kind": "Listing", "data": { "after": after(connection), "children": children } })
}

/// The JSON API's `after` of the next page of a GraphQL connection.
fn after(connection: &Value) -> Option<String> {
	if connection["pageInfo"]["hasNextPage"] == false {
		return None;
	}
	let cursor = STANDARD.decode(connection["pageInfo"]["endCursor"].as_str()?).ok()?;
	String::from_utf8(cursor).ok()
}

async fn feed(operation: &str, hash: &str, variables: Value, connection: &str) -> Result<Value, String> {
	let response = query(operation, hash, &variables).await?;
	let connection = response["data"].pointer(connection).ok_or_else(|| format!("{operation} returned no posts"))?;
	Ok(post_listing(connection))
}

async fn posts_by_ids(ids: Vec<&str>) -> Result<Value, String> {
	let variables = json_value!({ "ids": ids, "includePostContentPostHint": true, "includePostContentThumbnailEnabled": true });
	let response = query("PostsByIds", POSTS_BY_IDS_HASH, &variables).await?;
	let posts = response["data"]["postsInfoByIds"].as_array().ok_or("PostsByIds returned no posts")?;
	Ok(listing(posts.iter().filter_map(post).collect()))
}

/// r/all only returns the IDs of its posts.
async fn all(path: &str, sort: &str) -> Result<Value, String> {
	let mut variables = feed_variables(path, sort);
	variables["time"] = variables["range"].take();
	if let Some(variables) = variables.as_object_mut() {
		variables.remove("range");
		variables.remove("pageSize");
	}
	let response = query("AllFeedSdui", ALL_FEED_HASH, &variables).await?;
	let elements = &response["data"]["allV3"]["elements"];
	let ids: Vec<&str> = elements["edges"]
		.as_array()
		.into_iter()
		.flatten()
		// Ads are posts too, marked by their `adPayload`
		.filter(|edge| edge["node"]["adPayload"].is_null())
		.filter_map(|edge| edge["node"]["groupId"].as_str())
		.filter(|id| id.starts_with("t3_"))
		.collect();
	let mut posts = Box::pin(posts_by_ids(ids)).await?;
	posts["data"]["after"] = after(elements).into();
	Ok(posts)
}

/// Converts a GraphQL post into a post of the JSON API.
fn post(node: &Value) -> Option<Value> {
	let node = if node["postInfo"].is_object() { &node["postInfo"] } else { node };
	if !matches!(node["__typename"].as_str(), Some("SubredditPost" | "ProfilePost")) {
		return None;
	}
	let fullname = node["id"].as_str()?;
	let content = &node["content"];
	let richtext: Value = content["richtext"].as_str().and_then(|r| serde_json::from_str(r).ok()).unwrap_or_default();
	let (flair_type, flair_richtext) = flair_parts(&node["flair"]);
	let (author_flair_type, author_flair_richtext) = flair_parts(&node["authorFlair"]);
	let thumbnail = if node["isThumbnailEnabled"] == false { &Value::Null } else { &node["thumbnail"] };
	// PostsByIds calls it `postTitle`, the feeds `title`.
	let title = if node["title"].is_string() { &node["title"] } else { &node["postTitle"] };

	let mut data = json_value!({
		"id": fullname.strip_prefix("t3_").unwrap_or(fullname),
		"name": fullname,
		"title": title,
		"author": node["authorInfo"]["name"].as_str().unwrap_or("[deleted]"),
		"author_flair_type": author_flair_type,
		"author_flair_richtext": author_flair_richtext,
		"author_flair_text": node["authorFlair"]["text"],
		"author_flair_background_color": node["authorFlair"]["template"]["backgroundColor"],
		"author_flair_text_color": node["authorFlair"]["textColor"].as_str().map(str::to_lowercase),
		// Posts on a user's profile (`ProfilePost`) have a profile instead of a subreddit
		"subreddit": node["subreddit"]["name"].as_str().or(node["profile"]["name"].as_str()),
		"subreddit_name_prefixed": node["subreddit"]["prefixedName"].as_str().or(node["profile"]["redditorInfo"]["prefixedName"].as_str()),
		"permalink": node["permalink"],
		"url": node["url"],
		"domain": node["domain"],
		"is_self": node["isSelfPost"] == true,
		"selftext": content["markdown"].as_str().unwrap_or_default(),
		"selftext_html": if content.is_object() { format!("<div class=\"md\">{}</div>", render(&richtext["document"], &content["richtextMedia"])) } else { String::new() },
		"score": node["score"],
		"upvote_ratio": node["upvoteRatio"],
		"num_comments": node["commentCount"],
		"created_utc": timestamp(&node["createdAt"]).unwrap_or_default(),
		"edited": timestamp(&node["editedAt"]).map_or(Value::Bool(false), Value::from),
		"over_18": node["isNsfw"] == true,
		"spoiler": node["isSpoiler"] == true,
		"stickied": node["isStickied"] == true,
		"locked": node["isLocked"] == true,
		"archived": node["isArchived"] == true,
		"hide_score": node["isScoreHidden"] == true,
		"distinguished": node["distinguishedAs"].as_str().map(str::to_lowercase),
		"removed_by_category": node["removedByCategory"].as_str().map(str::to_lowercase),
		"link_flair_type": flair_type,
		"link_flair_richtext": flair_richtext,
		"link_flair_text": node["flair"]["text"],
		"link_flair_background_color": node["flair"]["template"]["backgroundColor"],
		"link_flair_text_color": node["flair"]["textColor"].as_str().map(str::to_lowercase),
		"thumbnail": thumbnail["url"].as_str().unwrap_or_default(),
		"thumbnail_width": thumbnail["dimensions"]["width"],
		"thumbnail_height": thumbnail["dimensions"]["height"],
		"post_hint": node["postHint"].as_str().map(|hint| hint.to_lowercase().replace('_', ":")),
		"all_awardings": awards(node),
	});
	if node["isSelfPost"] != true {
		data["url_overridden_by_dest"] = node["url"].clone();
	}

	let still = &node["media"]["still"]["source"];
	if still.is_object() {
		data["preview"] = json_value!({ "images": [{ "source": { "url": still["url"], "width": still["dimensions"]["width"], "height": still["dimensions"]["height"] } }] });
	}

	let streaming = &node["media"]["streaming"];
	if let Some(hls_url) = streaming["hlsUrl"].as_str() {
		data["secure_media"] = json_value!({ "reddit_video": { "fallback_url": video_fallback_url(node, streaming), "hls_url": hls_url, "is_gif": streaming["isGif"] == true } });
	}

	if let Some(items) = node["gallery"]["items"].as_array() {
		data["is_gallery"] = true.into();
		data["gallery_data"] = json_value!({ "items": items.iter().map(|item| json_value!({ "media_id": item["media"]["id"], "caption": item["caption"], "outbound_url": item["outboundUrl"] })).collect::<Vec<_>>() });
		data["media_metadata"] = items
			.iter()
			.map(|item| {
				let m = &item["media"];
				let gif = if m["mimetype"] == "image/gif" { m["url"].clone() } else { Value::Null };
				(
					m["id"].as_str().unwrap_or_default().to_string(),
					json_value!({ "m": m["mimetype"], "s": { "u": m["url"], "gif": gif, "x": m["width"], "y": m["height"] } }),
				)
			})
			.collect::<serde_json::Map<_, _>>()
			.into();
	}

	if let Some(parent) = post(&node["crosspostRoot"]["post"]) {
		data["crosspost_parent_list"] = json_value!([parent["data"]]);
	}

	Some(json_value!({ "kind": "t3", "data": data }))
}

const SUBREDDIT_INFO_HASH: &str = "6b9c1679e69097e1c6df364adc11183afe6e2b6545dd1c0c1cc8f7490448c3e5";

/// Why Reddit won't show a subreddit, as the JSON API's `reason` names it.
const UNAVAILABLE_REASONS: [&str; 4] = ["private", "banned", "quarantined", "gated"];

async fn about(name: &str, quarantine: bool) -> Result<Value, String> {
	let variables = json_value!({ "subredditName": name, "filterGated": !quarantine, "includeDevvitData": false, "includeRecapFields": false, "includeWelcomePage": false });
	let response = query("SubredditInfoByName", SUBREDDIT_INFO_HASH, &variables).await?;
	about_json(&response["data"]["subredditInfoByName"], name)
}

/// The `about.json` of a `SubredditInfoByName` node, or why Reddit won't show
/// the subreddit (`private`, `banned`, `quarantined` or `gated`).
fn about_json(node: &Value, name: &str) -> Result<Value, String> {
	if node["__typename"] == "UnavailableSubreddit" {
		return Err(match node["forbiddenReason"].as_str() {
			Some(reason) => reason.to_lowercase(),
			None => format!("r/{name} is unavailable"),
		});
	}
	subreddit(node).ok_or_else(|| format!("r/{name} not found"))
}

/// Converts a GraphQL subreddit into a subreddit of the JSON API (`about.json`).
fn subreddit(node: &Value) -> Option<Value> {
	let name = node["name"].as_str()?;
	let description = &node["description"];
	let richtext: Value = description["richtext"].as_str().and_then(|r| serde_json::from_str(r).ok()).unwrap_or_default();
	Some(json_value!({ "kind": "t5", "data": {
		"name": node["id"],
		"display_name": name,
		"display_name_prefixed": node["prefixedName"],
		"url": node["path"],
		"title": node["title"],
		"public_description": node["publicDescriptionText"],
		"description": description["markdown"],
		"description_html": format!("<div class=\"md\">{}</div>", render(&richtext["document"], &Value::Null)),
		"community_icon": node["styles"]["icon"],
		"icon_img": node["styles"]["legacyIcon"]["url"],
		"subscribers": node["subscribersCount"],
		"accounts_active": node["activeCount"],
		"wiki_enabled": node["wikiEditMode"].as_str().is_some_and(|mode| mode != "DISABLED"),
		"over18": node["isNsfw"] == true,
	} }))
}

const SEARCH_POSTS_HASH: &str = "8610e33521c90caa8d12576800c18b708ce0d19df3dfac8e2aad55d9860e4bf9";
const SEARCH_COMMUNITIES_HASH: &str = "a3333069c1ead3ceec6d9bf4da4185ff584f69d0ba8b586e545b32ea62170932";

/// Variables of a search query for the search `path`; its cursors are the base64 of the JSON API's `after`.
fn search_variables(path: &str, query: String) -> Value {
	let mut variables = json_value!({
		"query": query,
		"filters": [],
		"productSurface": "",
		"pageSize": param(path, "limit").and_then(|l| l.parse::<u32>().ok()).unwrap_or(25),
	});
	if let Some(after) = param(path, "after").filter(|a| !a.is_empty()) {
		variables["afterCursor"] = STANDARD.encode(after).into();
	}
	variables
}

async fn search(path: &str, sub: Option<&str>) -> Result<Value, String> {
	let mut query = param(path, "q").unwrap_or_default();
	if let Some(sub) = sub.filter(|_| param(path, "restrict_sr").is_some_and(|r| !r.is_empty())) {
		query = format!("{query} subreddit:{sub}");
	}
	let sort = param(path, "sort").filter(|s| !s.is_empty()).unwrap_or_else(|| "relevance".to_string());
	let mut variables = search_variables(path, query);
	variables["sort"] = sort.to_uppercase().into();
	if let Some(range) = param(path, "t").filter(|t| !t.is_empty() && t != "all" && sort != "new") {
		variables["filters"] = json_value!([{ "key": "time_range", "value": range }]);
	}
	let mut listing = feed("SearchPosts", SEARCH_POSTS_HASH, variables, "/search/general/posts").await?;
	if param(path, "include_over_18").is_none() {
		if let Some(posts) = listing["data"]["children"].as_array_mut() {
			posts.retain(|post| post["data"]["over_18"] != true);
		}
	}
	Ok(listing)
}

async fn search_subreddits(path: &str) -> Result<Value, String> {
	let variables = search_variables(path, param(path, "q").unwrap_or_default());
	let response = query("SearchCommunities", SEARCH_COMMUNITIES_HASH, &variables).await?;
	let communities = response["data"]["search"]["general"]["communities"]["edges"]
		.as_array()
		.ok_or("SearchCommunities returned no subreddits")?;
	Ok(listing(communities.iter().filter_map(|edge| subreddit(&edge["node"])).collect()))
}

const PROFILE_HASH: &str = "096ccf5d943485822cb6d3e0dc13f9977da1984514e1d539e1ed38a51158e689";
const SEARCH_PEOPLE_HASH: &str = "d9211ac33f72079f25cb4c8a8e00a2b4889e984f70278ec91283965c7856d33d";
const USER_POSTS_HASH: &str = "78a213c02a340db4a707d4bcd375ce34f733b75893379603acb4ec20388fafe6";
const USER_COMMENTS_HASH: &str = "e1b37f521fb40af2120946b70e7fbf6a3d924db2abaf28ff2164d4cfacca28db";
const COMMENTS_BY_IDS_HASH: &str = "51ab9f2e0eb87b6dac9aac02012ba7046049e516ec55ec442abfed0f19c6f311";

/// The profile only has the karma in search results.
async fn user_about(name: &str) -> Result<Value, String> {
	let (profile, people) = futures_lite::future::zip(
		query("ProfileDetailsByName", PROFILE_HASH, &json_value!({ "profileName": name })),
		query(
			"SearchPeople",
			SEARCH_PEOPLE_HASH,
			&json_value!({ "query": name, "filters": [], "pageSize": 5, "productSurface": "" }),
		),
	)
	.await;
	let karma = people.ok().and_then(|people| {
		let authors = people["data"]["search"]["general"]["authors"]["edges"].as_array()?.clone();
		authors
			.into_iter()
			.find(|a| a["node"]["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
			.map(|a| a["node"]["karma"]["total"].clone())
	});
	user(&profile?["data"]["profileByName"], karma.unwrap_or_default()).ok_or_else(|| format!("u/{name} not found"))
}

/// Converts a GraphQL profile into a user of the JSON API (`about.json`).
fn user(profile: &Value, karma: Value) -> Option<Value> {
	let name = profile["name"].as_str()?;
	Some(json_value!({ "kind": "t2", "data": {
		"name": name.strip_prefix("u_").unwrap_or(name),
		"created": timestamp(&profile["createdAt"]),
		"total_karma": karma,
		"subreddit": {
			"title": profile["title"],
			"icon_img": profile["styles"]["icon"],
			"banner_img": profile["styles"]["legacyBannerBackgroundImage"],
			"public_description": profile["publicDescriptionText"],
			"over_18": profile["isNsfw"] == true,
		},
	} }))
}

fn profile_variables(path: &str, after: Option<&str>) -> Value {
	let mut variables = feed_variables(path, &param(path, "sort").filter(|s| !s.is_empty()).unwrap_or_else(|| "new".to_string()));
	match after.filter(|a| !a.is_empty()) {
		Some(after) => variables["after"] = STANDARD.encode(after).into(),
		None => variables["after"] = Value::Null,
	}
	variables
}

async fn user_posts(path: &str, name: &str, cursor: Option<&str>) -> Result<Value, String> {
	let mut variables = profile_variables(path, cursor);
	variables["username"] = name.into();
	variables["includeSubredditInPosts"] = true.into();
	feed("UserSubmittedPosts", USER_POSTS_HASH, variables, "/postFeed/posts").await
}

/// The comments come without their content; `LiveCommentsByIds` fills it in.
async fn user_comments(path: &str, name: &str, cursor: Option<&str>) -> Result<Value, String> {
	let mut variables = profile_variables(path, cursor);
	variables["name"] = name.into();
	let response = query("SubmittedComments", USER_COMMENTS_HASH, &variables).await?;
	let connection = &response["data"]["redditorInfoByName"]["comments"];
	let ids: Vec<&str> = connection["edges"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|edge| edge["node"]["id"].as_str())
		.collect();
	let full = query(
		"LiveCommentsByIds",
		COMMENTS_BY_IDS_HASH,
		&json_value!({ "ids": ids, "includePremiumAvatarTreatment": false }),
	)
	.await?;
	let children = user_comment_items(connection, &full["data"]["commentsByIds"]);
	Ok(json_value!({ "kind": "Listing", "data": { "after": after(connection), "children": children } }))
}

/// JSON API comments of a user, from their `SubmittedComments` previews and full `LiveCommentsByIds` comments.
fn user_comment_items(previews: &Value, full: &Value) -> Vec<Value> {
	let full = full.as_array().map(Vec::as_slice).unwrap_or_default();
	previews["edges"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|edge| {
			let preview = &edge["node"];
			let comment = full.iter().find(|c| c["id"] == preview["id"])?;
			let (_, mut item) = item(&json_value!({ "depth": 0, "parentId": preview["postInfo"]["id"], "node": comment }), "")?;
			let permalink = comment["permalink"].as_str().unwrap_or_default();
			let subreddit = match permalink.split('/').nth(1) {
				Some("r") => permalink.split('/').nth(2).unwrap_or_default().to_string(),
				_ => format!("u_{}", permalink.split('/').nth(2).unwrap_or_default()),
			};
			item["data"]["link_title"] = preview["postInfo"]["title"].clone();
			item["data"]["link_id"] = preview["postInfo"]["id"].clone();
			item["data"]["permalink"] = permalink.into();
			item["data"]["subreddit"] = subreddit.into();
			Some(item)
		})
		.collect()
}

/// Posts and comments merged, newest first. The two have separate cursors, so
/// its `after` is `<post after>,<comment after>`, with `-` for a side that has no more.
async fn user_overview(path: &str, name: &str) -> Result<Value, String> {
	let after = param(path, "after").unwrap_or_default();
	let (post_after, comment_after) = after.split_once(',').unwrap_or(("", ""));
	let posts = async {
		match post_after {
			"-" => Ok(Value::Null),
			after => user_posts(path, name, Some(after)).await,
		}
	};
	let comments = async {
		match comment_after {
			"-" => Ok(Value::Null),
			after => user_comments(path, name, Some(after)).await,
		}
	};
	let (posts, comments) = futures_lite::future::zip(posts, comments).await;
	Ok(merge_overview(&posts?, &comments?, post_after, comment_after))
}

/// Merges a page of posts and a page of comments. Items older than the last one
/// of a side that has more pages wait for the next page, so the order holds across pages.
fn merge_overview(posts: &Value, comments: &Value, post_after: &str, comment_after: &str) -> Value {
	let created = |item: &Value| item["data"]["created_utc"].as_f64().unwrap_or_default();
	let sides = [(posts, post_after), (comments, comment_after)];
	let cutoff = sides
		.iter()
		.filter(|(listing, _)| listing["data"]["after"].is_string())
		.filter_map(|(listing, _)| listing["data"]["children"].as_array()?.last().map(created))
		.fold(f64::MIN, f64::max);
	let mut children = Vec::new();
	let mut next = Vec::new();
	for (listing, after) in sides {
		let page = listing["data"]["children"].as_array().map(Vec::as_slice).unwrap_or_default();
		let kept: Vec<Value> = page.iter().filter(|item| created(item) >= cutoff).cloned().collect();
		next.push(match kept.last() {
			_ if kept.len() == page.len() && !listing["data"]["after"].is_string() => "-".to_string(),
			Some(last) => last["data"]["name"].as_str().unwrap_or_default().to_string(),
			None => after.to_string(),
		});
		children.extend(kept);
	}
	children.sort_by(|a, b| created(b).total_cmp(&created(a)));
	let after = next.iter().any(|a| *a != "-").then(|| next.join(","));
	json_value!({ "kind": "Listing", "data": { "after": after, "children": children } })
}

/// The video-only MP4 the JSON API links as `fallback_url`: the largest rendition that fits the video.
fn video_fallback_url(node: &Value, streaming: &Value) -> String {
	let dimensions = &streaming["dimensions"];
	let size = dimensions["width"].as_u64().unwrap_or_default().min(dimensions["height"].as_u64().unwrap_or_default());
	let rendition = [1080, 720, 480, 360, 240].into_iter().find(|r| *r <= size).unwrap_or(240);
	format!("{}/CMAF_{rendition}.mp4?source=fallback", node["url"].as_str().unwrap_or_default().trim_end_matches('/'))
}

fn timestamp(value: &Value) -> Option<f64> {
	DateTime::parse_from_str(value.as_str()?, "%Y-%m-%dT%H:%M:%S%.f%z").ok().map(|t| t.timestamp() as f64)
}

/// Builds the reply tree from items in depth-first order. Goes by depth rather
/// than parent IDs, as deleted comments have no ID.
fn nest(items: Vec<(u64, Value)>) -> Vec<Value> {
	let mut top = Vec::new();
	let mut open: Vec<(u64, Value)> = Vec::new();
	for (depth, item) in items {
		while open.last().is_some_and(|(d, _)| *d >= depth) {
			close(&mut open, &mut top);
		}
		open.push((depth, item));
	}
	while !open.is_empty() {
		close(&mut open, &mut top);
	}
	top
}

/// Attaches the innermost open item to its parent, or to the top level.
fn close(open: &mut Vec<(u64, Value)>, top: &mut Vec<Value>) {
	let Some((_, item)) = open.pop() else { return };
	let Some((_, parent)) = open.last_mut() else {
		top.push(item);
		return;
	};
	let replies = &mut parent["data"]["replies"];
	if !replies.is_object() {
		*replies = listing(Vec::new());
	}
	if let Some(children) = replies["data"]["children"].as_array_mut() {
		children.push(item);
	}
}

/// Renders richtext (RTJSON) nodes as the HTML of the JSON API's `body_html`.
fn render(nodes: &Value, media: &Value) -> String {
	nodes.as_array().into_iter().flatten().map(|node| element(node, media)).collect()
}

fn element(node: &Value, media: &Value) -> String {
	let children = || render(&node["c"], media);
	match node["e"].as_str().unwrap_or_default() {
		// A paragraph of only images is a figure, like the JSON API's
		"par" if node["c"].as_array().is_some_and(|c| c.iter().all(|c| matches!(c["e"].as_str(), Some("img" | "gif")))) => children(),
		"par" => format!("<p>{}</p>", children()),
		"text" => formatted(node),
		"br" => "<br>".to_string(),
		"hr" => "<hr>".to_string(),
		"h" => {
			let level = node["l"].as_u64().unwrap_or(1).clamp(1, 6);
			format!("<h{level}>{}</h{level}>", children())
		}
		"blockquote" => format!("<blockquote>{}</blockquote>", children()),
		"list" => {
			let tag = if node["o"] == true { "ol" } else { "ul" };
			format!("<{tag}>{}</{tag}>", children())
		}
		"li" => format!("<li>{}</li>", children()),
		"code" => {
			let lines: Vec<&str> = node["c"].as_array().into_iter().flatten().map(|line| line["t"].as_str().unwrap_or_default()).collect();
			format!("<pre><code>{}</code></pre>", htmlescape::encode_minimal(&lines.join("\n")))
		}
		"spoilertext" => format!("<span class=\"md-spoiler-text\">{}</span>", children()),
		"link" => {
			let title = node["a"].as_str().map(|a| format!(" title=\"{}\"", htmlescape::encode_minimal(a))).unwrap_or_default();
			format!("<a href=\"{}\"{title}>{}</a>", url_attr(node["u"].as_str()), formatted(node))
		}
		kind @ ("r/" | "u/") => {
			let name = htmlescape::encode_minimal(node["t"].as_str().unwrap_or_default());
			let slash = if node["l"] == true { "/" } else { "" };
			format!("<a href=\"/{kind}{name}\">{slash}{kind}{name}</a>")
		}
		"img" | "gif" => {
			let Some(item) = media.as_array().and_then(|m| m.iter().find(|m| m["id"] == node["id"])) else {
				return String::new();
			};
			let src = url_attr(item["url"].as_str());
			format!("<figure><a href=\"{src}\"><img src=\"{src}\" loading=\"lazy\"></a></figure>")
		}
		"table" => table(node, media),
		// Unknown elements: keep their content
		_ if node["c"].is_array() => children(),
		_ => htmlescape::encode_minimal(node["t"].as_str().unwrap_or_default()),
	}
}

/// RTJSON format bits and their tags
const FORMATS: [(u64, &str); 6] = [(1, "strong"), (2, "em"), (8, "del"), (16, "sub"), (32, "sup"), (64, "code")];

/// Text with format ranges (`[bits, start, length]` in characters) as HTML.
fn formatted(node: &Value) -> String {
	let text: Vec<char> = node["t"].as_str().unwrap_or_default().chars().collect();
	let mut bits = vec![0u64; text.len()];
	for range in node["f"].as_array().into_iter().flatten() {
		let (Some(format), Some(start), Some(len)) = (range[0].as_u64(), range[1].as_u64(), range[2].as_u64()) else {
			continue;
		};
		for b in bits.iter_mut().skip(start as usize).take(len as usize) {
			*b |= format;
		}
	}

	let mut out = String::new();
	let mut start = 0;
	while start < text.len() {
		let end = (start..text.len()).find(|&i| bits[i] != bits[start]).unwrap_or(text.len());
		let tags: Vec<&str> = FORMATS.iter().filter(|(bit, _)| bits[start] & bit != 0).map(|(_, tag)| *tag).collect();
		tags.iter().for_each(|tag| out += &format!("<{tag}>"));
		out += &htmlescape::encode_minimal(&text[start..end].iter().collect::<String>());
		tags.iter().rev().for_each(|tag| out += &format!("</{tag}>"));
		start = end;
	}
	out
}

fn table(node: &Value, media: &Value) -> String {
	let align = |header: Option<&Value>| match header.and_then(|h| h["a"].as_str()) {
		Some("L") => " align=\"left\"",
		Some("C") => " align=\"center\"",
		Some("R") => " align=\"right\"",
		_ => "",
	};
	let headers = node["h"].as_array().map(Vec::as_slice).unwrap_or_default();

	let mut out = "<table><thead><tr>".to_string();
	for header in headers {
		out += &format!("<th{}>{}</th>", align(Some(header)), render(&header["c"], media));
	}
	out += "</tr></thead><tbody>";
	for row in node["c"].as_array().into_iter().flatten() {
		out += "<tr>";
		for (i, cell) in row.as_array().into_iter().flatten().enumerate() {
			out += &format!("<td{}>{}</td>", align(headers.get(i)), render(&cell["c"], media));
		}
		out += "</tr>";
	}
	out + "</tbody></table>"
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_render() {
		let doc = json_value!([
			{ "e": "par", "c": [
				{ "e": "text", "t": "a <b> ünï bold", "f": [[1, 10, 4], [3, 12, 2]] },
				{ "e": "link", "t": "x", "u": "javascript:alert(1)", "a": "t\"" },
				{ "e": "r/", "t": "rust", "l": true },
				{ "e": "spoilertext", "c": [{ "e": "text", "t": "s" }] },
			]},
			{ "e": "par", "c": [{ "e": "gif", "id": "giphy|g" }, { "e": "img", "id": "missing" }] },
			{ "e": "code", "c": [{ "e": "raw", "t": "<x>" }, { "e": "raw", "t": "y" }] },
			{ "e": "list", "o": true, "c": [{ "e": "li", "c": [{ "e": "par", "c": [{ "e": "text", "t": "i" }] }] }] },
			{ "e": "table", "h": [{ "a": "R", "c": [{ "e": "text", "t": "H" }] }, { "c": [] }], "c": [[{ "c": [{ "e": "text", "t": "1" }] }, { "c": [] }]] },
			{ "e": "future", "c": [{ "e": "text", "t": "kept" }] },
		]);
		let media = json_value!([{ "id": "giphy|g", "url": "https://external-preview.redd.it/g.gif?s=1&a=2" }]);
		assert_eq!(
			render(&doc, &media),
			concat!(
				r#"<p>a &lt;b&gt; ünï <strong>bo</strong><strong><em>ld</em></strong><a href="" title="t&quot;">x</a><a href="/r/rust">/r/rust</a><span class="md-spoiler-text">s</span></p>"#,
				r#"<figure><a href="/preview/external-pre/g.gif?s=1&amp;a=2"><img src="/preview/external-pre/g.gif?s=1&amp;a=2" loading="lazy"></a></figure>"#,
				"<pre><code>&lt;x&gt;\ny</code></pre>",
				"<ol><li><p>i</p></li></ol>",
				r#"<table><thead><tr><th align="right">H</th><th></th></tr></thead><tbody><tr><td align="right">1</td><td></td></tr></tbody></table>"#,
				"kept",
			)
		);
	}

	#[test]
	fn test_tree() {
		let comment = |id: &str, depth: u64, parent: Option<&str>| json_value!({ "depth": depth, "parentId": parent, "node": { "__typename": "Comment", "id": id, "authorInfo": { "name": "a" } } });
		let trees = [
			comment("t1_a", 0, None),
			// Deleted comments have no ID; their replies nest by depth
			json_value!({ "depth": 1, "parentId": "t1_a", "node": { "__typename": "DeletedComment" } }),
			comment("t1_c", 2, Some("t1_x")),
			json_value!({ "depth": 1, "parentId": "t1_a", "more": { "count": 5, "cursor": "c1" } }),
			comment("t1_d", 0, None),
			json_value!({ "depth": 0, "parentId": null, "more": { "count": 90, "cursor": "c2" } }),
		];
		let comments = nest(trees.iter().filter_map(|t| item(t, "p")).collect());
		assert_eq!(comments.len(), 3);

		let a = &comments[0]["data"];
		assert_eq!(a["parent_id"], "t3_p");
		let replies = a["replies"]["data"]["children"].as_array().unwrap();
		assert_eq!(replies.len(), 2);
		assert_eq!(replies[0]["data"]["author"], "[deleted]");
		assert_eq!(replies[0]["data"]["replies"]["data"]["children"][0]["data"]["id"], "c");
		assert_eq!(replies[1]["kind"], "more");
		assert_eq!(replies[1]["data"]["parent_id"], "t1_a");
		assert_eq!(replies[1]["data"]["count"], 5);
		assert_eq!(replies[1]["data"]["cursor"], "c1");

		assert_eq!(comments[1]["data"]["replies"], "");
		assert_eq!(comments[2]["kind"], "more");
		assert_eq!(comments[2]["data"]["parent_id"], "t3_p");
		assert_eq!(comments[2]["data"]["cursor"], "c2");
		assert_eq!(comments[2]["data"]["more_query"], MORE_QUERY);
	}

	fn request() -> hyper::Request<crate::body::Body> {
		hyper::Request::new(crate::body::Body::from(""))
	}

	#[test]
	fn test_post_comments_fixture() {
		let response: Value = serde_json::from_str(include_str!("../../tests/fixtures/gql/post_comments.json")).unwrap();
		let tree = listing(comment_tree(&response, "1xe5jpg").unwrap());
		let comments = crate::post::parse_comments(&tree, "/r/AskReddit/comments/1xe5jpg/x/", "op", "", &Default::default(), &request());

		assert_eq!(comments.len(), 2);
		assert_eq!(comments[0].author.name, "aubreynewmanoc");
		assert!(comments[0].body.contains("Red Bull Stratos"));
		assert!(!comments[0].replies.is_empty());
		let more = comments.last().unwrap();
		assert_eq!((more.kind.as_str(), more.parent_kind.as_str()), ("more", "t3"));
		assert!(more.more_href.contains("more_query=gql"));
		assert_eq!(more.more_count, 478);
	}

	#[test]
	fn test_overview_pages_keep_posts_and_comments_in_order() {
		let item = |name: &str, created: u32| json_value!({ "data": { "name": name, "created_utc": created } });
		let posts = json_value!({ "data": { "after": "t3_c", "children": [item("t3_a", 90), item("t3_b", 50), item("t3_c", 10)] } });
		let comments = json_value!({ "data": { "after": "t1_z", "children": [item("t1_x", 80), item("t1_y", 40), item("t1_z", 30)] } });

		let page = merge_overview(&posts, &comments, "", "");
		let names: Vec<_> = page["data"]["children"].as_array().unwrap().iter().map(|c| c["data"]["name"].clone()).collect();
		// t3_c is older than the last comment, so it waits for the next page
		assert_eq!(names, ["t3_a", "t1_x", "t3_b", "t1_y", "t1_z"]);
		assert_eq!(page["data"]["after"], "t3_b,t1_z");

		let last_comments = json_value!({ "data": { "after": null, "children": [item("t1_w", 5)] } });
		let page = merge_overview(&json_value!({ "data": { "after": null, "children": [item("t3_c", 10)] } }), &last_comments, "t3_b", "t1_z");
		assert_eq!(page["data"]["children"].as_array().unwrap().len(), 2);
		assert_eq!(page["data"]["after"], Value::Null);

		let page = merge_overview(&Value::Null, &comments, "-", "");
		assert_eq!(page["data"]["children"].as_array().unwrap().len(), 3);
		assert_eq!(page["data"]["after"], "-,t1_z");
	}

	fn fixture(name: &str) -> Value {
		serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/gql/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()
	}

	#[tokio::test]
	async fn test_subreddit_feed_fixture() {
		let listing = post_listing(&fixture("subreddit_feed.json")["data"]["postFeed"]["elements"]);
		assert_eq!(listing["data"]["after"], "t3_1xdtpdg");
		let children = listing["data"]["children"].as_array().unwrap();
		// The recommendation unit is not a post
		assert_eq!(children.len(), 4);

		let link = crate::utils::parse_post(&children[0]).await;
		assert_eq!(
			(link.title.as_str(), link.community.as_str(), link.post_type.as_str()),
			("This Week in Rust #674", "rust", "link")
		);
		assert!(link.flags.stickied);
		assert_eq!(link.flair.flair_parts.len(), 1);
		let text = crate::utils::parse_post(&children[2]).await;
		assert_eq!(text.post_type, "self");
		assert!(text.body.starts_with("<div class=\"md\"><p>"));
		let image = crate::utils::parse_post(&children[3]).await;
		assert_eq!((image.post_type.as_str(), image.media.url.as_str()), ("image", "/img/lhwr9uf5bkxh1.png"));
	}

	#[tokio::test]
	async fn test_posts_by_ids_fixture() {
		let posts = fixture("posts_by_ids.json")["data"]["postsInfoByIds"]
			.as_array()
			.unwrap()
			.iter()
			.filter_map(post)
			.collect::<Vec<_>>();
		assert_eq!(posts.len(), 3);

		let video = crate::utils::parse_post(&posts[0]).await;
		assert_eq!(video.post_type, "video");
		assert_eq!(video.title, "Secret knowledge");
		assert_eq!(video.media.url, "/vid/apxzua5q5ixh1/CMAF/720.mp4");
		assert!(video.media.alt_url.starts_with("/hls/apxzua5q5ixh1/HLSPlaylist.m3u8"));
		let gallery = crate::utils::parse_post(&posts[1]).await;
		assert_eq!((gallery.post_type.as_str(), gallery.gallery.len()), ("gallery", 2));
		assert_eq!(posts[2]["data"]["crosspost_parent_list"][0]["subreddit"], "gpui");
	}

	#[tokio::test]
	async fn test_search_fixtures() {
		let posts = post_listing(&fixture("search_posts.json")["data"]["search"]["general"]["posts"]);
		assert_eq!(posts["data"]["after"], "24");
		let first = crate::utils::parse_post(&posts["data"]["children"][0]).await;
		assert_eq!(first.title, "Rust features in the future");

		let communities = &fixture("search_communities.json")["data"]["search"]["general"]["communities"]["edges"];
		let subs: Vec<Value> = communities.as_array().unwrap().iter().filter_map(|edge| subreddit(&edge["node"])).collect();
		assert_eq!(subs.iter().map(|s| crate::utils::val(s, "display_name")).collect::<Vec<_>>(), ["rust", "playrust"]);
		assert_eq!(crate::utils::val(&subs[0], "url"), "/r/rust/");
		assert!(subs[0]["data"]["subscribers"].as_f64().is_some());
	}

	#[test]
	fn test_profile_posts_name_the_profile() {
		let node = json_value!({
			"__typename": "ProfilePost", "id": "t3_1vgbkge", "postTitle": "Hi", "subreddit": null,
			"profile": { "name": "u_spez", "redditorInfo": { "prefixedName": "u/spez" } },
		});
		let post = post(&node).unwrap();
		assert_eq!((&post["data"]["subreddit"], &post["data"]["subreddit_name_prefixed"]), (&"u_spez".into(), &"u/spez".into()));
	}

	#[test]
	fn test_user_fixtures() {
		let about = user(&fixture("profile.json")["data"]["profileByName"], 941120.into()).unwrap();
		assert_eq!(about["data"]["name"], "spez");
		assert_eq!(about["data"]["total_karma"], 941120);
		assert_eq!(about["data"]["subreddit"]["public_description"], "Reddit CEO");
		assert!(about["data"]["created"].as_f64().unwrap() > 1.1e9);

		let response = fixture("user_comments.json");
		let comments = user_comment_items(&response["data"]["redditorInfoByName"]["comments"], &response["data"]["commentsByIds"]);
		// Comments missing from `LiveCommentsByIds` are dropped
		assert_eq!(comments.len(), 1);
		let val = |key| crate::utils::val(&comments[0], key);
		assert_eq!((val("id"), val("subreddit")), ("p1wosm9".to_string(), "u_spez".to_string()));
		assert_eq!(comments[0]["kind"], "t1");
		assert_eq!(val("link_title"), "Modernizing Reddit’s infrastructure with you");
		assert!(val("body_html").contains("replicate the UI"));
		assert!(val("permalink").ends_with("/p1wosm9/"));
	}

	#[test]
	fn test_subreddit_info_fixture() {
		let about = subreddit(&fixture("subreddit_info.json")["data"]["subredditInfoByName"]).unwrap();
		let val = |key| crate::utils::val(&about, key);
		assert_eq!((val("display_name"), val("title")), ("rust".to_string(), "The Rust Programming Language".to_string()));
		assert!(val("public_description").starts_with("A place for all things related to the Rust"));
		assert!(val("community_icon").starts_with("https://styles.redditmedia.com/"));
		assert_eq!(about["data"]["subscribers"].as_u64(), Some(427823));
		let html = val("description_html");
		assert!(
			html.contains(r#"<h3>Please read <a href="https://www.rust-lang.org/policies/code-of-conduct"><strong>The Rust Community Code of Conduct</strong></a></h3><hr>"#),
			"{html}"
		);
	}

	#[test]
	fn test_unavailable_subreddits() {
		for (reason, expected) in [("PRIVATE", "private"), ("BANNED", "banned"), ("QUARANTINED", "quarantined"), ("GATED", "gated")] {
			let node = json_value!({ "__typename": "UnavailableSubreddit", "forbiddenReason": reason, "name": "x" });
			assert_eq!(about_json(&node, "x"), Err(expected.to_string()));
		}
		let node = json_value!({ "__typename": "UnavailableSubreddit", "name": "x" });
		assert_eq!(about_json(&node, "x"), Err("r/x is unavailable".to_string()));
		assert!(about_json(&fixture("subreddit_info.json")["data"]["subredditInfoByName"], "rust").is_ok());
	}
}
