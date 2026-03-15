pub mod ui;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::metrics::{
    CacheStatus, CompressionEvent, SessionStats, ToolCallInfo,
};

/// Update sent from the proxy to the TUI
#[derive(Debug, Clone)]
pub struct ProxyUpdate {
    pub tokens_original: usize,
    pub tokens_compressed: usize,
    pub events: Vec<CompressionEvent>,
    pub tool_calls: Vec<ToolCallInfo>,
    pub cache_status: CacheStatus,
    pub pipeline_duration: Duration,
    pub upstream_duration: Option<Duration>,
    pub error_status: Option<(u16, String)>,
}

/// Entry in the request history log
#[derive(Debug, Clone)]
pub struct RequestEntry {
    pub request_number: u64,
    pub tokens_original: usize,
    pub tokens_compressed: usize,
    pub cache_status: CacheStatus,
    pub error_status: Option<(u16, String)>,
}

/// Commands sent from TUI back to main for async operations
#[derive(Debug)]
pub enum TuiCommand {
    FlushCache,
}

/// TUI application state
pub struct TuiApp {
    pub should_quit: bool,
    pub paused: bool,
    pub scroll_offset: usize,
    pub stats: SessionStats,
    pub request_log: Vec<RequestEntry>,
    pub log_scroll: usize,
    pub last_request: Option<ProxyUpdate>,
    pub stage_breakdown: Vec<(String, usize)>,
    pub upstream_url: String,
    pub listen_addr: String,
    pub input_cost_per_1k: f64,
    pub tick_count: u64,
    pub last_error: Option<(u16, String, Instant)>,
    pub cmd_tx: mpsc::UnboundedSender<TuiCommand>,
    pub last_request_time: Option<Instant>,
    pub idle_flush_sent: bool,
    pub auto_flush_enabled: bool,
}

impl TuiApp {
    pub fn new(
        upstream_url: String,
        listen_addr: String,
        input_cost_per_1k: f64,
        cmd_tx: mpsc::UnboundedSender<TuiCommand>,
    ) -> Self {
        Self {
            should_quit: false,
            paused: false,
            scroll_offset: 0,
            stats: SessionStats::default(),
            request_log: Vec::new(),
            log_scroll: 0,
            last_request: None,
            stage_breakdown: Vec::new(),
            upstream_url,
            listen_addr,
            input_cost_per_1k,
            tick_count: 0,
            last_error: None,
            cmd_tx,
            last_request_time: None,
            idle_flush_sent: false,
            auto_flush_enabled: true,
        }
    }

    pub fn on_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('p') => self.paused = !self.paused,
            KeyCode::Char('r') => {
                self.stats = SessionStats::default();
                self.request_log.clear();
                self.log_scroll = 0;
                self.last_request = None;
                self.last_error = None;
                self.stage_breakdown.clear();
            }
            KeyCode::Char('f') => {
                let _ = self.cmd_tx.send(TuiCommand::FlushCache);
            }
            KeyCode::Char('a') => {
                self.auto_flush_enabled = !self.auto_flush_enabled;
            }
            KeyCode::Up => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                if self.log_scroll < self.request_log.len().saturating_sub(1) {
                    self.log_scroll += 1;
                }
            }
            _ => {}
        }
    }

    pub fn on_proxy_event(&mut self, update: ProxyUpdate) {
        if self.paused {
            return;
        }

        // Track request timing for idle flush
        self.last_request_time = Some(Instant::now());
        self.idle_flush_sent = false;

        // Update session stats
        self.stats.total_requests += 1;
        match &update.cache_status {
            CacheStatus::Hit { .. } => {
                self.stats.cache_hits += 1;
                // Cache hit saves all original tokens (no upstream call needed).
                // Don't add to original/compressed accumulators to avoid double-counting
                // in tokens_saved() which computes original - compressed.
                self.stats.cache_tokens_saved += update.tokens_original as u64;
            }
            CacheStatus::Miss => {
                self.stats.cache_misses += 1;
                self.stats.total_tokens_original += update.tokens_original as u64;
                self.stats.total_tokens_compressed += update.tokens_compressed as u64;
            }
            CacheStatus::Skipped => {
                self.stats.total_tokens_original += update.tokens_original as u64;
                self.stats.total_tokens_compressed += update.tokens_compressed as u64;
            }
        }

        // Update request log
        self.request_log.push(RequestEntry {
            request_number: self.stats.total_requests,
            tokens_original: update.tokens_original,
            tokens_compressed: update.tokens_compressed,
            cache_status: update.cache_status.clone(),
            error_status: update.error_status.clone(),
        });
        // Auto-scroll to bottom
        let visible = 8usize; // approximate visible rows
        if self.request_log.len() > visible {
            self.log_scroll = self.request_log.len() - visible;
        }

        // Update stage breakdown
        self.stage_breakdown.clear();
        let mut stage_map: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for event in &update.events {
            *stage_map
                .entry(event.stage_name.clone())
                .or_insert(0) += event.tokens_saved();
        }
        // Order: A, B1-B5, C, D
        for stage in &[
            "A_dedup",
            "B1_docstrings",
            "B2_comments",
            "B3_whitespace",
            "B4_stacktrace",
            "B5_dedup_blocks",
            "C_ast",
            "D_semantic",
        ] {
            let saved = stage_map.get(*stage).copied().unwrap_or(0);
            self.stage_breakdown.push((stage.to_string(), saved));
        }

        self.scroll_offset = 0;

        // Track errors
        if let Some((code, ref msg)) = update.error_status {
            self.last_error = Some((code, msg.clone(), Instant::now()));
        }

        self.last_request = Some(update);
    }
}

/// Run the TUI event loop in a blocking thread
pub fn run_tui(
    mut rx: mpsc::UnboundedReceiver<ProxyUpdate>,
    upstream_url: String,
    listen_addr: String,
    input_cost_per_1k: f64,
    cmd_tx: mpsc::UnboundedSender<TuiCommand>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new(upstream_url, listen_addr, input_cost_per_1k, cmd_tx);

    // Set up panic hook to restore terminal
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;
        app.tick_count = app.tick_count.wrapping_add(1);

        // Poll for keyboard events with 100ms timeout
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key.code);
                    if app.should_quit {
                        break;
                    }
                }
            }
        }

        // Check for proxy updates (non-blocking)
        while let Ok(update) = rx.try_recv() {
            app.on_proxy_event(update);
        }

        // Idle auto-flush: if no requests for 120s, flush cache
        if let Some(last) = app.last_request_time {
            if app.auto_flush_enabled && !app.idle_flush_sent && last.elapsed() > Duration::from_secs(120) {
                let _ = app.cmd_tx.send(TuiCommand::FlushCache);
                app.idle_flush_sent = true;
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
