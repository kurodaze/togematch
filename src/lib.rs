//! Finds the YouTube upload that is a track's own recording, preferring the licensed
//! "Provided to YouTube" master.
//!
//! Run every [`queries`] target in one `yt-dlp --flat-playlist --dump-json` call, turn the
//! rows into [`Candidate`]s and let [`best`] pick. This crate does no I/O itself.

/// What a catalogue (Spotify, Deezer) says about a track.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Track {
    pub name: String,
    /// Comma-separated.
    pub artist: String,
    pub album: String,
    /// Milliseconds, 0 if unknown.
    pub duration: i64,
    pub explicit: bool,
}

/// One flat yt-dlp search result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Candidate {
    /// YouTube video id.
    pub id: String,
    pub title: String,
    pub uploader: String,
    /// Truncated in flat results.
    pub description: String,
    pub album: String,
    /// Milliseconds, 0 if unknown.
    pub duration: i64,
}

const SEARCH_RESULTS: usize = 10;
/// A hit must carry at least this share of the track's core title tokens.
const MIN_TITLE_COVERAGE_PCT: i64 = 50;
/// Playback seeks into the file by Spotify's position, so any runtime gap plays as an
/// offset. Capped again at a fifth of the track: 10s on a 30s track is another recording.
const MAX_DURATION_DRIFT_MS: i64 = 10_000;
/// A core title this short ("EFN", "Dr.") identifies nothing on its own.
const WEAK_TITLE_CHARS: usize = 4;
/// Runtime match required to accept a title that shares no tokens at all.
const CROSS_SCRIPT_MAX_DRIFT_MS: i64 = 3_000;
/// Marks a licensed upload - the master Spotify serves.
const ART_TRACK_MARKER: &str = "provided to youtube by";
const INSTRUMENTAL: &[&str] = &["instrumental", "インスト", "off vocal"];
// 原曲歌手 = "original artist" - JP karaoke uploads name the source track this way.
const KARAOKE: &[&str] = &["karaoke", "カラオケ", "原曲歌手"];
// Karaoke and cover labels credit the original with the last three.
const COVER: &[&str] = &[
    "cover",
    "歌ってみた",
    "弾いてみた",
    "うたスキ",
    "originally performed",
    "in the style of",
    "made famous",
];
const LIVE: &[&str] = &["live", "tour", "concert", "ライブ", "コンサート"];
const REMIX: &[&str] = &["remix"];
const CAPPELLA: &[&str] = &["cappella", "アカペラ"];
/// Other masters of the same track at the same runtime, so duration can't separate them.
/// Compared by group: any marker in a group counts as that variant family.
const VARIANT_GROUPS: &[&[&str]] = &[
    INSTRUMENTAL,
    KARAOKE,
    COVER,
    LIVE,
    REMIX,
    &["edit"],
    &["english", "英語"],
    &["japanese", "日本語"],
    // "version" / ヴァージョン - see is_other_master for weak "ver." handling.
    &["version", "ヴァージョン", "バージョン"],
    CAPPELLA,
];

/// What a catalogue track is next to a typed search, by its version words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Version {
    /// No version words the search doesn't have.
    Asked,
    /// Live, a remix, instrumental or a cappella, when the search isn't.
    Other,
    /// Karaoke, a cover or pitch-shifted, when the search isn't.
    Cover,
}

/// A search for a track by name means its plain recording: other versions only when nothing
/// else fits, and never someone else's karaoke or cover.
fn version(track: &Track, typed: &str) -> Version {
    let name = track.name.to_lowercase();
    let artist = track.artist.to_lowercase();
    let typed = typed.to_lowercase();
    let adds = |group: &[&str], haystack: &str| {
        group.iter().any(|m| has_variant_marker(haystack, m))
            && !group.iter().any(|m| has_variant_marker(&typed, m))
    };
    // karaoke labels often say so in the artist name instead
    if adds(KARAOKE, &name)
        || adds(KARAOKE, &artist)
        || adds(COVER, &name)
        || (has_pitch_key_marker(&name) && !has_pitch_key_marker(&typed))
    {
        Version::Cover
    } else if [INSTRUMENTAL, LIVE, REMIX, CAPPELLA]
        .iter()
        .any(|g| adds(g, &name))
    {
        Version::Other
    } else {
        Version::Asked
    }
}

/// Typed words this long may be one letter off, as long as the rest are exact.
const TYPO_MIN_LEN: usize = 6;
/// Typed between artist and title ("basshunter - dota"), not part of either.
const SEPARATORS: &[char] = &[
    '-', '–', '\u{2014}', '&', '/', '|', ',', ':', ';', '+', '·', '・', '(', ')', '[', ']',
];

/// How a track holds the words of a typed search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fit {
    /// One word was a letter off.
    pub typo: bool,
    /// Typed words in a script the catalogue romanises ("結束バンド" is "kessoku band" on
    /// Deezer), left for the YouTube upload to confirm.
    pub unconfirmed: Vec<String>,
}

impl Fit {
    /// Every word as typed first, then with a typo, then with words left to confirm.
    fn rank(&self) -> u8 {
        if !self.unconfirmed.is_empty() {
            2
        } else if self.typo {
            1
        } else {
            0
        }
    }
}

/// The catalogue tracks a typed search means, best first, by their place in `tracks`: every
/// typed word in the artist or title, the plain recording before live versions and remixes, and
/// never a karaoke or cover release. Equal ranks keep the catalogue's order, so list the most
/// popular first.
#[must_use]
pub fn searched<'a>(text: &str, tracks: impl IntoIterator<Item = &'a Track>) -> Vec<(usize, Fit)> {
    let typed = tokens(text);
    let symbols = symbols(text);
    if typed.is_empty() && symbols.is_empty() {
        return Vec::new();
    }
    let mut fits: Vec<_> = tracks
        .into_iter()
        .enumerate()
        .filter_map(|(i, track)| {
            let version = version(track, text);
            let fit = fit(&typed, &symbols, track).filter(|_| version != Version::Cover)?;
            Some(((fit.rank(), version), i, fit))
        })
        .collect();
    // stable, so equal ranks stay in the catalogue's order
    fits.sort_by_key(|(rank, ..)| *rank);
    fits.into_iter().map(|(_, i, fit)| (i, fit)).collect()
}

/// Typed pieces with no letters or digits, like the artist "........." or the title "¿",
/// which must be in the track as they are.
#[must_use]
pub fn symbols(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .filter(|p| !p.chars().any(char::is_alphanumeric))
        .filter(|p| !p.chars().all(|c| SEPARATORS.contains(&c)))
        .collect()
}

fn fit(typed: &[String], symbols: &[&str], track: &Track) -> Option<Fit> {
    let have = format!("{} {}", track.artist, track.name).to_lowercase();
    if !symbols.iter().all(|s| have.contains(s)) {
        return None;
    }
    let have_words = tokens(&have);
    let mut fit = Fit {
        typo: false,
        unconfirmed: Vec::new(),
    };
    for w in typed {
        // scripts without spaces (CJK) match as substrings
        if joined(&have_words, w) || (!w.is_ascii() && have.contains(w.as_str())) {
            continue;
        }
        if !w.is_ascii() {
            fit.unconfirmed.push(w.clone());
        } else if !fit.typo && w.len() >= TYPO_MIN_LEN && have_words.iter().any(|h| one_off(w, h)) {
            fit.typo = true;
        } else {
            return None;
        }
    }
    // without the artist, the whole title: "funny cat video" isn't "Funny Cat and Dog Video Music"
    let artist = track.artist.to_lowercase();
    let artist_words = tokens(&artist);
    let names_artist = typed
        .iter()
        .any(|w| joined(&artist_words, w) || (!w.is_ascii() && artist.contains(w.as_str())));
    // up to its version: "Flashbacker -Fixed Star- Concert version", "DotA (Radio Edit)"
    let title = track
        .name
        .split(['(', '['])
        .next()
        .and_then(|t| t.split(" -").next())
        .unwrap_or_default();
    let letters = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).count();
    let typed_letters: usize = typed.iter().map(|w| letters(w)).sum();
    if !names_artist && typed_letters + 1 < letters(title) {
        return None;
    }
    Some(fit)
}

