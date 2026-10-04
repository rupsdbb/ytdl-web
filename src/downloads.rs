//! The single download slot.
//!
//! A download runs yt-dlp into `work_dir/job-<id>/`. Its progress is
//! published on a watch channel that every open page subscribes to. A
//! finished file waits until a browser has read all of it, a new download
//! starts, or `keep` runs out, and is then deleted.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{oneshot, watch};
use tracing::{info, warn};

use crate::ytdlp::{Proc, YtDlp, error_message, friendly_error, truncate};

/// Progress updates are sent at most this often (yt-dlp reports per chunk).
const PROGRESS_INTERVAL: Duration = Duration::from_millis(400);
/// After SIGTERM, how long yt-dlp and ffmpeg get before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
const PROGRESS_TEMPLATE: &str = "download:@P %(progress.downloaded_bytes)s %(progress.total_bytes)s \
     %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s %(info.format_id)s";
const POSTPROCESS_TEMPLATE: &str = "postprocess:@S %(progress.status)s %(progress.postprocessor)s";

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Starting,
    Downloading,
    Processing,
    Finished,
    Delivered,
    Failed,
    Cancelled,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Status {
    pub job: u64,
    pub phase: Phase,
    pub title: String,
    pub thumbnail: Option<String>,
    /// The chosen quality, e.g. "1080p60 · MP4 · H.264".
    pub label: String,
    pub downloaded: u64,
    pub total: u64,
    /// Bytes per second.
    pub speed: f64,
    pub eta: Option<u64>,
    pub part: u32,
    pub parts: u32,
    pub file: Option<String>,
    pub size: u64,
    pub error: Option<String>,
}

pub struct Downloads {
    ytdlp: Arc<YtDlp>,
    work_dir: PathBuf,
    keep: Duration,
    status: watch::Sender<Status>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_job: u64,
    running: Option<Running>,
    ready: Option<Ready>,
}

struct Running {
    job: u64,
    cancel: Option<oneshot::Sender<()>>,
}

struct Ready {
    job: u64,
    dir: PathBuf,
    path: PathBuf,
}

pub struct ReadyFile {
    pub path: PathBuf,
    pub name: String,
}

/// What the page shows about a download; only used for display.
pub struct JobInfo {
    pub title: String,
    pub thumbnail: Option<String>,
    pub label: String,
}

#[derive(Debug)]
pub enum StartError {
    Busy,
    Io(std::io::Error),
}

enum Outcome {
    Done(PathBuf),
    Failed(String),
    Cancelled,
}

