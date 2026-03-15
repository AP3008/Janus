mod cache;
mod config;
mod embed;
mod metrics;
mod pipeline;
mod proxy;
mod session;
mod stream_reassemble;
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
    /// Cache management
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Flush all cached entries
    Flush {
        /// Path to config file
        #[arg(short, long, default_value = "janus.toml")]
        config: PathBuf,
    },
    /// Show cache statistics
    Stats {
        /// Path to config file
        #[arg(short, long, default_value = "janus.toml")]
        config: PathBuf,
    },
    /// Test cache end-to-end (connect, embed, put, get)
    Test {
        /// Path to config file
        #[arg(short, long, default_value = "janus.toml")]
        config: PathBuf,
    },
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
            // Create command channel (TUI -> main for async ops like cache flush)
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<tui::TuiCommand>();

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

            // Initialize semantic cache (graceful degradation if Redis unavailable)
            let (cache_box, embedder): (
                Option<Box<dyn cache::SemanticCache>>,
                Option<embed::Embedder>,
            ) = if janus_config.cache.enabled {
                match cache::redis_cache::RedisSemanticCache::new(
                    &janus_config.cache.redis_url,
                )
                .await
                {
                    Ok(redis_cache) => {
                        // Flush stale cache from previous sessions
                        use cache::SemanticCache;
                        match redis_cache.flush().await {
                            Ok(count) if count > 0 => {
                                tracing::info!(count, "Flushed stale cache entries from previous session");
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to flush stale cache on startup");
                            }
                            _ => {}
                        }
                        match embed::Embedder::new() {
                            Ok(emb) => {
                                tracing::info!("Semantic cache enabled with Redis");
                                (Some(Box::new(redis_cache) as Box<dyn cache::SemanticCache>), Some(emb))
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to initialize embedder, cache disabled");
                                (None, None)
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Redis unavailable, cache disabled");
                        (None, None)
                    }
                }
            } else {
                (None, None)
            };

            let ast_pruning_default = janus_config.pipeline.ast_pruning;
            let state = Arc::new(proxy::AppState {
                config: janus_config,
                client: reqwest::Client::new(),
                start_time: Instant::now(),
                tokenizer: tok,
                tui_tx: tui_tx.clone(),
                session_store,
                cache: cache_box,
                embedder,
                ast_pruning_enabled: std::sync::atomic::AtomicBool::new(ast_pruning_default),
                inmem_cache: dashmap::DashMap::new(),
                inflight: dashmap::DashMap::new(),
            });

            let state_for_shutdown = state.clone();
            let app = proxy::create_router(state);

            // Spawn TUI in a separate OS thread
            let tui_handle = if !no_tui {
                let upstream = upstream_url.clone();
                let listen = listen_addr.clone();
                Some(std::thread::spawn(move || {
                    if let Err(e) = tui::run_tui(tui_rx, upstream, listen, input_cost, cmd_tx) {
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
                drop(cmd_rx);
                axum::serve(listener, app).await?;
            } else {
                // Run server until TUI quits, handling cache flush commands
                let state_for_cmd = state_for_shutdown.clone();
                let server = axum::serve(listener, app);

                // Spawn command handler for TUI commands (cache flush, etc.)
                let cmd_handle = tokio::spawn(async move {
                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            tui::TuiCommand::FlushCache => {
                                state_for_cmd.inmem_cache.clear();
                                if let Some(ref cache) = state_for_cmd.cache {
                                    match cache.flush().await {
                                        Ok(count) => {
                                            tracing::info!(count, "Cache flushed via TUI command");
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = %e, "Cache flush failed");
                                        }
                                    }
                                }
                            }
                            tui::TuiCommand::ToggleAstPruning => {
                                let prev = state_for_cmd.ast_pruning_enabled.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
                                tracing::info!(enabled = !prev, "AST pruning toggled via TUI");
                            }
                        }
                    }
                });

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

                cmd_handle.abort();
            }

            // Flush cache on shutdown for session isolation
            if let Some(ref cache) = state_for_shutdown.cache {
                match cache.flush().await {
                    Ok(count) if count > 0 => {
                        eprintln!("Flushed {} cache entries on shutdown", count);
                    }
                    _ => {}
                }
            }
        }
        Commands::Cache { action } => {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
                .init();

            match action {
                CacheAction::Flush { config } => {
                    let janus_config = config::JanusConfig::load(&config)?;
                    let redis_cache = cache::redis_cache::RedisSemanticCache::new(
                        &janus_config.cache.redis_url,
                    )
                    .await?;
                    let count = cache::SemanticCache::flush(&redis_cache).await?;
                    println!("Flushed {} cached entries", count);
                }
                CacheAction::Stats { config } => {
                    let janus_config = config::JanusConfig::load(&config)?;
                    let redis_cache = cache::redis_cache::RedisSemanticCache::new(
                        &janus_config.cache.redis_url,
                    )
                    .await?;
                    let stats = cache::SemanticCache::stats(&redis_cache).await?;
                    println!("Cache Statistics:");
                    println!("  Total entries: {}", stats.total_entries);
                }
                CacheAction::Test { config } => {
                    let janus_config = config::JanusConfig::load(&config)?;

                    // Step 1: Connect to Redis (includes RediSearch check)
                    print!("[1/4] Redis + RediSearch connection... ");
                    let redis_cache = cache::redis_cache::RedisSemanticCache::new(
                        &janus_config.cache.redis_url,
                    )
                    .await
                    .map_err(|e| {
                        println!("FAIL");
                        e
                    })?;
                    println!("OK");

                    // Step 2: Initialize embedder
                    print!("[2/4] Embedder init... ");
                    let embedder = embed::Embedder::new().map_err(|e| {
                        println!("FAIL");
                        e
                    })?;
                    println!("OK (384 dims)");

                    // Step 3: Embed + PUT
                    print!("[3/4] Cache PUT... ");
                    let test_query = "What is the capital of France?";
                    let embedding = embedder.embed_one(test_query).await?;
                    let mock_response = b"{\"content\":[{\"text\":\"Paris is the capital of France.\"}]}";
                    let model = "test-model";
                    cache::SemanticCache::put(
                        &redis_cache,
                        &embedding,
                        mock_response,
                        model,
                        0,
                        janus_config.cache.ttl_seconds,
                    )
                    .await
                    .map_err(|e| {
                        println!("FAIL");
                        e
                    })?;
                    println!("OK (TTL {}s)", janus_config.cache.ttl_seconds);

                    // Step 4: GET and verify hit
                    print!("[4/4] Cache GET... ");
                    let t0 = Instant::now();
                    let result = cache::SemanticCache::get(
                        &redis_cache,
                        &embedding,
                        janus_config.cache.similarity_cutoff,
                        model,
                    )
                    .await
                    .map_err(|e| {
                        println!("FAIL");
                        e
                    })?;
                    let latency_ms = t0.elapsed().as_secs_f64() * 1000.0;

                    match result {
                        Some(cached) => {
                            println!(
                                "OK (similarity: {:.3}, latency: {:.1}ms)",
                                cached.similarity, latency_ms
                            );
                            println!();
                            println!("Cache is working correctly.");
                        }
                        None => {
                            println!("FAIL (no hit returned for identical query)");
                            println!();
                            println!(
                                "Cache PUT succeeded but GET missed. \
                                 Check similarity_cutoff ({}) or Redis index.",
                                janus_config.cache.similarity_cutoff
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

