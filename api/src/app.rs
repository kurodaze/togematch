//! Answers one question: which YouTube video is this Spotify link or search?
//!
//! Matches are stored per recording (by ISRC when Deezer knows it), so every way of asking for
//! one recording (searches, Spotify links) shares one YouTube search.

use crate::cache::{Cache, Entry};
use crate::{deezer, spotify, youtube};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use togematch::Track;
use tracing::{debug, trace, warn};
use ureq::Agent;

pub struct App {
    pub cache: Cache,
    pub ytdlp: youtube::Ytdlp,
    pub http: Agent,
    /// questions being answered now, so the same one asked again waits for that answer
    pub running: Mutex<HashMap<String, Weak<Mutex<()>>>>,
}

pub struct Found {
    /// what to ask again for this track alone: a `spotify:track:` URI, or the search text
    pub query: String,
    pub entry: Entry,
    pub cached: bool,
}

pub struct Answer {
    /// album or playlist name
    pub playlist: Option<String>,
    pub tracks: Vec<Found>,
}

/// Why there's no answer: the question (400) or a service we rely on (502).
pub enum Failure {
    Bad(String),
    Upstream(String),
}

impl App {
    pub fn answer(&self, q: &str) -> Result<Answer, Failure> {
        if let Some(link) = spotify::link(&self.http, q) {
            debug!("spotify link: kind={}, id={}", link.kind, link.id);
            if link.kind == "track" {
                let key = format!("spotify:track:{}", link.id);
                let turn = self.turn(&key);
                let _turn = turn.lock().unwrap_or_else(PoisonError::into_inner);
                // known already, so Spotify isn't asked what it is
                let found = match self.cached(&key) {
                    Some(entry) => {
                        debug!("cache hit: {key} -> {:?}", entry.video);
                        Found {
                            query: key,
                            entry,
                            cached: true,
                        }
                    }
                    None => {
                        let (id, track) = spotify::fetch(&self.http, &link)
                            .map_err(Failure::Upstream)?
                            .tracks
                            .into_iter()
                            .next()
                            .ok_or_else(|| Failure::Upstream("Spotify listed no track".into()))?;
                        debug!(
                            "Spotify metadata: {:?} - {:?} ({}s)",
                            track.artist,
                            track.name,
                            track.duration / 1000
                        );
                        self.spotify_track(&id, track).map_err(Failure::Upstream)?
                    }
                };
                return Ok(Answer {
                    playlist: None,
                    tracks: vec![found],
                });
            }
            let listing = spotify::fetch(&self.http, &link).map_err(Failure::Upstream)?;
            debug!(
                "Spotify fetched {} track(s) for {:?}",
                listing.tracks.len(),
                listing.name
            );
            // a whole list would take minutes to match: answer what's known, the rest are
            // asked for one by one as they come up
            let tracks = listing
                .tracks
                .into_iter()
                .map(|(id, track)| {
                    let key = format!("spotify:track:{id}");
                    match self.cached(&key) {
                        Some(entry) => Found {
                            query: key,
                            entry,
                            cached: true,
                        },
                        None => Found {
                            query: key,
                            entry: entry(&track, None, "pending"),
                            cached: false,
                        },
                    }
                })
                .collect();
            return Ok(Answer {
                playlist: listing.name,
                tracks,
            });
        }
        if other_link(q) {
            return Err(Failure::Bad(OTHER_LINK.into()));
        }
        let found = self.text(q).map_err(Failure::Upstream)?;
        Ok(Answer {
            playlist: None,
            tracks: vec![found],
        })
    }

    /// How a question is asked: `spotify:track:<id>` or `text:<search>`.
    pub fn asked(&self, q: &str) -> Result<String, Failure> {
        if let Some(link) = spotify::link(&self.http, q) {
            if link.kind != "track" {
                return Err(Failure::Bad(
                    "that's a whole list, give one track's link".into(),
                ));
            }
            return Ok(format!("spotify:track:{}", link.id));
        }
        if other_link(q) {
            return Err(Failure::Bad(OTHER_LINK.into()));
        }
        Ok(text_key(q))
    }