impl Downloads {
    pub fn new(ytdlp: Arc<YtDlp>, work_dir: PathBuf, keep: Duration) -> Arc<Self> {
        let first_job =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(1);
        Arc::new(Downloads {
            ytdlp,
            work_dir,
            keep,
            status: watch::Sender::new(Status::default()),
            inner: Mutex::new(Inner { next_job: first_job, ..Default::default() }),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    pub fn is_busy(&self) -> bool {
        self.inner.lock().unwrap().running.is_some()
    }

    pub fn start(self: &Arc<Self>, url: String, selector: String, about: JobInfo) -> Result<u64, StartError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.running.is_some() {
            return Err(StartError::Busy);
        }
        if let Some(old) = inner.ready.take() {
            remove_dir(old.dir);
        }
        let job = inner.next_job;
        inner.next_job += 1;
        let dir = self.work_dir.join(format!("job-{job}"));
        // _all: tmp cleaners may have removed the (idle) work dir itself.
        std::fs::create_dir_all(&dir).map_err(StartError::Io)?;
        let (cancel_tx, cancel_rx) = oneshot::channel();
        inner.running = Some(Running { job, cancel: Some(cancel_tx) });
        drop(inner);

        self.status.send_replace(Status {
            job,
            phase: Phase::Starting,
            title: about.title,
            thumbnail: about.thumbnail,
            label: about.label,
            parts: 1,
            part: 1,
            ..Default::default()
        });
        info!("job {job}: downloading {url} as {selector}");
        let this = self.clone();
        tokio::spawn(async move {
            let outcome = this.run(&dir, &url, &selector, cancel_rx).await;
            this.finish(job, dir, outcome);
        });
        Ok(job)
    }

    /// Returns false when nothing is running.
    pub fn cancel(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.running.as_mut() {
            Some(running) => {
                if let Some(tx) = running.cancel.take() {
                    info!("job {}: cancelling", running.job);
                    let _ = tx.send(());
                }
                true
            }
            None => false,
        }
    }

    pub fn ready_file(&self, job: u64) -> Option<ReadyFile> {
        let inner = self.inner.lock().unwrap();
        let ready = inner.ready.as_ref().filter(|r| r.job == job)?;
        let name = ready.path.file_name()?.to_string_lossy().into_owned();
        Some(ReadyFile { path: ready.path.clone(), name })
    }

    /// A browser has read the whole file: delete it.
    pub fn delivered(&self, job: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.ready.as_ref().is_some_and(|r| r.job == job) {
            let ready = inner.ready.take().expect("checked");
            remove_dir(ready.dir);
            info!("job {job}: delivered");
            self.status.send_if_modified(|s| {
                let current = s.job == job && s.phase == Phase::Finished;
                if current {
                    s.phase = Phase::Delivered;
                }
                current
            });
        }
    }

    async fn run(&self, dir: &Path, url: &str, selector: &str, mut cancel: oneshot::Receiver<()>) -> Outcome {
        let mut cmd = self.ytdlp.command();
        cmd.args(["--quiet", "--progress", "--newline", "--no-mtime", "--windows-filenames"])
            // The format yt-dlp settled on, e.g. "137+140" (two parts), and its
            // expected size (the sum of the parts for a merged format).
            .args(["--no-simulate", "--print", "before_dl:@F %(format_id)s %(filesize,filesize_approx)s"])
            .args(["--progress-template", PROGRESS_TEMPLATE, "--progress-template", POSTPROCESS_TEMPLATE])
            .arg("--paths")
            .arg(dir)
            .args(["--output", "%(title).200B.%(ext)s", "--format", selector, "--"])
            .arg(url);
        let mut proc = match Proc::spawn(cmd) {
            Ok(p) => p,
            Err(e) => return Outcome::Failed(format!("{e:#}")),
        };
        let mut out = BufReader::new(proc.child.stdout.take().expect("piped")).lines();
        let mut err = BufReader::new(proc.child.stderr.take().expect("piped")).lines();
        let (mut out_open, mut err_open) = (true, true);
        let mut stderr_tail: Vec<u8> = Vec::new();
        let mut tracker = Tracker::default();
        let mut cancelled = false;
        let kill_at = tokio::time::sleep(Duration::MAX);
        tokio::pin!(kill_at);

        while out_open || err_open {
            let line = tokio::select! {
                l = out.next_line(), if out_open => l.ok().flatten().or_else(|| { out_open = false; None }),
                l = err.next_line(), if err_open => match l.ok().flatten() {
                    Some(l) if !l.starts_with('@') => {
                        tracing::debug!("yt-dlp: {l}");
                        stderr_tail.extend_from_slice(l.as_bytes());
                        stderr_tail.push(b'\n');
                        if stderr_tail.len() > 16 * 1024 {
                            stderr_tail.drain(..stderr_tail.len() - 8 * 1024);
                        }
                        None
                    }
                    Some(l) => Some(l),
                    None => { err_open = false; None }
                },
                _ = &mut cancel, if !cancelled => {
                    cancelled = true;
                    proc.signal(libc::SIGTERM);
                    kill_at.as_mut().reset(tokio::time::Instant::now() + KILL_GRACE);
                    None
                }
                _ = &mut kill_at => {
                    proc.signal(libc::SIGKILL);
                    kill_at.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3600));
                    None
                }
            };
            if let Some(line) = line
                && !cancelled
            {
                self.track(&mut tracker, &line);
            }
        }

        let status = tokio::select! {
            s = proc.wait() => s,
            _ = tokio::time::sleep(KILL_GRACE) => {
                proc.signal(libc::SIGKILL);
                proc.wait().await
            }
        };
        if cancelled {
            return Outcome::Cancelled;
        }
        match status {
            Ok(s) if s.success() => match find_output(dir) {
                Some(path) => Outcome::Done(path),
                None => Outcome::Failed("yt-dlp finished but no file was written".into()),
            },
            Ok(_) => Outcome::Failed(error_message(&stderr_tail)),
            Err(e) => Outcome::Failed(format!("waiting for yt-dlp: {e}")),
        }
    }

