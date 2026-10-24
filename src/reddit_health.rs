//! Tracks whether Reddit serves post pages, from the last post requests visitors made.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

const WINDOW: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
	/// Reddit returned the post's JSON.
	Ok,
	/// Reddit blocked the JSON, the scrape fallback filled in.
	Scraped,
	/// Reddit blocked the post and nothing got through.
	Blocked,
}

static RECENT: LazyLock<Mutex<VecDeque<(Instant, Outcome)>>> = LazyLock::new(Default::default);

pub fn record(outcome: Outcome) {
	let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
	if recent.len() == WINDOW {
		recent.pop_front();
	}
	recent.push_back((Instant::now(), outcome));
}

/// Whether most recent post requests got Reddit's JSON, and a one-line summary.
pub fn status() -> (bool, String) {
	summarize(&RECENT.lock().unwrap_or_else(|e| e.into_inner()))
}

fn summarize(recent: &VecDeque<(Instant, Outcome)>) -> (bool, String) {
	let Some((newest, _)) = recent.back() else {
		return (true, "ok: no post requests yet\n".to_string());
	};
	let count = |o: Outcome| recent.iter().filter(|(_, x)| *x == o).count();
	let (ok, scraped, blocked) = (count(Outcome::Ok), count(Outcome::Scraped), count(Outcome::Blocked));
	let healthy = ok * 2 >= recent.len();
	let summary = format!(
		"{}: last {} post requests: {ok} ok, {scraped} scraped, {blocked} blocked; newest {}s ago\n",
		if healthy { "ok" } else { "blocked" },
		recent.len(),
		newest.elapsed().as_secs()
	);
	(healthy, summary)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn of(outcomes: &[Outcome]) -> VecDeque<(Instant, Outcome)> {
		outcomes.iter().map(|o| (Instant::now(), *o)).collect()
	}

	#[test]
	fn healthy_until_most_requests_fail() {
		use Outcome::*;
		assert!(summarize(&of(&[])).0);
		assert!(summarize(&of(&[Ok, Blocked])).0);
		assert!(!summarize(&of(&[Ok, Scraped, Blocked])).0);
		let (healthy, text) = summarize(&of(&[Blocked, Scraped, Ok, Ok]));
		assert!(healthy);
		assert!(text.starts_with("ok: last 4 post requests: 2 ok, 1 scraped, 1 blocked;"), "{text}");
	}

	#[test]
	fn keeps_only_the_window() {
		for _ in 0..WINDOW + 5 {
			record(Outcome::Blocked);
		}
		assert_eq!(RECENT.lock().unwrap().len(), WINDOW);
	}
}
