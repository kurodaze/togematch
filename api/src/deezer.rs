//! Deezer's public search (no key) turns typed text into a track with a canonical artist,
//! title, album and length for the matcher, and names its recording by ISRC, so different
//! ways of asking for one recording share a match.

use serde_json::Value;
use std::collections::HashSet;
use std::thread;
use togematch::{Candidate, Track};
use ureq::Agent;

/// Searches this short also ask for each word alone: Deezer leaves out tracks it should find
/// ("basshunter dota" lacks DotA (Radio Edit), which "dota" lists first).
const WORD_SEARCHES_UP_TO: usize = 4;

/// A track Deezer listed for a search.
pub struct Listed {
    /// `isrc:<code>`
    pub recording: String,
    pub track: Track,
    /// listed for the whole search, not one word of it
    pub whole: bool,
}

/// Deezer's tracks for the text, most popular first (its own order ranks a little-played single
/// version above the radio edit everybody means), each with its recording key: `isrc:<code>`.
fn search(http: &Agent, text: &str) -> Result<Vec<(String, Track)>, String> {
    let mut tracks = query(http, text)?;
    tracks.sort_by_key(|(rank, ..)| std::cmp::Reverse(*rank));
    Ok(tracks
        .into_iter()
        .map(|(_, recording, track)| (recording, track))
        .collect())
}

/// Like [`search`], plus each word of a short search alone, asked at once.
pub fn search_words(http: &Agent, text: &str) -> Result<Vec<Listed>, String> {
    let words = words(text);
    let alone = if (2..=WORD_SEARCHES_UP_TO).contains(&words.len()) {
        &words[..]
    } else {
        &[]
    };
    let (whole, alone) = thread::scope(|s| {
        let alone: Vec<_> = alone.iter().map(|w| s.spawn(|| query(http, w))).collect();
        let whole = query(http, text);
        let alone: Vec<_> = alone
            .into_iter()
            .filter_map(|t| t.join().ok()?.ok())
            .collect();
        (whole, alone)
    });
    let mut seen = HashSet::new();
    let mut tracks: Vec<(i64, Listed)> = whole?
        .into_iter()
        .map(|t| (t, true))
        .chain(alone.into_iter().flatten().map(|t| (t, false)))
        .filter(|((_, recording, _), _)| seen.insert(recording.clone()))
        .map(|((rank, recording, track), whole)| {
            let listed = Listed {
                recording,
                track,
                whole,
            };
            (rank, listed)
        })
        .collect();
    tracks.sort_by_key(|(rank, _)| std::cmp::Reverse(*rank));
    Ok(tracks.into_iter().map(|(_, listed)| listed).collect())
}

/// Deezer's tracks for one search, with their popularity.
fn query(http: &Agent, text: &str) -> Result<Vec<(i64, String, Track)>, String> {
    let body = http
        .get("https://api.deezer.com/search")
        .query("q", text)
        .query("limit", "25")
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| format!("Deezer didn't answer: {e}"))?;
    let data: Value =
        serde_json::from_str(&body).map_err(|e| format!("Deezer said something odd: {e}"))?;
    Ok(data["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let track = Track {
                name: s["title"].as_str()?.into(),
                artist: s["artist"]["name"].as_str()?.into(),
                album: s["album"]["title"].as_str().unwrap_or_default().into(),
                duration: s["duration"].as_i64().unwrap_or(0) * 1000,
                explicit: s["explicit_lyrics"].as_bool().unwrap_or(false),
            };
            let recording = match s["isrc"].as_str().filter(|i| !i.is_empty()) {
                Some(isrc) => format!("isrc:{isrc}"),
                None => format!("deezer:{}", s["id"].as_i64()?),
            };
            Some((s["rank"].as_i64().unwrap_or(0), recording, track))
        })
        .collect())
}

