pub mod sparkline;
pub mod ui;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;
use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::metrics::{
    CacheStatus, CompressionEvent, SessionStats, ToolCallInfo,
};
use sparkline::TokenHistory;

/// Seconds of inactivity before a session is considered idle
const SESSION_IDLE_SECS: u64 = 45;
/// Seconds of inactivity before a session is considered ended
const SESSION_ENDED_SECS: u64 = 120;
/// Seconds after "ended" before auto-removing from display
const SESSION_CLEANUP_SECS: u64 = 600;

/// Update payload sent from the proxy on request completion
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

/// Two-phase message from proxy to TUI for session tracking
#[derive(Debug, Clone)]
pub enum TuiMessage {
    RequestStarted {
        session_id: String,
    },
    RequestCompleted {
        session_id: String,
        update: ProxyUpdate,
    },
}

/// Per-session tracking state
pub struct SessionInfo {
    pub last_activity: Instant,
    pub in_flight: u32,
    pub total_requests: u64,
    pub tokens_saved: u64,
}

/// Computed session display state
#[derive(Clone, Copy, PartialEq)]
pub enum SessionState {
    Active,
    Idle,
    Ended,
}

impl SessionInfo {
    pub fn new() -> Self {
        Self {
            last_activity: Instant::now(),
            in_flight: 0,
            total_requests: 0,
            tokens_saved: 0,
        }
    }

    pub fn state(&self) -> SessionState {
        if self.in_flight > 0 {
            SessionState::Active
        } else if self.last_activity.elapsed().as_secs() < SESSION_IDLE_SECS {
            SessionState::Idle
        } else {
            SessionState::Ended
        }
    }

    pub fn should_cleanup(&self) -> bool {
        self.in_flight == 0
            && self.last_activity.elapsed().as_secs() >= SESSION_ENDED_SECS + SESSION_CLEANUP_SECS
    }

    pub fn age_str(&self) -> String {
        let secs = self.last_activity.elapsed().as_secs();
        if secs < 60 {
            format!("{}s ago", secs)
        } else {
            format!("{}m ago", secs / 60)
        }
    }
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
    pub listen_addr: String,
    pub input_cost_per_1k: f64,
    pub tick_count: u64,
    pub sessions: HashMap<String, SessionInfo>,
}

impl TuiApp {
    pub fn new(upstream_url: String, listen_addr: String, input_cost_per_1k: f64) -> Self {
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
            listen_addr,
            input_cost_per_1k,
            tick_count: 0,
            sessions: HashMap::new(),
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
                self.sessions.clear();
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

    pub fn on_tui_message(&mut self, msg: TuiMessage) {
        if self.paused {
            return;
        }

        match msg {
            TuiMessage::RequestStarted { session_id } => {
                let session = self.sessions.entry(session_id).or_insert_with(SessionInfo::new);
                session.in_flight += 1;
                session.last_activity = Instant::now();
            }
            TuiMessage::RequestCompleted { session_id, update } => {
                // Update session tracking
                let session = self.sessions.entry(session_id).or_insert_with(SessionInfo::new);
                session.in_flight = session.in_flight.saturating_sub(1);
                session.last_activity = Instant::now();
                session.total_requests += 1;
                session.tokens_saved += update.tokens_original.saturating_sub(update.tokens_compressed) as u64;

                // Update aggregate stats (same logic as old on_proxy_event)
                self.stats.total_requests += 1;
                self.stats.total_tokens_original += update.tokens_original as u64;
                self.stats.total_tokens_compressed += update.tokens_compressed as u64;
                match &update.cache_status {
                    CacheStatus::Hit { .. } => {
                        self.stats.cache_hits += 1;
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
    }

    /// Count sessions by state for display
    pub fn session_counts(&self) -> (usize, usize, usize) {
        let mut active = 0;
        let mut idle = 0;
        let mut ended = 0;
        for session in self.sessions.values() {
            match session.state() {
                SessionState::Active => active += 1,
                SessionState::Idle => idle += 1,
                SessionState::Ended => ended += 1,
            }
        }
        (active, idle, ended)
    }

    /// Get sessions sorted by last activity (most recent first), limited to 5
    pub fn sorted_sessions(&self) -> Vec<(&String, &SessionInfo)> {
        let mut entries: Vec<_> = self.sessions.iter().collect();
        entries.sort_by(|a, b| b.1.last_activity.cmp(&a.1.last_activity));
        entries.truncate(5);
        entries
    }
}

/// Run the TUI event loop in a blocking thread
pub fn run_tui(
    mut rx: mpsc::UnboundedReceiver<TuiMessage>,
    upstream_url: String,
    listen_addr: String,
    input_cost_per_1k: f64,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new(upstream_url, listen_addr, input_cost_per_1k);

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
        while let Ok(msg) = rx.try_recv() {
            app.on_tui_message(msg);
        }

        // Auto-cleanup old ended sessions (every ~5 seconds = 50 ticks)
        if app.tick_count % 50 == 0 {
            app.sessions.retain(|_, info| !info.should_cleanup());
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
