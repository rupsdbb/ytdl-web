use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "ytdl-web", version, about = "Small self-hosted web frontend for yt-dlp")]
pub struct Config {
    /// Address and port for the web interface
    #[arg(long, env = "YTDL_LISTEN", default_value = "127.0.0.1:9000")]
    pub listen: SocketAddr,

    // Paths are read as text so that an empty value (`YTDL_DENO=` in an env
    // file) means "not set" instead of stopping the server; see the methods below.
    /// yt-dlp executable (a path, or a name looked up on PATH)
    #[arg(long, env = "YTDL_YTDLP", default_value = "yt-dlp")]
    ytdlp: String,

    /// Directory holding ffmpeg/ffprobe, if they are not on PATH
    #[arg(long, env = "YTDL_FFMPEG_LOCATION", default_value = "", hide_default_value = true)]
    ffmpeg_location: String,

    /// deno executable, or the directory holding it, if it is not on PATH
    #[arg(long, env = "YTDL_DENO", default_value = "", hide_default_value = true)]
    deno: String,

    /// Passed to yt-dlp --remote-components (needed for full YouTube formats); empty disables
    #[arg(long, env = "YTDL_REMOTE_COMPONENTS", default_value = "ejs:github")]
    pub remote_components: String,

    /// Where downloads are written until the browser fetches them [default: $TMPDIR/ytdl-web]
    #[arg(long, env = "YTDL_WORK_DIR", default_value = "", hide_default_value = true)]
    work_dir: String,

    /// Minutes a finished download waits to be fetched before it is deleted
    #[arg(long, env = "YTDL_KEEP_MINUTES", default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    pub keep_minutes: u64,
}

impl Config {
    pub fn ytdlp(&self) -> PathBuf {
        non_empty(&self.ytdlp).unwrap_or_else(|| "yt-dlp".into())
    }

    pub fn ffmpeg_location(&self) -> Option<PathBuf> {
        non_empty(&self.ffmpeg_location)
    }

    pub fn deno(&self) -> Option<PathBuf> {
        non_empty(&self.deno)
    }

    pub fn work_dir(&self) -> PathBuf {
        non_empty(&self.work_dir).unwrap_or_else(|| std::env::temp_dir().join("ytdl-web"))
    }
}

fn non_empty(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_paths_mean_unset() {
        let cfg = Config::try_parse_from(["ytdl-web", "--ytdlp", "", "--deno", " ", "--ffmpeg-location", ""])
            .expect("empty values are accepted");
        assert_eq!(cfg.ytdlp(), PathBuf::from("yt-dlp"));
        assert_eq!(cfg.deno(), None);
        assert_eq!(cfg.ffmpeg_location(), None);
        assert_eq!(cfg.work_dir(), std::env::temp_dir().join("ytdl-web"));

        let cfg = Config::try_parse_from(["ytdl-web", "--deno", "/opt/ytdl-web/bin/deno"]).unwrap();
        assert_eq!(cfg.deno(), Some(PathBuf::from("/opt/ytdl-web/bin/deno")));
    }
}
