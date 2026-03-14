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
            run_benchmark()?;
        }
    }

    Ok(())
}

fn run_benchmark() -> anyhow::Result<()> {
    let fixtures_dir = std::path::Path::new("benches/fixtures");
    if !fixtures_dir.exists() {
        anyhow::bail!("Fixtures directory not found at benches/fixtures/");
    }

    let tokenizer = tokenizer::Tokenizer::new();
    let pipeline_config = config::PipelineConfig {
        tool_dedup: true,
        regex_structural: true,
        ast_pruning: true,
        semantic_trim: false,
        semantic_threshold: 0.35,
        min_lines_for_ast: 30,
    };

    struct BenchResult {
        name: String,
        original: usize,
        compressed: usize,
        saved: usize,
        percent: f64,
        top_stage: String,
    }

    let mut results: Vec<BenchResult> = Vec::new();
    let mut total_orig: usize = 0;
    let mut total_comp: usize = 0;

    // Define fixture order to match PRD
    let fixture_names = [
        "python_docstrings",
        "react_jsdoc",
        "node_stacktrace",
        "multi_file",
        "agentic_5turn",
        "cache_repeat",
        "chat_general",
    ];

    for fixture_name in &fixture_names {
        let path = fixtures_dir.join(format!("{}.txt", fixture_name));
        if !path.exists() {
            eprintln!("  Skipping {} (file not found)", fixture_name);
            continue;
        }

        let content = std::fs::read_to_string(&path)?;
        let mut body: serde_json::Value = serde_json::from_str(&content)?;

        let orig_tokens = tokenizer.count_message_tokens(&body);

        // For agentic fixture, create a session store to test dedup
        let session_store = session::SessionStore::new();
        let session_data = if *fixture_name == "agentic_5turn" {
            if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
                let sid = session::SessionStore::derive_session_id(messages);
                Some(session_store.get_or_create(&sid))
            } else {
                None
            }
        } else {
            None
        };

        let result = pipeline::process(
            &mut body,
            &tokenizer,
            &pipeline_config,
            session_data.as_ref(),
        );

        let comp_tokens = tokenizer.count_message_tokens(&body);
        let saved = orig_tokens.saturating_sub(comp_tokens);
        let pct = if orig_tokens > 0 {
            saved as f64 / orig_tokens as f64 * 100.0
        } else {
            0.0
        };

        // Find top contributing stage
        let top_stage = result
            .events
            .iter()
            .max_by_key(|e| e.tokens_saved())
            .map(|e| {
                // Map stage names to short labels
                match e.stage_name.as_str() {
                    s if s.starts_with("A_") => "A",
                    s if s.starts_with("B") => "B",
                    s if s.starts_with("C_") => "C",
                    s if s.starts_with("D_") => "D",
                    _ => "?",
                }
            })
            .unwrap_or("-");

        // Build stage combination string
        let mut stages_used = std::collections::BTreeSet::new();
        for event in &result.events {
            if event.tokens_saved() > 0 {
                let stage = match event.stage_name.as_str() {
                    s if s.starts_with("A_") => "A",
                    s if s.starts_with("B") => "B",
                    s if s.starts_with("C_") => "C",
                    s if s.starts_with("D_") => "D",
                    _ => "?",
                };
                stages_used.insert(stage);
            }
        }
        let stage_str = if stages_used.is_empty() {
            "-".to_string()
        } else {
            stages_used.into_iter().collect::<Vec<_>>().join("+")
        };

        total_orig += orig_tokens;
        total_comp += comp_tokens;

        results.push(BenchResult {
            name: fixture_name.to_string(),
            original: orig_tokens,
            compressed: comp_tokens,
            saved,
            percent: pct,
            top_stage: stage_str,
        });
    }

    // Print formatted table
    println!();
    println!("Janus Benchmark {}", "─".repeat(50));
    println!(
        "{:<20} {:>8} {:>8} {:>8} {:>6} {:<10}",
        "Fixture", "Orig", "Comp", "Saved", "%", "Stage"
    );
    println!("{}", "─".repeat(66));
    for r in &results {
        println!(
            "{:<20} {:>8} {:>8} {:>8} {:>5.1}% {:<10}",
            r.name, r.original, r.compressed, r.saved, r.percent, r.top_stage
        );
    }
    println!("{}", "─".repeat(66));

    let total_saved = total_orig.saturating_sub(total_comp);
    let total_pct = if total_orig > 0 {
        total_saved as f64 / total_orig as f64 * 100.0
    } else {
        0.0
    };
    let est_savings = total_saved as f64 / 1000.0 * 0.003;
    println!(
        "{:<20} {:>8} {:>8} {:>8} {:>5.1}% ${:.3} saved",
        "TOTAL", total_orig, total_comp, total_saved, total_pct, est_savings
    );
    println!();

    Ok(())
}
