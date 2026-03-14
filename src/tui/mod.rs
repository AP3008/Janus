pub mod sparkline;
pub mod ui;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::metrics::{
    CacheStatus, CompressionEvent, SessionStats, ToolCallInfo,
};
use sparkline::TokenHistory;

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
}

/// TUI application state
pub struct TuiApp {
    pub should_quit: bool,
    pub paused: bool,
    pub scroll_offset: usize,
    pub stats: SessionStats,
    pub token_history_original: TokenHistory,
    pub token_history_compressed: TokenHistory,
    pub last_request: Option<ProxyUpdate>,
    pub stage_breakdown: Vec<(String, usize)>,
    pub upstream_url: String,
    pub input_cost_per_1k: f64,
}

impl TuiApp {
    pub fn new(upstream_url: String, input_cost_per_1k: f64) -> Self {
        Self {
            should_quit: false,
            paused: false,
            scroll_offset: 0,
            stats: SessionStats::default(),
            token_history_original: TokenHistory::new(30),
            token_history_compressed: TokenHistory::new(30),
            last_request: None,
            stage_breakdown: Vec::new(),
            upstream_url,
            input_cost_per_1k,
        }
    }

    pub fn on_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('p') => self.paused = !self.paused,
            KeyCode::Char('r') => {
                self.stats = SessionStats::default();
                self.token_history_original = TokenHistory::new(30);
                self.token_history_compressed = TokenHistory::new(30);
                self.last_request = None;
                self.stage_breakdown.clear();
            }
            KeyCode::Char('f') => {
                // Cache flush would be handled via a command channel back to proxy
                // For now just log
            }
            KeyCode::Up => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            KeyCode::Down => {
                self.scroll_offset += 1;
            }
            _ => {}
        }
    }

    pub fn on_proxy_event(&mut self, update: ProxyUpdate) {
        if self.paused {
            return;
        }

        // Update session stats
        self.stats.total_requests += 1;
        self.stats.total_tokens_original += update.tokens_original as u64;
        self.stats.total_tokens_compressed += update.tokens_compressed as u64;
        match &update.cache_status {
            CacheStatus::Hit { .. } => {
                self.stats.cache_hits += 1;
                // Cache hit saves all original tokens (no upstream call needed)
                self.stats.cache_tokens_saved += update.tokens_original as u64;
            }
            CacheStatus::Miss => self.stats.cache_misses += 1,
            CacheStatus::Skipped => {}
        }

        // Update sparklines
        self.token_history_original
            .push(update.tokens_original as u64);
        self.token_history_compressed
            .push(update.tokens_compressed as u64);

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
        self.last_request = Some(update);
    }
}

/// Run the TUI event loop in a blocking thread
pub fn run_tui(
    mut rx: mpsc::UnboundedReceiver<ProxyUpdate>,
    upstream_url: String,
    input_cost_per_1k: f64,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new(upstream_url, input_cost_per_1k);

    // Set up panic hook to restore terminal
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;

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
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
