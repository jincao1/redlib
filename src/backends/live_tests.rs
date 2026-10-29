//! Live tests that call one backend directly instead of going through the
//! backend order, so a broken backend can't hide behind a working one.
//!
//! The session backends need `REDLIB_REDDIT_COOKIE`; without it their tests
//! pass without doing anything (CI sets it from a repository secret, which
//! pull requests from forks don't get).

use super::{cookied_json::CookiedJson, gql::Gql, Backend};
use crate::client::block_on_shared;
use serde_json::Value;

/// A post that has comments and stays up.
const POST: &str = "/r/redditdev/comments/1wubcvf.json?raw_json=1";

fn get(backend: &dyn Backend, path: &str) -> Value {
	block_on_shared(backend.fetch(path, false)).unwrap_or_else(|failure| panic!("{} {path}: {failure:?}", backend.name()))
}

/// Whether the session backends can run; says so when they can't.
fn has_session(backend: &dyn Backend) -> bool {
	let available = backend.available();
	if !available {
		eprintln!("{}: REDLIB_REDDIT_COOKIE isn't set, nothing to test", backend.name());
	}
	available
}

/// The listing's children, asserting there are some.
fn children<'a>(listing: &'a Value, what: &str) -> &'a [Value] {
	let children = listing["data"]["children"].as_array().map(Vec::as_slice).unwrap_or_default();
	assert!(!children.is_empty(), "{what}: empty listing: {listing}");
	children
}

/// Asserts the fields every post page and listing reads.
fn assert_post(post: &Value, what: &str) {
	assert_eq!(post["kind"], "t3", "{what}: {post}");
	let data = &post["data"];
	for field in ["id", "title", "author", "subreddit", "permalink"] {
		assert!(data[field].as_str().is_some_and(|v| !v.is_empty()), "{what}: post without {field}: {data}");
	}
}

fn assert_posts(listing: &Value, what: &str) {
	for post in children(listing, what) {
		assert_post(post, what);
	}
}

fn assert_thread(thread: &Value, what: &str) {
	assert_post(&children(&thread[0], what)[0], what);
	let comments = children(&thread[1], what);
	let comment = &comments.iter().find(|c| c["kind"] == "t1").unwrap_or_else(|| panic!("{what}: no comments: {thread}"))["data"];
	assert!(comment["body"].as_str().is_some_and(|b| !b.is_empty()), "{what}: comment without body: {comment}");
	assert!(comment["author"].as_str().is_some(), "{what}: comment without author: {comment}");
}

fn assert_subreddit(about: &Value, what: &str) {
	assert_eq!(about["kind"], "t5", "{what}: {about}");
	assert_eq!(about["data"]["display_name"].as_str().map(str::to_lowercase).as_deref(), Some("rust"), "{what}: {about}");
}

fn assert_user(about: &Value, what: &str) {
	assert_eq!(about["kind"], "t2", "{what}: {about}");
	assert_eq!(about["data"]["name"], "spez", "{what}: {about}");
}

/// The paths every backend must serve.
fn core_paths(backend: &dyn Backend) {
	let name = backend.name();
	assert_thread(&get(backend, POST), &format!("{name} post"));
	assert_posts(&get(backend, "/r/rust/hot.json?raw_json=1"), &format!("{name} subreddit"));
	assert_subreddit(&get(backend, "/r/rust/about.json?raw_json=1"), &format!("{name} about"));
	assert_user(&get(backend, "/user/spez/about.json?raw_json=1"), &format!("{name} user"));
}

#[test]
fn test_gql_post_comments() {
	// PostsByIds for the post, PostComments for the comments
	assert_thread(&get(&Gql, POST), "gql post");
}

#[test]
fn test_gql_posts_by_ids() {
	assert_posts(&get(&Gql, "/by_id/t3_1wubcvf.json?raw_json=1"), "gql by_id");
}

#[test]
fn test_gql_subreddit_feed() {
	assert_posts(&get(&Gql, "/r/rust/hot.json?raw_json=1"), "gql subreddit");
	assert_posts(&get(&Gql, "/r/rust/top.json?t=week&raw_json=1"), "gql subreddit top");
}

#[test]
fn test_gql_popular_and_all() {
	assert_posts(&get(&Gql, "/r/popular/hot.json?raw_json=1"), "gql popular");
	assert_posts(&get(&Gql, "/r/all/hot.json?raw_json=1"), "gql all");
}

#[test]
fn test_gql_subreddit_info() {
	assert_subreddit(&get(&Gql, "/r/rust/about.json?raw_json=1"), "gql about");
}

#[test]
fn test_gql_search() {
	assert_posts(&get(&Gql, "/search.json?q=rust&raw_json=1"), "gql search");
	assert_posts(&get(&Gql, "/r/rust/search.json?q=async&restrict_sr=on&raw_json=1"), "gql search in subreddit");
	let subreddits = get(&Gql, "/subreddits/search.json?q=rust&limit=3");
	assert!(children(&subreddits, "gql subreddit search").iter().all(|s| s["kind"] == "t5"), "{subreddits}");
}

#[test]
fn test_gql_user() {
	assert_user(&get(&Gql, "/user/spez/about.json?raw_json=1"), "gql user");
	assert!(
		get(&Gql, "/user/spez/about.json?raw_json=1")["data"]["total_karma"].as_i64().is_some_and(|k| k > 0),
		"gql user karma (SearchPeople)"
	);
	assert_posts(&get(&Gql, "/user/spez/submitted.json?raw_json=1"), "gql user posts");
	let comments = get(&Gql, "/user/spez/comments.json?raw_json=1");
	assert!(children(&comments, "gql user comments").iter().all(|c| c["kind"] == "t1"), "{comments}");
	children(&get(&Gql, "/user/spez/overview.json?raw_json=1"), "gql user overview");
}

#[test]
fn test_cookied_json() {
	if has_session(&CookiedJson) {
		core_paths(&CookiedJson);
	}
}

#[cfg(feature = "obscura")]
#[test]
fn test_obscura_cookied_json() {
	use super::obscura::ObscuraCookiedJson;
	if has_session(&ObscuraCookiedJson) {
		core_paths(&ObscuraCookiedJson);
	}
}
