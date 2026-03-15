use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Bar, BarChart, BarGroup, Block, Borders, Gauge, Paragraph, Sparkline, Wrap},
};

use super::TuiApp;

// ── helpers ──────────────────────────────────────────────────────────────

fn friendly_stage_name(raw: &str) -> &str {
    match raw {
        "A_dedup" => "Deduplication",
        "B1_docstrings" => "Docstrings",
        "B2_comments" => "Comments",
        "B3_whitespace" => "Whitespace",
        "B4_stacktrace" => "Stack Traces",
        "B5_dedup_blocks" => "Block Dedup",
        "C_ast" => "AST Pruning",
        "D_semantic" => "Semantic",
        other => other,
    }
}

fn savings_color(ratio: f64) -> Color {
    if ratio > 0.30 {
        Color::Green
    } else if ratio > 0.10 {
        Color::Yellow
    } else {
        Color::White
    }
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

// ── top-level draw ───────────────────────────────────────────────────────

pub fn draw(frame: &mut Frame, app: &TuiApp) {
    if app.stats.total_requests == 0 && app.last_request.is_none() {
        draw_welcome(frame, app);
    } else {
        draw_active(frame, app);
    }
}

// ── welcome state ────────────────────────────────────────────────────────

fn draw_welcome(frame: &mut Frame, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(10),  // Welcome content
            Constraint::Length(1), // Footer
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], app);

    // Animated dots
    let dots = match (app.tick_count / 3) % 4 {
        0 => "",
        1 => ".",
        2 => "..",
        _ => "...",
    };

    let welcome_text = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Welcome to Janus",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Janus reduces your LLM token usage through",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "compression and semantic caching.",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("Waiting for first request{}", dots),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Point your client at: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("http://{}", app.listen_addr),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Upstream: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&app.upstream_url),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    frame.render_widget(
        Paragraph::new(welcome_text)
            .block(block)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        chunks[1],
    );

    draw_footer(frame, chunks[2]);
}

// ── active state ─────────────────────────────────────────────────────────

fn draw_active(frame: &mut Frame, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Length(5), // Hero savings
            Constraint::Min(10),  // Main panels
            Constraint::Length(7), // Sparklines
            Constraint::Length(1), // Footer
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_hero_savings(frame, chunks[1], app);
    draw_main_panels(frame, chunks[2], app);
    draw_sparklines(frame, chunks[3], app);
    draw_footer(frame, chunks[4]);
}

// ── header (3 lines) ─────────────────────────────────────────────────────

fn draw_header(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let status = if app.paused { "PAUSED" } else { "LIVE" };
    let status_color = if app.paused { Color::Yellow } else { Color::Green };
    let status_icon = if app.paused { "⏸" } else { "●" };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                " JANUS ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{} {}", status_icon, status),
                Style::default().fg(status_color),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Upstream: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&app.upstream_url),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ── hero savings bar ─────────────────────────────────────────────────────