/// The word, or neighbouring words run together: "r906" is in "r-906", "acdc" in "AC/DC".
fn joined(words: &[String], word: &str) -> bool {
    (0..words.len()).any(|start| {
        let mut rest = word;
        for w in &words[start..] {
            let Some(after) = rest.strip_prefix(w.as_str()) else {
                return false;
            };
            if after.is_empty() {
                return true;
            }
            rest = after;
        }
        false
    })
}

/// One letter added, dropped or changed: "hochimachi" for "hoshimachi".
fn one_off(a: &str, b: &str) -> bool {
    let (mut a, mut b) = (a.chars(), b.chars());
    loop {
        let (a_rest, b_rest) = (a.as_str(), b.as_str());
        match (a.next(), b.next()) {
            (Some(x), Some(y)) if x == y => {}
            (Some(_), Some(_)) => {
                let (a_after, b_after) = (a.as_str(), b.as_str());
                return a_after == b_after || a_rest == b_after || a_after == b_rest;
            }
            (Some(_), None) => return a.as_str().is_empty(),
            (None, Some(_)) => return b.as_str().is_empty(),
            (None, None) => return false,
        }
    }
}

/// YouTube's top result, for when [`best`] finds nothing: only the artist's own upload or one
/// naming them, within 3 s of the track and no other version, since its title may just be in
/// another script. Anything else is someone else's track.
#[must_use]
pub fn top<'a>(track: &Track, candidates: &'a [Candidate]) -> Option<&'a Candidate> {
    let artists = artists(track);
    let want_lower = track.name.to_lowercase();
    candidates.iter().find(|c| {
        let title = c.title.to_lowercase();
        let uploader = c.uploader.to_lowercase();
        let hay = format!("{title} {uploader} {}", c.description.to_lowercase());
        track.duration > 0
            && c.duration > 0
            && (track.duration - c.duration).abs() <= CROSS_SCRIPT_MAX_DRIFT_MS
            && !is_other_master(&title, &want_lower)
            && (artists.iter().any(|a| uploader_is_artist(&uploader, a))
                || mentions_artist(&artists, &hay))
    })
}

/// The artist's name in an upload's title, channel or description. Names compare by token
/// (punctuation differs: "りぶ(CV:…)" vs "りぶ（CV:…）"), except digit-only tokens, which need
/// the full name.
fn mentions_artist(artists: &[String], hay: &str) -> bool {
    artists
        .iter()
        .any(|a| a.chars().count() >= 3 && hay.contains(a))
        || {
            let mentions = tokens(hay);
            artists.iter().any(|a| {
                let words = tokens(a);
                // "MinChen" for "Min Chen", but "2021" isn't the artist "2 0 2 1"
                let run = words.concat();
                (words.len() > 1 && artist_token_usable(&run) && mentions.contains(&run))
                    || words
                        .iter()
                        .filter(|t| artist_token_usable(t))
                        .any(|t| joined(&mentions, t))
            })
        }
}

/// Songs to take from [`music_search`], with their full details.
pub const MUSIC_SEARCH_RESULTS: usize = 2;

/// The same search on YouTube Music's songs, which lists only official uploads and reaches
/// some a signed-out YouTube search hides. Its flat rows carry no channel or length, so run
/// it without `--flat-playlist` for the first [`MUSIC_SEARCH_RESULTS`] songs, then [`best`].
#[must_use]
pub fn music_search(track: &Track) -> String {
    music_url(&build_query(track))
}

/// YouTube Music's songs for any text.
#[must_use]
pub fn music_url(text: &str) -> String {
    use std::fmt::Write;
    let mut url = String::from("https://music.youtube.com/search?q=");
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                url.push(char::from(b));
            }
            b' ' => url.push('+'),
            _ => {
                let _ = write!(url, "%{b:02X}");
            }
        }
    }
    url.push_str("#songs");
    url
}

/// yt-dlp search targets (`ytsearch10:...`) that together find the track's master.
#[must_use]
pub fn queries(track: &Track) -> Vec<String> {
    let query = build_query(track);
    // Plain search can rank the licensed master low or omit it; only licensed uploads carry
    // the marker, so a quoted-phrase search returns that catalogue alone.
    let mut targets = vec![
        format!("ytsearch{SEARCH_RESULTS}:{query}"),
        format!("ytsearch{SEARCH_RESULTS}:{query} \"{ART_TRACK_MARKER}\""),
    ];
    // Romanised Spotify titles miss kana Topic uploads in English ytsearch.
    if let Some(kana) = ascii_to_katakana(&track.name) {
        let query = search_terms(&format!("{} {kana}", first_artist(track)));
        targets.push(format!("ytsearch{SEARCH_RESULTS}:{query}"));
    }
    targets
}

/// How well a search result matches the track, or `None` if it is not this track.
#[must_use]
pub fn score(track: &Track, candidate: &Candidate) -> Option<i64> {
    let c = candidate;
    score_candidate(
        track,
        &c.title,
        &c.uploader,
        &c.description,
        &c.album,
        c.duration,
    )
}

/// The best-scoring candidate; the first one wins a tie.
#[must_use]
pub fn best<'a>(track: &Track, candidates: &'a [Candidate]) -> Option<&'a Candidate> {
    candidates
        .iter()
        .filter_map(|c| Some((score(track, c)?, c)))
        .reduce(|best, next| if next.0 > best.0 { next } else { best })
        .map(|(_, c)| c)
}

/// A licensed upload or one on the artist's own channel. Anything else [`best`] accepts is
/// the recording uploaded by someone else, picked because a search left the official one out.
#[must_use]
pub fn is_official(track: &Track, candidate: &Candidate) -> bool {
    let uploader = candidate.uploader.to_lowercase();
    uploader.ends_with("- topic")
        || credits_own_title(&candidate.title, &candidate.description)
        || artists(track)
            .iter()
            .any(|a| uploader_is_artist(&uploader, a))
}

/// "Provided to YouTube by Columbia Bohemian Rhapsody · Queen" on a video titled "Bohemian
/// Rhapsody", whatever the channel is called ("Queen Official"). A re-upload copying the line
/// rarely keeps the title as it is.
fn credits_own_title(title: &str, description: &str) -> bool {
    let desc = description.to_lowercase();
    let title = title.trim().to_lowercase();
    let Some(start) = desc.find(ART_TRACK_MARKER) else {
        return false;
    };
    desc[start..]
        .split_once(" · ")
        .is_some_and(|(credit, _)| !title.is_empty() && credit.trim_end().ends_with(&title))
}

/// A name with '&' counts whole ("MYTH & ROID") and as its parts.
fn artists(track: &Track) -> Vec<String> {
    let mut names = Vec::new();
    for name in track.artist.split(',').map(|a| a.trim().to_lowercase()) {
        if name.contains('&') {
            names.extend(
                name.split('&')
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty()),
            );
        }
        if !name.is_empty() {
            names.push(name);
        }
    }
    names
}

/// True when the title names a different master than the one requested.
fn is_other_master(title_lower: &str, want_lower: &str) -> bool {
    // "+3Key" / "-1Key" karaoke pitch shifts - not in VARIANT_GROUPS (token is "3key").
    if has_pitch_key_marker(title_lower) && !has_pitch_key_marker(want_lower) {
        return true;
    }
    for group in VARIANT_GROUPS {
        let version_family = group.contains(&"version");
        if version_family {
            // "Soundtrack Version" is the OST master, not a language/remix cut.
            let title_v = title_lower.replace("soundtrack version", " ");
            let want_v = want_lower.replace("soundtrack version", " ");
            // Spotify "ver." and Topic "Version" are one family, but a bare Spotify "ver."
            // ("2025 ver." remaster) must not require a version word on the title.
            let title_strong = group.iter().any(|m| has_variant_marker(&title_v, m));
            let want_strong = group.iter().any(|m| has_variant_marker(&want_v, m));
            if title_strong && !(want_strong || has_variant_marker(&want_v, "ver")) {
                return true;
            }
            if want_strong && !(title_strong || has_variant_marker(&title_v, "ver")) {
                return true;
            }
            continue;
        }

        let title_has = group.iter().any(|m| has_variant_marker(title_lower, m));
        let want_has = group.iter().any(|m| has_variant_marker(want_lower, m));
        // Title adds a variant Spotify didn't ask for.
        if title_has && !want_has {
            return true;
        }
        // Spotify asked for a variant; title must carry some form of it.
        if want_has && !title_has {
            return true;
        }
    }
    false
}

