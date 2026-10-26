//! Tracks behind a Spotify link, from its public embed page: no API keys, playlists stop at 100.

use serde_json::Value;
use togematch::Track;
use ureq::{Agent, ResponseExt};

pub struct Link {
    /// "track", "album" or "playlist"
    pub kind: String,
    pub id: String,
}

pub struct Listing {
    /// album or playlist name
    pub name: Option<String>,
    /// Spotify track id and what Spotify says about it
    pub tracks: Vec<(String, Track)>,
}

/// `open.spotify.com/...`, `spotify:` URIs, and spotify.link short links (followed).
pub fn link(http: &Agent, query: &str) -> Option<Link> {
    if let Some(uri) = query.strip_prefix("spotify:") {
        let (kind, id) = uri.split_once(':')?;
        return Some(Link {
            kind: kind.into(),
            id: id.into(),
        })
        .filter(|l| is_kind(&l.kind) && is_id(&l.id));
    }
    let mut url: ureq::http::Uri = query.parse().ok()?;
    if url.host() == Some("spotify.link") {
        url = http.get(query).call().ok()?.get_uri().clone();
    }
    if url.host() != Some("open.spotify.com") {
        return None;
    }
    // "/intl-de/track/<id>", "/track/<id>"
    let mut parts = url
        .path()
        .split('/')
        .filter(|p| !p.is_empty() && !p.starts_with("intl-"));
    let (kind, id) = (parts.next()?, parts.next()?);
    (is_kind(kind) && is_id(id)).then(|| Link {
        kind: kind.into(),
        id: id.into(),
    })
}

fn is_kind(kind: &str) -> bool {
    matches!(kind, "track" | "album" | "playlist")
}

/// 22 letters and digits.
fn is_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub fn fetch(http: &Agent, link: &Link) -> Result<Listing, String> {
    let page = http
        .get(format!(
            "https://open.spotify.com/embed/{}/{}",
            link.kind, link.id
        ))
        .header("User-Agent", "Mozilla/5.0")
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| format!("Spotify didn't answer: {e}"))?;
    let entity = entity(&page).ok_or("couldn't read that Spotify page")?;
    let unreadable = || "couldn't read that Spotify page".to_string();
    if link.kind == "track" {
        let artists: Vec<&str> = entity["artists"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["name"].as_str())
            .collect();
        let track = Track {
            name: entity["name"].as_str().ok_or_else(unreadable)?.into(),
            artist: artists.join(", "),
            album: String::new(),
            duration: entity["duration"].as_i64().unwrap_or(0),
            explicit: entity["isExplicit"].as_bool().unwrap_or(false),
        };
        return Ok(Listing {
            name: None,
            tracks: vec![(link.id.clone(), track)],
        });
    }
    let name = entity["name"].as_str().unwrap_or_default().to_string();
    // album tracks share the album's name; playlist entries don't say
    let album = if link.kind == "album" {
        name.clone()
    } else {
        String::new()
    };
    let tracks = entity["trackList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let id = t["uri"].as_str()?.strip_prefix("spotify:track:")?;
            let track = Track {
                name: t["title"].as_str()?.into(),
                // "A,\u{a0}B"
                artist: t["subtitle"].as_str()?.replace('\u{a0}', " "),
                album: album.clone(),
                duration: t["duration"].as_i64().unwrap_or(0),
                explicit: t["isExplicit"].as_bool().unwrap_or(false),
            };
            Some((id.to_string(), track))
        })
        .collect();
    Ok(Listing {
        name: Some(name),
        tracks,
    })
}

/// The JSON the embed page renders from.
fn entity(page: &str) -> Option<Value> {
    let start = page.find(r#"<script id="__NEXT_DATA__" type="application/json">"#)?;
    let json = &page[start..];
    let json = &json[json.find('>')? + 1..json.find("</script>")?];
    let mut data: Value = serde_json::from_str(json).ok()?;
    Some(data["props"]["pageProps"]["state"]["data"]["entity"].take()).filter(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_page() {
        let page = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"state":{"data":{"entity":{"name":"Track"}}}}}}</script>"#;
        assert_eq!(entity(page).unwrap()["name"], "Track");
        assert!(entity("<html></html>").is_none());
    }
}
