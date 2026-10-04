//! Running the yt-dlp executable.
//!
//! Every child gets its own process group, so cancelling or timing out also
//! stops the ffmpeg (and JS runtime) processes yt-dlp starts.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::config::Config;

const INFO_TIMEOUT: Duration = Duration::from_secs(90);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(180);
const QUICK_TIMEOUT: Duration = Duration::from_secs(30);

pub struct YtDlp {
    bin: PathBuf,
    common: Vec<OsString>,
    cache_dir: Option<PathBuf>,
}

/// The parts of `yt-dlp -J` output the UI needs.
#[derive(Deserialize)]
pub struct RawInfo {
    #[serde(rename = "_type")]
    pub kind: Option<String>,
    pub title: Option<String>,
    pub uploader: Option<String>,
    pub channel: Option<String>,
    pub duration: Option<f64>,
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub formats: Vec<crate::formats::RawFormat>,
}

impl YtDlp {
    pub fn new(cfg: &Config) -> Self {
        let mut common: Vec<OsString> =
            ["--ignore-config", "--no-playlist", "--color", "never", "--socket-timeout", "30"]
                .into_iter()
                .map(Into::into)
                .collect();
        if !cfg.remote_components.trim().is_empty() {
            common.push("--remote-components".into());
            common.push(cfg.remote_components.trim().into());
        }
        if let Some(dir) = cfg.ffmpeg_location() {
            common.push("--ffmpeg-location".into());
            common.push(dir.into());
        }
        if let Some(path) = cfg.deno() {
            let mut runtime = OsString::from("deno:");
            runtime.push(path);
            common.push("--js-runtimes".into());
            common.push(runtime);
        }
        YtDlp { bin: cfg.ytdlp(), common, cache_dir: cfg.cache_dir() }
    }

    /// A command with the shared options; callers add theirs and finish with `-- URL`.
    pub fn command(&self) -> Command {
        let mut cmd = self.base();
        cmd.args(&self.common);
        cmd
    }

    fn base(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // yt-dlp keeps $XDG_CACHE_HOME/yt-dlp and deno $XDG_CACHE_HOME/deno.
        if let Some(dir) = &self.cache_dir {
            cmd.env("XDG_CACHE_HOME", dir);
        }
        cmd
    }

    pub async fn info(&self, url: &str) -> anyhow::Result<RawInfo> {
        let mut cmd = self.command();
        // --flat-playlist: a playlist or channel link is answered from its
        // index (and then refused) instead of looking up every video in it.
        cmd.args(["--dump-single-json", "--flat-playlist", "--quiet", "--no-warnings", "--"]).arg(url);
        let out = run(cmd, INFO_TIMEOUT).await?;
        if !out.status.success() {
            bail!("{}", error_message(&out.stderr));
        }
        serde_json::from_slice(&out.stdout).context("could not parse yt-dlp output")
    }

    pub async fn version(&self) -> anyhow::Result<String> {
        let out = self.simple(&["--version"], QUICK_TIMEOUT).await?;
        Ok(out.trim().to_string())
    }

    /// `yt-dlp --update` installs right away when a newer release exists.
    pub async fn update(&self) -> anyhow::Result<String> {
        self.simple(&["--update"], UPDATE_TIMEOUT).await
    }

    pub async fn extractors(&self) -> anyhow::Result<String> {
        self.simple(&["--extractor-descriptions"], QUICK_TIMEOUT).await
    }

