//! Every answer, so each recording is searched for on YouTube once.
//!
//! `matches` maps a recording (`isrc:<code>`, or `spotify:track:<id>` / `text:<search>` when no
//! catalogue knows one) to its video. `aliases` maps the other ways of asking for a recording
//! (a search, a Spotify track) to it, so they share one match.

use rusqlite::{Connection, OptionalExtension, params};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// "Nothing found" is asked again soon: an empty search is more often YouTube refusing for a
/// while than nothing being there.
const NONE_RETRY_SECS: i64 = 10 * 60;
/// The recording uploaded by someone else is asked again soon, since the search that found it
/// may have left the official upload out.
const UNOFFICIAL_SECS: i64 = 86_400;
/// YouTube's top result for a search can change, unlike a matched recording.
const TOP_RESULT_SECS: i64 = 30 * 86_400;
/// Which recording a search means can change too (a new single gets popular); a Spotify
/// track is always the same recording.
const TEXT_ALIAS_SECS: i64 = 30 * 86_400;
/// The `updated` of an alias set by hand: never older than `TEXT_ALIAS_SECS`.
const PINNED: i64 = i64::MAX;

pub struct Cache(Mutex<Connection>);

#[derive(Clone)]
pub struct Entry {
    pub video: Option<String>,
    pub title: String,
    pub artist: String,
    /// milliseconds, 0 if unknown
    pub duration: i64,
    /// "match", "unofficial" (matched, uploaded by someone else), "youtube" (top search result),
    /// "manual", "none", or "pending" (not looked up yet)
    pub source: &'static str,
}

