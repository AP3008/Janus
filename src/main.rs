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
    /// Run compression benchmarks
    Benchmark,
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

            let state = Arc::new(proxy::AppState {
                config: janus_config,
                client: reqwest::Client::new(),
                start_time: Instant::now(),
                tokenizer: tok,
                tui_tx: tui_tx.clone(),
                session_store,
                cache: cache_box,
                embedder,
            });

            let state_for_shutdown = state.clone();
            let app = proxy::create_router(state);

            // Spawn TUI in a separate OS thread
            let tui_handle = if !no_tui {
                let upstream = upstream_url.clone();
                let listen = listen_addr.clone();
                Some(std::thread::spawn(move || {
                    if let Err(e) = tui::run_tui(tui_rx, upstream, listen, input_cost) {
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
        Commands::Benchmark => {
            run_benchmark().await?;
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

async fn run_benchmark() -> anyhow::Result<()> {
    use cache::SemanticCache;

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

    // Initialize cache infrastructure (graceful if unavailable)
    let janus_config = config::JanusConfig::load(std::path::Path::new("janus.toml"))
        .unwrap_or_default();

    let embedder = embed::Embedder::new().ok();
    let redis_cache = if janus_config.cache.enabled {
        match cache::redis_cache::RedisSemanticCache::new(&janus_config.cache.redis_url).await {
            Ok(c) => {
                println!("  Cache: Redis connected");
                Some(c)
            }
            Err(e) => {
                println!("  Cache: unavailable ({})", e);
                None
            }
        }
    } else {
        println!("  Cache: disabled in config");
        None
    };

    struct BenchResult {
        name: String,
        original: usize,
        compressed: usize,
        saved: usize,
        percent: f64,
        top_stage: String,
        cache_col: String,
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

        // Cache test for cache_repeat fixture
        let cache_col = if *fixture_name == "cache_repeat" {
            if let (Some(ref embedder), Some(ref cache)) = (&embedder, &redis_cache) {
                let query_text = body.get("messages")
                    .and_then(|m| m.as_array())
                    .and_then(|msgs| msgs.first())
                    .and_then(|msg| msg.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");

                match embedder.embed_one(query_text).await {
                    Ok(embedding) => {
                        // Try GET first (may hit from a previous run)
                        let t0 = Instant::now();
                        let hit = cache.get(
                            &embedding,
                            janus_config.cache.similarity_cutoff,
                            model,
                        ).await;
                        let get_ms = t0.elapsed().as_secs_f64() * 1000.0;

                        match hit {
                            Ok(Some(_)) => {
                                format!("HIT {:.1}ms", get_ms)
                            }
                            _ => {
                                // PUT a mock response, then GET to verify
                                let mock = b"{\"content\":[{\"text\":\"Paris is the capital.\"}]}";
                                if let Err(e) = cache.put(&embedding, mock, model, saved, janus_config.cache.ttl_seconds).await {
                                    format!("PUT err: {}", e)
                                } else {
                                    let t1 = Instant::now();
                                    match cache.get(&embedding, janus_config.cache.similarity_cutoff, model).await {
                                        Ok(Some(_)) => format!("HIT {:.1}ms", t1.elapsed().as_secs_f64() * 1000.0),
                                        Ok(None) => format!("MISS {:.1}ms", get_ms),
                                        Err(e) => format!("ERR: {}", e),
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => format!("embed err: {}", e),
                }
            } else {
                "N/A".to_string()
            }
        } else {
            "-".to_string()
        };

        // Cache hit = entire request skipped upstream, all tokens saved
        let (saved, comp_tokens, pct, stage_str) = if *fixture_name == "cache_repeat" && cache_col.starts_with("HIT") {
            (orig_tokens, 0usize, 100.0, "cache".to_string())
        } else {
            (saved, comp_tokens, pct, stage_str)
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
            cache_col,
        });
    }

    // Print formatted table
    println!();
    println!("Janus Benchmark {}", "─".repeat(50));
    println!(
        "{:<20} {:>8} {:>8} {:>8} {:>6} {:<10} {:<14}",
        "Fixture", "Orig", "Comp", "Saved", "%", "Stage", "Cache"
    );
    println!("{}", "─".repeat(80));
    for r in &results {
        println!(
            "{:<20} {:>8} {:>8} {:>8} {:>5.1}% {:<10} {:<14}",
            r.name, r.original, r.compressed, r.saved, r.percent, r.top_stage, r.cache_col
        );
    }
    println!("{}", "─".repeat(80));

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
