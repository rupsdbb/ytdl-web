use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "ytdl-web", version, about = "Small self-hosted web frontend for yt-dlp")]
pub struct Config {
    /// Address and port for the web interface
    #[arg(long, env = "YTDL_LISTEN", default_value = "127.0.0.1:9000")]
    pub listen: SocketAddr,

    /// yt-dlp executable (a path, or a name looked up on PATH)
    #[arg(long, env = "YTDL_YTDLP", default_value = "yt-dlp")]
    pub ytdlp: PathBuf,

    /// Directory holding ffmpeg/ffprobe, if they are not on PATH
    #[arg(long, env = "YTDL_FFMPEG_LOCATION")]
    pub ffmpeg_location: Option<PathBuf>,

    /// Passed to yt-dlp --remote-components (needed for full YouTube formats); empty disables
    #[arg(long, env = "YTDL_REMOTE_COMPONENTS", default_value = "ejs:github")]
    pub remote_components: String,

    /// Where downloads are written until the browser fetches them [default: $TMPDIR/ytdl-web]
    #[arg(long, env = "YTDL_WORK_DIR")]
    pub work_dir: Option<PathBuf>,

    /// Minutes a finished download waits to be fetched before it is deleted
    #[arg(long, env = "YTDL_KEEP_MINUTES", default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    pub keep_minutes: u64,
}

impl Config {
    pub fn work_dir(&self) -> PathBuf {
        self.work_dir.clone().unwrap_or_else(|| std::env::temp_dir().join("ytdl-web"))
    }
}
