mod config;
mod metrics;
mod pipeline;
mod proxy;
mod tokenizer;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

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
    },
    /// Run compression benchmarks
    Benchmark,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { config } => {
            let janus_config = config::JanusConfig::load(&config)?;
            let listen_addr = janus_config.server.listen.clone();

            tracing::info!(
                listen = %listen_addr,
                upstream = %janus_config.server.upstream_url,
                "Starting Janus proxy"
            );

            let tok = tokenizer::Tokenizer::new();

            let state = Arc::new(proxy::AppState {
                config: janus_config,
                client: reqwest::Client::new(),
                start_time: Instant::now(),
                tokenizer: tok,
            });

            let app = proxy::create_router(state);

            let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
            tracing::info!("Janus listening on {}", listen_addr);
            axum::serve(listener, app).await?;
        }
        Commands::Benchmark => {
            tracing::info!("Benchmark not yet implemented");
        }
    }

    Ok(())
}
