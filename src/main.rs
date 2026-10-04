mod config;
mod downloads;
mod formats;
mod web;
mod ytdlp;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("YTDL_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    match serve(Config::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(cfg: Config) -> anyhow::Result<()> {
    let ytdlp = Arc::new(ytdlp::YtDlp::new(&cfg));
    match ytdlp.version().await {
        Ok(v) => info!("yt-dlp {v} at {}", cfg.ytdlp().display()),
        Err(e) => warn!("could not run yt-dlp ({}): {e:#}", cfg.ytdlp().display()),
    }

    let work_dir = cfg.work_dir();
    std::fs::create_dir_all(&work_dir).with_context(|| format!("creating {}", work_dir.display()))?;
    downloads::remove_leftovers(&work_dir);
    let downloads = downloads::Downloads::new(ytdlp.clone(), work_dir, Duration::from_secs(cfg.keep_minutes * 60));

    let shutdown = tokio_util::sync::CancellationToken::new();
    let app = web::router(web::AppState::new(ytdlp, downloads, shutdown.clone()));
    let listener =
        tokio::net::TcpListener::bind(cfg.listen).await.with_context(|| format!("binding {}", cfg.listen))?;
    info!("ytdl-web {} listening on http://{}", env!("CARGO_PKG_VERSION"), cfg.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Lets open event streams end so the server can stop.
            shutdown.cancel();
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutting down");
}
