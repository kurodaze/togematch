//! YouTube searches through yt-dlp, flat (titles, channels and lengths, no streams).

use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use togematch::Candidate;
use tracing::{debug, error, info, warn};

pub const YTDLP_BIN: &str = "data/bin/yt-dlp";
pub const DENO_BIN: &str = "data/bin/deno";

const TIMEOUT: Duration = Duration::from_secs(90);
/// Long enough to download a new release on a slow connection.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Results kept per search target; a YouTube Music search would page on and on otherwise.
const PER_TARGET: usize = 10;
/// YouTube Music songs that count as what a search means.
const MUSIC_SONGS: usize = 3;
/// How often yt-dlp checks for a new release of itself, besides at startup. YouTube changes
/// often, and an old yt-dlp stops working.
const UPDATE_EVERY: Duration = Duration::from_secs(6 * 3600);
/// A search that fails asking for an update checks again at most this often.
const UPDATE_GAP: Duration = Duration::from_secs(600);
/// yt-dlp's own words when a newer version may fix a failure.
const OUTDATED: &str = "Confirm you are on the latest version";
/// yt-dlp's reasons it can't update itself, like a copy from a package manager.
const CANT_UPDATE: &[&str] = &[
    "Use that to update",
    "Re-download the latest release",
    "cannot be updated",
    "Use git to pull",
    "Build the executable again",
    "Unable to write to",
];
/// When yt-dlp last checked for a new release, and last installed one.
static UPDATES: Mutex<(Option<Instant>, Option<Instant>)> = Mutex::new((None, None));
/// Set once yt-dlp has said it can't update itself.
static UPDATES_OFF: AtomicBool = AtomicBool::new(false);
/// yt-dlp rewrites its cookies file when it's done, so a run starting meanwhile could read it
/// half written. Each run gets its own copy, and copies go in and out one at a time.
static COOKIES: Mutex<()> = Mutex::new(());
pub static ACTIVE_COOKIES: Mutex<Option<String>> = Mutex::new(None);
/// How long the cookies are used after YouTube asks to sign in.
const LOGGED_IN_FOR: Duration = Duration::from_secs(3600);
/// When YouTube last asked to sign in.
static ASKED: Mutex<Option<Instant>> = Mutex::new(None);

/// Cookies of a logged-in YouTube account if given: YouTube trusts
/// a logged-in session much more before asking the server to prove it isn't a bot.
pub struct Ytdlp {
    pub cookies: Option<String>,
}

impl Ytdlp {
    pub fn cookies_file(&self) -> Option<String> {
        if let Some(file) = &self.cookies {
            return Some(file.clone());
        }
        crate::find_cookies()
    }

    /// Lookups use the cookies now.
    pub fn logged_in(&self) -> bool {
        self.cookies_file().is_some()
            && ASKED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|t| t.elapsed() < LOGGED_IN_FOR)
    }
}

/// Checks for a new yt-dlp at startup and every few hours, while the server runs.
pub fn keep_updated() {
    thread::spawn(|| {
        if let Err(e) = ensure_tools() {
            error!("yt-dlp: {e}");
        }
        loop {
            update(Duration::ZERO);
            thread::sleep(UPDATE_EVERY);
        }
    });
}