    /// The recording a question was found to be, without looking anything up.
    pub fn recording_of(&self, asked: &str) -> String {
        self.cache.alias(asked).unwrap_or_else(|| asked.to_string())
    }

    /// Every catalogue track a search fits, best first.
    pub fn candidates(&self, text: &str) -> Result<Vec<deezer::Meant>, String> {
        Ok(deezer::candidates(
            text,
            deezer::search_words(&self.http, text)?,
        ))
    }

    /// The recording a Spotify track is, looking it up on Spotify and Deezer if it isn't
    /// known yet, with what the catalogue says about it.
    pub fn find_recording(&self, asked: &str) -> Result<(String, Option<Track>), String> {
        if let Some(recording) = self.cache.alias(asked) {
            return Ok((recording, None));
        }
        let id = asked.strip_prefix("spotify:track:").unwrap_or(asked);
        let link = spotify::Link {
            kind: "track".into(),
            id: id.into(),
        };
        let (_, track) = spotify::fetch(&self.http, &link)?
            .tracks
            .into_iter()
            .next()
            .ok_or("Spotify listed no track")?;
        Ok(match deezer::same(&self.http, &track)? {
            Some((recording, _)) => (recording, Some(track)),
            None => (asked.to_string(), Some(track)),
        })
    }

    /// The stored answer for a question, through its recording.
    fn cached(&self, asked: &str) -> Option<Entry> {
        self.cache.get(&self.recording_of(asked))
    }

