//! Scores yt-dlp flat search rows for one track, to see why a match won or lost.
//! yt-dlp --flat-playlist --dump-json "ytsearch10:..." | cargo run --example score -- "<artist>" "<title>" <ms> [album]

use std::io::BufRead;
use togematch::{Candidate, Track, score};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let track = Track {
        artist: a[0].clone(),
        name: a[1].clone(),
        duration: a[2].parse().unwrap(),
        album: a.get(3).cloned().unwrap_or_default(),
        explicit: false,
    };
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let c = Candidate {
            id: text("id"),
            title: text("title"),
            uploader: text("uploader"),
            description: text("description"),
            album: text("album"),
            duration: (v["duration"].as_f64().unwrap_or(0.0) * 1000.0) as i64,
        };
        let s = score(&track, &c).map_or("rejected".into(), |s| s.to_string());
        let desc: String = c.description.chars().take(80).collect();
        println!(
            "{s:>9}  {}  {} | {} | {}s | {}",
            c.id,
            c.title,
            c.uploader,
            c.duration / 1000,
            desc.replace('\n', "⏎")
        );
    }
}
