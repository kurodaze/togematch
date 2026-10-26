//! togematch-api: an HTTP API that turns Spotify links and track searches into YouTube links,
//! remembering every answer in SQLite so each track is searched for once.

mod app;
mod cache;
mod cli;
mod deezer;
mod http;
mod spotify;
mod youtube;

use app::{Answer, App, Failure};
use serde_json::{Value, json};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

/// Requests answered at once; each waits on yt-dlp most of the time.
const WORKERS: usize = 4;

const USAGE: &str =
    "togematch-api [--port 7094] [--cookies data/cookies.txt] [--db data/togematch.db]
    runs the API: GET /match?q=<Spotify link or search> answers with the tracks and their
    YouTube links, remembering every answer in the database";

struct Args {
    port: u16,
    /// cookies.txt of a logged-in YouTube account
    cookies: Option<String>,
    db: String,
    /// a command for the CLI, if any
    words: Vec<String>,
}

fn args() -> Result<Args, String> {
    let mut args = Args {
        port: 7094,
        cookies: None,
        db: "data/togematch.db".into(),
        words: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--port" => args.port = value()?.parse().map_err(|_| "--port must be a number")?,
            "--cookies" => args.cookies = Some(value()?),
            "--db" => args.db = value()?,
            "-h" | "--help" => {
                println!("{USAGE}\n\n{}", cli::USAGE);
                std::process::exit(0);
            }
            _ if arg.starts_with("--") => {
                return Err(format!("unknown option {arg}\n\n{USAGE}\n\n{}", cli::USAGE));
            }
            _ => args.words.push(arg),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let cache = match cache::Cache::open(&args.db) {
        Ok(cache) => cache,
        Err(e) => {
            eprintln!("{}: {e}", args.db);
            return ExitCode::FAILURE;
        }
    };
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build();
    let cookies = match args.cookies {
        Some(path) => {
            if !std::path::Path::new(&path).is_file() {
                eprintln!("--cookies: no file at {path}");
                return ExitCode::FAILURE;
            }
            Some(path)
        }
        None => None,
    };
    let ytdlp = youtube::Ytdlp { cookies };
    let app = App {
        cache,
        ytdlp,
        http: config.into(),
        running: Mutex::default(),
    };
    if !args.words.is_empty() {
        return match cli::run(&app, &args.words) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    let default_filter = if cfg!(debug_assertions) {
        "togematch_api=debug,togematch=debug,info"
    } else {
        "info"
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    let listener = match TcpListener::bind(("0.0.0.0", args.port)) {
        Ok(listener) => Arc::new(listener),
        Err(e) => {
            error!("can't listen on port {}: {e}", args.port);
            return ExitCode::FAILURE;
        }
    };
    let app = Arc::new(app);
    youtube::keep_updated();
    info!(
        "togematch-api: listening on port {} for the local network",
        args.port
    );
    if let Some(path) = app.ytdlp.cookies_file() {
        info!("YouTube cookies found at {path} (used when challenged)");
    } else {
        debug!("no YouTube cookies found in data/");
    }
    // each worker takes the next connection itself, so no more than WORKERS are read at once
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (listener, app) = (listener.clone(), app.clone());
            thread::spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => handle(&app, &stream),
                        // out of file descriptors, say: wait for some to close
                        Err(_) => thread::sleep(Duration::from_millis(100)),
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    ExitCode::SUCCESS
}

fn handle(app: &App, stream: &TcpStream) {
    // every search runs from this connection, so only the local network may ask - checked
    // before the request is read, so a stranger's bytes are never parsed or answered
    if !stream.peer_addr().is_ok_and(|a| is_local(a.ip())) {
        let body = json!({ "error": "only for the local network" });
        respond(stream, 403, &[], &body);
        return;
    }
    let started = Instant::now();
    let request = match http::read(stream) {
        Ok(request) => request,
        Err(Some(status)) => {
            let body = json!({ "error": "that's not a request this API reads" });
            respond(stream, status, &[], &body);
            return;
        }
        Err(None) => return,
    };
    if request.method != "GET" {
        respond(
            stream,
            405,
            &[("Allow", "GET")],
            &json!({ "error": "only GET" }),
        );
        return;
    }
    let url = request.target;
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let q = param(query, "q").filter(|_| path == "/match");
    let (status, body) = match q.as_deref().map(str::trim) {
        None => (
            404,
            json!({ "error": "ask GET /match?q=<Spotify link or search>" }),
        ),
        Some("") => (400, json!({ "error": "q is empty" })),
        Some(q) => {
            debug!("GET /match?q={q:?}");
            match app.answer(q) {
                Ok(answer) => (200, to_json(&answer)),
                Err(Failure::Bad(e)) => {
                    debug!("400 for {q:?}: {e}");
                    (400, json!({ "error": e }))
                }
                Err(Failure::Upstream(e)) => {
                    debug!("502 for {q:?}: {e}");
                    (502, json!({ "error": e }))
                }
            }
        }
    };
    let summary = match body["tracks"].as_array() {
        Some(tracks) if tracks.len() == 1 => {
            format!("{} {}", tracks[0]["source"], tracks[0]["youtube"])
        }
        Some(tracks) => format!("{} tracks", tracks.len()),
        None => body["error"].to_string(),
    };
    info!(
        "{status} {:?} -> {summary} in {:.1}s",
        q.unwrap_or_default(),
        started.elapsed().as_secs_f32()
    );
    respond(stream, status, &[], &body);
}

fn respond(stream: &TcpStream, status: u16, headers: &[(&str, &str)], body: &Value) {
    http::respond(stream, status, headers, &body.to_string());
}

fn to_json(answer: &Answer) -> Value {
    let tracks: Vec<Value> = answer
        .tracks
        .iter()
        .map(|f| {
            json!({
                "title": f.entry.title,
                "artist": f.entry.artist,
                "duration": f.entry.duration,
                "youtube": f.entry.video.as_deref().map(youtube::link),
                "source": f.entry.source,
                "cached": f.cached,
                "query": f.query,
            })
        })
        .collect();
    json!({ "playlist": answer.playlist, "tracks": tracks })
}

/// Loopback, private (10/8, 172.16/12, 192.168/16), link-local, or IPv6 unique local.
fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_local(IpAddr::V4(v4)),
            None => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
        },
    }
}