/// ASCII markers match whole tokens ("live" ≠ "Alive"); CJK / multi-word stay substrings.
fn has_variant_marker(haystack_lower: &str, marker: &str) -> bool {
    if marker.contains(' ') || !marker.is_ascii() {
        haystack_lower.contains(marker)
    } else {
        haystack_lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|t| t == marker)
    }
}

/// `+3Key` / `-1Key` karaoke pitch labels tokenize as `3key` / `1key`.
fn has_pitch_key_marker(haystack_lower: &str) -> bool {
    haystack_lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|t| {
            let Some(rest) = t.strip_suffix("key") else {
                return false;
            };
            !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
        })
}

/// The text outside brackets; an unclosed one runs to the end.
fn unbracketed(s: &str) -> String {
    let mut depth = 0_u32;
    s.chars()
        .filter(|&c| match c {
            '(' | '[' => {
                depth += 1;
                false
            }
            ')' | ']' => {
                depth = depth.saturating_sub(1);
                false
            }
            _ => depth == 0,
        })
        .collect()
}

fn tokens(value: &str) -> Vec<String> {
    value
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Digit-only tokens ("2","0") from artists like "2 0 2 1" match any numbered description.
fn artist_token_usable(t: &str) -> bool {
    !t.is_empty() && !t.chars().all(|c| c.is_ascii_digit())
}

/// `Title · Artist` line `YouTube` puts on every Art Track description.
fn art_track_credited_artist(desc_lower: &str) -> Option<String> {
    if !desc_lower.contains(ART_TRACK_MARKER) {
        return None;
    }
    let after = desc_lower.split(" · ").nth(1)?;
    let credited = after
        .split(['\n', '/', '℗'])
        .next()
        .unwrap_or(after)
        .split("  ")
        .next()
        .unwrap_or(after)
        .trim();
    // Flattened Art Tracks glue album on with one space, and EP titles contain '/':
    // "Iron Frill 大河よ共に泣いてくれ/Nope!!!!!" - keep the latin name, drop the CJK tail.
    // The name may start with a digit ("3-nen E-gumi Utatan バイバイ YESTERDAY").
    let credited = match credited.find(|c: char| !c.is_ascii() && !c.is_whitespace()) {
        Some(i) if credited[..i].contains(|c: char| c.is_ascii_alphabetic()) => {
            credited[..i].trim()
        }
        _ => credited,
    };
    (!credited.is_empty()).then(|| credited.to_string())
}

fn artist_credit_matches(credited: &str, artists: &[String], album: &str, title: &str) -> bool {
    let cred = tokens(credited);
    let album_tok = tokens(album);
    // a single's album usually carries its title, the one place a missing album can be guessed
    let title_tok = tokens(title);
    artists.iter().any(|a| {
        let at: Vec<String> = tokens(a)
            .into_iter()
            .filter(|t| artist_token_usable(t))
            .collect();
        if at.is_empty() || !cred.starts_with(&at) {
            return false;
        }
        let rest = &cred[at.len()..];
        let glued = |tail: &[String]| !tail.is_empty() && rest.starts_with(tail);
        // "Ray" can go on to "Ray Real", but a CJK name rarely goes on past a space, so
        // what follows is the album, whatever Spotify calls it ("仮病 毒針" vs "Dokubari")
        let cjk = at.iter().all(|t| !t.is_ascii());
        rest.is_empty() || cjk || glued(&album_tok) || (album_tok.is_empty() && glued(&title_tok))
    })
}

fn has_latin_token(s: &str) -> bool {
    tokens(s)
        .iter()
        .any(|t| artist_token_usable(t) && t.is_ascii())
}

fn art_track_credits_other_artist(
    credited: &str,
    artists: &[String],
    album: &str,
    title: &str,
) -> bool {
    if artist_credit_matches(credited, artists, album, title) {
        return false;
    }
    let cred_cjk = !credited.is_ascii();
    let us_cjk = artists.iter().any(|a| !a.is_ascii());
    let cred_latin = has_latin_token(credited);
    let us_latin = artists.iter().any(|a| has_latin_token(a));
    (cred_cjk && us_cjk) || (cred_latin && us_latin)
}

/// Rank one search hit. `None` means it is not this track.
fn score_candidate(
    track: &Track,
    title: &str,
    uploader: &str,
    description: &str,
    yt_album: &str,
    duration_ms: i64,
) -> Option<i64> {
    // Spotify keeps version info after " - " and credits in "(feat. X)". Credits match
    // every other track by the same artist with the same guest, so they cannot identify one.
    let want_lower = track.name.to_lowercase();
    let (core, suffix) = want_lower
        .split_once(" - ")
        .unwrap_or((want_lower.as_str(), ""));
    let core = ["(feat.", " feat.", "(with ", "(ft.", " ft."]
        .iter()
        .filter_map(|credit| core.find(credit))
        .min()
        .map_or(core, |i| &core[..i]);
    // Bracketed words are version info too, and a live album repeats its "(Live at Steamboat
    // 1874 ...)" on every track, so they can't tell its tracks apart.
    let whole_core = tokens(core);
    let bare = tokens(&unbracketed(core));
    let want_core = if bare.is_empty() {
        whole_core.clone()
    } else {
        bare
    };
    let version_words: Vec<String> = tokens(suffix)
        .into_iter()
        .chain(whole_core.into_iter().filter(|t| !want_core.contains(t)))
        .collect();
    let have = tokens(title);
    // "¿ (feat. Halsey)" → core "¿" → no alphanumeric tokens. Still a real title.
    let want_glyphs: String = core.chars().filter(|c| !c.is_whitespace()).collect();
    let symbol_only = want_core.is_empty();
    if symbol_only && want_glyphs.is_empty() {
        return None;
    }

    let title_lower = title.to_lowercase();
    let uploader = uploader.to_lowercase();
    let desc_lower = description.to_lowercase();
    if is_other_master(&title_lower, &want_lower) {
        return None;
    }
    // A censored edit of an explicit track is not the requested master.
    if track.explicit
        && have
            .iter()
            .any(|t| t == "clean" || t == "sfw" || t == "censored")
    {
        return None;
    }
    let artists = artists(track);
    let on_artist_channel = artists.iter().any(|a| uploader_is_artist(&uploader, a));
    // Flat search truncates descriptions, so a "- Topic" channel (which cannot be faked,
    // unlike a copied marker) also counts as licensed.
    let topic_channel = uploader.ends_with("- topic");
    let licensed = topic_channel || desc_lower.contains(ART_TRACK_MARKER);

    // Reject Art Tracks credited to a different artist. Skip when scripts don't overlap.
    if let Some(credited) = art_track_credited_artist(&desc_lower)
        && art_track_credits_other_artist(&credited, &artists, &track.album, title)
        && !on_artist_channel
    {
        return None;
    }

    // A shared title word is not evidence: require the licensed catalogue or a trace of the
    // artist, and both for generic titles.
    let artist_hay = format!("{title_lower} {uploader} {desc_lower}");
    let names_artist = on_artist_channel || mentions_artist(&artists, &artist_hay);
    let generic_title =
        symbol_only || (want_core.len() == 1 && want_core[0].chars().count() <= WEAK_TITLE_CHARS);
    // "Nope" is too short to identify a track, but アイアンフリル vs Iron Frill - Topic
    // shares no tokens either - still the licensed master of this recording.
    let cjk_artist_latin_topic = artists.iter().any(|a| !a.is_ascii())
        && !artists.iter().any(|a| has_latin_token(a))
        && has_latin_token(&uploader);
    if !(licensed || names_artist)
        || (generic_title && !(licensed && (names_artist || cjk_artist_latin_topic)))
    {
        return None;
    }

    let drift =
        (track.duration > 0 && duration_ms > 0).then(|| (track.duration - duration_ms).abs());
    if drift.is_some_and(|d| d > MAX_DURATION_DRIFT_MS.min(track.duration / 5)) {
        return None;
    }

    // CJK has no word breaks, so a token there is a whole phrase - match it as a substring.
    // Symbol-only titles compare glyphs ("¿" ↔ "¿"), not alphanumeric tokens.
    let coverage = if symbol_only {
        let title_glyphs: String = title_lower.chars().filter(|c| !c.is_whitespace()).collect();
        let want = want_glyphs.to_lowercase();
        if title_glyphs == want || title_glyphs.starts_with(&want) {
            100
        } else {
            0
        }
    } else {
        let title_kana = ascii_to_katakana(title);
        // by letters, so the "D" of "theater D" isn't half of it
        let letters = |t: &String| t.chars().count() as i64;
        let hits: i64 = want_core
            .iter()
            .filter(|t| {
                have.contains(t)
                    || (!t.is_ascii() && title_lower.contains(t.as_str()))
                    || title_kana.as_ref().is_some_and(|k| {
                        *t == k || k.contains(t.as_str()) || t.contains(k.as_str())
                    })
            })
            .map(letters)
            .sum();
        hits * 100 / want_core.iter().map(letters).sum::<i64>()
    };

    // Cross-script titles (フラッシュバッカー → Flashbacker) share no tokens: trust licensed
    // catalogue + runtime. Same-script zero coverage is a different track.
    let want_non_ascii = want_core.iter().any(|t| !t.is_ascii());
    let title_non_ascii = !title_lower.is_ascii();
    if coverage < MIN_TITLE_COVERAGE_PCT
        && !(licensed
            && drift.is_some_and(|d| d <= CROSS_SCRIPT_MAX_DRIFT_MS)
            && want_non_ascii != title_non_ascii)
    {
        return None;
    }
    // Licensed "Blue Memory" must not beat "blue memories" for artist 2 0 2 1: without
    // artist evidence, every title token has to match (partial + Art Track is not enough).
    if !names_artist && !want_non_ascii && coverage < 100 {
        return None;
    }

    // Coverage is mostly a gate: cross-script titles ("バイバイ" vs "Bye bye") score
    // low while still being the right track, so it must not dominate the ranking.
    let mut score = coverage / 2;

    // Version words we asked for and got, e.g. the remix rather than the original.
    score += version_words.iter().filter(|t| have.contains(t)).count() as i64 * 10;

    if licensed {
        score += 90;
    }
    // Auto-generated, so unlike the description marker a re-upload cannot copy it.
    if topic_channel {
        score += 15;
    }
    // Authoritative for their own tracks: a re-upload never outranks this on title alone.
    if on_artist_channel {
        score += 100;
    }
    if artists.iter().any(|a| title_lower.contains(a)) {
        score += 20;
    }

    if track.explicit && have.iter().any(|t| t == "explicit") {
        score += 15;
    }

    // Same master ⇒ same runtime. Steep, so a 0.2s match clearly beats a 10s one.
    if let Some(drift) = drift {
        score += 60_000 / (1_000 + drift);
    }

    // Album is packaging, not identity (Spotify #TWICE3 vs YT Breakthrough for the same
    // master). Bonus only on licensed/Topic hits when tokens overlap.
    if licensed && !track.album.is_empty() {
        let want_album = tokens(&track.album);
        if !want_album.is_empty() {
            let album_hay = format!("{} {}", yt_album.to_lowercase(), desc_lower);
            let hay_tokens = tokens(&album_hay);
            let album_hits = want_album
                .iter()
                .filter(|t| {
                    hay_tokens.contains(t) || (!t.is_ascii() && album_hay.contains(t.as_str()))
                })
                .count() as i64;
            if album_hits * 100 / want_album.len() as i64 >= MIN_TITLE_COVERAGE_PCT {
                score += 25;
            }
        }
    }

    Some(score)
}

fn build_query(track: &Track) -> String {
    search_terms(&format!("{} {}", first_artist(track), track.name))
}

fn first_artist(track: &Track) -> &str {
    track.artist.split(',').next().unwrap_or_default().trim()
}

/// Words without YouTube's search operators ('-' excludes a word, quotes make a phrase).
fn search_terms(raw: &str) -> String {
    let mut query = String::with_capacity(raw.len());
    for word in raw
        .split(|c: char| c == '-' || c == '"' || c == '!' || c.is_whitespace())
        .filter(|s| !s.is_empty())
    {
        if !query.is_empty() {
            query.push(' ');
        }
        query.push_str(word);
    }
    query
}

/// Uploader must start with the artist, not merely contain it: "X", "X - Topic" or "X Official
/// Channel". A channel shared by an artist's personas ("Tommy february6 / Tommy heavenly6")
/// counts for each name.
fn uploader_is_artist(uploader: &str, artist: &str) -> bool {
    let a = artist.trim();
    if a.is_empty() {
        return false;
    }
    uploader.split(" / ").any(|name| {
        let Some(rest) = name.trim().strip_prefix(a) else {
            return false;
        };
        rest.is_empty()
            || rest.starts_with(" -")
            || rest.starts_with('-')
            || rest
                .strip_prefix(' ')
                .is_some_and(|r| r == "official" || r.starts_with("official "))
    })
}

/// Best-effort English title → katakana for a second ytsearch.
fn ascii_to_katakana(title: &str) -> Option<String> {
    let core = title
        .split(['-', '('])
        .next()
        .unwrap_or(title)
        .trim()
        .to_lowercase();
    if core.is_empty() || !core.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let kana = romaji_to_katakana(&english_to_loan_romaji(&core));
    kana.chars()
        .any(|c| ('ァ'..='ヶ').contains(&c))
        .then_some(kana)
}

fn english_to_loan_romaji(s: &str) -> String {
    let mut s = s.to_string();
    // sl- → sur- so "slide" becomes スライド, not スリデ.
    if let Some(rest) = s.strip_prefix("sl") {
        s = format!("sur{rest}");
    }
    // Doubled letters are one sound ("satellite" → "satelite"); input is one ASCII word.
    let mut b = s.into_bytes();
    b.dedup();
    s = String::from_utf8(b).unwrap_or_default();
    // Magic e: "-ate" → "-eit", "-ite" → "-ait", ...
    if let [.., _, v @ (b'a' | b'i' | b'o' | b'u'), c, b'e'] = *s.as_bytes()
        && !b"aeiou".contains(&c)
    {
        let long = match v {
            b'a' => "ei",
            b'i' => "ai",
            b'o' => "ou",
            _ => "yu",
        };
        s = format!("{}{long}{}", &s[..s.len() - 3], c as char);
    }
    s = s.replace('l', "r");
    match s.bytes().last() {
        Some(b't' | b'd') => s.push('o'),
        Some(c) if b"bcfghjkmprsvwxyz".contains(&c) => s.push('u'),
        _ => {}
    }
    s
}

fn romaji_to_katakana(s: &str) -> String {
    // Longest morae first: the scan takes the first prefix that matches.
    #[rustfmt::skip]
    const MORA: &[(&str, &str)] = &[
        ("kya", "キャ"), ("kyu", "キュ"), ("kyo", "キョ"), ("sha", "シャ"), ("shu", "シュ"),
        ("sho", "ショ"), ("cha", "チャ"), ("chu", "チュ"), ("cho", "チョ"), ("nya", "ニャ"),
        ("nyu", "ニュ"), ("nyo", "ニョ"), ("hya", "ヒャ"), ("hyu", "ヒュ"), ("hyo", "ヒョ"),
        ("mya", "ミャ"), ("myu", "ミュ"), ("myo", "ミョ"), ("rya", "リャ"), ("ryu", "リュ"),
        ("ryo", "リョ"), ("gya", "ギャ"), ("gyu", "ギュ"), ("gyo", "ギョ"), ("bya", "ビャ"),
        ("byu", "ビュ"), ("byo", "ビョ"), ("pya", "ピャ"), ("pyu", "ピュ"), ("pyo", "ピョ"),
        ("tsu", "ツ"), ("chi", "チ"), ("shi", "シ"), ("ka", "カ"), ("ki", "キ"),
        ("ku", "ク"), ("ke", "ケ"), ("ko", "コ"), ("sa", "サ"), ("si", "シ"),
        ("su", "ス"), ("se", "セ"), ("so", "ソ"), ("ta", "タ"), ("ti", "チ"),
        ("tu", "ツ"), ("te", "テ"), ("to", "ト"), ("na", "ナ"), ("ni", "ニ"),
        ("nu", "ヌ"), ("ne", "ネ"), ("no", "ノ"), ("ha", "ハ"), ("hi", "ヒ"),
        ("fu", "フ"), ("he", "ヘ"), ("ho", "ホ"), ("ma", "マ"), ("mi", "ミ"),
        ("mu", "ム"), ("me", "メ"), ("mo", "モ"), ("ya", "ヤ"), ("yu", "ユ"),
        ("yo", "ヨ"), ("ra", "ラ"), ("ri", "リ"), ("ru", "ル"), ("re", "レ"),
        ("ro", "ロ"), ("wa", "ワ"), ("wo", "ヲ"), ("ga", "ガ"), ("gi", "ギ"),
        ("gu", "グ"), ("ge", "ゲ"), ("go", "ゴ"), ("za", "ザ"), ("zi", "ジ"),
        ("zu", "ズ"), ("ze", "ゼ"), ("zo", "ゾ"), ("da", "ダ"), ("di", "ヂ"),
        ("du", "ヅ"), ("de", "デ"), ("do", "ド"), ("ba", "バ"), ("bi", "ビ"),
        ("bu", "ブ"), ("be", "ベ"), ("bo", "ボ"), ("pa", "パ"), ("pi", "ピ"),
        ("pu", "プ"), ("pe", "ペ"), ("po", "ポ"), ("ja", "ジャ"), ("ju", "ジュ"),
        ("jo", "ジョ"), ("n", "ン"), ("a", "ア"), ("i", "イ"), ("u", "ウ"),
        ("e", "エ"), ("o", "オ"),
    ];
    let mora = |text: &str| MORA.iter().find(|(from, _)| text.starts_with(from));
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        if let Some((from, to)) = mora(rest) {
            out.push_str(to);
            rest = &rest[from.len()..];
            continue;
        }
        // a consonant with no vowel takes one, as in loanwords: "metro" is メトロ
        let vowel = if matches!(rest.as_bytes()[0], b't' | b'd') {
            "o"
        } else {
            "u"
        };
        if let Some((_, to)) = mora(&format!("{}{vowel}", &rest[..1])) {
            out.push_str(to);
        }
        rest = &rest[1..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures are real `yt-dlp --flat-playlist` rows: (title, uploader, description, secs)
    /// or (title, uploader, description, album, secs).
    fn pick<'a>(track: &Track, hits: &[(&'a str, &str, &str, i64)]) -> Option<&'a str> {
        pick_with_album(
            track,
            &hits
                .iter()
                .map(|(t, u, d, s)| (*t, *u, *d, "", *s))
                .collect::<Vec<_>>(),
        )
    }

    fn pick_with_album<'a>(
        track: &Track,
        hits: &[(&'a str, &str, &str, &str, i64)],
    ) -> Option<&'a str> {
        hits.iter()
            .filter_map(|(title, uploader, desc, album, secs)| {
                score_candidate(track, title, uploader, desc, album, secs * 1000)
                    .map(|s| (s, *title))
            })
            .reduce(|best, next| if next.0 > best.0 { next } else { best })
            .map(|(_, title)| title)
    }

    fn track(artist: &str, name: &str, duration: i64) -> Track {
        Track {
            name: name.into(),
            artist: artist.into(),
            album: String::new(),
            duration,
            explicit: false,
        }
    }

    #[test]
    fn best_picks_the_topic_master_over_the_music_video() {
        let t = track("Basshunter", "DotA - Radio Edit", 202_000);
        let candidate = |id: &str, title: &str, uploader: &str, secs: i64| Candidate {
            id: id.into(),
            title: title.into(),
            uploader: uploader.into(),
            duration: secs * 1000,
            ..Default::default()
        };
        let hits = [
            candidate("mv", "Basshunter DotA (Offical Video)", "Basshunter", 236),
            candidate("lyrics", "Basshunter - DotA (Lyrics)", "Lyric Vault", 203),
            candidate("topic", "DotA (Radio Edit)", "Basshunter - Topic", 202),
        ];
        assert_eq!(best(&t, &hits).map(|c| c.id.as_str()), Some("topic"));
        let q = queries(&t);
        assert_eq!(q[0], "ytsearch10:Basshunter DotA Radio Edit");
        assert!(q[1].ends_with(r#" "provided to youtube by""#));
    }

    /// The licensed upload sits on a channel the artist shares with another persona, and its
    /// truncated credit line glues the album on ("· Tommy heavenly6 LIVING DEAD …"). It must
    /// still beat the lyric video on that channel.
    #[test]
    fn a_channel_shared_by_personas_is_the_artists_own() {
        let t = track(
            "Tommy heavenly6, 奥田俊作",
            "LIVING DEAD DINER GIRLS -HALLOWEEN-",
            353_274,
        );
        let hits = [
            (
                "Tommy heavenly6 - LIVING DEAD DINER GIRLS -HALLOWEEN- (Lyric Video)",
                "Tommy february6 / Tommy heavenly6",
                "Tommy heavenly6 \"LIVING DEAD DINER GIRLS -HALLOWEEN-\"\n▶STREAMING & DL\nhttps://tommyheavenly6.lnk.to/LDDG-HALLOWEEN\n\n“Tommy ...",
                353,
            ),
            (
                "LIVING DEAD DINER GIRLS HALLOWEEN",
                "Tommy february6 / Tommy heavenly6",
                "Provided to YouTube by Sony Music Labels Inc. LIVING DEAD DINER GIRLS HALLOWEEN · Tommy heavenly6 LIVING DEAD\u{a0}...",
                354,
            ),
            (
                "Tommy Heavenly6 - LIVING DEAD DINER GIRLS",
                "Phantomheart's Brothers",
                "(⁰▿⁰)◜✧˖°HAPPY BIRTHDAY 2026 TMY°˖✧◝(⁰▿⁰)◜✧˖° Living Dead Dining Girls from Tommy Heavenly6 number 2 ...",
                354,
            ),
            (
                "Tommy heavenly6 - LIVING DEAD DINER GIRLS",
                "Tomoko Kawase Music",
                "Album: Tommy's Halloween Fairy Tale Label: HOUSE OF TMY RECORDS (Universal Music) Year: 2015 Apple Music: ...",
                251,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("LIVING DEAD DINER GIRLS HALLOWEEN"));
    }

    /// 蝶 is 2.2s from the target; the requested track is 8.2s out.
    #[test]
    fn title_identity_outranks_a_closer_duration() {
        let t = track(
            "Acid Black Cherry",
            "少女の祈り III - ～2025 ver.～",
            296_800,
        );
        let hits = [
            (
                "Acid Black Cherry「少女の祈り III」MV",
                "AcidBlackChannel",
                "",
                305,
            ),
            ("Acid Black Cherry「蝶」MV", "AcidBlackChannel", "", 299),
        ];
        assert_eq!(
            pick(&t, &hits),
            Some("Acid Black Cherry「少女の祈り III」MV")
        );

        // "(feat. 初音ミク)" fits every track by this artist with that guest, so credits
        // are stripped before gating - otherwise イレヴンス clears it on feat + 初音ミク.
        let t = track(
            "ポリスピカデリー, Hatsune Miku",
            "シアンブルー (feat. 初音ミク)",
            174_461,
        );
        let other = "イレヴンス - ポリスピカデリー feat. 初音ミク";
        assert!(
            score_candidate(
                &t,
                other,
                "ポリスピカデリー / Police Piccadilly",
                "",
                "",
                202_000
            )
            .is_none()
        );
    }

    /// "EFN" fits any number of unrelated uploads, and 30s of slack on a 52s track is
    /// another recording entirely. Nothing here is VRIGGER's master, so nothing matches.
    #[test]
    fn a_generic_title_matches_nothing_without_a_licensed_master() {
        let mut t = track("VRIGGER", "EFN", 52_755);
        t.explicit = true;
        for (title, uploader, desc, secs) in [
            (
                "VRIGGER (EFN)",
                "Vault uploads - Topic",
                "Provided to YouTube by DistroKid  VRIGGER (EFN) · Vault Uploads · Vau · Addy",
                75,
            ),
            (
                "EFN",
                "1ly9 - Topic",
                "Provided to YouTube by Routenote  EFN · 1ly9 · 22xlstl",
                56,
            ),
            (
                "VRIGGER - EFN || SFW Version [Archived]",
                "ArchLiveX",
                "",
                53,
            ),
        ] {
            assert!(
                score_candidate(&t, title, uploader, desc, "", secs * 1000).is_none(),
                "{title} should not match"
            );
        }

        // Sharing one common word is not evidence, and this artist's name cannot appear
        // in any title, so only the licensed catalogue could ever identify the track.
        let t = track(".........", "Satellite", 332_199);
        assert!(
            score_candidate(
                &t,
                "Satellite - Emotional Future Bass | Melodic Trap | JO3Y-XNG",
                "JO3Y-XNG",
                "Artist: JO3Y-XNG  Genre: EDM",
                "",
                312_000,
            )
            .is_none()
        );
    }

    #[test]
    fn ascii_title_to_katakana_for_search() {
        assert_eq!(ascii_to_katakana("Slide").as_deref(), Some("スライド"));
        assert_eq!(
            ascii_to_katakana("Satellite").as_deref(),
            Some("サテライト")
        );
        assert_eq!(ascii_to_katakana("Atena").as_deref(), Some("アテナ"));
        assert_eq!(ascii_to_katakana("Metro").as_deref(), Some("メトロ"));
        assert!(ascii_to_katakana("フラッシュバッカー").is_none());
    }

    #[test]
    fn kana_title_beats_another_same_length_topic_track() {
        let mut t = track("Abingdon Boys School", "アテナ", 291_000);
        t.album = "abingdon boys school".into();
        let atena = "Provided to YouTube by Sony Music Labels Inc. Atena · abingdon boys school abingdon boys school ℗ 2007 Epic Records Japan";
        let lost = "Provided to YouTube by Sony Music Labels Inc. LOST REASON · abingdon boys school Nephilim ℗ 2007 Sony Music";
        let hits = [
            ("LOST REASON", "abingdon boys school - Topic", lost, 292),
            ("Atena", "abingdon boys school - Topic", atena, 292),
        ];
        assert_eq!(pick(&t, &hits), Some("Atena"));
        assert!(
            score_candidate(
                &t,
                "LOST REASON",
                "abingdon boys school - Topic",
                lost,
                "Nephilim",
                292_000
            )
            .unwrap()
                < score_candidate(
                    &t,
                    "Atena",
                    "abingdon boys school - Topic",
                    atena,
                    "abingdon boys school",
                    292_000
                )
                .unwrap()
        );
    }

    #[test]
    fn same_script_topic_catalogue_is_not_a_romanisation() {
        let t = track("The Joy Formidable", "Endtapes", 249_000);
        let lotus = "Provided to YouTube by Warner Music Dance of the Lotus · The Joy Formidable";
        let ost = "Provided to YouTube by Chop Shop/Atlantic Endtapes (Soundtrack Version) · The Joy Formidable";
        let hits = [
            (
                "Dance of the Lotus",
                "The Joy Formidable - Topic",
                lotus,
                249,
            ),
            (
                "Endtapes (Soundtrack Version)",
                "The Joy Formidable",
                ost,
                250,
            ),
            (
                "The Joy Formidable - Endtapes Lyrics (The Twilight Saga: Breaking Dawn Part 1)",
                "lyric channel",
                "",
                250,
            ),
            (
                "■ The Joy Formidable - 'Endtapes' Lyric Video ■",
                "another",
                "",
                250,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("Endtapes (Soundtrack Version)"));
        assert!(
            score_candidate(
                &t,
                "Dance of the Lotus",
                "The Joy Formidable - Topic",
                lotus,
                "",
                249_000
            )
            .is_none()
        );
    }

    #[test]
    fn bangs_in_the_title_still_match_the_topic_master() {
        let t = track("アイアンフリル", "Nope!!!!!", 235_000);
        let hits = [
            (
                "Nope!!!!!",
                "Iron Frill - Topic",
                "Provided to YouTube by avex pictures Nope!!!!! · Iron Frill 大河よ共に泣いてくれ/Nope!!!!! ℗ AVEX PICTURES INC. Released on:",
                236,
            ),
            (
                "『Nope!!!!!』by Iron Frill 【日中双字幕】",
                "ここねkokone Ch.",
                "",
                236,
            ),
            (
                "FANTASTIC LOVERS",
                "Iron Frill - Topic",
                "Provided to YouTube by avex pictures FANTASTIC LOVERS · Iron Frill",
                248,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("Nope!!!!!"));
    }

    #[test]
    fn short_artist_is_not_a_substring_of_another_artist() {
        let mut t = track("RAY", "Slide", 256_000);
        t.album = "Pink".into();
        let donnie =
            "Provided to YouTube by Vydia Hip Slide · Donnie Ray  Let's Go Dancing  ℗ 2000";
        let ray =
            "Provided to YouTube by TuneCore Japan スライド · RAY Pink ℗ 2020 DISTORTED RECORDS";
        let hits = [
            ("Hip Slide", "Donnie Ray - Topic", donnie, 252),
            ("スライド", "RAY - Topic", ray, 256),
            (
                "SLIDE",
                "RAY REAL",
                "Provided to YouTube by DistroKid SLIDE · Ray Real SLIDE",
                180,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("スライド"));
        assert!(
            score_candidate(
                &t,
                "Hip Slide",
                "Donnie Ray - Topic",
                donnie,
                "Let's Go Dancing",
                252_000,
            )
            .is_none(),
            "Donnie Ray is not RAY"
        );
    }

    #[test]
    fn prefers_the_licensed_master_over_other_masters() {
        let provided =
            "Provided to YouTube by Universal Music Group Fragile Violet · TOGENASHI TOGEARI";
        let t = track("TOGENASHI TOGEARI", "Fragile Violet", 187_107);
        let hits = [
            ("Fragile Violet", "TOGENASHI TOGEARI - Topic", provided, 188),
            (
                "Fragile Violet (Instrumental)",
                "TOGENASHI TOGEARI - Topic",
                provided,
                188,
            ),
            (
                "【Official Music Video】TOGENASHI TOGEARI \"Fragile Violet\"",
                "ガールズバンドクライ",
                "",
                188,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("Fragile Violet"));

        // Same list, opposite ask: instrumental Spotify title must not pick the vocal.
        let t = track(
            "TOGENASHI TOGEARI",
            "Fragile Violet (Instrumental)",
            187_107,
        );
        assert_eq!(pick(&t, &hits), Some("Fragile Violet (Instrumental)"));

        // Same licensed runtime, different language master - YOASOBI UNDEAD.
        let provided = "Provided to YouTube by Sony Music Labels Inc.";
        let t = track("YOASOBI", "UNDEAD", 182_000);
        let hits = [
            ("UNDEAD", "YOASOBI - Topic", provided, 182),
            ("UNDEAD (English Version)", "YOASOBI - Topic", provided, 182),
        ];
        assert_eq!(pick(&t, &hits), Some("UNDEAD"));

        // Spotify's English master is titled with parentheses, not " - ".
        let t = track("YOASOBI", "UNDEAD (English Version)", 182_000);
        assert_eq!(
            pick(&t, &hits),
            Some("UNDEAD (English Version)"),
            "playing the English Version must not fall back to the Japanese master"
        );

        // HALCALI names the instrumental with ヴァージョン, not "instrumental"; the
        // instrumental is 1s closer to Spotify's runtime, so duration alone picks wrong.
        let provided = "Provided to YouTube by FOR LIFE MUSIC ENTERTAINMENT, INC.";
        let t = track("HALCALI", "ストロベリーチップス", 363_000);
        let hits = [
            ("ストロベリーチップス", "HALCALI", provided, 364),
            (
                "ストロベリーチップス (2人ともいないヴァージョン)",
                "HALCALI",
                provided,
                363,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("ストロベリーチップス"));
        let t = track(
            "HALCALI",
            "ストロベリーチップス (2人ともいないヴァージョン)",
            363_000,
        );
        assert_eq!(
            pick(&t, &hits),
            Some("ストロベリーチップス (2人ともいないヴァージョン)")
        );

        // Spotify "Japanese ver." vs Topic "Japanese Version"; tour is not live-tagged as "live".
        let provided = "Provided to YouTube by WM Japan FANCY (Japanese Version) · TWICE";
        let t = track("TWICE", "FANCY - Japanese ver.", 216_000);
        let hits = [
            ("FANCY (Japanese Version)", "TWICE - Topic", provided, 217),
            (
                "TWICE「FANCY -Japanese ver.-」Music Video",
                "TWICE JAPAN OFFICIAL YouTube Channel",
                "",
                219,
            ),
            (
                "TWICE -「FANCY -Japanese ver.-」4TH WORLD TOUR ‘III’ IN JAPAN",
                "TWICE World",
                "",
                217,
            ),
            (
                "FANCY",
                "TWICE - Topic",
                "Provided to YouTube by JYP Entertainment FANCY · TWICE",
                214,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("FANCY (Japanese Version)"));

        // Album is a soft bonus on licensed hits, not a gate - mismatched packaging
        // (Breakthrough vs #TWICE3) must still match on title variants.
        let provided = "Provided to YouTube by WM Japan";
        let mut t = track("TWICE", "FANCY - Japanese ver.", 216_000);
        t.album = "Breakthrough".into();
        let with = score_candidate(
            &t,
            "FANCY (Japanese Version)",
            "TWICE - Topic",
            provided,
            "Breakthrough",
            217_000,
        )
        .unwrap();
        let without = score_candidate(
            &t,
            "FANCY (Japanese Version)",
            "TWICE - Topic",
            provided,
            "Other Record",
            217_000,
        )
        .unwrap();
        assert!(with > without, "{with} should beat {without}");
        t.album = "#TWICE3".into();
        assert!(
            score_candidate(
                &t,
                "FANCY (Japanese Version)",
                "TWICE - Topic",
                provided,
                "Breakthrough",
                217_000,
            )
            .is_some(),
            "diverging album packaging must not reject the licensed master"
        );

        // Licensed same-title, different artist must not win on CJK coverage.
        let provided = "Provided to YouTube by Aniplex Inc. Flashbacker · kessoku band";
        let otoha = "Provided to YouTube by The Orchard Enterprises フラッシュバッカー · 音羽-otoha- / Unlockable";
        let t = track("結束バンド", "フラッシュバッカー", 275_000);
        let hits = [
            ("Flashbacker", "kessoku band - Topic", provided, 275),
            (
                "【LIVE映像】「フラッシュバッカー」/ ぼっち・ざ・ろっく！-SPECIAL STUDIO LIVE-",
                "アニプレックス チャンネル",
                "",
                293,
            ),
            ("フラッシュバッカー", "音羽-otoha-", otoha, 266),
            (
                "結束バンド - フラッシュバッカー 【結束バンド】",
                "some channel",
                "",
                275,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("Flashbacker"));
        let t = track("kessoku band, 結束バンド", "フラッシュバッカー", 275_000);
        assert_eq!(pick(&t, &hits), Some("Flashbacker"));

        // Symbol-only Spotify title ("¿") tokenizes to nothing after feat credits are stripped.
        let provided =
            "Provided to YouTube by RCA Records Label · ¿ · Bring Me The Horizon · Halsey";
        let t = track("Bring Me The Horizon", "¿ (feat. Halsey)", 313_000);
        let hits = [
            ("¿", "Bring Me The Horizon", provided, 314),
            (
                "Bring Me The Horizon ft. Halsey - ¿ (Lyrics)",
                "lyric videos",
                "",
                314,
            ),
            (
                "Sigrid, Bring Me The Horizon - Bad Life (Official Video)",
                "Sigrid",
                "",
                227,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("¿"));

        // "ライブ" is one CJK token, so markers have to match as substrings.
        let t = track("Perfume", "コンピューターシティ", 284_000);
        let hits = [
            (
                "[2011字幕でライブ] Perfume 「コンピューターシティ」",
                "prfm-uk",
                "",
                285,
            ),
            ("Perfume - コンピューターシティ", "wpta 005", "", 283),
        ];
        assert_eq!(pick(&t, &hits), Some("Perfume - コンピューターシティ"));

        // Two licensed uploads of one title; only the runtime says which is this
        // recording - the 259s one is a different singer's cover.
        let t = track(
            "3年E組うた担(渚&茅野&業&磯貝&前原)",
            "バイバイ YESTERDAY",
            248_800,
        );
        let hits = [
            (
                "Bye bye YESTERDAY",
                "3rd GRADE CLASS E",
                "Provided to YouTube by avex trax  Bye bye YESTERDAY · 3-nen E-gumi Utatan",
                249,
            ),
            (
                "バイバイ YESTERDAY",
                "Release - Topic",
                "Provided to YouTube by NexTone Inc.  バイバイ YESTERDAY · 堀部糸成 (緒方恵美)",
                259,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("Bye bye YESTERDAY"));
    }

    /// Pitch-shifted karaoke (`-1Key` / 原曲歌手) must not beat the lyric video / Topic master.
    #[test]
    fn rejects_karaoke_pitch_shift_uploads() {
        let t = track("結束バンド", "忘れてやらない", 223_000);
        let hits = [
            (
                "【Lyric Video】結束バンド「忘れてやらない」／ TVアニメ「ぼっち・ざ・ろっく！」第12話劇中曲",
                "Aniplex",
                "",
                223,
            ),
            (
                "Never forget",
                "kessoku band - Topic",
                "Provided to YouTube by Aniplex Inc. Never forget · kessoku band",
                223,
            ),
            (
                "忘れてやらない -1Key (原曲歌手:結束バンド)",
                "Karaoke Channel",
                "",
                223,
            ),
            (
                "忘れてやらない +3Key (原曲歌手:結束バンド)",
                "Karaoke Channel",
                "",
                223,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("Never forget"));
        assert!(
            score_candidate(
                &t,
                "忘れてやらない -1Key (原曲歌手:結束バンド)",
                "Karaoke Channel",
                "",
                "",
                223_000,
            )
            .is_none()
        );
    }

    /// Artist "2 0 2 1" must not let licensed "Blue Memory" (Nico Morano) win on digit tokens.
    #[test]
    fn digit_artist_needs_full_name_not_licensed_near_title() {
        let t = track("2 0 2 1", "blue memories", 350_000);
        let hits = [
            (
                "2 0 2 1 - blue memories",
                "theuppermostinlife",
                "Presenting blue memories from reverie by 2 0 2 1",
                352,
            ),
            ("2 0 2 1 - blue memories [BREAKCORE]", "fan", "", 350),
            (
                "Blue Memory",
                "NICO MORANO",
                "Provided to YouTube by Paradise Entertainment & Distribution GmbH Blue Memory · Nico Morano feat. Mewhy",
                347,
            ),
        ];
        let picked = pick(&t, &hits).expect("should match a 2 0 2 1 upload");
        assert!(
            picked.starts_with("2 0 2 1 - blue memories"),
            "picked wrong master: {picked}"
        );
        assert!(
            score_candidate(
                &t,
                "Blue Memory",
                "NICO MORANO",
                "Provided to YouTube by Paradise Entertainment Blue Memory · Nico Morano",
                "",
                347_000,
            )
            .is_none()
        );
    }

    /// "スクラマイズ" shares no tokens with "Sukuramaizu"; スヂ on the same channel is 20s out.
    #[test]
    fn rescues_a_romanised_title_only_on_an_exact_runtime() {
        let t = track("Maretu", "Sukuramaizu", 234_156);
        let hits = [
            (
                "【初音ミク】　スクラマイズ　【オリジナル】",
                "MARETU",
                "Provided to YouTube by TuneCore Japan  スクラマイズ · MARETU",
                234,
            ),
            (
                "MARETU ft. 初音ミク \"Scrumize\" (English Subtitles)",
                "FreedomT1 subs",
                "",
                234,
            ),
        ];
        assert_eq!(
            pick(&t, &hits),
            Some("【初音ミク】　スクラマイズ　【オリジナル】")
        );
        // A different track on the same channel, 20s out, must not be rescued.
        assert!(
            score_candidate(
                &t,
                "【初音ミク】　スヂ　【オリジナル】",
                "MARETU",
                "Provided to YouTube by TuneCore Japan  スヂ · MARETU",
                "",
                214_000
            )
            .is_none()
        );
        // Nor may the catalogue vouch for a different track by an artist it never names:
        // A9's "Time Machine" is 253.8s and their "yaminichirusakura" lands 0.76s away.
        let a9 = track("A9", "Time Machine", 253_761);
        assert!(
            score_candidate(
                &a9,
                "yaminichirusakura",
                "Alice Nine - Topic",
                "Provided to YouTube by Rightsscale  yaminichirusakura · Alice Nine",
                "",
                253_000
            )
            .is_none()
        );

        // Nor may a fan channel named after the artist vouch for one of their other tracks.
        assert!(
            score_candidate(
                &t,
                "MARETU - スヂ ⦗Kanji•Romaji⦘",
                "MARETU SubEspañol",
                "",
                "",
                233_000
            )
            .is_none()
        );
    }

    /// `YouTube` reads a leading '-' as "exclude this word", so the raw Spotify title
    /// asked for the track while excluding the word that identifies it.
    #[test]
    fn a_search_means_the_plain_recording() {
        let v = |artist, name, typed| version(&track(artist, name, 0), typed);
        assert_eq!(
            v(
                "歌っちゃ王",
                "忘れてやらない -3Key(原曲歌手:結束バンド)",
                "結束バンド 忘れてやらない"
            ),
            Version::Cover
        );
        assert_eq!(
            v(
                "Mobile Melody Series",
                "GUM & DROP (Cover) [Originally Performed by Suisei Hoshimachi]",
                "gum drop suisei"
            ),
            Version::Cover
        );
        assert_eq!(
            v("Karaoke Hits Band", "DotA", "basshunter dota"),
            Version::Cover
        );
        assert_eq!(
            v("Bruce Springsteen", "Cover Me", "springsteen cover me"),
            Version::Asked
        );
        assert_eq!(
            v(
                "kessoku band",
                "Flashbacker -Fixed Star- Concert version",
                "kessoku band flashbacker"
            ),
            Version::Other
        );
        assert_eq!(
            v(
                "kessoku band",
                "Flashbacker -We will B- Live version",
                "flashbacker live"
            ),
            Version::Asked
        );
        assert_eq!(
            v("Basshunter", "DotA (Radio Edit)", "basshunter dota"),
            Version::Asked
        );
        assert_eq!(
            v("Oasis", "Live Forever", "oasis live forever"),
            Version::Asked
        );
    }

    #[test]
    fn a_credit_starting_with_a_digit_is_still_a_latin_name() {
        let t = track(
            "3年E組うた担(渚&茅野&業&磯貝&前原)",
            "Bye Bye YESTERDAY",
            250_000,
        );
        let hits = [
            (
                "バイバイ YESTERDAY【Bye Bye YESTERDAY】/ 暗殺教室 Opening Theme 歌詞",
                "Saijo",
                "",
                249,
            ),
            (
                "Bye bye YESTERDAY",
                "3nenEgumiutatan(nagisa&kayano&karma&isogai&maehara) - Topic",
                "Provided to YouTube by avex trax Bye bye YESTERDAY · 3-nen E-gumi Utatan \
                 バイバイ YESTERDAY ℗ Avex Pictures Inc.",
                249,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("Bye bye YESTERDAY"));
    }

    #[test]
    fn a_credit_glued_to_its_single_is_still_the_artist() {
        let t = track("Mika Nakashima", "祈り、終われば", 237_188);
        let hits = [
            (
                "中島美嘉 나카시마 미카 - 祈り、終われば #無職転生 (2026.10.03)",
                "Mika Bangumi",
                "中島美嘉 #無職転生",
                235,
            ),
            (
                "Inori Owareba",
                "中島美嘉 Official YouTube Channel",
                "Provided to YouTube by Sony Music Labels Inc. Inori Owareba · Mika Nakashima \
                 Inori Owareba ℗ 2026 Sony Music Labels Inc.",
                238,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("Inori Owareba"));
    }

    /// ヒトリゴト is another track by them, 2.7s away
    #[test]
    fn a_cjk_credit_glued_to_its_album_is_still_the_artist() {
        let mut t = track("仮病", "Gosh!!", 199_680);
        let hits = [
            (
                "ヒトリゴト",
                "KEBYO OFFICIAL YouTube CHANNEL",
                "Provided to YouTube by NexTone Inc. ヒトリゴト · 仮病 ヒトリゴト Released on: 2018-02-21",
                197,
            ),
            (
                "ゴッシュ‼",
                "KEBYO OFFICIAL YouTube CHANNEL",
                "Provided to YouTube by NexTone Inc. ゴッシュ‼ · 仮病 毒針 Released on: 2019-01-16",
                200,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("ゴッシュ‼"));
        t.album = "Dokubari".into();
        assert_eq!(pick(&t, &hits), Some("ゴッシュ‼"));
    }

    /// "MYTH & ROID" is one artist, and its "Official Channel" is its own
    #[test]
    fn an_ampersand_name_on_its_official_channel() {
        let t = track("MYTH & ROID", "theater D", 221_106);
        let channel = "MYTH & ROID Official Channel";
        let hits = [
            ("MYTH & ROID   theater D", "Viina", "", 223),
            (
                "theater D",
                channel,
                "Provided to YouTube by KADOKAWA CORPORATION SRAV theater D · MYTH & ROID MYTH & ROID BEST",
                221,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some("theater D"));
        let raison = "Provided to YouTube by KADOKAWA CORPORATION RAISON D'ETRE · MYTH & ROID";
        assert!(
            score_candidate(&t, "RAISON D'ETRE", channel, raison, "", 222_000).is_none(),
            "a shared \"D\" is not the title"
        );
    }

    /// Every track of a live album carries the same "(Live at ...)"
    #[test]
    fn a_live_album_title_is_not_the_track() {
        let t = track(
            "Stevie Ray Vaughan & Double Trouble",
            "Slide Thing (Live at Steamboat 1874, Austin, TX - April 1980)",
            184_000,
        );
        let hits = [
            (
                "They Call Me Guitar Hurricane (Live at Steamboat 1874, Austin, TX - April 1980)",
                "Stevie Ray Vaughan",
                "Provided to YouTube by Epic They Call Me Guitar Hurricane (Live at Steamboat 1874, Austin, TX - April 1980) · Stevie Ray Vaughan & Double Trouble",
                183,
            ),
            (
                "Slide Thing (Live at Steamboat 1874, Austin, TX - April 1980)",
                "Stevie Ray Vaughan",
                "Provided to YouTube by Epic Slide Thing (Live at Steamboat 1874, Austin, TX - April 1980) · Stevie Ray Vaughan & Double Trouble",
                175,
            ),
        ];
        assert_eq!(pick(&t, &hits), Some(hits[1].0));
    }

    #[test]
    fn official_uploads_are_licensed_or_the_artists_own() {
        let t = track("YOASOBI", "UNDEAD", 182_000);
        let upload = |uploader: &str, description: &str| Candidate {
            uploader: uploader.into(),
            description: description.into(),
            ..Default::default()
        };
        assert!(is_official(&t, &upload("YOASOBI", "")));
        assert!(is_official(&t, &upload("YOASOBI - Topic", "")));
        let licensed = |title: &str, uploader: &str, description: &str| Candidate {
            title: title.into(),
            ..upload(uploader, description)
        };
        let credit = "Provided to YouTube by Sony Music UNDEAD · YOASOBI UNDEAD ℗ 2024";
        assert!(is_official(
            &t,
            &licensed("UNDEAD", "YOASOBI Official", credit)
        ));
        assert!(!is_official(
            &t,
            &licensed("UNDEAD (sped up)", "Release", credit)
        ));
        assert!(!is_official(&t, &upload("mimanimi", "lyrics")));
        assert!(!is_official(&t, &upload("YOASOBI fan", "")));
    }

    #[test]
    fn the_query_carries_no_search_operators() {
        let t = track(
            "Tommy heavenly6",
            "LIVING DEAD DINER GIRLS -HALLOWEEN-",
            353_274,
        );
        let query = build_query(&t);
        assert!(!query.contains('-'), "{query}");
        assert!(query.contains("HALLOWEEN"), "{query}");
        // the katakana search's artist too
        let kana = queries(&track("-M-", "Slide", 200_000));
        assert_eq!(kana[2], "ytsearch10:M スライド");

        // Runs of '!' zero YouTube search ("Nope!!!!!" → no hits; "Nope" does).
        let t = track("アイアンフリル", "Nope!!!!!", 235_000);
        let query = build_query(&t);
        assert!(!query.contains('!'), "{query}");
        assert_eq!(query, "アイアンフリル Nope");

        // Spotify "explicit" is not a YouTube search term - it zeroed lexycat's Topic hit.
        let mut t = track("lexycat", "⚜ luxury (sparkle 4 me) ˚｡⋆", 198_000);
        t.explicit = true;
        let query = build_query(&t);
        assert!(!query.to_lowercase().contains("explicit"), "{query}");
        assert!(
            score_candidate(
                &t,
                "⚜ luxury (sparkle 4 me) ˚｡⋆",
                "lexycat - Topic",
                "Provided to YouTube by release.global",
                "⚜ luxury (sparkle 4 me) ˚｡⋆",
                199_000,
            )
            .is_some()
        );
    }
}