    /// A lock for one question, held while it's answered, so the same question asked again at
    /// once waits and finds the stored answer instead of searching again.
    fn turn(&self, asked: &str) -> Arc<Mutex<()>> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.retain(|_, turn| turn.strong_count() > 0);
        if let Some(turn) = running.get(asked).and_then(Weak::upgrade) {
            return turn;
        }
        let turn = Arc::new(Mutex::new(()));
        running.insert(asked.to_string(), Arc::downgrade(&turn));
        turn
    }

    /// Matches a Spotify track nothing is stored for yet.
    fn spotify_track(&self, id: &str, track: Track) -> Result<Found, String> {
        let key = format!("spotify:track:{id}");
        // the same recording on Deezer shares its match with searches, and names the album
        let same = deezer::same(&self.http, &track).unwrap_or_else(|e| {
            warn!("deezer: {e}");
            None
        });
        let (recording, track) = match same {
            Some((recording, theirs)) => {
                debug!(
                    "Deezer matched recording {recording} (album: {:?})",
                    theirs.album
                );
                (
                    recording,
                    Track {
                        album: theirs.album,
                        ..track
                    },
                )
            }
            None => {
                debug!("Deezer found no recording match, using {key}");
                (key.clone(), track)
            }
        };
        if recording != key {
            self.cache.set_alias(&key, &recording);
            if let Some(entry) = self.cache.get(&recording) {
                debug!(
                    "cache hit via alias {recording} for {key}: {:?}",
                    entry.video
                );
                return Ok(Found {
                    query: key,
                    entry,
                    cached: true,
                });
            }
        }
        let queries = togematch::queries(&track);
        let results = youtube::search(&self.ytdlp, &queries)?;
        for c in &results {
            let score = togematch::score(&track, c);
            let official = togematch::is_official(&track, c);
            if let Some(s) = score {
                debug!(
                    "candidate {} [{:.0}s] score={s} official={official} {:?} ({:?})",
                    c.id,
                    c.duration as f32 / 1000.0,
                    c.title,
                    c.uploader
                );
            } else {
                trace!(
                    "rejected {} [{:.0}s] official={official} {:?} ({:?})",
                    c.id,
                    c.duration as f32 / 1000.0,
                    c.title,
                    c.uploader
                );
            }
        }
        // someone else's upload only when YouTube Music has no official one either
        let hit = match togematch::best(&track, &results).cloned() {
            Some(hit) if togematch::is_official(&track, &hit) => {
                debug!("official match: {} ({:?})", hit.id, hit.title);
                Some(hit)
            }
            other => {
                debug!("no official match in search results, checking YouTube Music...");
                let music = self.music_upload(&track);
                if let Some(m) = &music {
                    debug!("YouTube Music matched: {} ({:?})", m.id, m.title);
                } else if let Some(o) = &other {
                    debug!(
                        "YouTube Music had no match, falling back to unofficial: {}",
                        o.id
                    );
                }
                music.or(other)
            }
        };
        let entry = match &hit {
            Some(hit) => picked(&track, hit),
            // YouTube's top result for the plain search, if it's the artist's at the same length
            None => match togematch::top(&track, &results) {
                Some(top) => {
                    debug!("using top YouTube result: {} ({:?})", top.id, top.title);
                    entry(&track, Some(&top.id), "youtube")
                }
                None => {
                    debug!("no match found");
                    entry(&track, None, "none")
                }
            },
        };
        self.cache.put(&recording, &entry);
        Ok(Found {
            query: key,
            entry,
            cached: false,
        })
    }

    /// The track's upload among YouTube Music's songs, which reaches some a signed-out YouTube
    /// search hides.
    fn music_upload(&self, track: &Track) -> Option<togematch::Candidate> {
        let songs = youtube::music_details(&self.ytdlp, track)
            .inspect_err(|e| warn!("youtube music: {e}"))
            .ok()?;
        for c in &songs {
            let score = togematch::score(track, c);
            let official = togematch::is_official(track, c);
            if let Some(s) = score {
                debug!(
                    "YouTube Music candidate {} [{:.0}s] score={s} official={official} {:?} ({:?})",
                    c.id,
                    c.duration as f32 / 1000.0,
                    c.title,
                    c.uploader
                );
            } else {
                trace!(
                    "YouTube Music rejected {} [{:.0}s] official={official} {:?} ({:?})",
                    c.id,
                    c.duration as f32 / 1000.0,
                    c.title,
                    c.uploader
                );
            }
        }
        togematch::best(track, &songs).cloned()
    }

    /// The catalogue track Deezer says the search means, matched like a Spotify track; anything
    /// else is YouTube's top result, as if typed into YouTube.
    fn text(&self, text: &str) -> Result<Found, String> {
        let key = text_key(text);
        let turn = self.turn(&key);
        let _turn = turn.lock().unwrap_or_else(PoisonError::into_inner);
        let found = |entry: Entry, cached| Found {
            query: text.into(),
            entry,
            cached,
        };
        if let Some(entry) = self.cached(&key) {
            debug!("cache hit: {text:?} -> {:?}", entry.video);
            return Ok(found(entry, true));
        }
        let tracks = deezer::search_words(&self.http, text).unwrap_or_else(|e| {
            warn!("deezer: {e}");
            Vec::new()
        });
        let meant = deezer::meant(text, tracks);
        if let Some(m) = &meant {
            debug!(
                "Deezer meant track: {:?} - {:?} ({})",
                m.track.artist, m.track.name, m.recording
            );
        }
        // Typed words Deezer's romanised names lack ("結束バンド" for "kessoku band") are confirmed
        // by the upload's own names, or by YouTube Music listing the same upload for the search.
        // That listing is one yt-dlp run, asked for once here, however many candidates ask.
        let songs = std::cell::OnceCell::new();
        let confirmed = |meant: &deezer::Meant, hit: &togematch::Candidate| {
            meant.confirmed_by(hit)
                || songs
                    .get_or_init(|| youtube::music_songs(&self.ytdlp, text))
                    .as_ref()
                    .is_ok_and(|songs| songs.contains(&hit.id))
        };
        let matched = |meant: &deezer::Meant, hit: &togematch::Candidate| {
            let (entry, cached) = match self
                .cache
                .get(&meant.recording)
                .filter(|e| e.source == "manual")
            {
                Some(entry) => {
                    debug!(
                        "using manual override for {}: {:?}",
                        meant.recording, entry.video
                    );
                    (entry, true)
                }
                None => {
                    let entry = picked(&meant.track, hit);
                    self.cache.put(&meant.recording, &entry);
                    (entry, false)
                }
            };
            self.cache.set_alias(&key, &meant.recording);
            found(entry, cached)
        };
        if let Some(meant) = &meant {
            if meant.confirmed()
                && let Some(entry) = self.cache.get(&meant.recording)
            {
                debug!(
                    "confirmed meant track in cache: {} -> {:?}",
                    meant.recording, entry.video
                );
                self.cache.set_alias(&key, &meant.recording);
                return Ok(found(entry, true));
            }
            let queries = togematch::queries(&meant.track);
            let results = youtube::search(&self.ytdlp, &queries)?;
            for c in &results {
                let score = togematch::score(&meant.track, c);
                let official = togematch::is_official(&meant.track, c);
                let confirms = confirmed(meant, c);
                if let Some(s) = score {
                    debug!(
                        "candidate {} [{:.0}s] score={s} official={official} confirms={confirms} {:?} ({:?})",
                        c.id,
                        c.duration as f32 / 1000.0,
                        c.title,
                        c.uploader
                    );
                } else {
                    trace!(
                        "rejected {} [{:.0}s] official={official} confirms={confirms} {:?} ({:?})",
                        c.id,
                        c.duration as f32 / 1000.0,
                        c.title,
                        c.uploader
                    );
                }
            }
            if let Some(hit) =
                togematch::best(&meant.track, &results).filter(|hit| confirmed(meant, hit))
            {
                debug!("picked match: {} ({:?})", hit.id, hit.title);
                return Ok(matched(meant, hit));
            }
        }
        let results = youtube::search(&self.ytdlp, &[format!("ytsearch1:{text}")])?;
        // YouTube's search drops symbols: its top result for "bring me the horizon ¿" is any of
        // their tracks, so it has to carry them
        let symbols = togematch::symbols(text);
        let top = results.first().filter(|top| {
            let names = format!("{} {}", top.title, top.uploader).to_lowercase();
            symbols.iter().all(|s| names.contains(&s.to_lowercase()))
        });
        if let Some(t) = top {
            debug!("top YouTube result: {} ({:?})", t.id, t.title);
        }
        // nothing there either: the track may still be on YouTube Music
        if top.is_none()
            && let Some(meant) = &meant
            && let Some(hit) = self
                .music_upload(&meant.track)
                .filter(|hit| confirmed(meant, hit))
        {
            debug!("YouTube Music fallback hit: {} ({:?})", hit.id, hit.title);
            return Ok(matched(meant, &hit));
        }
        let entry = match top {
            Some(top) => Entry {
                video: Some(top.id.clone()),
                title: top.title.clone(),
                artist: top.uploader.clone(),
                duration: top.duration,
                source: "youtube",
            },
            None => Entry {
                video: None,
                title: text.into(),
                artist: String::new(),
                duration: 0,
                source: "none",
            },
        };
        // an alias to a recording with nothing stored would hide this answer
        self.cache.unalias(&key);
        self.cache.put(&key, &entry);
        Ok(found(entry, false))
    }
}

const OTHER_LINK: &str = "only Spotify track, album and playlist links, and searches";

/// A link, but not to a Spotify track, album or playlist.
fn other_link(q: &str) -> bool {
    ["https://", "http://", "spotify:"]
        .iter()
        .any(|scheme| q.starts_with(scheme))
}

/// Searches differing only in case and spacing are the same search.
fn text_key(text: &str) -> String {
    format!(
        "text:{}",
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    )
}

/// The matcher's pick: "match" when it's official, else "unofficial", searched again soon.
fn picked(track: &Track, hit: &togematch::Candidate) -> Entry {
    let official = togematch::is_official(track, hit);
    entry(
        track,
        Some(&hit.id),
        if official { "match" } else { "unofficial" },
    )
}

pub fn entry(track: &Track, video: Option<&str>, source: &'static str) -> Entry {
    Entry {
        video: video.map(str::to_string),
        title: track.name.clone(),
        artist: track.artist.clone(),
        duration: track.duration,
        source,
    }
}
