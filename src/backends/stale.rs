//! The last good copy of each Reddit response, for when Reddit stops serving
//! it. RAM only and zstd-compressed, bounded by `REDLIB_STALE_CACHE_MB` and
//! `REDLIB_STALE_CACHE_MAX_AGE` (minutes).

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use log::{debug, warn};
use lru::LruCache;
use serde_json::Value;

use crate::config::get_setting;

const DEFAULT_MB: usize = 32;
const DEFAULT_MAX_AGE_MINUTES: u64 = 360;
/// Rough bookkeeping cost of an entry besides its compressed body.
const ENTRY_OVERHEAD: usize = 96;

type Key = (String, bool);

struct Entry {
	body: Vec<u8>,
	backend: &'static str,
	stored: Instant,
}

impl Entry {
	fn size(&self, key: &Key) -> usize {
		self.body.len() + key.0.len() + ENTRY_OVERHEAD
	}
}

struct Store {
	entries: LruCache<Key, Entry>,
	bytes: usize,
	budget: usize,
	max_age: Duration,
}

impl Store {
	fn new(budget: usize, max_age: Duration) -> Self {
		Self {
			entries: LruCache::unbounded(),
			bytes: 0,
			budget,
			max_age,
		}
	}

	fn put(&mut self, key: Key, entry: Entry) {
		let size = entry.size(&key);
		if size > self.budget {
			return;
		}
		if let Some(old) = self.entries.pop(&key) {
			self.bytes -= old.size(&key);
		}
		self.bytes += size;
		self.entries.put(key, entry);
		while self.bytes > self.budget {
			let Some((key, old)) = self.entries.pop_lru() else { break };
			self.bytes -= old.size(&key);
		}
	}

	fn get(&mut self, key: &Key, now: Instant) -> Option<&Entry> {
		let age = now.saturating_duration_since(self.entries.peek(key)?.stored);
		if age > self.max_age {
			let old = self.entries.pop(key)?;
			self.bytes -= old.size(key);
			return None;
		}
		self.entries.get(key)
	}
}

static STORE: LazyLock<Option<Mutex<Store>>> = LazyLock::new(|| {
	let mb = get_setting("REDLIB_STALE_CACHE_MB").and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_MB);
	let minutes = get_setting("REDLIB_STALE_CACHE_MAX_AGE")
		.and_then(|v| v.trim().parse().ok())
		.unwrap_or(DEFAULT_MAX_AGE_MINUTES);
	(mb > 0 && minutes > 0).then(|| Mutex::new(Store::new(mb * 1024 * 1024, Duration::from_secs(minutes * 60))))
});

/// Keeps `json` as the last good response for `path`.
pub fn store(path: &str, quarantine: bool, json: &Value, backend: &'static str) {
	let Some(store) = STORE.as_ref() else { return };
	let Ok(raw) = serde_json::to_vec(json) else { return };
	let Ok(body) = zstd::bulk::compress(&raw, 3) else { return };
	debug!("Keeping {path} for the stale cache: {} of {} bytes", body.len(), raw.len());
	let entry = Entry {
		body,
		backend,
		stored: Instant::now(),
	};
	if let Ok(mut store) = store.lock() {
		store.put((path.to_string(), quarantine), entry);
	}
}

/// The last good response for `path`.
pub fn load(path: &str, quarantine: bool) -> Option<(Value, &'static str)> {
	let now = Instant::now();
	let (body, backend, age) = {
		let mut store = STORE.as_ref()?.lock().ok()?;
		let entry = store.get(&(path.to_string(), quarantine), now)?;
		(entry.body.clone(), entry.backend, now.saturating_duration_since(entry.stored))
	};
	let json = serde_json::from_slice(&zstd::stream::decode_all(body.as_slice()).ok()?).ok()?;
	warn!("Serving {path} from the stale cache, {}s old", age.as_secs());
	Some((json, backend))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn entry(len: usize, stored: Instant) -> Entry {
		Entry {
			body: vec![0; len],
			backend: "json",
			stored,
		}
	}

	#[test]
	fn test_least_recently_used_entries_go_first_when_over_budget() {
		let now = Instant::now();
		let mut store = Store::new(3 * (100 + 1 + ENTRY_OVERHEAD), Duration::from_secs(60));
		for key in ["a", "b", "c"] {
			store.put((key.into(), false), entry(100, now));
		}
		assert!(store.get(&("a".into(), false), now).is_some());
		store.put(("d".into(), false), entry(100, now));
		assert!(store.get(&("b".into(), false), now).is_none());
		for key in ["a", "c", "d"] {
			assert!(store.get(&(key.into(), false), now).is_some(), "{key}");
		}
		assert_eq!(store.bytes, 3 * (100 + 1 + ENTRY_OVERHEAD));
	}

	#[test]
	fn test_entries_older_than_the_max_age_are_dropped() {
		let now = Instant::now();
		let mut store = Store::new(1 << 20, Duration::from_secs(60));
		store.put(("a".into(), false), entry(10, now));
		assert!(store.get(&("a".into(), false), now + Duration::from_secs(59)).is_some());
		assert!(store.get(&("a".into(), false), now + Duration::from_secs(61)).is_none());
		assert_eq!(store.bytes, 0);
	}

	#[test]
	fn test_replacing_an_entry_keeps_the_byte_count_right() {
		let now = Instant::now();
		let mut store = Store::new(1 << 20, Duration::from_secs(60));
		store.put(("a".into(), true), entry(500, now));
		store.put(("a".into(), true), entry(10, now));
		store.put(("a".into(), false), entry(10, now));
		assert_eq!(store.bytes, 2 * (10 + 1 + ENTRY_OVERHEAD));
	}
}