    async fn simple(&self, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
        let mut cmd = self.base();
        cmd.arg("--ignore-config").args(args);
        let out = run(cmd, timeout).await?;
        if !out.status.success() {
            bail!("{}", error_message(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run to completion, collecting output. On timeout (or if the caller is
/// dropped, e.g. the browser went away) the whole process group is killed.
async fn run(cmd: Command, timeout: Duration) -> anyhow::Result<Output> {
    let mut proc = Proc::spawn(cmd)?;
    let mut stdout = proc.child.stdout.take().expect("piped");
    let mut stderr = proc.child.stderr.take().expect("piped");
    let work = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a?;
        b?;
        let status = proc.wait().await?;
        anyhow::Ok(Output { status, stdout: out, stderr: err })
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(r) => r,
        Err(_) => bail!("yt-dlp did not finish within {} seconds", timeout.as_secs()),
    }
}

/// A child process leading its own process group. Dropping it before the
/// child has been waited on kills the group.
pub struct Proc {
    pub child: Child,
    pgid: Option<i32>,
}

impl Proc {
    pub fn spawn(mut cmd: Command) -> anyhow::Result<Self> {
        cmd.process_group(0).kill_on_drop(true);
        let program = cmd.as_std().get_program().to_owned();
        let child = cmd.spawn().with_context(|| format!("starting {}", Path::new(&program).display()))?;
        let pgid = child.id().map(|id| id as i32);
        Ok(Proc { child, pgid })
    }

    pub fn signal(&self, sig: libc::c_int) {
        if let Some(pgid) = self.pgid {
            // SAFETY: plain syscall; a negative pid addresses the process group.
            unsafe {
                libc::kill(-pgid, sig);
            }
        }
    }

    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait().await?;
        // yt-dlp exited on its own; anything it started has finished with it.
        self.pgid = None;
        Ok(status)
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}

/// The useful part of yt-dlp's stderr: its ERROR lines, or the last line.
pub fn error_message(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = strip_ansi(&text);
    let errors: Vec<&str> =
        text.lines().filter_map(|l| l.strip_prefix("ERROR: ")).map(str::trim).filter(|l| !l.is_empty()).collect();
    let msg = if errors.is_empty() {
        text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("yt-dlp failed").to_string()
    } else {
        errors.join("\n")
    };
    truncate(&msg, 600)
}

/// yt-dlp's message reworded for the page. Messages it doesn't recognize are
/// shown without the "[extractor] id:" prefix and "(caused by …)" details.
pub fn friendly_error(msg: &str) -> String {
    let lower = msg.to_ascii_lowercase();
    let known = [
        ("unsupported url", "This site isn't supported, or the link doesn't point to a video."),
        ("http error 404", "Nothing was found at that link (404)."),
        ("http error 403", "The site refused the request (403). Updating yt-dlp often helps."),
        ("private video", "This video is private."),
        ("sign in to confirm your age", "This video is age-restricted and needs a signed-in account."),
        ("sign in to confirm", "YouTube wants a signed-in account for this video (bot check)."),
        ("members-only", "This video is for channel members only."),
        // After the specific sign-in cases above, whose messages also mention cookies.
        ("--cookies", "This site needs a logged-in account, which this app doesn't support."),
        ("requested format is not available", "That quality isn't available any more. Look the link up again."),
        ("is not a valid url", "That doesn't look like a valid link."),
        ("name or service not known", "Couldn't reach the site. Check the server's internet connection."),
        ("name resolution", "Couldn't reach the site. Check the server's internet connection."),
        ("timed out", "The site took too long to answer. Try again."),
        ("did not finish within", "The site took too long to answer. Try again."),
    ];
    if let Some((_, nice)) = known.iter().find(|(needle, _)| lower.contains(needle)) {
        return (*nice).to_string();
    }
    let first = msg.lines().next().unwrap_or(msg);
    let without_prefix = match first.strip_prefix('[').and_then(|r| r.split_once("] ")) {
        Some((_, rest)) => rest.split_once(": ").map_or(rest, |(_, m)| m),
        None => first,
    };
    let trimmed = without_prefix.split(" (caused by").next().unwrap_or(without_prefix).trim();
    if trimmed.is_empty() { first.to_string() } else { trimmed.to_string() }
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_error_lines() {
        let err = b"WARNING: something\nERROR: [youtube] abc: \x1b[0;31mVideo unavailable\x1b[0m\n";
        assert_eq!(error_message(err), "[youtube] abc: Video unavailable");
        assert_eq!(error_message(b"usage: yt-dlp\nbad option\n"), "bad option");
        assert_eq!(error_message(b""), "yt-dlp failed");
    }

    #[test]
    fn friendly_errors() {
        assert_eq!(
            friendly_error(
                "[generic] x: Unable to download webpage: HTTP Error 404: Not Found (caused by <HTTPError 404>)"
            ),
            "Nothing was found at that link (404)."
        );
        assert_eq!(
            friendly_error("[youtube] abc: Video unavailable. This video has been removed"),
            "Video unavailable. This video has been removed"
        );
        assert_eq!(friendly_error("something odd (caused by <Foo>)"), "something odd");
        assert_eq!(
            friendly_error(
                "[vimeo] 1: The web client only works when logged-in. Use --cookies, --cookies-from-browser"
            ),
            "This site needs a logged-in account, which this app doesn't support."
        );
        assert_eq!(
            friendly_error("[youtube] x: Sign in to confirm you're not a bot. Use --cookies-from-browser or --cookies"),
            "YouTube wants a signed-in account for this video (bot check)."
        );
    }
}
