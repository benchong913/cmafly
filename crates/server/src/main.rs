//! `cmafly-serve` — HTTP origin for CMAF HLS over `.idx` + `.mp4` pairs.
//!
//! Boot sequence:
//! 1. Parse CLI flags and resolve the [`Config`] — this also runs the
//!    host tunable verification and emits the startup audit log line.
//! 2. Construct the bounded LRU [`IndexRegistry`] and the in-flight
//!    segment-assembly [`Semaphore`] (admission control).
//! 3. Build the axum router via [`handlers::build_app`] and bind / serve.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Semaphore;

mod config;
mod error;
mod handlers;
mod registry;

use config::{Config, ResolveArgs};
use handlers::{AppState, build_app};
use registry::IndexRegistry;

#[derive(Parser, Debug)]
#[command(
    name = "cmafly-serve",
    about = "HTTP origin: dynamically assembles CMAF HLS from .idx + .mp4 pairs"
)]
struct Cli {
    /// Directory holding source MP4 files (named `{:id}.mp4`).
    #[arg(long)]
    media_dir: PathBuf,
    /// Directory holding `.idx` files (named `{:id}.idx`).
    #[arg(long)]
    index_dir: PathBuf,
    /// Bind address, e.g. `127.0.0.1:8080`.
    #[arg(long)]
    bind: String,
    /// Override auto-derived LRU registry capacity. See `config.rs` for
    /// the formula used when omitted.
    #[arg(long)]
    max_open_archives: Option<usize>,
    /// Override auto-derived segment-assembly admission semaphore size.
    #[arg(long)]
    max_inflight_segments: Option<usize>,
    /// Maximum seconds a segment request waits for an admission permit
    /// before responding 503; defaults to 5 s.
    #[arg(long)]
    permit_wait_timeout: Option<f64>,
}

impl From<Cli> for ResolveArgs {
    fn from(cli: Cli) -> Self {
        ResolveArgs {
            media_dir: cli.media_dir,
            index_dir: cli.index_dir,
            bind: cli.bind,
            max_open_archives: cli.max_open_archives,
            max_inflight_segments: cli.max_inflight_segments,
            permit_wait_timeout_secs: cli.permit_wait_timeout,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::resolve(ResolveArgs::from(cli))?;
    println!("{}", config.audit_line());

    let registry = Arc::new(IndexRegistry::new(
        config.media_dir.clone(),
        config.index_dir.clone(),
        config.max_open_archives,
    ));
    let inflight = Arc::new(Semaphore::new(config.max_inflight_segments));
    let state = AppState {
        registry,
        inflight,
        permit_wait_timeout: config.permit_wait_timeout,
    };

    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("bind {}", config.bind))?;
    axum::serve(listener, app).await.context("axum::serve")?;
    Ok(())
}