fn draw_hero_savings(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let total_saved = app.stats.tokens_saved() + app.stats.cache_tokens_saved;
    let total_original = app.stats.total_tokens_original;
    let ratio = if total_original > 0 {
        (total_saved as f64) / (total_original as f64)
    } else {
        0.0
    };
    let ratio_clamped = ratio.min(1.0);
    let color = savings_color(ratio);
    let saved_dollars =
        total_saved as f64 / 1000.0 * app.input_cost_per_1k;

    let compression_saved = app.stats.tokens_saved();
    let cache_saved = app.stats.cache_tokens_saved;
    let cache_hit_ratio = app.stats.cache_hit_ratio() * 100.0;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // headline
            Constraint::Length(1), // gauge
            Constraint::Length(1), // breakdown
        ])
        .margin(1)
        .split(area);

    // Block wrapper
    let block = Block::default()
        .title(Span::styled(
            " Total Savings ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    frame.render_widget(block, area);

    // Line 1: headline
    let headline = Line::from(vec![
        Span::styled(
            format!("Saved {} tokens ", format_number(total_saved)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("(~${:.2})", saved_dollars),
            Style::default().fg(color),
        ),
    ]);
    frame.render_widget(Paragraph::new(headline), chunks[0]);

    // Line 2: gauge
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(color).bg(Color::DarkGray))
        .ratio(ratio_clamped)
        .label(format!("{:.0}% reduction", ratio * 100.0));
    frame.render_widget(gauge, chunks[1]);

    // Line 3: breakdown
    let breakdown = Line::from(vec![
        Span::styled("Compression: ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_number(compression_saved)),
        Span::styled(" │ Cache: ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_number(cache_saved)),
        Span::styled(
            format!(" ({:.0}% hit rate)", cache_hit_ratio),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(breakdown), chunks[2]);
}

// ── main panels ──────────────────────────────────────────────────────────

fn draw_main_panels(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    draw_last_request(frame, chunks[0], app);
    draw_right_panel(frame, chunks[1], app);
}

// ── left: last request ──────────────────────────────────────────────────

fn draw_last_request(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let block = Block::default()
        .title(Span::styled(
            " Last Request ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    if app.last_request.is_none() {
        let text = Paragraph::new(Line::from(Span::styled(
            "No requests yet",
            Style::default().fg(Color::DarkGray),
        )))
        .block(block)
        .alignment(Alignment::Center);
        frame.render_widget(text, area);
        return;
    }

    let lr = app.last_request.as_ref().unwrap();

    // Split: top for request info, bottom for stage breakdown
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(3)])
        .split(inner);

    // Request info
    let is_cache_hit = matches!(&lr.cache_status, crate::metrics::CacheStatus::Hit { .. });

    let status_span = match &lr.cache_status {
        crate::metrics::CacheStatus::Hit { similarity } => Span::styled(
            format!("CACHE HIT ({:.2})", similarity),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        crate::metrics::CacheStatus::Miss => Span::styled(
            "FORWARDED",
            Style::default().fg(Color::Cyan),
        ),
        crate::metrics::CacheStatus::Skipped => Span::styled(
            "SKIPPED",
            Style::default().fg(Color::DarkGray),
        ),
    };

    let mut lines = vec![Line::from(vec![
        Span::styled("  Status    ", Style::default().fg(Color::DarkGray)),
        status_span,
    ])];

    if is_cache_hit {
        lines.push(Line::from(vec![
            Span::styled("  Tokens    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}", format_number(lr.tokens_original as u64))),
            Span::styled(" (100% saved)", Style::default().fg(Color::Green)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Upstream  ", Style::default().fg(Color::DarkGray)),
            Span::styled("skipped", Style::default().fg(Color::DarkGray)),
        ]));
    } else {
        let pct = if lr.tokens_original > 0 {
            lr.tokens_original.saturating_sub(lr.tokens_compressed) as f64 / lr.tokens_original as f64 * 100.0
        } else {
            0.0
        };
        lines.push(Line::from(vec![
            Span::styled("  Tokens    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "{} → {}",
                format_number(lr.tokens_original as u64),
                format_number(lr.tokens_compressed as u64)
            )),
            Span::styled(format!(" ({:.0}% saved)", pct), Style::default().fg(Color::Green)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Pipeline  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}ms", lr.pipeline_duration.as_millis())),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Upstream  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "{}ms",
                lr.upstream_duration.map(|d| d.as_millis()).unwrap_or(0)
            )),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), chunks[0]);

    // Stage breakdown
    draw_stage_breakdown(frame, chunks[1], app);
}

fn draw_stage_breakdown(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let bars: Vec<Bar> = app
        .stage_breakdown
        .iter()
        .map(|(name, saved)| {
            Bar::default()
                .label(Line::from(friendly_stage_name(name).to_string()))
                .value(*saved as u64)
                .style(if *saved > 0 {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                })
        })
        .collect();

    let chart = BarChart::default()
        .block(
            Block::default()
                .title(Span::styled(
                    " Stage Breakdown ",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .data(BarGroup::default().bars(&bars))
        .bar_width(3)
        .bar_gap(1)
        .direction(Direction::Horizontal)
        .max(
            app.stage_breakdown
                .iter()
                .map(|(_, v)| *v)
                .max()
                .unwrap_or(1) as u64,
        );

    frame.render_widget(chart, area);
}

// ── right panel: contextual ──────────────────────────────────────────────

fn draw_right_panel(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let has_tool_calls = app
        .last_request
        .as_ref()
        .map(|lr| !lr.tool_calls.is_empty())
        .unwrap_or(false);

    if has_tool_calls {
        draw_tool_calls(frame, area, app);
    } else {
        draw_session_summary(frame, area, app);
    }
}

fn draw_session_summary(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let block = Block::default()
        .title(Span::styled(
            " Session ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // stats text
            Constraint::Length(1), // spacer
            Constraint::Length(1), // cache gauge label
            Constraint::Length(1), // cache gauge
            Constraint::Min(0),   // padding
        ])
        .split(inner);

    let ratio = app.stats.compression_ratio() * 100.0;

    let stats_text = vec![
        Line::from(vec![
            Span::styled("  Requests      ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_number(app.stats.total_requests)),
        ]),
        Line::from(vec![
            Span::styled("  Tokens in     ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_number(app.stats.total_tokens_original)),
        ]),
        Line::from(vec![
            Span::styled("  Tokens out    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_number(app.stats.total_tokens_compressed)),
        ]),
        Line::from(vec![
            Span::styled("  Avg compress  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.0}%", ratio),
                Style::default().fg(savings_color(app.stats.compression_ratio())),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(stats_text), chunks[0]);

    // Cache hit rate gauge
    let cache_label = Line::from(vec![
        Span::styled("  Cache hit rate ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            "({}/{})",
            app.stats.cache_hits,
            app.stats.cache_hits + app.stats.cache_misses
        )),
    ]);
    frame.render_widget(Paragraph::new(cache_label), chunks[2]);

    let cache_ratio = app.stats.cache_hit_ratio();
    let cache_gauge = Gauge::default()
        .gauge_style(
            Style::default()
                .fg(savings_color(cache_ratio))
                .bg(Color::DarkGray),
        )
        .ratio(cache_ratio.min(1.0))
        .label(format!("{:.0}%", cache_ratio * 100.0));
    frame.render_widget(cache_gauge, chunks[3]);
}

fn draw_tool_calls(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let mut lines = Vec::new();

    if let Some(ref lr) = app.last_request {
        let start = app.scroll_offset;
        let block_inner_height = area.height.saturating_sub(2) as usize;
        for tc in lr.tool_calls.iter().skip(start).take(block_inner_height) {
            let status_span = match &tc.status {
                crate::metrics::ToolCallStatus::Kept => {
                    Span::styled(" KEPT ", Style::default().fg(Color::Green))
                }
                crate::metrics::ToolCallStatus::Deduped => {
                    Span::styled(" DEDUPED ", Style::default().fg(Color::Yellow))
                }
            };
            let saved_style = if tc.tokens_saved > 0 {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                status_span,
                Span::raw(format!(" {} ", tc.tool_name)),
                Span::styled(tc.input_summary.clone(), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("  -{} tok", tc.tokens_saved), saved_style),
            ]));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No tool calls",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let total = app
        .last_request
        .as_ref()
        .map(|lr| lr.tool_calls.len())
        .unwrap_or(0);
    let title_str = if total > 0 {
        format!(" Tool Calls ({}) ", total)
    } else {
        " Tool Calls ".to_string()
    };

    let block = Block::default()
        .title(Span::styled(
            title_str,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

// ── sparklines ───────────────────────────────────────────────────────────

fn draw_sparklines(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let orig_data = app.token_history_original.as_vec();
    let comp_data = app.token_history_compressed.as_vec();

    // Shared max for comparable scaling
    let shared_max = orig_data
        .iter()
        .chain(comp_data.iter())
        .copied()
        .max()
        .unwrap_or(1)
        .max(1);

    let orig_sparkline = Sparkline::default()
        .block(
            Block::default()
                .title(Span::styled(
                    " Original Tokens (per request) ",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::LEFT | Borders::TOP | Borders::RIGHT)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .data(&orig_data)
        .max(shared_max)
        .style(Style::default().fg(Color::Blue));

    let comp_sparkline = Sparkline::default()
        .block(
            Block::default()
                .title(Span::styled(
                    " After Compression ",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::LEFT | Borders::BOTTOM | Borders::RIGHT)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .data(&comp_data)
        .max(shared_max)
        .style(Style::default().fg(Color::Green));

    frame.render_widget(orig_sparkline, chunks[0]);
    frame.render_widget(comp_sparkline, chunks[1]);
}

// ── footer ───────────────────────────────────────────────────────────────

fn draw_footer(frame: &mut Frame, area: Rect) {
    let sep = Span::styled(" │ ", Style::default().fg(Color::DarkGray));

    let footer = Line::from(vec![
        Span::styled(" q", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" Quit"),
        sep.clone(),
        Span::styled("p", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" Pause"),
        sep.clone(),
        Span::styled("r", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" Reset"),
        sep.clone(),
        Span::styled("f", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" Flush Cache"),
        sep,
        Span::styled("↑↓", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" Scroll"),
    ]);

    frame.render_widget(Paragraph::new(footer), area);
}
