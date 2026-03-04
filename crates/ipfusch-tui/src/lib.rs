use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, terminal};
use futures::StreamExt;
use ipfusch_core::report::IntervalStats;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Stdout};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub async fn run_dashboard(mut updates: mpsc::UnboundedReceiver<IntervalStats>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut paused = false;
    let mut status = String::from("running");
    let mut intervals = VecDeque::<IntervalStats>::new();
    let mut latest = IntervalStats::default();

    loop {
        terminal.draw(|frame| draw_ui(frame, &latest, &intervals, paused, &status))?;

        tokio::select! {
            maybe_event = events.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('p') => {
                            paused = !paused;
                            status = if paused { "paused".to_string() } else { "running".to_string() };
                        }
                        KeyCode::Char('e') => {
                            let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                            let path = format!("ipfusch-snapshot-{ts}.json");
                            if let Ok(json) = serde_json::to_string_pretty(&latest) {
                                if fs::write(&path, json).is_ok() {
                                    status = format!("snapshot exported: {path}");
                                } else {
                                    status = "snapshot export failed".to_string();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ = ticker.tick() => {
                if paused {
                    continue;
                }
                while let Ok(update) = updates.try_recv() {
                    latest = update.clone();
                    intervals.push_back(update);
                    if intervals.len() > 120 {
                        intervals.pop_front();
                    }
                }
            }
            maybe_interval = updates.recv() => {
                let Some(update) = maybe_interval else {
                    break;
                };
                if !paused {
                    latest = update.clone();
                    intervals.push_back(update);
                    if intervals.len() > 120 {
                        intervals.pop_front();
                    }
                }
            }
        }
    }

    restore_terminal(&mut terminal)?;
    Ok(())
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    terminal.show_cursor()?;
    Ok(())
}

fn draw_ui(
    frame: &mut ratatui::Frame,
    latest: &IntervalStats,
    intervals: &VecDeque<IntervalStats>,
    paused: bool,
    status: &str,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let title = if paused {
        "ipfusch live dashboard [paused]"
    } else {
        "ipfusch live dashboard"
    };

    let summary = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Throughput: ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("{:.2} Mbps", latest.throughput_bps / 1_000_000.0)),
            Span::raw("   "),
            Span::styled("Packet rate: ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("{:.2} pps", latest.packet_rate_pps)),
            Span::raw("   "),
            Span::styled("Loss: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("{:.2}%", latest.loss_pct)),
        ]),
        Line::from(vec![
            Span::styled("Latency p50/p95/p99: ", Style::default().fg(Color::Magenta)),
            Span::raw(format!(
                "{:.2}/{:.2}/{:.2} ms",
                latest.p50_ms, latest.p95_ms, latest.p99_ms
            )),
            Span::raw("   "),
            Span::styled("Jitter: ", Style::default().fg(Color::Green)),
            Span::raw(format!("{:.2} ms", latest.jitter_ms)),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL).title(title));

    frame.render_widget(summary, chunks[0]);

    let row = Row::new(vec![
        Cell::from(latest.index.to_string()),
        Cell::from(latest.start_ms.to_string()),
        Cell::from(latest.end_ms.to_string()),
        Cell::from(format!("{:.2}", latest.throughput_bps / 1_000_000.0)),
        Cell::from(format!("{:.2}", latest.packet_rate_pps)),
        Cell::from(format!("{:.2}", latest.loss_pct)),
        Cell::from(format!("{:.2}", latest.p99_ms)),
    ]);

    let table = Table::new(
        vec![row],
        [
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(vec!["idx", "start", "end", "mbps", "pps", "loss%", "p99"])
            .style(Style::default().fg(Color::White)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("current interval"),
    );
    frame.render_widget(table, chunks[1]);

    let rows: Vec<Row> = intervals
        .iter()
        .rev()
        .take(20)
        .map(|intv| {
            Row::new(vec![
                Cell::from(intv.index.to_string()),
                Cell::from(format!("{:.1}", intv.throughput_bps / 1_000_000.0)),
                Cell::from(format!("{:.2}", intv.packet_rate_pps)),
                Cell::from(format!("{:.2}", intv.loss_pct)),
                Cell::from(format!("{:.2}", intv.jitter_ms)),
                Cell::from(format!("{:.2}", intv.p95_ms)),
                Cell::from(format!("{:.2}", intv.p99_ms)),
            ])
        })
        .collect();

    let history = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(Row::new(vec![
        "idx", "mbps", "pps", "loss%", "jitter", "p95", "p99",
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("recent intervals"),
    );
    frame.render_widget(history, chunks[2]);

    let footer = Paragraph::new(format!(
        "keys: q quit | p pause | e export snapshot    status: {status}"
    ))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[3]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_history_keeps_order() {
        let mut queue = VecDeque::new();
        queue.push_back(IntervalStats {
            index: 1,
            ..IntervalStats::default()
        });
        queue.push_back(IntervalStats {
            index: 2,
            ..IntervalStats::default()
        });
        assert_eq!(queue.front().map(|v| v.index), Some(1));
        assert_eq!(queue.back().map(|v| v.index), Some(2));
    }
}
