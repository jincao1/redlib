use crate::body::Body;
use crate::{
	config::{Config, CONFIG},
	server::RequestExt,
	utils::{ErrorTemplate, Preferences},
};
use askama::Template;
use build_html::{Container, Html, HtmlContainer, Table};
use hyper::{http::Error, Request, Response};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use time::OffsetDateTime;

/// This is the local static that is initialized at runtime (technically at
/// the first request to the info endpoint) and contains the data
/// retrieved from the info endpoint.
pub static INSTANCE_INFO: LazyLock<InstanceInfo> = LazyLock::new(InstanceInfo::new);

/// Handles instance info endpoint
pub async fn instance_info(req: Request<Body>) -> Result<Response<Body>, String> {
	// This will retrieve the extension given, or create a new string - which will
	// simply become the last option, an HTML page.
	let extension = req.param("extension").unwrap_or_default();
	let response = match extension.as_str() {
		"yaml" | "yml" => info_yaml(),
		"txt" => info_txt(),
		"json" => info_json(),
		"html" | "" => info_html(&req),
		_ => {
			let error = ErrorTemplate {
				msg: "Error: Invalid info extension".into(),
				prefs: Preferences::new(&req),
				url: req.uri().to_string(),
			}
			.render()
			.unwrap();
			Response::builder().status(404).header("content-type", "text/html; charset=utf-8").body(error.into())
		}
	};
	response.map_err(|err| format!("{err}"))
}

fn info_json() -> Result<Response<Body>, Error> {
	if let Ok(body) = serde_json::to_string(&*INSTANCE_INFO) {
		Response::builder().status(200).header("content-type", "application/json").body(body.into())
	} else {
		Response::builder()
			.status(500)
			.header("content-type", "text/plain")
			.body(Body::from("Error serializing JSON"))
	}
}

fn info_yaml() -> Result<Response<Body>, Error> {
	if let Ok(body) = serde_yaml::to_string(&*INSTANCE_INFO) {
		// We can use `application/yaml` as media type, though there is no guarantee
		// that browsers will honor it. But we'll do it anyway. See:
		// https://github.com/ietf-wg-httpapi/mediatypes/blob/main/draft-ietf-httpapi-yaml-mediatypes.md#media-type-applicationyaml-application-yaml
		Response::builder().status(200).header("content-type", "application/yaml").body(body.into())
	} else {
		Response::builder()
			.status(500)
			.header("content-type", "text/plain")
			.body(Body::from("Error serializing YAML."))
	}
}

fn info_txt() -> Result<Response<Body>, Error> {
	Response::builder()
		.status(200)
		.header("content-type", "text/plain")
		.body(Body::from(INSTANCE_INFO.to_string(&StringType::Raw)))
}
fn info_html(req: &Request<Body>) -> Result<Response<Body>, Error> {
	let message = MessageTemplate {
		title: String::from("Instance information"),
		body: INSTANCE_INFO.to_string(&StringType::Html),
		prefs: Preferences::new(req),
		url: req.uri().to_string(),
	}
	.render()
	.unwrap();
	Response::builder().status(200).header("content-type", "text/html; charset=utf8").body(Body::from(message))
}
#[derive(Serialize, Deserialize, Default)]
pub struct InstanceInfo {
	package_name: String,
	crate_version: String,
	pub git_commit: String,
	deploy_date: String,
	compile_mode: String,
	deploy_unix_ts: i64,
	config: Config,
}