/// The same recording as a track from elsewhere (Spotify): the same title words and first
/// artist, and a length within 2 s.
pub fn same(http: &Agent, track: &Track) -> Result<Option<(String, Track)>, String> {
    let first_artist = track.artist.split(',').next().unwrap_or_default();
    let found = search(http, &format!("{first_artist} {}", track.name))?;
    let mut title = words(&track.name.to_lowercase());
    title.sort();
    let artist = words(&first_artist.to_lowercase());
    Ok(found.into_iter().find(|(_, s)| {
        let mut their_title = words(&s.name.to_lowercase());
        their_title.sort();
        let their_artist = words(&s.artist.to_lowercase());
        title == their_title
            && !artist.is_empty()
            && artist.iter().all(|w| their_artist.contains(w))
            && (s.duration - track.duration).abs() <= 2_000
    }))
}

/// The track a search means.
pub struct Meant {
    pub recording: String,
    pub track: Track,
    /// Typed words in a script Deezer romanises ("結束バンド" is "kessoku band" there), left
    /// for the YouTube upload to confirm.
    unconfirmed: Vec<String>,
}

impl Meant {
    /// Every typed word is in Deezer's names.
    pub fn confirmed(&self) -> bool {
        self.unconfirmed.is_empty()
    }

    /// The upload names what Deezer's names didn't: its title, channel and "Provided to
    /// YouTube" credit keep the original script.
    pub fn confirmed_by(&self, upload: &Candidate) -> bool {
        let names = format!(
            "{} {} {}",
            upload.title, upload.uploader, upload.description
        )
        .to_lowercase();
        self.unconfirmed.iter().all(|w| names.contains(w.as_str()))
    }
}

/// Deezer's most popular track that fits the search: every typed word in its artist or title,
/// the plain recording before live versions and remixes, and never a karaoke or cover release.
/// Words Deezer's romanised names lack are only left to confirm on tracks it listed for the
/// whole search, since one word alone lists the artist's every track.
pub fn meant(text: &str, tracks: Vec<Listed>) -> Option<Meant> {
    candidates(text, tracks).into_iter().next()
}