/// Runs yt-dlp's own updater, unless it ran less than `gap` ago. True if a new version came
/// in, then or within `gap`. One not downloaded yet comes new on first use anyway.
fn update(gap: Duration) -> bool {
    if UPDATES_OFF.load(Ordering::Relaxed) || !Path::new(YTDLP_BIN).is_file() {
        return false;
    }
    let mut last = UPDATES.lock().unwrap_or_else(|e| e.into_inner());
    let recent = |at: Option<Instant>| at.is_some_and(|at| at.elapsed() < gap);
    if recent(last.0) {
        return recent(last.1);
    }
    last.0 = Some(Instant::now());
    let mut cmd = Command::new(YTDLP_BIN);
    let out = match finish(cmd.arg("-U"), UPDATE_TIMEOUT) {
        Ok(out) => out,
        Err(e) => {
            warn!("yt-dlp: couldn't check for a new version: {e}");
            return false;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(line) = stdout.lines().find(|l| l.starts_with("Updated yt-dlp to")) {
        info!("{line}");
        last.1 = Some(Instant::now());
        return true;
    }
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let why = stderr
            .lines()
            .rfind(|l| l.starts_with("ERROR:"))
            .unwrap_or("yt-dlp failed");
        if CANT_UPDATE.iter().any(|reason| why.contains(reason)) {
            UPDATES_OFF.store(true, Ordering::Relaxed);
            warn!(
                "yt-dlp can't update itself, so delete {YTDLP_BIN} to get a new one. It says: {why}"
            );
        } else {
            warn!("yt-dlp: couldn't update, trying again later: {why}");
        }
    }
    false
}

/// All results for these search targets (`ytsearch10:...`), in order. An error when yt-dlp
/// failed outright, so a blocked YouTube isn't remembered as "nothing found". A failure yt-dlp
/// says a newer version may fix is tried again once it has updated.
pub fn search(ytdlp: &Ytdlp, targets: &[String]) -> Result<Vec<Candidate>, String> {
    run(ytdlp, targets, true, PER_TARGET)
}

/// YouTube Music's first songs for the track, with the channel, credit and length the
/// matcher needs, which its flat rows lack.
pub fn music_details(ytdlp: &Ytdlp, track: &togematch::Track) -> Result<Vec<Candidate>, String> {
    let target = [togematch::music_search(track)];
    run(ytdlp, &target, false, togematch::MUSIC_SEARCH_RESULTS)
}

fn run(
    ytdlp: &Ytdlp,
    targets: &[String],
    flat: bool,
    per_target: usize,
) -> Result<Vec<Candidate>, String> {
    let mut updated = false;
    loop {
        let logged_in = ytdlp.logged_in();
        match run_once(ytdlp, targets, flat, per_target, logged_in) {
            Err(e)
                if !logged_in
                    && ytdlp.cookies_file().is_some()
                    && (e.contains("Sign in to confirm you") || e.contains("confirm your age")) =>
            {
                info!("YouTube asked to sign in, so lookups use the cookies for the next hour");
                *ASKED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                continue;
            }
            Err(e) if !updated && e.contains(OUTDATED) && update(UPDATE_GAP) => {
                info!("yt-dlp: trying again with its new version, after: {e}");
                updated = true;
                continue;
            }
            result => return result,
        }
    }
}

fn run_once(
    ytdlp: &Ytdlp,
    targets: &[String],
    flat: bool,
    per_target: usize,
    logged_in: bool,
) -> Result<Vec<Candidate>, String> {
    let start = Instant::now();
    debug!(
        "yt-dlp querying {} target(s) (flat={flat}, limit={per_target}): {:?}",
        targets.len(),
        targets
    );
    let js = ensure_tools()?;
    let mut command = ytdlp_cmd(js);
    let cookies_path = ytdlp.cookies_file();
    let file = cookies_path.as_deref().filter(|_| logged_in);
    if logged_in {
        let mut active = ACTIVE_COOKIES.lock().unwrap_or_else(|e| e.into_inner());
        if *active != cookies_path {
            if let Some(path) = &cookies_path {
                info!("using YouTube cookies from {path}");
            } else if active.is_some() {
                warn!("YouTube cookies file removed; continuing without cookies");
            }
            *active = cookies_path.clone();
        }
    }
    let cookies = match file {
        Some(file) => Some(Cookies::copy(file).map_err(|e| format!("cookies {file}: {e}"))?),
        None => None,
    };
    if let Some(cookies) = &cookies {
        command.arg("--cookies").arg(&cookies.copy);
    }
    if flat {
        command.arg("--flat-playlist");
    }
    command
        .args([
            "--no-warnings",
            "--dump-json",
            "--no-download",
            "--playlist-end",
        ])
        .arg(per_target.to_string())
        .arg("--")
        .args(targets);
    let out = finish(&mut command, TIMEOUT)?;
    if let Some(cookies) = &cookies {
        cookies.save();
    }
    let rows: Vec<Candidate> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(candidate)
        .collect();
    debug!(
        "yt-dlp returned {} candidate(s) in {:.2}s",
        rows.len(),
        start.elapsed().as_secs_f32()
    );
    if rows.is_empty() && !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let error = stderr
            .lines()
            .rfind(|l| l.starts_with("ERROR:"))
            .unwrap_or("yt-dlp failed");
        return Err(error.trim_start_matches("ERROR: ").to_string());
    }
    Ok(rows)
}

