# ytdl-web

A small self-hosted web frontend for [yt-dlp](https://github.com/yt-dlp/yt-dlp).
Paste a link, pick a quality, and the file downloads to your browser.

It is a single ~3 MB binary that uses about 5 MB of memory. The page,
script and styles are built in. It runs the `yt-dlp` program for the real
work, so there is no Python environment to maintain beyond yt-dlp itself.

Built for one person on a home network. There is no login; see
[Security](#security).

## Features

- **Paste and go.** Pasting a link looks it up right away, and phones get
  a Paste button. Installed as an app on Android, it appears in the share
  menu, so you can send a link straight from the YouTube app. Opening
  `…/?url=<link>` works too, e.g. from an iOS Shortcut.
- **A short quality list.** Video and Audio tabs show one row per
  resolution, each with its approximate size, using the format most
  devices can play (H.264, then AV1, then VP9; SDR before HDR). "Show all
  formats" lists every container, codec and HDR variant. Your last choice
  is remembered.
- **Sensible picks.** When several streams look the same, the best one is
  kept: a direct download over HLS fragments, the original audio over
  YouTube's compressed "DRC" copy, then the highest bitrate. Video-only
  streams are paired with audio in the same container (M4A for MP4, Opus
  for WebM), so an MP4 stays an MP4.
- **Live progress** (percent, size, speed, time left, video or audio
  part) inside the video card, pushed to every open tab and shown in the
  tab title. Reloading mid-download picks it back up.
- **Cancel** stops yt-dlp *and* the ffmpeg it started, and removes the
  partial files.
- **Supported sites** opens a searchable list of every site the installed
  yt-dlp handles, grouped per site.
- **Plain error messages** for the common failures (unsupported site,
  private or age-restricted video, 404, bot checks).
- **One download at a time.** The finished file streams to the browser
  without being buffered and is deleted once it has been sent completely.
  If the transfer breaks off, "Save file" can try again. Files nobody
  fetches are deleted after `YTDL_KEEP_MINUTES`.
- **Check for update** runs `yt-dlp --update`. The new version is used
  for the next request; no restart needed.

The share menu entry needs the page served over HTTPS with a certificate
the phone trusts, and the app added to the home screen.

## Requirements

- [yt-dlp](https://github.com/yt-dlp/yt-dlp#installation). The
  recommended build is the `yt-dlp` zipapp release, which needs
  `python3` and can update itself.
- [ffmpeg](https://ffmpeg.org/), to merge separate video and audio
  streams (most resolutions above 360p).
- [deno](https://deno.com/), for the full set of YouTube formats.
  Optional.
- Rust 1.88 or newer to build.

## Build and run

```bash
cargo build --release
./target/release/ytdl-web          # http://127.0.0.1:9000
```

Configuration is through environment variables or flags. See
[`.env.example`](.env.example) or run `ytdl-web --help`:

| Variable | Default | |
|---|---|---|
| `YTDL_LISTEN` | `127.0.0.1:9000` | Address and port |
| `YTDL_YTDLP` | `yt-dlp` | yt-dlp executable |
| `YTDL_FFMPEG_LOCATION` | | Directory with ffmpeg, if not on `PATH` |
| `YTDL_REMOTE_COMPONENTS` | `ejs:github` | yt-dlp `--remote-components`; empty disables |
| `YTDL_WORK_DIR` | `$TMPDIR/ytdl-web` | Where downloads wait to be fetched |
| `YTDL_KEEP_MINUTES` | `60` | How long an unfetched download is kept |
| `YTDL_LOG` | `info` | Log filter, e.g. `ytdl_web=debug` to see yt-dlp's messages |

yt-dlp runs with `--ignore-config`, so a yt-dlp config file in the
service user's home doesn't change its behavior.

## Deployment (systemd + nginx)

[`contrib/ytdl-web.service`](contrib/ytdl-web.service) runs it as a
dedicated `ytdl` user. Its header comments list the install steps. Put
yt-dlp in `/var/lib/ytdl-web/bin/` so the service can update it.
Downloads wait in `/var/cache/ytdl-web/downloads`, so make sure that
filesystem has room for your largest video.

To serve it under a subpath of an existing site (all paths in the page
are relative, so any prefix works):

```nginx
    location = /yt-dlp {
        return 301 /yt-dlp/;
    }

    location /yt-dlp/ {
        proxy_pass http://127.0.0.1:9000/;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_set_header Host $host;
        # Large files and the live progress stream must not be buffered.
        proxy_buffering off;
        # yt-dlp --update and slow lookups can take a while.
        proxy_read_timeout 300s;
    }
```

## How it works

```
browser ──fetch / EventSource──▶ ytdl-web (axum)
                                   ├── POST /api/info      yt-dlp --dump-single-json
                                   ├── POST /api/download  yt-dlp --progress-template … (one at a time)
                                   ├── GET  /api/events    progress as server-sent events
                                   ├── GET  /api/file/{id} stream the finished file, then delete it
                                   ├── POST /api/cancel    SIGTERM the yt-dlp process group
                                   ├── POST /api/update    yt-dlp --update
                                   └── GET  /api/sites     yt-dlp --extractor-descriptions (cached)
```

- `src/ytdlp.rs` starts yt-dlp. Each child leads its own process group,
  so a cancel, a timeout or a closed browser request also stops ffmpeg
  and deno.
- `src/formats.rs` turns yt-dlp's format list into the menu.
- `src/downloads.rs` is the download slot. It reads yt-dlp's progress
  lines, publishes them on a watch channel, and owns the finished file.
- `src/web.rs` has the routes and the embedded page.

A link is always passed after `--`, so it can never be read as a yt-dlp
option. Only plain `http(s)://` links and simple format selectors are
accepted.

## Security

Anyone who can reach the port can look up links, start or cancel
downloads, and update yt-dlp. Keep it on localhost behind nginx on your
home network, or put an authenticating proxy in front. POST requests must
carry an `X-Ytdl` header, so other websites you visit can't trigger these
actions through your browser.

## Development

```bash
cargo test
cargo clippy --all-targets
YTDL_LOG=ytdl_web=debug cargo run
```

CI runs the same checks plus `cargo fmt --check`.

Static files are compiled in, so rebuild after editing `static/`. The
app icons are rendered from `contrib/icon.svg` with the command in that
file.