impl Cache {
    pub fn open(path: &str) -> rusqlite::Result<Cache> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Connection::open(path)?;
        // busy_timeout: the CLI writes this same file while the server runs
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS matches (
                 key TEXT PRIMARY KEY,
                 video TEXT,
                 title TEXT NOT NULL,
                 artist TEXT NOT NULL,
                 duration INTEGER NOT NULL,
                 source TEXT NOT NULL,
                 updated INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS aliases (
                 key TEXT PRIMARY KEY,
                 recording TEXT NOT NULL,
                 updated INTEGER NOT NULL
             );",
        )?;
        Ok(Cache(Mutex::new(db)))
    }

    /// A stored answer that's still good.
    pub fn get(&self, key: &str) -> Option<Entry> {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let sql =
            "SELECT video, title, artist, duration, source, updated FROM matches WHERE key = ?1";
        let row = db.query_row(sql, [key], |r| {
            let source: String = r.get(4)?;
            let entry = Entry {
                video: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                duration: r.get(3)?,
                source: source_name(&source),
            };
            Ok((entry, r.get::<_, i64>(5)?))
        });
        let (entry, updated) = row
            .optional()
            .inspect_err(|e| warn!("cache: couldn't read {key}: {e}"))
            .ok()??;
        let age = now() - updated;
        let fresh = match entry.source {
            "none" => age < NONE_RETRY_SECS,
            "unofficial" => age < UNOFFICIAL_SECS,
            "youtube" => age < TOP_RESULT_SECS,
            _ => true,
        };
        fresh.then_some(entry)
    }

    /// Every stored answer of one kind, or all of them, newest first.
    pub fn list(&self, source: Option<&str>) -> rusqlite::Result<Vec<(String, Entry)>> {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut query = db.prepare(
            "SELECT key, video, title, artist, duration, source FROM matches
             WHERE ?1 IS NULL OR source = ?1 ORDER BY updated DESC",
        )?;
        let rows = query.query_map([source], |r| {
            let source: String = r.get(5)?;
            let entry = Entry {
                video: r.get(1)?,
                title: r.get(2)?,
                artist: r.get(3)?,
                duration: r.get(4)?,
                source: source_name(&source),
            };
            Ok((r.get(0)?, entry))
        })?;
        rows.collect()
    }

    /// The recording a search or Spotify track was found to be, if still known.
    pub fn alias(&self, key: &str) -> Option<String> {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let sql = "SELECT recording, updated FROM aliases WHERE key = ?1";
        let (recording, updated): (String, i64) = db
            .query_row(sql, [key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .inspect_err(|e| warn!("cache: couldn't read {key}: {e}"))
            .ok()??;
        let fresh = !key.starts_with("text:") || now() - updated < TEXT_ALIAS_SECS;
        fresh.then_some(recording)
    }

    pub fn set_alias(&self, key: &str, recording: &str) {
        self.save_alias(key, recording, now());
    }

    /// Like [`Cache::set_alias`], but a search set by hand keeps meaning that recording.
    pub fn pin_alias(&self, key: &str, recording: &str) {
        self.save_alias(key, recording, PINNED);
    }

    fn save_alias(&self, key: &str, recording: &str, updated: i64) {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let sql = "INSERT OR REPLACE INTO aliases VALUES (?1, ?2, ?3)";
        if let Err(err) = db.execute(sql, params![key, recording, updated]) {
            warn!("cache: couldn't save {key}: {err}");
        }
    }

    /// Lets a question stand for itself again.
    pub fn unalias(&self, key: &str) {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(err) = db.execute("DELETE FROM aliases WHERE key = ?1", [key]) {
            warn!("cache: couldn't drop {key}: {err}");
        }
    }

    /// Drops a match and every alias of it. Whether there was a match to drop.
    pub fn forget(&self, recording: &str) -> rusqlite::Result<bool> {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        db.execute(
            "DELETE FROM aliases WHERE recording = ?1 OR key = ?1",
            [recording],
        )?;
        Ok(db.execute("DELETE FROM matches WHERE key = ?1", [recording])? > 0)
    }

    pub fn put(&self, key: &str, e: &Entry) {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let sql = "INSERT OR REPLACE INTO matches VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
        if let Err(err) = db.execute(
            sql,
            params![key, e.video, e.title, e.artist, e.duration, e.source, now()],
        ) {
            warn!("cache: couldn't save {key}: {err}");
        }
    }
}

fn source_name(stored: &str) -> &'static str {
    match stored {
        "match" => "match",
        "youtube" => "youtube",
        "none" => "none",
        "unofficial" => "unofficial",
        _ => "manual",
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_expires() {
        let cache = Cache::open(":memory:").unwrap();
        let entry = |source| Entry {
            video: Some("abc".into()),
            title: "t".into(),
            artist: "a".into(),
            duration: 1,
            source,
        };
        cache.put("k", &entry("match"));
        assert_eq!(cache.get("k").unwrap().video.as_deref(), Some("abc"));
        cache.put("old", &entry("none"));
        cache.put("fan", &entry("unofficial"));
        assert!(cache.get("fan").is_some());
        cache
            .0
            .lock()
            .unwrap()
            .execute(
                "UPDATE matches SET updated = 0 WHERE key IN ('old', 'fan')",
                [],
            )
            .unwrap();
        assert!(cache.get("old").is_none());
        assert!(cache.get("fan").is_none());
        assert!(cache.get("missing").is_none());
        assert_eq!(cache.list(Some("match")).unwrap().len(), 1);
        cache.set_alias("text:some track", "k");
        cache.set_alias("text:stale", "k");
        cache
            .0
            .lock()
            .unwrap()
            .execute(
                "UPDATE aliases SET updated = 0 WHERE key = 'text:stale'",
                [],
            )
            .unwrap();
        assert_eq!(cache.alias("text:some track").as_deref(), Some("k"));
        assert!(cache.alias("text:stale").is_none());
        cache.pin_alias("text:chosen", "k");
        assert_eq!(cache.alias("text:chosen").as_deref(), Some("k"));
        cache.unalias("text:chosen");
        assert!(cache.alias("text:chosen").is_none());
        assert!(cache.forget("k").unwrap());
        assert!(cache.alias("text:some track").is_none());
        assert!(!cache.forget("k").unwrap());
    }
}
