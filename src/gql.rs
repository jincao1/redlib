// Fallback for JSON API requests Reddit blocks for logged-out clients: the same
// data from Reddit's GraphQL API (gql-fed.reddit.com), which the Android app
// uses and which answers the app-only OAuth token.
//
// Each response is converted into the shape of the JSON API response it
// replaces, so the parsers and templates work unchanged.

use crate::client::{json as reddit_json, CLIENT, OAUTH_CLIENT};
use crate::utils::{format_url, param};
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

/// The JSON API response for `path`, rebuilt from GraphQL. `None` if no query covers `path`.
pub async fn json(path: &str, quarantine: bool) -> Option<Result<Value, String>> {
	let route = path.split('?').next().unwrap_or_default();
	if route.contains("/comments/") {
		return Some(thread(path, quarantine).await);
	}
	None
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
		let mut builder = CLIENT.post(GQL_URL);
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
	let flair_richtext: Value = flair["richtext"].as_str().and_then(|r| serde_json::from_str(r).ok()).unwrap_or_default();
	let flair_type = if flair_richtext.as_array().is_some_and(|r| !r.is_empty()) {
		"richtext"
	} else if flair["text"].as_str().is_some_and(|t| !t.is_empty()) {
		"text"
	} else {
		""
	};

	let awards: Vec<Value> = node["awardings"]
		.as_array()
		.into_iter()
		.flatten()
		.map(|a| json_value!({ "name": a["award"]["name"], "count": a["total"], "resized_icons": [{ "url": a["award"]["static_icon_16"]["url"] }] }))
		.collect();

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
				"all_awardings": awards,
				"parent_id": parent,
				"replies": "",
			}
		}),
	))
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
		let response: Value = serde_json::from_str(include_str!("../tests/fixtures/gql/post_comments.json")).unwrap();
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
}