/// A query string parameter, percent-decoded.
fn param(query: &str, name: &str) -> Option<String> {
    let raw = query
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))?;
    let mut bytes = Vec::with_capacity(raw.len());
    let mut rest = raw.as_bytes();
    while let Some(&b) = rest.first() {
        if b == b'%'
            && let Some([h, l]) = rest.get(1..3)
            && let (Some(h), Some(l)) = (char::from(*h).to_digit(16), char::from(*l).to_digit(16))
        {
            bytes.push((h * 16 + l) as u8);
            rest = &rest[3..];
        } else {
            bytes.push(if b == b'+' { b' ' } else { b });
            rest = &rest[1..];
        }
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Finds a cookies file in `data/`, checking standard names first then any `*cookie*.txt` file.
pub fn find_cookies() -> Option<String> {
    for path in ["data/cookies.txt", "data/youtube-cookies.txt"] {
        if std::path::Path::new(path).is_file() {
            return Some(path.into());
        }
    }
    let entries = std::fs::read_dir("data").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            let lower = name.to_lowercase();
            if lower.contains("cookie") && lower.ends_with(".txt") {
                return path.to_str().map(String::from);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_network_only() {
        for local in [
            "127.0.0.1",
            "192.168.1.20",
            "10.1.2.3",
            "172.16.0.9",
            "fd00::1",
            "::1",
            "::ffff:192.168.1.5",
        ] {
            assert!(is_local(local.parse().unwrap()), "{local}");
        }
        for public in [
            "8.8.8.8",
            "172.32.0.1",
            "100.64.0.1",
            "2001:db8::1",
            "::ffff:1.1.1.1",
        ] {
            assert!(!is_local(public.parse().unwrap()), "{public}");
        }
    }
}