/// `YouTube` n-sig needs a JS runtime, so yt-dlp always runs with the vendored Deno.
fn ytdlp_cmd(js: &str) -> Command {
    let mut cmd = Command::new(YTDLP_BIN);
    cmd.args(["--js-runtimes", js]).stdin(Stdio::null());
    cmd
}

/// yt-dlp (and Deno unless one is on PATH) in data/bin, downloaded and SHA-256 verified on
/// first use (delete one to force a refresh). Returns the `--js-runtimes` spec.
fn ensure_tools() -> Result<&'static str, String> {
    static SYSTEM_DENO: LazyLock<bool> = LazyLock::new(|| {
        // A packaged Deno (the container's) wins: Deno only publishes glibc builds.
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("deno").is_file()))
    });
    static JS_RUNTIME: OnceLock<String> = OnceLock::new();
    if !Path::new(YTDLP_BIN).is_file() || (!*SYSTEM_DENO && !Path::new(DENO_BIN).is_file()) {
        let mut last = UPDATES.lock().unwrap_or_else(|e| e.into_inner());
        let arch = std::env::consts::ARCH;
        if !Path::new(YTDLP_BIN).is_file() {
            // Each build runs only on its own libc (gcompat is not enough). The system
            // ldd names it, as in detect-libc; a stray musl loader (musl-tools) does not.
            let musl = fs::read_to_string("/usr/bin/ldd").is_ok_and(|s| s.contains("musl"));
            let libc = if musl { "musllinux" } else { "linux" };
            let name = if arch == "aarch64" {
                format!("yt-dlp_{libc}_aarch64")
            } else {
                format!("yt-dlp_{libc}")
            };
            let base = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
            let sums = format!("{base}/SHA2-256SUMS");
            let bin = download_verified(&format!("{base}/{name}"), &sums, &name)?;
            install_bin(YTDLP_BIN, &bin)?;
            UPDATES_OFF.store(false, Ordering::Relaxed);
            last.0 = Some(Instant::now());
        }
        if !*SYSTEM_DENO && !Path::new(DENO_BIN).is_file() {
            let name = format!("deno-{arch}-unknown-linux-gnu.zip");
            let url = format!("https://github.com/denoland/deno/releases/latest/download/{name}");
            let zip = download_verified(&url, &format!("{url}.sha256sum"), &name)?;
            install_bin(DENO_BIN, &unzip_single(&zip)?)?;
        }
    }
    let spec = JS_RUNTIME.get_or_init(|| {
        let spec = if *SYSTEM_DENO {
            "deno".to_string()
        } else {
            let path = fs::canonicalize(DENO_BIN).unwrap_or_else(|_| PathBuf::from(DENO_BIN));
            format!("deno:{}", path.display())
        };
        info!("yt-dlp EJS runtime {spec}");
        spec
    });
    Ok(spec.as_str())
}

/// Runs a command to its end, reading both pipes meanwhile so a full one can't stall it, or
/// kills it after `timeout`.
fn finish(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run {program}: {e}"))?;
    let read = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = read(Box::new(child.stdout.take().expect("piped")));
    let stderr = read(Box::new(child.stderr.take().expect("piped")));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} timed out"));
            }
        }
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// One run's copy of the cookies file, removed when dropped.
struct Cookies<'a> {
    file: &'a str,
    copy: PathBuf,
    /// the file as copied
    read: Vec<u8>,
}

