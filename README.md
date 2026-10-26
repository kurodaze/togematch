# togematch <sub><sup>(ˈtōɡāˌmaCH)</sup></sub>

Finds a track's official audio upload on YouTube, the one marked "Provided to YouTube", from a Spotify link or a search such as "basshunter dota". Live versions, covers and karaoke are rejected, and music videos, lyric videos and re-uploads are only used when no official upload turns up, since they can add intros and sound effects, use a different mix, or have worse audio.

Two crates:

- `togematch`: the matcher, a Rust library with no dependencies that makes no network calls or file access.
- `togematch-api`: an HTTP API and CLI around the matcher. Matches are stored, so each track is searched for once, and can be corrected by hand.

Used by [musicart](https://github.com/kurodaze/musicart) and [togewire](https://github.com/kurodaze/togewire). The matcher originated in togewire.

## How it matches

- **Spotify links**: the track's title, artists, length and explicit flag come from Spotify's public embed page (no API keys). Albums and playlists list their tracks, each matched when asked for on its own.
- **Searches**: Deezer's public search turns the text into a track, the most popular one containing every word you typed, with one typo allowed in a longer word. "r906 no nein disco" becomes r-906's No Nein Disco (feat. IA AI). Symbols typed on their own must be there as they are ("bring me the horizon ¿" is the track ¿), and also in YouTube's top result, since YouTube's search ignores them. A live version, remix or instrumental is picked only when typed or when nothing else fits, and a karaoke or cover release never is. Deezer lists many Japanese releases under romanised names (結束バンド is kessoku band), so typed words it lacks are confirmed by the upload's own names or by YouTube Music listing the same upload. Text that isn't a track ("funny cat video") gets YouTube's top result, as if typed into YouTube.
- **Matching**: one flat yt-dlp search with a few queries: plain, restricted to licensed uploads, and in katakana for romanised Japanese titles. Every result is scored on title words, version words (live, remix, karaoke, instrumental...), whether the artist's channel or a licensed credit is behind it, and how close its length is. A result that's a different track or version is rejected outright. When that finds no match, YouTube Music's song search is tried too, since it reaches official uploads a signed-out YouTube search sometimes hides.
- **Remembering**: matches are stored per recording, by ISRC when Deezer knows the track. The Spotify link for No Nein Disco, "r-906 no nein disco" and "no nein disco" are the same recording, so they share one YouTube search.

## API

Needs a residential internet connection, or [YouTube cookies](#are-you-a-bot) (required for VPS/datacenter IPs, where YouTube blocks signed-out requests).

### 1. Run via Podman / Docker (Recommended)

The image has deno, fetches yt-dlp into its data volume on first use, and keeps its database there too.

```bash
podman compose up -d   # with compose.yaml from this repo
# or: docker compose up -d
```

Or run directly:
```bash
# Podman
podman run -d --name togematch-api --restart unless-stopped --init -p 7094:7094 -v togematch:/app/data ghcr.io/kurodaze/togematch:latest

# Docker
docker run -d --name togematch-api --restart unless-stopped --init -p 7094:7094 -v togematch:/app/data ghcr.io/kurodaze/togematch:latest
```

`compose.dev.yaml` builds the image from the checkout instead, with the faster-to-build dev profile.

To give the container cookies, stream them into the data volume:
```bash
docker exec -i togematch-api sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
# or with Podman:
podman exec -i togematch-api sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
```
The API automatically detects `cookies.txt` (or any `*cookie*.txt`) in the data volume.

### 2. Or Install Native Binary

Download a static `togematch-api` binary (`-x86_64` / `-aarch64`) from [GitHub Releases](https://github.com/kurodaze/togematch/releases) and check it with `sha256sum -c --ignore-missing SHA256SUMS`, or build it from source:

```bash
# Runtime: yt-dlp, and deno unless one is on PATH, are downloaded into data/bin/ on
# first use; yt-dlp is kept current. Building needs Rust.
git clone https://github.com/kurodaze/togematch.git
cd togematch
./scripts/build.sh --release   # a static ./togematch-api, see --help
./togematch-api --port 7094
```

To run it as a systemd user service, with the checkout at `~/togematch`:

```sh
systemctl --user link ~/togematch/togematch-api.service
loginctl enable-linger "$USER"   # keeps it running after logout and starts it at boot
systemctl --user enable --now togematch-api
journalctl --user -u togematch-api -f
```

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | `7094` | HTTP port to listen on |
| `--cookies` | `data/cookies.txt` (if present) | Cookies file of a spare YouTube account |
| `--db` | `data/togematch.db` | SQLite database file |

The API keeps [yt-dlp](https://github.com/yt-dlp/yt-dlp/releases) current: it downloads the official release and Deno into `data/bin/` on first use, checks for a new version at startup and every 6 hours, and retries right away when yt-dlp says a newer version may fix a failure.

It answers only the local network (private, loopback and link-local addresses) and refuses everything else with 403, since every search runs from your connection. Port forwarding that hides the caller's address makes every request look local, as rootless Podman's does by default and Docker's does for IPv6 callers, so don't open the port beyond your network.

`GET /match?q=<Spotify link or search>`:

```sh
curl 'http://localhost:7094/match?q=https://open.spotify.com/track/2Ga4vY0qYw5wg7GCU3CNs2'
```

```json
{
  "playlist": null,
  "tracks": [
    {
      "title": "No Nein Disco (feat. IA AI)",
      "artist": "r-906, IA AI",
      "duration": 218571,
      "youtube": "https://www.youtube.com/watch?v=HxvTaCequys",
      "source": "match",
      "cached": false,
      "query": "spotify:track:2Ga4vY0qYw5wg7GCU3CNs2"
    }
  ]
}
```

That's the licensed upload, "Provided to YouTube by TuneCore Japan", titled ノウナイディスコ on YouTube.

| `source` | Meaning |
| --- | --- |
| `match` | The matcher's pick, an official upload |
| `unofficial` | The matcher's pick, the recording uploaded by someone else, because no official upload was found |
| `manual` | Set by hand with the CLI |
| `youtube` | No match, so YouTube's top result, as it's the artist's and the same length |
| `none` | Nothing found |
| `pending` | A track of an album or playlist not looked up yet: ask for its `query` (`spotify:track:<id>`) on its own |

`duration` is in milliseconds. Errors come as `{"error": "..."}`, with 400 for a bad question and 502 when Spotify, Deezer or YouTube failed. A first answer takes 2 to 6 seconds, a remembered one well under one.

If YouTube asks "Sign in to confirm you're not a bot", pass the cookies of a spare YouTube account with `--cookies`; see [Are you a bot?](#are-you-a-bot) below.

## CLI

Works on the same database, also while the API runs.

```sh
togematch-api get "no nein disco"                          # what's stored, and which recording the question is
togematch-api set <Spotify link or search> <YouTube link>  # fix a match by hand
togematch-api forget <Spotify link or search>              # drop it, so it's matched again
togematch-api list [match|unofficial|youtube|manual|none]  # every recording stored, or one kind
```

A fix applies to the recording, so to every way of asking for it. When a search fits several tracks, `set` lists them and asks which one, and the search keeps meaning the chosen one; without a terminal to ask in, it stops with the list. Matches and fixes are kept forever, unofficial picks for a day (a search can leave the official upload out), YouTube's top results for 30 days, and "nothing found" for 10 minutes, since an empty search is more often YouTube refusing for a while. What a search means (which recording) is checked again after 30 days, unless chosen with `set`.

## Library

```toml
togematch = "0.1"
```

```rust
let track = togematch::Track {
    name: "No Nein Disco (feat. IA AI)".into(),
    artist: "r-906, IA AI".into(),
    duration: 218_571,
    ..Default::default()
};
// run every query in one `yt-dlp --flat-playlist --dump-json` call, turn the rows into Candidates
let queries = togematch::queries(&track);
let best = togematch::best(&track, &candidates);
// no match: run togematch::music_search(&track) without --flat-playlist, then best again
```

For a typed search, `togematch::searched` picks the catalogue tracks it means from a search's results, best first.

## Are you a bot?

YouTube blocks signed-out requests from datacenter/VPS addresses and sometimes answers "Sign in to confirm you're not a bot" on busy residential connections. A logged-in session is trusted much more, so give yt-dlp the cookies of a spare YouTube account (YouTube can flag accounts used like this):

1. In a private/incognito window, log into YouTube and go to `https://www.youtube.com/robots.txt` in the same tab.
2. Export the youtube.com cookies in Netscape cookies.txt format, e.g. with the "Get cookies.txt LOCALLY" extension (allow it in private windows), and save it into the `data/` folder (e.g. `data/cookies.txt` or `data/www.youtube.com_cookies.txt`).
3. Close the private window without logging out, or the cookies stop working.
4. togematch-api automatically finds and loads cookie files in `data/` (including when added or replaced while running) and logs `using YouTube cookies from ...`. In a container, stream them into the data volume:
   ```bash
   docker exec -i togematch-api sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
   # or with Podman:
   podman exec -i togematch-api sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
   ```
   Or pass `--cookies <path>` when running the native binary.

The API still looks things up logged out, which is faster and keeps the account safe, and only uses the cookies for an hour after YouTube asks to sign in.

Cookies don't last forever. When YouTube stops accepting them, or the file is removed, togematch-api carries on without cookies and logs a warning. Export new ones the same way; replacing the file is enough, no restart needed.

## Disclaimer & Acknowledgements

togematch is **not affiliated with Spotify, Deezer or YouTube**.

togematch-api uses [yt-dlp](https://github.com/yt-dlp/yt-dlp) with [Deno](https://deno.com/) to search YouTube and YouTube Music, [Deezer's public API](https://developers.deezer.com/api) to tell which track a search means, and [SQLite](https://sqlite.org/) to store its matches. The library itself has no dependencies.

## License

Copyright (C) 2026 KuroDaze. Licensed under the GNU General Public License v3.0 or later, see [LICENSE](LICENSE).