    fn track(&self, t: &mut Tracker, line: &str) {
        match t.line(line) {
            Some(Update::Parts(parts)) => self.status.send_modify(|s| s.parts = parts),
            Some(Update::Progress(p)) => self.status.send_modify(|s| {
                s.phase = Phase::Downloading;
                s.downloaded = p.downloaded;
                s.total = p.total;
                s.speed = p.speed;
                s.eta = p.eta;
                s.part = p.part;
                s.parts = s.parts.max(p.part);
            }),
            Some(Update::Processing) => self.status.send_modify(|s| s.phase = Phase::Processing),
            None => {}
        }
    }

    fn finish(self: &Arc<Self>, job: u64, dir: PathBuf, outcome: Outcome) {
        let mut inner = self.inner.lock().unwrap();
        inner.running = None;
        match outcome {
            Outcome::Done(path) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let file = path.file_name().map(|n| n.to_string_lossy().into_owned());
                info!("job {job}: finished, {size} bytes");
                inner.ready = Some(Ready { job, dir, path });
                self.status.send_modify(|s| {
                    s.phase = Phase::Finished;
                    s.file = file;
                    s.size = size;
                    s.eta = None;
                });
                let this = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(this.keep).await;
                    this.expire(job);
                });
            }
            Outcome::Failed(msg) => {
                warn!("job {job}: failed: {msg}");
                remove_dir(dir);
                self.status.send_modify(|s| {
                    s.phase = Phase::Failed;
                    s.error = Some(truncate(&friendly_error(&msg), 600));
                });
            }
            Outcome::Cancelled => {
                info!("job {job}: cancelled");
                remove_dir(dir);
                self.status.send_modify(|s| s.phase = Phase::Cancelled);
            }
        }
    }

    fn expire(&self, job: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.ready.as_ref().is_some_and(|r| r.job == job) {
            let ready = inner.ready.take().expect("checked");
            remove_dir(ready.dir);
            info!("job {job}: not fetched in time, deleted");
            self.status.send_if_modified(|s| {
                let current = s.job == job && s.phase == Phase::Finished;
                if current {
                    *s = Status::default();
                }
                current
            });
        }
    }
}

/// Turns yt-dlp's output lines into progress for the whole download.
///
/// yt-dlp reports each part (video, then audio) separately, from 0 to its
/// own size; this adds them up so the bar only moves forward.
#[derive(Default)]
struct Tracker {
    format_id: String,
    part: u32,
    parts: u32,
    /// Expected size of all parts together, if yt-dlp knows it.
    expected: Option<u64>,
    /// Bytes of the parts already finished.
    done: u64,
    part_downloaded: u64,
    part_total: u64,
    last_sent: Option<Instant>,
}

#[derive(Debug, PartialEq)]
enum Update {
    Parts(u32),
    Progress(Progress),
    Processing,
}

#[derive(Debug, PartialEq)]
struct Progress {
    downloaded: u64,
    total: u64,
    speed: f64,
    eta: Option<u64>,
    part: u32,
}

impl Tracker {
    fn line(&mut self, line: &str) -> Option<Update> {
        if let Some(rest) = line.strip_prefix("@P ") {
            self.progress(rest)
        } else if let Some(rest) = line.strip_prefix("@F ") {
            let mut fields = rest.split_whitespace();
            self.parts = fields.next().map_or(1, |id| id.split('+').count() as u32);
            self.expected = fields.next().and_then(parse_num).filter(|v| *v > 0.0).map(|v| v as u64);
            Some(Update::Parts(self.parts))
        } else if let Some(rest) = line.strip_prefix("@S ") {
            // Merging and fixups can take a while on a Pi; moving files is instant.
            (rest.starts_with("started") && !rest.ends_with("MoveFiles")).then_some(Update::Processing)
        } else {
            None
        }
    }

    fn progress(&mut self, rest: &str) -> Option<Update> {
        let f: Vec<&str> = rest.split_whitespace().collect();
        let num = |i: usize| f.get(i).copied().and_then(parse_num);
        let format_id = f.get(5).copied().unwrap_or("");
        let new_part = self.format_id != format_id;
        if new_part {
            if self.part > 0 {
                self.done += self.part_total.max(self.part_downloaded);
            }
            self.format_id = format_id.to_string();
            self.part += 1;
        }
        let downloaded = num(0).unwrap_or(0.0) as u64;
        let total = num(1).or(num(2)).unwrap_or(0.0) as u64;
        self.part_downloaded = downloaded;
        self.part_total = total;

        let complete = total > 0 && downloaded >= total;
        let due = self.last_sent.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL);
        if !(due || new_part || complete) {
            return None;
        }
        self.last_sent = Some(Instant::now());