impl<'a> Cookies<'a> {
    fn copy(file: &'a str) -> io::Result<Self> {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let copy =
            std::env::temp_dir().join(format!("togematch-{}-{run}.cookies", std::process::id()));
        let read = {
            let _turn = COOKIES.lock().unwrap_or_else(PoisonError::into_inner);
            fs::read(file)?
        };
        let cookies = Cookies { file, copy, read };
        // one left behind by a run that was killed
        let _ = fs::remove_file(&cookies.copy);
        // only for us: it holds a logged-in session
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&cookies.copy)?.write_all(&cookies.read)?;
        Ok(cookies)
    }

    /// Hands what yt-dlp learned back to the file, unless it was replaced meanwhile, by hand or
    /// by another run: that copy is as new.
    fn save(&self) {
        let Ok(learned) = fs::read(&self.copy) else {
            return;
        };
        if learned == self.read {
            return;
        }
        let _turn = COOKIES.lock().unwrap_or_else(PoisonError::into_inner);
        if fs::read(self.file).is_ok_and(|now| now == self.read)
            && let Err(e) = fs::write(self.file, learned)
        {
            warn!("cookies: couldn't save {}: {e}", self.file);
        }
    }
}

impl Drop for Cookies<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.copy);
    }
}

/// The songs YouTube Music lists first for a search, as video ids. It lists only official
/// uploads, and knows their names in the original script where YouTube's search shows them
/// romanised.
pub fn music_songs(ytdlp: &Ytdlp, text: &str) -> Result<Vec<String>, String> {
    let songs = run(ytdlp, &[togematch::music_url(text)], true, MUSIC_SONGS)?;
    Ok(songs.into_iter().map(|c| c.id).collect())
}

/// The video id in a YouTube link (watch, youtu.be, music, shorts), or a bare id.
pub fn video_id(link: &str) -> Option<String> {
    let link = link.trim();
    let id = if let Some((_, query)) = link.split_once("watch?") {
        query.split('&').find_map(|p| p.strip_prefix("v="))?
    } else if let Some((_, rest)) = link
        .split_once("youtu.be/")
        .or_else(|| link.split_once("/shorts/"))
    {
        rest.split(['?', '&', '/']).next()?
    } else {
        link
    };
    let valid = id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then(|| id.to_string())
}

/// A video's title and channel from YouTube's oEmbed endpoint: one light request, no search,
/// and an error if the video doesn't exist.
pub fn about(http: &ureq::Agent, id: &str) -> Result<(String, String), String> {
    let body = http
        .get("https://www.youtube.com/oembed")
        .query("url", link(id))
        .query("format", "json")
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| format!("YouTube doesn't know video {id}: {e}"))?;
    let v: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    Ok((text("title"), text("author_name")))
}

pub fn link(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// Write an executable via a temp file, so an interrupted install never looks complete.
fn install_bin(path: &str, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = Path::new(path).with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename to {path}: {e}"))?;
    Ok(())
}

/// The single deflated entry of a Deno release zip (integrity is the zip's SHA-256).
fn unzip_single(zip: &[u8]) -> Result<Vec<u8>, String> {
    let le16 = |i: usize| {
        zip.get(i..i + 2)
            .map_or(0, |b| usize::from(b[0]) | usize::from(b[1]) << 8)
    };
    if !zip.starts_with(b"PK\x03\x04") || le16(8) != 8 {
        return Err("Deno release is not a deflated zip".to_string());
    }
    // Local header: 30 fixed bytes, then the file name and extra field.
    let data = zip.get(30 + le16(26) + le16(28)..).unwrap_or_default();
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| format!("inflate Deno: {e}"))?;
    Ok(out)
}

/// Fetch a release asset and check it against the published SHA-256.
fn download_verified(url: &str, sums_url: &str, name: &str) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};
    info!("downloading {url}");
    fs::create_dir_all("data/bin").map_err(|e| format!("create data/bin: {e}"))?;
    // Tens of MB: a slow link needs longer than default timeouts.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .into();
    let get = |u: &str| -> Result<Vec<u8>, String> {
        let mut resp = agent.get(u).call().map_err(|e| format!("GET {u}: {e}"))?;
        let mut buf = Vec::new();
        resp.body_mut()
            .as_reader()
            .read_to_end(&mut buf)
            .map_err(|e| format!("read {u}: {e}"))?;
        Ok(buf)
    };
    let sums = get(sums_url)?;
    let sums_text = String::from_utf8_lossy(&sums);
    let expected =
        parse_sha256sums(&sums_text, name).ok_or_else(|| format!("checksum missing for {name}"))?;
    let bytes = get(url)?;
    let actual = hex(&Sha256::digest(&bytes));
    if actual != expected {
        return Err(format!(
            "{name} checksum mismatch (expected {expected}, got {actual})"
        ));
    }
    Ok(bytes)
}

