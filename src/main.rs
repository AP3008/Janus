mod config;
mod metrics;
mod pipeline;
mod proxy;
mod session;
mod tokenizer;
mod tui;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "janus", version, about = "LLM API token compression proxy")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the proxy server
    Serve {
        /// Path to config file
        #[arg(short, long, default_value = "janus.toml")]
        config: PathBuf,
        /// Disable TUI (log to stdout instead)
        #[arg(long)]
        no_tui: bool,
    },
    /// Run compression benchmarks
    Benchmark,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { config, no_tui } => {
            let janus_config = config::JanusConfig::load(&config)?;
            let listen_addr = janus_config.server.listen.clone();
            let upstream_url = janus_config.server.upstream_url.clone();
            let input_cost = janus_config.pricing.input_cost_per_1k;

            // Create TUI channel
            let (tui_tx, tui_rx) = mpsc::unbounded_channel::<tui::ProxyUpdate>();

            if no_tui {
                // Initialize tracing for non-TUI mode
                tracing_subscriber::fmt()
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                    )
                    .init();
            } else {
                // In TUI mode, only log errors to avoid interfering with display
                tracing_subscriber::fmt()
                    .with_env_filter(tracing_subscriber::EnvFilter::new("error"))
                    .init();
            }

            tracing::info!(
                listen = %listen_addr,
                upstream = %upstream_url,
                "Starting Janus proxy"
            );

            let tok = tokenizer::Tokenizer::new();

            let session_store = session::SessionStore::new();

            let state = Arc::new(proxy::AppState {
                config: janus_config,
                client: reqwest::Client::new(),
                start_time: Instant::now(),
                tokenizer: tok,
                tui_tx: tui_tx.clone(),
                session_store,
            });

            let app = proxy::create_router(state);

            // Spawn TUI in a separate OS thread
            let tui_handle = if !no_tui {
                let upstream = upstream_url.clone();
                Some(std::thread::spawn(move || {
                    if let Err(e) = tui::run_tui(tui_rx, upstream, input_cost) {
                        eprintln!("TUI error: {}", e);
                    }
                }))
            } else {
                // Drop the receiver so sends don't block
                drop(tui_rx);
                None
            };

            let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
            tracing::info!("Janus listening on {}", listen_addr);

            if no_tui {
                axum::serve(listener, app).await?;
            } else {
                // Run server until TUI quits
                let server = axum::serve(listener, app);
                tokio::select! {
                    result = server => {
                        result?;
                    }
                    _ = tokio::task::spawn_blocking(move || {
                        if let Some(handle) = tui_handle {
                            let _ = handle.join();
                        }
                    }) => {
                        // TUI quit, exit gracefully
                    }
                }
            }
        }
        Commands::Benchmark => {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
                .init();
            eprintln!("Benchmark not yet implemented");
        }
    }

    Ok(())
}