        let so_far = self.done + downloaded;
        let known = if total > 0 { self.done + total } else { 0 };
        // On the last part the sizes are exact. Before that, the expected size
        // also covers the parts still to come; without it the total is unknown
        // (0), rather than a total that would grow and push the bar back.
        let last_part = self.part >= self.parts;
        let total_all = match self.expected {
            _ if last_part => known,
            Some(expected) => expected.max(known),
            None => 0,
        };
        let speed = num(3).unwrap_or(0.0);
        let eta = if speed > 0.0 && total_all > so_far {
            Some(((total_all - so_far) as f64 / speed) as u64)
        } else {
            num(4).map(|v| v as u64)
        };
        Some(Update::Progress(Progress { downloaded: so_far, total: total_all, speed, eta, part: self.part }))
    }
}

fn parse_num(v: &str) -> Option<f64> {
    v.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)
}

/// The finished file: the largest regular file that is not a leftover fragment.
fn find_output(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            !(name.ends_with(".part") || name.ends_with(".ytdl") || name.contains(".part-Frag"))
        })
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| (meta.len(), e.path()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, path)| path)
}

fn remove_dir(dir: PathBuf) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!("removing {}: {e}", dir.display());
        }
    });
}

/// Job directories from a previous run (e.g. after a crash or restart).
pub fn remove_leftovers(work_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(work_dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        if entry.file_name().to_string_lossy().starts_with("job-") && entry.path().is_dir() {
            info!("removing leftover {}", entry.path().display());
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                warn!("removing {}: {e}", entry.path().display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(t: &mut Tracker, line: &str) -> Progress {
        // Skip the rate limit so every line reports.
        t.last_sent = None;
        match t.line(line) {
            Some(Update::Progress(p)) => p,
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn two_parts_add_up() {
        let mut t = Tracker::default();
        assert_eq!(t.line("@F 137+140 1100"), Some(Update::Parts(2)));
        // Video: 1000 bytes, expected total covers the audio too.
        let p = progress(&mut t, "@P 500 1000 NA 100 5 137");
        assert_eq!((p.downloaded, p.total, p.part), (500, 1100, 1));
        assert_eq!(p.eta, Some(6));
        let p = progress(&mut t, "@P 1000 1000 NA 100 0 137");
        assert_eq!((p.downloaded, p.total), (1000, 1100));
        // Audio starts: the bar keeps going instead of dropping to 0.
        let p = progress(&mut t, "@P 50 120 NA 100 1 140");
        assert_eq!((p.downloaded, p.total, p.part), (1050, 1120, 2));
        let p = progress(&mut t, "@P 120 120 NA 100 0 140");
        assert_eq!((p.downloaded, p.total), (1120, 1120));
    }

    #[test]
    fn two_parts_without_expected_size_never_go_back() {
        let mut t = Tracker::default();
        t.line("@F 137+140 NA");
        let p = progress(&mut t, "@P 1000 1000 NA 100 0 137");
        assert_eq!((p.downloaded, p.total), (1000, 0));
        let p = progress(&mut t, "@P 50 120 NA 100 1 140");
        assert_eq!((p.downloaded, p.total), (1050, 1120));
    }

    #[test]
    fn unknown_sizes_and_other_lines() {
        let mut t = Tracker::default();
        assert_eq!(t.line("@F 18 NA"), Some(Update::Parts(1)));
        let p = progress(&mut t, "@P 300 NA NA NA NA 18");
        assert_eq!((p.downloaded, p.total, p.eta), (300, 0, None));
        // Falls back to the estimate when the exact size is unknown.
        let p = progress(&mut t, "@P 300 NA 900 NA NA 18");
        assert_eq!(p.total, 900);
        assert_eq!(t.line("@S started Merger"), Some(Update::Processing));
        assert_eq!(t.line("@S started MoveFiles"), None);
        assert_eq!(t.line("@S finished Merger"), None);
        assert_eq!(t.line("[download] something else"), None);
    }
}