/// Hash for `filename` from a sums file: GNU `hash  name` lines (name may carry a path or
/// a `*` binary marker), or a single-asset file holding just the hash.
fn parse_sha256sums(text: &str, filename: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let named = parts.next().map(|n| n.trim_start_matches('*'));
        let ok = hash.len() == 64
            && hash.bytes().all(|b| b.is_ascii_hexdigit())
            && named.is_none_or(|n| n == filename || n.ends_with(&format!("/{filename}")));
        ok.then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn candidate(line: &str) -> Option<Candidate> {
    let v: Value = serde_json::from_str(line).ok()?;
    let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let id = text("id");
    let uploader = Some(text("uploader"))
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| text("channel"));
    (!id.is_empty()).then(|| Candidate {
        id,
        title: text("title"),
        uploader,
        description: text("description"),
        album: text("album"),
        duration: (v["duration"].as_f64().unwrap_or(0.0) * 1000.0) as i64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stuck_command_is_killed() {
        let started = Instant::now();
        let stuck = finish(Command::new("sleep").arg("10"), Duration::from_millis(100));
        assert!(stuck.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        let out = finish(Command::new("echo").arg("hi"), TIMEOUT).unwrap();
        assert_eq!(out.stdout, b"hi\n");
    }

    #[test]
    fn cookies_go_back_unless_replaced() {
        let file = std::env::temp_dir().join(format!("togematch-test-{}.txt", std::process::id()));
        let name = file.to_str().unwrap();
        fs::write(&file, "exported").unwrap();
        let run = Cookies::copy(name).unwrap();
        fs::write(&run.copy, "learned").unwrap();
        run.save();
        assert_eq!(fs::read_to_string(&file).unwrap(), "learned");

        let run = Cookies::copy(name).unwrap();
        fs::write(&run.copy, "stale").unwrap();
        fs::write(&file, "exported again").unwrap();
        run.save();
        assert_eq!(fs::read_to_string(&file).unwrap(), "exported again");
        let copy = run.copy.clone();
        drop(run);
        assert!(!copy.exists());
        fs::remove_file(&file).unwrap();
    }

    #[test]
    fn unzip_single_inflates_the_only_entry() {
        use std::io::Write;
        let mut deflate = flate2::write::DeflateEncoder::new(Vec::new(), Default::default());
        deflate.write_all(b"deno binary").unwrap();
        let mut zip = b"PK\x03\x04\x14\0\0\0\x08\0".to_vec(); // version, flags, deflate
        zip.resize(26, 0);
        zip.extend(b"\x04\0\0\0deno"); // name and extra lengths, name
        zip.extend(deflate.finish().unwrap());
        assert_eq!(unzip_single(&zip).unwrap(), b"deno binary");
        assert!(unzip_single(b"not a zip").is_err());
    }

    #[test]
    fn parse_sha256sums_gnu_format() {
        let text = "\
6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae  yt-dlp_linux
b6ce97646773070d7a7ffd6bbbdcaecb47c48483909c54c915bf08a7a9b5e0b1  yt-dlp_linux_aarch64
";
        assert_eq!(
            parse_sha256sums(text, "yt-dlp_linux").as_deref(),
            Some("6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae")
        );
        assert!(parse_sha256sums(text, "yt-dlp.exe").is_none());
        assert!(parse_sha256sums("not-a-hash  yt-dlp_linux", "yt-dlp_linux").is_none());
        // Deno publishes one hash per asset, sometimes bare.
        let bare = "6BBB3D314CDE4FEBE36E5FA1D55462E29C974F63444E707871834F6D8CC210AE\n";
        assert_eq!(
            parse_sha256sums(bare, "deno.zip").as_deref(),
            Some("6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae")
        );
    }
}
