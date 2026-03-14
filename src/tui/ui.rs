use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Bar, BarChart, BarGroup, Block, Borders, Paragraph, Sparkline, Wrap},
};

use super::TuiApp;

pub fn draw(frame: &mut Frame, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // Header
            Constraint::Min(10),   // Main content
            Constraint::Length(5), // Tool calls
            Constraint::Length(5), // Sparkline
            Constraint::Length(1), // Footer
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_main_panels(frame, chunks[1], app);
    draw_tool_calls(frame, chunks[2], app);
    draw_sparklines(frame, chunks[3], app);
    draw_footer(frame, chunks[4]);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let status = if app.paused { "PAUSED" } else { "● LIVE" };
    let status_color = if app.paused { Color::Yellow } else { Color::Green };

    let header = Line::from(vec![
        Span::styled(" JANUS ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("v"),
        Span::raw(env!("CARGO_PKG_VERSION")),
        Span::raw("  "),
        Span::styled(format!("[{}]", status), Style::default().fg(status_color)),
        Span::raw(format!("  upstream: {}", app.upstream_url)),
    ]);

    frame.render_widget(Paragraph::new(header), area);
}

fn draw_main_panels(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    draw_session_panel(frame, chunks[0], app);
    draw_last_request_panel(frame, chunks[1], app);
}

fn draw_session_panel(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    // Session stats
    let ratio = app.stats.compression_ratio() * 100.0;
    let saved_dollars = app.stats.tokens_saved() as f64 / 1000.0 * app.input_cost_per_1k;

    let session_text = vec![
        Line::from(format!("  Requests     {}", app.stats.total_requests)),
        Line::from("  Total tokens"),
        Line::from(format!("    original   {}", app.stats.total_tokens_original)),
        Line::from(format!("    saved      {}", app.stats.tokens_saved())),
        Line::from(format!("    ratio      {:.0}%", ratio)),
        Line::from(format!("  Est. $ saved ${:.2}", saved_dollars)),
    ];

    let session_block = Block::default()
        .title(" SESSION ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(session_text).block(session_block),
        chunks[0],
    );

    // Cache stats
    let cache_ratio = app.stats.cache_hit_ratio() * 100.0;
    let cache_saved_dollars = app.stats.cache_tokens_saved as f64 / 1000.0 * app.input_cost_per_1k;
    let cache_text = vec![
        Line::from(format!("  hits       {}", app.stats.cache_hits)),
        Line::from(format!("  misses     {}", app.stats.cache_misses)),
        Line::from(format!("  ratio      {:.0}%", cache_ratio)),
        Line::from(format!("  tok saved  {}", app.stats.cache_tokens_saved)),
        Line::from(format!("  $ saved    ${:.2}", cache_saved_dollars)),
    ];

    let cache_block = Block::default()
        .title(" CACHE ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(cache_text).block(cache_block), chunks[1]);
}

fn draw_last_request_panel(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    // Last request info
    let request_text = if let Some(ref lr) = app.last_request {
        let is_cache_hit = matches!(&lr.cache_status, crate::metrics::CacheStatus::Hit { .. });
        let status = match &lr.cache_status {
            crate::metrics::CacheStatus::Hit { similarity } => {
                format!("CACHE HIT ({:.2} sim)", similarity)
            }
            crate::metrics::CacheStatus::Miss => "MISS -> forwarded".to_string(),
            crate::metrics::CacheStatus::Skipped => "skipped".to_string(),
        };

        if is_cache_hit {
            vec![
                Line::from(vec![
                    Span::raw("  Cache       "),
                    Span::styled(status, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                ]),
                Line::from(format!("  Tokens      {} (100% saved)", lr.tokens_original)),
                Line::from("  Upstream    skipped (served from cache)"),
            ]
        } else {
            let pct = if lr.tokens_original > 0 {
                (lr.tokens_original - lr.tokens_compressed) as f64
                    / lr.tokens_original as f64
                    * 100.0
            } else {
                0.0
            };
            vec![
                Line::from(format!("  Cache       {}", status)),
                Line::from(format!("  Original    {} tokens", lr.tokens_original)),
                Line::from(format!("  Compressed  {} tokens ({:.1}% saved)", lr.tokens_compressed, pct)),
                Line::from(format!("  Pipeline    {}ms", lr.pipeline_duration.as_millis())),
                Line::from(format!(
                    "  Upstream    {}ms",
                    lr.upstream_duration.map(|d| d.as_millis()).unwrap_or(0)
                )),
            ]
        }
    } else {
        vec![Line::from("  waiting...")]
    };

    let request_block = Block::default()
        .title(" LAST REQUEST ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(request_text).block(request_block),
        chunks[0],
    );

    // Stage breakdown as bar chart
    draw_stage_breakdown(frame, chunks[1], app);
}

fn draw_stage_breakdown(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let bars: Vec<Bar> = app
        .stage_breakdown
        .iter()
        .map(|(name, saved)| {
            Bar::default()
                .label(Line::from(name.as_str().to_string()))
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
                .title(" STAGE BREAKDOWN ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .data(BarGroup::default().bars(&bars))
        .bar_width(3)
        .bar_gap(1)
        .direction(Direction::Horizontal)
        .max(app.stage_breakdown.iter().map(|(_, v)| *v).max().unwrap_or(1) as u64);

    frame.render_widget(chart, area);
}

fn draw_tool_calls(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let mut lines = Vec::new();

    if let Some(ref lr) = app.last_request {
        let start = app.scroll_offset;
        let visible = (area.height as usize).saturating_sub(2);
        for tc in lr.tool_calls.iter().skip(start).take(visible) {
            let status_str = match &tc.status {
                crate::metrics::ToolCallStatus::Kept => {
                    Span::styled("KEPT", Style::default().fg(Color::Green))
                }
                crate::metrics::ToolCallStatus::Deduped => {
                    Span::styled("DEDUPED", Style::default().fg(Color::Yellow))
                }
            };
            lines.push(Line::from(vec![
                Span::raw(format!("  ► {} {} ", tc.tool_name, tc.input_summary)),
                Span::raw("["),
                status_str,
                Span::raw(format!(" {} ", tc.tool_use_id)),
                Span::raw(format!("-{} tok]", tc.tokens_saved)),
            ]));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from("  No tool calls yet"));
    }

    let block = Block::default()
        .title(" TOOL CALLS (this request) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_sparklines(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let orig_data = app.token_history_original.as_vec();
    let comp_data = app.token_history_compressed.as_vec();

    let orig_sparkline = Sparkline::default()
        .block(
            Block::default()
                .title(" orig ")
                .borders(Borders::LEFT | Borders::TOP | Borders::RIGHT),
        )
        .data(&orig_data)
        .style(Style::default().fg(Color::Blue));

    let comp_sparkline = Sparkline::default()
        .block(
            Block::default()
                .title(" comp ")
                .borders(Borders::LEFT | Borders::BOTTOM | Borders::RIGHT),
        )
        .data(&comp_data)
        .style(Style::default().fg(Color::Green));

    frame.render_widget(orig_sparkline, chunks[0]);
    frame.render_widget(comp_sparkline, chunks[1]);
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    let footer = Line::from(vec![
        Span::styled(" [q] ", Style::default().fg(Color::Yellow)),
        Span::raw("quit  "),
        Span::styled("[f] ", Style::default().fg(Color::Yellow)),
        Span::raw("flush cache  "),
        Span::styled("[r] ", Style::default().fg(Color::Yellow)),
        Span::raw("reset stats  "),
        Span::styled("[p] ", Style::default().fg(Color::Yellow)),
        Span::raw("pause"),
    ]);

    frame.render_widget(Paragraph::new(footer), area);
}
