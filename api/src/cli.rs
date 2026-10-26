//! Looking at and fixing stored answers by hand. Safe next to a running server: SQLite lets
//! both use the database. A fix applies to the recording, so to every way of asking for it.

use crate::app::{App, Failure};
use crate::cache::Entry;
use crate::youtube;
use std::io::{IsTerminal, Write};
use togematch::Track;

/// Tracks a search is offered as, when it fits several.
const CHOICES: usize = 9;

pub const USAGE: &str =
    "togematch-api get <Spotify link or search>                what's stored for it
togematch-api set <Spotify link or search> <YouTube link>  fix a match by hand
togematch-api forget <Spotify link or search>             drop it, so it's matched again
togematch-api list [match|unofficial|youtube|manual|none]  every recording stored, or one kind";

pub fn run(app: &App, words: &[String]) -> Result<(), String> {
    let asked = |q: &str| {
        app.asked(q).map_err(|f| match f {
            Failure::Bad(e) | Failure::Upstream(e) => e,
        })
    };
    match words {
        [cmd, q] if cmd == "get" => {
            let asked = asked(q)?;
            let recording = app.recording_of(&asked);
            let via = if recording == asked {
                String::new()
            } else {
                format!("{asked} -> ")
            };
            match app.cache.get(&recording) {
                Some(entry) => println!("{via}{}", line(&recording, &entry)),
                None => println!("{via}{recording}: nothing stored (or it expired)"),
            }
        }
        [cmd, q, video] if cmd == "set" => {
            let asked = asked(q)?;
            let id = youtube::video_id(video).ok_or("that's not a YouTube link")?;
            // also checks the video exists
            let (title, channel) = youtube::about(&app.http, &id)?;
            let (recording, track) = if asked.starts_with("text:") {
                pick(app, &asked, q)?
            } else {
                app.find_recording(&asked)?
            };
            let entry = match track {
                Some(track) => crate::app::entry(&track, Some(&id), "manual"),
                None => match app.cache.get(&recording) {
                    // keep what's known about the track, change only the video
                    Some(old) => Entry {
                        video: Some(id),
                        source: "manual",
                        ..old
                    },
                    None => Entry {
                        video: Some(id),
                        title,
                        artist: channel,
                        duration: 0,
                        source: "manual",
                    },
                },
            };
            app.cache.put(&recording, &entry);
            if recording != asked {
                app.cache.pin_alias(&asked, &recording);
            }
            println!("{}", line(&recording, &entry));
        }
        [cmd, q] if cmd == "forget" => {
            let recording = app.recording_of(&asked(q)?);
            let forgotten = app.cache.forget(&recording).map_err(|e| e.to_string())?;
            println!(
                "{recording}: {}",
                if forgotten {
                    "forgotten"
                } else {
                    "nothing was stored"
                }
            );
        }
        [cmd, rest @ ..] if cmd == "list" && rest.len() <= 1 => {
            let source = rest.first().map(String::as_str);
            if source
                .is_some_and(|s| !["match", "unofficial", "youtube", "manual", "none"].contains(&s))
            {
                return Err(USAGE.into());
            }
            for (key, entry) in app.cache.list(source).map_err(|e| e.to_string())? {
                println!("{}", line(&key, &entry));
            }
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// The track a search means, asked in a terminal when it fits several. Elsewhere nobody can
/// answer, so it stops with the list.
fn pick(app: &App, asked: &str, text: &str) -> Result<(String, Option<Track>), String> {
    let mut fits = app.candidates(text)?;
    if fits.len() < 2 {
        return Ok(match fits.pop() {
            Some(only) => (only.recording, Some(only.track)),
            None => (asked.to_string(), None),
        });
    }
    fits.truncate(CHOICES);
    let now = app.cache.alias(asked);
    let default = fits
        .iter()
        .position(|m| Some(&m.recording) == now.as_ref())
        .unwrap_or(0);
    let list: Vec<String> = fits
        .iter()
        .enumerate()
        .map(|(i, m)| format!("  {}. {}", i + 1, describe(&m.track)))
        .collect();
    let list = list.join("\n");
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "\"{text}\" fits several tracks; use a Spotify link or a more specific search:\n{list}"
        ));
    }
    println!("\"{text}\" fits several tracks:\n{list}");
    print!("Which one? [{}] ", default + 1);
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    let chosen = choose(&answer, fits.len(), default).ok_or("no such number, nothing changed")?;
    let m = fits.swap_remove(chosen);
    Ok((m.recording, Some(m.track)))
}

/// `artist - title (album, m:ss)`
fn describe(track: &Track) -> String {
    let secs = track.duration / 1000;
    let length = format!("{}:{:02}", secs / 60, secs % 60);
    if track.album.is_empty() {
        format!("{} - {} ({length})", track.artist, track.name)
    } else {
        format!(
            "{} - {} ({}, {length})",
            track.artist, track.name, track.album
        )
    }
}

/// The number typed, counted from 1, or the default for just Enter.
fn choose(answer: &str, count: usize, default: usize) -> Option<usize> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Some(default);
    }
    let n: usize = answer.parse().ok()?;
    (1..=count).contains(&n).then(|| n - 1)
}

/// `key  source  link  artist - title`
fn line(key: &str, entry: &Entry) -> String {
    let link = entry.video.as_deref().map_or("-".into(), youtube::link);
    format!(
        "{key}\t{}\t{link}\t{} - {}",
        entry.source, entry.artist, entry.title
    )
}