/// Every track that fits the search, best first, as [`meant`] ranks them.
pub fn candidates(text: &str, tracks: Vec<Listed>) -> Vec<Meant> {
    let fits = togematch::searched(text, tracks.iter().map(|l| &l.track));
    let mut tracks: Vec<Option<Listed>> = tracks.into_iter().map(Some).collect();
    fits.into_iter()
        .filter_map(|(i, fit)| {
            let Listed {
                recording,
                track,
                whole,
            } = tracks[i].take()?;
            (whole || fit.unconfirmed.is_empty()).then_some(Meant {
                recording,
                track,
                unconfirmed: fit.unconfirmed,
            })
        })
        .collect()
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "artist - title" of what the search means among these tracks, most popular first.
    fn pick(text: &str, tracks: &[(&str, &str)]) -> Option<String> {
        let tracks = tracks
            .iter()
            .enumerate()
            .map(|(i, (artist, name))| {
                let track = Track {
                    name: (*name).into(),
                    artist: (*artist).into(),
                    ..Default::default()
                };
                Listed {
                    recording: format!("deezer:{i}"),
                    track,
                    whole: true,
                }
            })
            .collect();
        meant(text, tracks).map(|m| format!("{} - {}", m.track.artist, m.track.name))
    }

    #[test]
    fn typed_words_must_all_be_in_the_track() {
        let dota = [("Basshunter", "DotA (Radio Edit)")];
        assert!(pick("basshunter dota", &dota).is_some());
        assert!(pick("DotA", &dota).is_some());
        assert!(pick("dota funny video", &dota).is_none());
        let stock = [("Vlog & Reel Sound Lab", "Funny Cat and Dog Video Music")];
        assert!(pick("funny cat video", &stock).is_none());
        assert!(pick("", &dota).is_none());
        assert!(pick("yoasobi 夜に", &[("YOASOBI", "夜に駆ける")]).is_some());
        let nein = [("r-906", "No Nein Disco (feat. IA AI)")];
        assert!(pick("r906 no nein disco", &nein).is_some());
        assert!(pick("r-906 no nein disco", &nein).is_some());
        assert!(pick("r90 no nein disco", &nein).is_none());
    }

    #[test]
    fn typed_symbols_must_be_in_the_track() {
        let tracks = [("Harry Styles", "Satellite"), (".........", "Satellite")];
        let dots = pick("satellite .........", &tracks);
        assert_eq!(dots.as_deref(), Some("......... - Satellite"));
        let tracks = [
            ("Bring Me The Horizon", "Can You Feel My Heart"),
            ("Bring Me The Horizon, Halsey", "¿ (feat. Halsey)"),
        ];
        let bmth = pick("bring me the horizon ¿", &tracks);
        assert_eq!(
            bmth.as_deref(),
            Some("Bring Me The Horizon, Halsey - ¿ (feat. Halsey)")
        );
        assert!(pick("basshunter - dota", &[("Basshunter", "DotA (Radio Edit)")]).is_some());
    }

    #[test]
    fn never_a_karaoke_or_cover_release() {
        let tracks = [
            ("kessoku band", "Never forget"),
            ("kessoku band", "Never forget -Fixed Star- Concert version"),
            ("歌っちゃ王", "忘れてやらない -3Key(原曲歌手:結束バンド)"),
        ];
        let jp = pick("結束バンド 忘れてやらない", &tracks);
        assert_eq!(jp.as_deref(), Some("kessoku band - Never forget"));
        let tracks = [
            ("DJ Kay Slay", "Rolling 200 Deep"),
            (
                "Mobile Melody Series",
                "GUM & DROP (Cover) [Originally Performed by Suisei Hoshimachi]",
            ),
        ];
        assert_eq!(pick("gum drop suisei", &tracks), None);
    }
    #[test]
    fn candidates_are_ranked_like_the_pick() {
        let tracks = [
            ("kessoku band", "Flashbacker -Fixed Star- Concert version"),
            ("kessoku band", "Flashbacker"),
            ("Someone", "Flashbacker (Karaoke)"),
        ];
        let listed: Vec<Listed> = tracks
            .iter()
            .enumerate()
            .map(|(i, (artist, name))| Listed {
                recording: format!("deezer:{i}"),
                track: Track {
                    name: (*name).into(),
                    artist: (*artist).into(),
                    ..Default::default()
                },
                whole: true,
            })
            .collect();
        let names: Vec<String> = candidates("flashbacker", listed)
            .into_iter()
            .map(|m| m.track.name)
            .collect();
        assert_eq!(
            names,
            ["Flashbacker", "Flashbacker -Fixed Star- Concert version"]
        );
    }

    #[test]
    fn the_plain_recording_before_a_live_one() {
        let live = ("kessoku band", "Flashbacker -Fixed Star- Concert version");
        let tracks = [live, ("kessoku band", "Flashbacker")];
        let plain = pick("kessoku band flashbacker", &tracks);
        assert_eq!(plain.as_deref(), Some("kessoku band - Flashbacker"));
        assert!(pick("kessoku band flashbacker", &[live]).is_some());
    }

    #[test]
    fn one_typo_only_when_nothing_is_exact() {
        let suisei = [("Hoshimachi Suisei", "GUM&DROP")];
        assert!(pick("gum&drop hochimachi", &suisei).is_some());
        assert!(pick("gum&drop hochimachi suisey", &suisei).is_none());
        assert!(pick("basshunter dotb", &[("Basshunter", "DotA")]).is_none());
        let tracks = [("Someone", "Tiny Danger"), ("Elton John", "Tiny Dancer")];
        let exact = pick("tiny dancer", &tracks);
        assert_eq!(exact.as_deref(), Some("Elton John - Tiny Dancer"));
    }

    #[test]
    fn romanised_names_are_confirmed_by_the_upload() {
        let track = Track {
            name: "Computer City".into(),
            artist: "Perfume".into(),
            ..Default::default()
        };
        let listed = |track, whole| Listed {
            recording: "isrc:x".into(),
            track,
            whole,
        };
        let text = "perfume コンピューターシティ";
        assert!(meant(text, vec![listed(track.clone(), false)]).is_none());
        let meant = meant(text, vec![listed(track, true)]).unwrap();
        assert!(!meant.confirmed());
        let upload = |title: &str| Candidate {
            title: title.into(),
            uploader: "Perfume - Topic".into(),
            ..Default::default()
        };
        assert!(meant.confirmed_by(&upload("コンピューターシティ")));
        assert!(!meant.confirmed_by(&upload("Computer City")));
    }
}