impl InstanceInfo {
	pub fn new() -> Self {
		Self {
			package_name: env!("CARGO_PKG_NAME").to_string(),
			crate_version: env!("CARGO_PKG_VERSION").to_string(),
			git_commit: env!("GIT_HASH").to_string(),
			deploy_date: OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc()).to_string(),
			#[cfg(debug_assertions)]
			compile_mode: "Debug".into(),
			#[cfg(not(debug_assertions))]
			compile_mode: "Release".into(),
			deploy_unix_ts: OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc()).unix_timestamp(),
			config: CONFIG.clone(),
		}
	}
	fn to_table(&self) -> String {
		let mut container = Container::default();
		if let Some(banner) = &self.config.banner {
			container.add_header(3, "Instance banner");
			container.add_raw("<br />");
			container.add_paragraph(banner);
			container.add_raw("<br />");
		}
		container.add_table(
			Table::from([
				["Package name", &self.package_name],
				["Crate version", &self.crate_version],
				["Git commit", &self.git_commit],
				["Deploy date", &self.deploy_date],
				["Deploy timestamp", &self.deploy_unix_ts.to_string()],
				["Compile mode", &self.compile_mode],
				["SFW only", &effective(&self.config.sfw_only, "off")],
				["Pushshift frontend", &effective(&self.config.pushshift, crate::config::DEFAULT_PUSHSHIFT_FRONTEND)],
				["RSS enabled", &effective(&self.config.enable_rss, "off")],
				["RedGifs enabled", &effective(&self.config.enable_redgifs, "on")],
				["Scrape fallback enabled", &effective(&self.config.enable_scrape_fallback, "on")],
				["Full URL", &effective(&self.config.full_url, "none")],
				["Remove default feeds", &effective(&self.config.default_remove_default_feeds, "off")],
			])
			.with_header_row(["Settings"]),
		);
		container.add_raw("<br />");
		container.add_table(
			Table::from([
				["Hide awards", &effective(&self.config.default_hide_awards, "off")],
				["Hide score", &effective(&self.config.default_hide_score, "off")],
				["Theme", &effective(&self.config.default_theme, "system")],
				["Front page", &effective(&self.config.default_front_page, "default")],
				["Layout", &effective(&self.config.default_layout, "card")],
				["Wide", &effective(&self.config.default_wide, "off")],
				["Comment sort", &effective(&self.config.default_comment_sort, "confidence")],
				["Post sort", &effective(&self.config.default_post_sort, "hot")],
				["Blur Spoiler", &effective(&self.config.default_blur_spoiler, "off")],
				["Show NSFW", &effective(&self.config.default_show_nsfw, "off")],
				["Blur NSFW", &effective(&self.config.default_blur_nsfw, "off")],
				["Use HLS", &effective(&self.config.default_use_hls, "off")],
				["Hide HLS notification", &effective(&self.config.default_hide_hls_notification, "off")],
				["Subscriptions", &effective(&self.config.default_subscriptions, "none")],
				["Filters", &effective(&self.config.default_filters, "none")],
				["Geo filter", &effective(&self.config.default_geo_filter, "GLOBAL")],
				["Clean URLs", &effective(&self.config.default_clean_urls, "off")],
				["Posts per page", &effective(&self.config.default_posts_per_page, "25")],
				["Max comment thread depth", &effective(&self.config.default_max_comment_thread_depth, "0")],
			])
			.with_header_row(["Default preferences"]),
		);
		container.to_html_string().replace("<th>", "<th colspan=\"2\">")
	}
	fn to_string(&self, string_type: &StringType) -> String {
		match string_type {
			StringType::Raw => format!(
				concat!(
					"Package name: {}\n",
					"Crate version: {}\n",
					"Git commit: {}\n",
					"Deploy date: {}\n",
					"Deploy timestamp: {}\n",
					"Compile mode: {}\n",
					"SFW only: {}\n",
					"RSS enabled: {}\n",
					"RedGifs enabled: {}\n",
					"Scrape fallback enabled: {}\n",
					"Full URL: {}\n",
					"Remove default feeds: {}\n",
					"Pushshift frontend: {}\n",
					"Config:\n",
					"    Banner: {:?}\n",
					"    Hide awards: {}\n",
					"    Hide score: {}\n",
					"    Default theme: {}\n",
					"    Default front page: {}\n",
					"    Default layout: {}\n",
					"    Default wide: {}\n",
					"    Default comment sort: {}\n",
					"    Default post sort: {}\n",
					"    Default blur Spoiler: {}\n",
					"    Default show NSFW: {}\n",
					"    Default blur NSFW: {}\n",
					"    Default use HLS: {}\n",
					"    Default hide HLS notification: {}\n",
					"    Default clean urls: {}\n",
					"    Default subscriptions: {}\n",
					"    Default filters: {}\n",
					"    Default geo filter: {}\n",
					"    Default posts per page: {}\n",
					"    Default max comment thread depth: {}\n",
				),
				self.package_name,
				self.crate_version,
				self.git_commit,
				self.deploy_date,
				self.deploy_unix_ts,
				self.compile_mode,
				effective(&self.config.sfw_only, "off"),
				effective(&self.config.enable_rss, "off"),
				effective(&self.config.enable_redgifs, "on"),
				effective(&self.config.enable_scrape_fallback, "on"),
				effective(&self.config.full_url, "none"),
				effective(&self.config.default_remove_default_feeds, "off"),
				effective(&self.config.pushshift, crate::config::DEFAULT_PUSHSHIFT_FRONTEND),
				self.config.banner,
				effective(&self.config.default_hide_awards, "off"),
				effective(&self.config.default_hide_score, "off"),
				effective(&self.config.default_theme, "system"),
				effective(&self.config.default_front_page, "default"),
				effective(&self.config.default_layout, "card"),
				effective(&self.config.default_wide, "off"),
				effective(&self.config.default_comment_sort, "confidence"),
				effective(&self.config.default_post_sort, "hot"),
				effective(&self.config.default_blur_spoiler, "off"),
				effective(&self.config.default_show_nsfw, "off"),
				effective(&self.config.default_blur_nsfw, "off"),
				effective(&self.config.default_use_hls, "off"),
				effective(&self.config.default_hide_hls_notification, "off"),
				effective(&self.config.default_clean_urls, "off"),
				effective(&self.config.default_subscriptions, "none"),
				effective(&self.config.default_filters, "none"),
				effective(&self.config.default_geo_filter, "GLOBAL"),
				effective(&self.config.default_posts_per_page, "25"),
				effective(&self.config.default_max_comment_thread_depth, "0"),
			),
			StringType::Html => self.to_table(),
		}
	}
}
/// The configured value, or the value Redlib uses when it is unset or empty.
fn effective(value: &Option<String>, default: &str) -> String {
	match value.as_deref() {
		Some(value) if !value.is_empty() => value.to_string(),
		_ => format!("{default} (default)"),
	}
}

enum StringType {
	Raw,
	Html,
}
#[derive(Template)]
#[template(path = "message.html")]
struct MessageTemplate {
	title: String,
	body: String,
	prefs: Preferences,
	url: String,
}

#[cfg(test)]
mod tests {
	use super::effective;

	#[test]
	fn effective_shows_the_default_when_unset() {
		assert_eq!(effective(&Some("on".into()), "off"), "on");
		assert_eq!(effective(&None, "on"), "on (default)");
		assert_eq!(effective(&Some(String::new()), "none"), "none (default)");
	}
}
