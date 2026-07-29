use std::{cmp::Ordering, collections::BTreeMap};

use chrono::{DateTime, Utc};
use ratatui::{
    backend::TestBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, Wrap},
    Frame, Terminal,
};

use crate::{
    logs::{RawLogSnapshot, RawLogTarget},
    model::{
        AzureResource, ChangePoint, EvidenceState, LogSignalResult, MetricSeries, RecentChange,
        Snapshot,
    },
};

pub const CATEGORIES: [&str; 9] = [
    "all",
    "compute/web",
    "data",
    "network/edge",
    "ai",
    "storage",
    "monitoring",
    "security",
    "other",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Attention,
    Name,
    Category,
    Control,
    Signal,
    Changed,
}

impl SortKey {
    pub fn next(self) -> Self {
        match self {
            Self::Attention => Self::Name,
            Self::Name => Self::Category,
            Self::Category => Self::Control,
            Self::Control => Self::Signal,
            Self::Signal => Self::Changed,
            Self::Changed => Self::Attention,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Attention => "attention",
            Self::Name => "name",
            Self::Category => "category",
            Self::Control => "control",
            Self::Signal => "signal",
            Self::Changed => "changed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    Operations,
    Inventory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChooserMode {
    Group,
    Subscription,
}

#[derive(Clone, Debug)]
pub struct ChooserState {
    pub mode: ChooserMode,
    pub query: String,
    pub selected: usize,
}

#[derive(Clone, Debug, Default)]
pub enum Overlay {
    #[default]
    None,
    Help,
    Detail,
    RecentChanges,
    Chooser(ChooserState),
    LogSignals {
        loading: bool,
        result: Option<Box<LogSignalResult>>,
        error: String,
    },
    RawConfirm(Option<RawLogTarget>),
    RawLogs(Box<RawLogSnapshot>),
}

#[derive(Clone, Debug)]
pub struct UiState {
    pub selected_id: String,
    pub sort: SortKey,
    pub reverse: bool,
    pub category_index: usize,
    pub view: ViewMode,
    pub window_hours: u64,
    pub interval_minutes: u64,
    pub overlay: Overlay,
    pub operation: String,
    pub color: bool,
    pub watchlist_only: bool,
    /// Render-only: the requested window was changed and matching series have
    /// not arrived yet, so hidden stale evidence is reported as LOADING
    /// rather than NO MATCHING DATA.
    pub window_loading: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            selected_id: String::new(),
            sort: SortKey::Attention,
            reverse: false,
            category_index: 0,
            view: ViewMode::Operations,
            window_hours: 1,
            interval_minutes: 1,
            overlay: Overlay::None,
            operation: String::new(),
            color: true,
            watchlist_only: false,
            window_loading: false,
        }
    }
}

pub fn draw(frame: &mut Frame, snapshot: &Snapshot, state: &UiState) {
    let area = frame.area();
    let vertical = Layout::vertical([
        Constraint::Length(if state.operation.is_empty() { 1 } else { 2 }),
        Constraint::Min(10),
        Constraint::Length(1),
    ])
    .split(area);
    render_header(frame, vertical[0], snapshot, state);
    match state.view {
        ViewMode::Operations => render_operations(frame, vertical[1], snapshot, state),
        ViewMode::Inventory => render_inventory(frame, vertical[1], snapshot, state),
    }
    let window = format!("{}h/{}", state.window_hours, interval_label(state));
    let mut hints = if area.width < 100 {
        vec![
            ("q", "quit".to_string()),
            ("r", "refresh".into()),
            ("g", "group".into()),
            ("j/k", "select".into()),
            ("w", window),
            ("?", "help".into()),
        ]
    } else {
        vec![
            ("q", "quit".to_string()),
            ("r", "refresh".into()),
            ("g", "group".into()),
            ("s", "sub".into()),
            ("j/k", "select".into()),
            ("v", "view".into()),
            ("w", format!("window {window}")),
        ]
    };
    if area.width >= 100 {
        hints.extend([
            ("t/T", "star/watch".into()),
            ("d", "changes".into()),
            ("l", "logs".into()),
            ("?", "help".into()),
        ]);
    }
    let mut spans = Vec::new();
    for (key, label) in hints {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(key, style_header(state.color)));
        spans.push(Span::styled(format!(" {label}"), style_muted(state.color)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), vertical[2]);
    render_overlay(frame, area, snapshot, state);
}

fn render_header(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    let cloud = snapshot
        .subscriptions
        .iter()
        .find(|subscription| subscription.subscription_id == snapshot.selected_subscription_id)
        .or_else(|| {
            snapshot
                .subscriptions
                .iter()
                .find(|subscription| subscription.name == snapshot.selected_subscription_name)
        })
        .map(|subscription| subscription.cloud.as_str())
        .unwrap_or("Unknown");
    let view = match state.view {
        ViewMode::Operations => "operations",
        ViewMode::Inventory => "inventory",
    };
    let dim = style_muted(state.color);
    let counts = attention_counts(&snapshot.resources);
    let compact = area.width < 110;
    let mut left = if compact {
        vec![Span::styled(
            truncate(&snapshot.selected_resource_group, 24),
            Style::default().add_modifier(Modifier::BOLD),
        )]
    } else {
        vec![Span::styled(truncate(cloud, 22), style_header(state.color))]
    };
    if compact {
        left.extend([
            Span::styled("  view ", dim),
            Span::raw(if state.watchlist_only {
                "watchlist"
            } else {
                view
            }),
        ]);
    } else {
        left.extend([
            Span::styled(" › ", dim),
            Span::raw(truncate(&snapshot.selected_subscription_name, 26)),
            Span::styled(" › ", dim),
            Span::styled(
                truncate(&snapshot.selected_resource_group, 28),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("  view ", dim),
            Span::raw(if state.watchlist_only {
                "watchlist"
            } else {
                view
            }),
        ]);
    }
    let watched = snapshot
        .resources
        .iter()
        .filter(|resource| resource.watched)
        .count();
    let watched_attention = snapshot
        .resources
        .iter()
        .filter(|resource| {
            resource.watched && matches!(resource_attention(resource).0, "BAD" | "WRN" | "STOP")
        })
        .count();
    let mut right = vec![Span::styled("★ ", style_muted(state.color))];
    if watched == 0 {
        right.push(Span::raw("none"));
        if !compact {
            right.push(Span::styled(" · t add", dim));
        }
    } else {
        right.push(Span::raw(format!("{watched_attention}/{watched}")));
    }
    right.extend([
        Span::styled("  ", dim),
        Span::styled("access ", dim),
        Span::raw(snapshot.access_state.to_ascii_uppercase()),
    ]);
    if snapshot.origin == "cache" {
        right.extend([
            Span::styled("  CACHE ", dim),
            Span::raw(age(&snapshot.cache_saved_at)),
        ]);
    }
    if area.width >= 110 {
        right.extend([
            Span::styled("  res ", dim),
            Span::raw(snapshot.resources.len().to_string()),
        ]);
    }
    if area.width >= 160 {
        for label in ["BAD", "WRN", "LIM", "ND", "CAP", "INV"] {
            right.push(Span::styled(
                format!("  {} ", label.to_ascii_lowercase()),
                dim,
            ));
            right.push(Span::raw(
                counts.get(label).copied().unwrap_or(0).to_string(),
            ));
        }
    }
    let used: usize = left.iter().chain(right.iter()).map(Span::width).sum();
    let mut spans = left;
    spans.push(Span::raw(
        " ".repeat((area.width as usize).saturating_sub(used)),
    ));
    spans.extend(right);
    let mut lines = vec![Line::from(spans)];
    if !state.operation.is_empty() {
        lines.push(Line::styled(
            state.operation.clone(),
            style_warning(state.color),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_operations(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    if area.width >= 108 {
        let pulse = if area.height >= 44 { 34 } else { 30 };
        let rows = Layout::vertical([
            Constraint::Percentage(pulse),
            Constraint::Percentage(100 - pulse),
        ])
        .split(area);
        render_pulse(frame, rows[0], snapshot, state);
        let columns = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(rows[1]);
        render_selected(frame, columns[0], snapshot, state);
        if columns[1].height >= 18 {
            let right = Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)])
                .split(columns[1]);
            render_attention(frame, right[0], snapshot, state);
            render_recent_changes(frame, right[1], snapshot, state);
        } else {
            render_attention(frame, columns[1], snapshot, state);
        }
    } else if area.height >= 30 {
        let rows = Layout::vertical([
            Constraint::Percentage(32),
            Constraint::Percentage(27),
            Constraint::Percentage(28),
            Constraint::Percentage(13),
        ])
        .split(area);
        render_pulse(frame, rows[0], snapshot, state);
        render_selected(frame, rows[1], snapshot, state);
        render_attention(frame, rows[2], snapshot, state);
        render_recent_changes(frame, rows[3], snapshot, state);
    } else {
        let rows = Layout::vertical([
            Constraint::Percentage(34),
            Constraint::Percentage(30),
            Constraint::Percentage(36),
        ])
        .split(area);
        render_pulse(frame, rows[0], snapshot, state);
        render_selected(frame, rows[1], snapshot, state);
        render_attention(frame, rows[2], snapshot, state);
    }
}

const PULSE_LANES: [(&str, &[&str], bool); 4] = [
    (
        "TRAFFIC",
        &[
            "requests",
            "total_calls",
            "transactions",
            "api_hits",
            "runs_started",
            "search_qps",
        ],
        false,
    ),
    (
        "FAILURES",
        &[
            "http_5xx",
            "http_5xx_percent",
            "total_errors",
            "runs_failed",
            "errors",
            "search_throttled_percent",
        ],
        false,
    ),
    (
        "LATENCY",
        &[
            "response_time",
            "latency",
            "search_latency",
            "total_latency",
            "server_latency",
        ],
        true,
    ),
    (
        "PRESSURE",
        &[
            "cpu_percent",
            "memory_percent",
            "storage_percent",
            "ru_percent",
            "server_load_percent",
        ],
        true,
    ),
];

fn render_pulse(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    let block = panel(
        "resource group pulse · bounded fleet",
        Some(chip(
            format!("{}h/{}", state.window_hours, interval_label(state)),
            state.color,
        )),
        state.color,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let counts = attention_counts(&snapshot.resources);
    let dim = style_muted(state.color);
    let mut count_spans = vec![
        Span::styled("res ", dim),
        Span::raw(snapshot.resources.len().to_string()),
        Span::styled("  run ", dim),
        Span::raw(snapshot.running_count().to_string()),
        Span::styled("  stop ", dim),
        Span::raw(snapshot.stopped_count().to_string()),
    ];
    for label in ["BAD", "WRN", "LIM", "ND", "CAP", "INV"] {
        count_spans.push(Span::styled(
            format!("  {} ", label.to_ascii_lowercase()),
            dim,
        ));
        count_spans.push(Span::raw(
            counts.get(label).copied().unwrap_or(0).to_string(),
        ));
    }
    if inner.width >= 100 {
        let capable = snapshot
            .resources
            .iter()
            .filter(|resource| resource.evidence_state != EvidenceState::InventoryOnly)
            .count();
        let sampled = snapshot
            .resources
            .iter()
            .filter(|resource| {
                resource.fleet_metrics.values().any(|metric| {
                    metric
                        .query
                        .matches(state.window_hours, state.interval_minutes)
                        && metric.query.cohort == snapshot.fleet_query.cohort
                        && metric.state == "available"
                })
            })
            .count();
        count_spans.extend([
            Span::styled("  sampled ", dim),
            Span::raw(format!("{sampled}/{capable}")),
        ]);
    }
    if inner.width >= 140 {
        let query_matches = snapshot
            .fleet_query
            .matches(state.window_hours, state.interval_minutes);
        let fleet_state = if query_matches {
            snapshot.fleet_state.as_str()
        } else if state.window_loading {
            "loading"
        } else {
            "no-match"
        };
        let read_age = if query_matches && !snapshot.fleet_query.queried_at.is_empty() {
            age(&snapshot.fleet_query.queried_at)
        } else {
            "—".into()
        };
        count_spans.extend([
            Span::styled("  fleet ", dim),
            Span::raw(if fleet_state.is_empty() {
                "unknown"
            } else {
                fleet_state
            }),
            Span::styled(" · read ", dim),
            Span::raw(read_age),
        ]);
    }
    frame.render_widget(
        Paragraph::new(Line::from(count_spans)),
        Rect { height: 1, ..inner },
    );
    let body = Rect {
        y: inner.y + 1,
        height: inner.height - 1,
        ..inner
    };
    if body.height == 0 {
        return;
    }
    let lanes = PULSE_LANES
        .iter()
        .map(|(label, names, maximum)| {
            (
                *label,
                first_aggregate(
                    snapshot,
                    names,
                    *maximum,
                    state.window_hours,
                    state.interval_minutes,
                ),
            )
        })
        .collect::<Vec<_>>();
    if lanes.iter().all(|(_, entry)| entry.is_none()) {
        // Stale lane series for a different window are hidden, never drawn
        // beneath the requested-window title; say so instead of going blank.
        let stale = snapshot.resources.iter().any(|resource| {
            resource.fleet_metrics.iter().any(|(name, metric)| {
                metric.state == "available"
                    && (!metric
                        .query
                        .matches(state.window_hours, state.interval_minutes)
                        || metric.query.cohort != snapshot.fleet_query.cohort)
                    && PULSE_LANES
                        .iter()
                        .any(|(_, names, _)| names.contains(&name.as_str()))
            })
        });
        let mut lines = vec![Line::styled(
            if stale {
                window_state_message(state)
            } else {
                "? NO COMPARABLE METRIC SERIES · no health inference".into()
            },
            style_unknown(state.color),
        )];
        for signal in snapshot
            .details
            .signals
            .iter()
            .take(body.height.saturating_sub(1) as usize)
        {
            lines.push(Line::from(format!(
                "{} {} · {} [{}]",
                state_symbol(&signal.state),
                signal.name,
                signal.detail,
                if signal.scope.is_empty() {
                    "scope unknown"
                } else {
                    &signal.scope
                }
            )));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), body);
        return;
    }
    if body.height >= 6 && body.width >= 84 {
        render_pulse_graphs(frame, body, &lanes, &snapshot.details.changes, state);
    } else {
        render_pulse_compact(frame, body, &lanes, &snapshot.details.changes, state);
    }
}

/// Lane summary with the coherent provider family and contributor/sample
/// coverage, so an aggregate is never mistaken for "all resource traffic".
fn lane_summary(label: &str, lane: &LaneAggregate, color: bool) -> Line<'static> {
    let mut line = metric_summary_line(label, lane.name, &lane.series, color);
    let filled = lane.series.values.iter().flatten().count();
    line.push_span(Span::styled(
        format!(
            "  {} · {}res · {}/{} bins",
            lane.family,
            lane.contributors,
            filled,
            lane.series.values.len()
        ),
        style_muted(color),
    ));
    line
}

/// One full lane column: summary, an area graph spanning the whole lane
/// width, and (space permitting) change markers plus the source line. Lanes
/// are never overlaid on a shared scale; a lane without data is an explicit
/// state rather than an empty chart.
fn render_pulse_lane(
    frame: &mut Frame,
    rect: Rect,
    label: &str,
    lane: Option<&LaneAggregate>,
    changes: &[ChangePoint],
    state: &UiState,
    primary: bool,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let Some(lane) = lane else {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{label} "), style_header(state.color)),
                Span::styled("no comparable series", style_muted(state.color)),
            ])),
            rect,
        );
        return;
    };
    let series = &lane.series;
    let mut lines = vec![lane_summary(label, lane, state.color)];
    let source_row = rect.height >= 6;
    let change_row = primary && !changes.is_empty() && rect.height >= 7;
    let graph_rows = (rect.height as usize)
        .saturating_sub(1 + usize::from(source_row) + usize::from(change_row))
        .max(1);
    for row in braille_graph(
        &series.values,
        rect.width as usize,
        graph_rows,
        scale_top(lane.name),
    ) {
        lines.push(Line::styled(row, style_metric(label, state.color)));
    }
    if change_row {
        lines.push(Line::styled(
            change_timeline(series, changes, rect.width as usize),
            style_change(state.color),
        ));
    }
    if source_row {
        let change_count = changes.iter().map(|point| point.count).sum::<u64>();
        lines.push(Line::styled(
            format!(
                "SOURCE {} · {} · {} bins · last {}{}",
                series.source,
                series.window,
                series.interval,
                age(series.latest_timestamp()),
                if primary && change_count > 0 {
                    format!(" · Δ {change_count} resource changes/24h")
                } else {
                    String::new()
                }
            ),
            style_muted(state.color),
        ));
    }
    frame.render_widget(Paragraph::new(lines), rect);
}

/// btop-style composition. Present lanes own the canvas; unsupported lanes
/// remain an explicit compact rail instead of consuming three quarters of an
/// ultrawide screen.
fn render_pulse_graphs(
    frame: &mut Frame,
    body: Rect,
    lanes: &[(&'static str, Option<LaneAggregate>)],
    changes: &[ChangePoint],
    state: &UiState,
) {
    let primary = lanes
        .iter()
        .position(|(_, entry)| entry.is_some())
        .unwrap_or(0);
    if body.width >= 220 {
        let present = lanes
            .iter()
            .enumerate()
            .filter(|(_, (_, lane))| lane.is_some())
            .collect::<Vec<_>>();
        let missing = lanes
            .iter()
            .filter(|(_, lane)| lane.is_none())
            .collect::<Vec<_>>();
        let regions = if missing.is_empty() {
            vec![body]
        } else {
            Layout::horizontal([Constraint::Percentage(88), Constraint::Percentage(12)])
                .spacing(2)
                .split(body)
                .to_vec()
        };
        let graph_area = regions[0];
        let present_count = present.len().max(1) as u32;
        let constraints = (0..present_count)
            .map(|_| Constraint::Ratio(1, present_count))
            .collect::<Vec<_>>();
        let columns = Layout::horizontal(constraints).spacing(2).split(graph_area);
        for (column, (index, (label, lane))) in columns.iter().zip(present) {
            render_pulse_lane(
                frame,
                *column,
                label,
                lane.as_ref(),
                changes,
                state,
                index == primary,
            );
        }
        if let Some(rail) = regions.get(1).copied() {
            let lines = missing
                .into_iter()
                .map(|(label, _)| {
                    Line::from(vec![
                        Span::styled(format!("{label} "), style_header(state.color)),
                        Span::styled("NO DATA", style_muted(state.color)),
                    ])
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), rail);
        }
        return;
    }
    let columns = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
        .spacing(1)
        .split(body);
    let (label, entry) = &lanes[primary];
    render_pulse_lane(
        frame,
        columns[0],
        label,
        entry.as_ref(),
        changes,
        state,
        true,
    );
    let rail = columns[1];
    let rail_lanes = lanes
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != primary)
        .map(|(_, lane)| lane)
        .collect::<Vec<_>>();
    if rail_lanes.is_empty() || rail.width == 0 {
        return;
    }
    let base_height = rail.height as usize / rail_lanes.len();
    let mut extra = rail.height as usize % rail_lanes.len();
    let mut lines = Vec::with_capacity(rail.height as usize);
    for (label, entry) in rail_lanes {
        let lane_height = base_height + usize::from(extra > 0);
        extra = extra.saturating_sub(1);
        if lane_height == 0 {
            continue;
        }
        match entry {
            Some(lane) => {
                lines.push(lane_summary(label, lane, state.color));
                if lane_height > 1 {
                    for row in braille_graph(
                        &lane.series.values,
                        rail.width as usize,
                        lane_height - 1,
                        scale_top(lane.name),
                    ) {
                        lines.push(Line::styled(row, style_metric(label, state.color)));
                    }
                }
            }
            None => {
                lines.push(Line::from(vec![
                    Span::styled(format!("{label} "), style_header(state.color)),
                    Span::styled("no comparable series", style_muted(state.color)),
                ]));
                for _ in 1..lane_height {
                    lines.push(Line::default());
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), rail);
}

fn render_pulse_compact(
    frame: &mut Frame,
    body: Rect,
    lanes: &[(&'static str, Option<LaneAggregate>)],
    changes: &[ChangePoint],
    state: &UiState,
) {
    let capacity = (body.height as usize / 2).max(1);
    let mut lines = Vec::new();
    let mut source = None;
    for (label, entry) in lanes {
        let Some(lane) = entry else {
            continue;
        };
        if source.is_none() {
            source = Some(&lane.series);
        }
        lines.push(lane_summary(label, lane, state.color));
        lines.push(Line::styled(
            sparkline(&lane.series.values, body.width.saturating_sub(2) as usize),
            style_metric(label, state.color),
        ));
        if lines.len() / 2 >= capacity {
            break;
        }
    }
    if let Some(source) = source.filter(|_| lines.len() < body.height as usize) {
        lines.push(Line::styled(
            format!(
                "SOURCE {} · {} · {} bins · last {}",
                source.source,
                source.window,
                source.interval,
                age(source.latest_timestamp())
            ),
            style_muted(state.color),
        ));
    }
    if !changes.is_empty() && lines.len() < body.height as usize {
        lines.push(Line::styled(
            format!(
                "Δ {} resource changes/24h · fixed safe aggregate",
                changes.iter().map(|point| point.count).sum::<u64>()
            ),
            style_change(state.color),
        ));
    }
    frame.render_widget(Paragraph::new(lines), body);
}

fn render_selected(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    let selected = selected_resource(snapshot, state);
    let Some(resource) = selected else {
        frame.render_widget(
            Paragraph::new("? no visible resource in this scope/filter").block(panel(
                "selected resource",
                None,
                state.color,
            )),
            area,
        );
        return;
    };
    let (attention, reason) = resource_attention(resource);
    let dim = style_muted(state.color);
    let block = panel(
        "selected resource metrics",
        Some(Line::from(Span::styled(
            format!(" {attention} "),
            style_attention(attention, state.color),
        ))),
        state.color,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let header = vec![
        Line::from(vec![
            Span::styled(
                if resource.watched { "★ " } else { "" },
                style_header(state.color),
            ),
            Span::styled(
                if resource.watch_alias.is_empty() {
                    resource.name.clone()
                } else {
                    format!("{} ({})", resource.watch_alias, resource.name)
                },
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" [{}]", resource.category), dim),
            Span::styled(
                format!("  {} · ", short_control(&resource.control_state)),
                dim,
            ),
            Span::styled(attention, style_attention(attention, state.color)),
        ]),
        Line::styled(format!("ATTENTION {reason}"), dim),
        Line::styled(
            format!(
                "EVID {} {} · sample {}",
                resource.evidence_label(),
                resource.evidence_detail,
                last_seen(resource)
            ),
            style_evidence(resource.evidence_state, state.color),
        ),
    ];
    let mut tail = Vec::new();
    if resource.evidence_state == EvidenceState::InventoryOnly {
        tail.push(Line::styled(
            "INV INVENTORY ONLY · no fixed metric adapter",
            style_unknown(state.color),
        ));
    } else if matches!(
        resource.evidence_state,
        EvidenceState::NoData | EvidenceState::Limited | EvidenceState::NotSampled
    ) {
        tail.push(Line::styled(
            format!("{} {}", resource.evidence_label(), resource.evidence_detail),
            style_evidence(resource.evidence_state, state.color),
        ));
    }
    if !resource.hosting_plan_id.is_empty() && backing_plan(snapshot, resource).is_none() {
        tail.push(Line::styled(
            "LIM SHARED PLAN outside visible scope or unavailable",
            style_warning(state.color),
        ));
    }
    if !resource.relationships.is_empty() {
        tail.push(Line::styled(
            format!("REL {}", relationship_summary(resource)),
            style_accent(state.color),
        ));
    }
    tail.push(Line::from(vec![
        Span::styled("AZ ", dim),
        Span::raw(short_state(&resource.resource_health_state)),
        Span::styled("  APP ", dim),
        Span::raw(short_state(&resource.health_state)),
        Span::styled("  PROV ", dim),
        Span::raw(short_state(&resource.provisioning_state)),
        Span::styled("  DIAG ", dim),
        Span::raw(short_state(&resource.diagnostic_state)),
    ]));
    let header_height = (header.len() as u16).min(inner.height);
    frame.render_widget(
        Paragraph::new(header),
        Rect {
            height: header_height,
            ..inner
        },
    );
    let tail_height = (tail.len() as u16).min(inner.height - header_height);
    if tail_height > 0 {
        frame.render_widget(
            Paragraph::new(tail),
            Rect {
                y: inner.y + inner.height - tail_height,
                height: tail_height,
                ..inner
            },
        );
    }
    let metrics_area = Rect {
        y: inner.y + header_height,
        height: inner.height - header_height - tail_height,
        ..inner
    };
    let series = selected_series(snapshot, resource, state);
    if metrics_area.height == 0 {
        return;
    }
    if series.is_empty() {
        // Available series for another window are hidden, not drawn beneath
        // the requested-window chrome; the metrics region says why.
        if resource
            .metrics
            .values()
            .any(|metric| metric.state == "available")
        {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    window_state_message(state),
                    style_unknown(state.color),
                )),
                Rect {
                    height: 1,
                    ..metrics_area
                },
            );
        }
        return;
    }
    // Keep enough vertical space for each visible metric to remain a chart,
    // rather than collapsing a long provider adapter into summary-only rows.
    // All adapter metrics remain eligible and become visible as the terminal
    // grows; the cap keeps an unusually large/future adapter legible.
    let available = metrics_area.height as usize;
    let max_visible = available.div_ceil(3).clamp(1, 8);
    let count = max_visible.min(series.len());
    let hidden = series.len().saturating_sub(count);
    let block_height = available / count;
    let extra_rows = available % count;
    let mut y = metrics_area.y;
    for (index, (label, name, metric)) in series.into_iter().take(count).enumerate() {
        let height = (block_height + usize::from(index < extra_rows)) as u16;
        let rect = Rect {
            y,
            height,
            ..metrics_area
        };
        y += height;
        let mut summary = metric_summary_line(&label, name, metric, state.color);
        if name.ends_with("_percent") {
            if let Some(value) = metric.latest() {
                summary.push_span(Span::raw("  "));
                summary.push_span(Span::styled(meter(value, 10), style_accent(state.color)));
            }
        }
        summary.push_span(Span::styled(
            format!("  {}/{}", metric.window, metric.interval),
            style_muted(state.color),
        ));
        if index + 1 == count && hidden > 0 {
            summary.push_span(Span::styled(
                format!("  +{hidden} more · resize"),
                style_muted(state.color),
            ));
        }
        let mut lines = vec![summary];
        if height >= 3 {
            // Silhouette line, not an area fill: a near-constant series reads
            // as a level line instead of an opaque wall dominating the panel.
            for row in braille_line_graph(
                &metric.values,
                rect.width as usize,
                height as usize - 1,
                scale_top(name),
            ) {
                lines.push(Line::styled(row, style_metric(&label, state.color)));
            }
        } else if height == 2 {
            lines.push(Line::styled(
                sparkline(&metric.values, rect.width as usize),
                style_metric(&label, state.color),
            ));
        }
        frame.render_widget(Paragraph::new(lines), rect);
    }
}

fn render_attention(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    let resources = visible_resources(snapshot, state);
    let selected = resources
        .iter()
        .position(|resource| resource.resource_id == state.selected_id)
        .unwrap_or(0);
    let visible_rows = area.height.saturating_sub(4) as usize;
    let start = selected
        .saturating_sub(visible_rows / 2)
        .min(resources.len().saturating_sub(visible_rows));
    let wide = area.width >= 94;
    let ultra = area.width >= 140;
    let dim = style_muted(state.color);
    let rows = resources
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, resource)| {
            let attention = resource_attention(resource).0;
            let badge = style_attention(attention, state.color);
            let mut cells = vec![
                Cell::from(if index == selected { "›" } else { " " }),
                Cell::from(if resource.watched { "★" } else { " " })
                    .style(style_header(state.color)),
                Cell::from(attention).style(badge),
                Cell::from(resource.name.clone()),
            ];
            if wide {
                cells.push(Cell::from(resource.type_label().to_string()).style(dim));
                cells.push(Cell::from(short_control(&resource.control_state)).style(dim));
                cells.push(Cell::from(short_state(&resource.resource_health_state)).style(dim));
                cells.push(Cell::from(short_state(&resource.health_state)).style(dim));
                cells.push(
                    Cell::from(resource.evidence_label())
                        .style(style_evidence(resource.evidence_state, state.color)),
                );
                cells.push(Cell::from(primary_signal(resource)).style(badge));
                if ultra {
                    cells.push(
                        Cell::from(
                            if resource.version.is_empty()
                                || resource.version.eq_ignore_ascii_case("unknown")
                            {
                                "—".into()
                            } else {
                                resource.version.clone()
                            },
                        )
                        .style(dim),
                    );
                }
                cells.push(Cell::from(Text::from(last_seen(resource)).right_aligned()).style(dim));
            } else {
                cells.push(Cell::from(short_control(&resource.control_state)).style(dim));
                cells.push(
                    Cell::from(resource.evidence_label())
                        .style(style_evidence(resource.evidence_state, state.color)),
                );
            }
            let row = Row::new(cells);
            if index == selected {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        });
    // RESOURCE fits the longest visible name but never absorbs all extra
    // width: operational columns stay grouped next to it and leftover
    // ultrawide space is left blank at the right edge.
    let name_width = resources
        .iter()
        .map(|resource| resource.name.chars().count() + 2)
        .max()
        .unwrap_or(18)
        .clamp(18, if ultra { 45 } else { 42 })
        .min(
            (area.width as usize)
                .saturating_sub(if ultra { 96 } else { 73 })
                .max(18),
        ) as u16;
    let (type_width, signal_width) = if area.width >= 200 {
        (22, 18)
    } else {
        (14, 12)
    };
    let age_label = if area.width >= 100 {
        "DATA AGE"
    } else {
        "DATA"
    };
    let widths = if ultra {
        vec![
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(5),
            Constraint::Length(name_width),
            Constraint::Length(type_width),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(signal_width),
            Constraint::Length(18),
            Constraint::Length(8),
        ]
    } else if wide {
        vec![
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(5),
            Constraint::Length(name_width),
            Constraint::Length(type_width),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(signal_width),
            Constraint::Length(8),
        ]
    } else {
        vec![
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(5),
            Constraint::Min(14),
            Constraint::Length(7),
            Constraint::Length(6),
        ]
    };
    let header = if ultra {
        Row::new([
            "", "★", "PRI", "RESOURCE", "TYPE", "CTRL", "AZ", "APP", "EVID", "SIGNAL", "VERSION",
            age_label,
        ])
    } else if wide {
        Row::new([
            "", "★", "PRI", "RESOURCE", "TYPE", "CTRL", "AZ", "APP", "EVID", "SIGNAL", age_label,
        ])
    } else {
        Row::new(["", "★", "PRI", "RESOURCE", "CTRL", "EVID"])
    }
    .style(style_header(state.color));
    frame.render_widget(
        Table::new(rows, widths).header(header).block(panel(
            "attention queue",
            Some(chip(
                format!(
                    "{} · sort={}{} · filter={}{}",
                    if state.view == ViewMode::Operations {
                        format!(
                            "{}/{} operational",
                            resources.len(),
                            table_scope_count(snapshot, state)
                        )
                    } else {
                        format!("{} resources", resources.len())
                    },
                    state.sort.label(),
                    if state.reverse { "↓" } else { "↑" },
                    CATEGORIES[state.category_index],
                    if state.watchlist_only {
                        " · WATCH"
                    } else {
                        ""
                    }
                ),
                state.color,
            )),
            state.color,
        )),
        area,
    );
}

fn render_recent_changes(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    let available = area.height.saturating_sub(2) as usize;
    let changes = snapshot
        .details
        .recent_changes
        .iter()
        .take(available)
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    if changes.is_empty() {
        let (state_label, detail) = recent_changes_status(snapshot);
        lines.push(Line::styled(
            format!("{state_label} · {detail}"),
            style_muted(state.color),
        ));
    } else {
        for change in changes {
            let type_label = change
                .resource_type
                .rsplit('/')
                .next()
                .unwrap_or(&change.resource_type);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<7}", age(&change.timestamp)),
                    style_muted(state.color),
                ),
                Span::styled(format!("{:<8}", change.event), style_change(state.color)),
                Span::raw(format!(
                    "{} [{}]",
                    truncate(
                        &change.resource_name,
                        area.width.saturating_sub(31) as usize
                    ),
                    truncate(type_label, 16)
                )),
            ]));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel(
            "recent changes",
            Some(chip("24h · metadata only · d details".into(), state.color)),
            state.color,
        )),
        area,
    );
}

fn format_recent_change(change: &RecentChange) -> String {
    let type_label = change
        .resource_type
        .rsplit('/')
        .next()
        .unwrap_or(&change.resource_type);
    format!(
        "{:<9} {:<8} {:<28} {:<20} {} · {}",
        age(&change.timestamp),
        truncate(&change.event, 8),
        truncate(&change.resource_name, 28),
        truncate(type_label, 20),
        change.detail,
        change.source
    )
}

fn recent_changes_status(snapshot: &Snapshot) -> (&'static str, String) {
    if let Some(signal) = snapshot
        .details
        .signals
        .iter()
        .find(|signal| signal.name == "recent change events")
    {
        return match signal.state.as_str() {
            "unavailable" | "limited" => ("UNAVAILABLE", signal.detail.clone()),
            "no_data" => (
                "NO DATA",
                "query succeeded with no safe metadata change records in 24h".into(),
            ),
            "signal" | "available" => ("AVAILABLE", signal.detail.clone()),
            _ => ("UNKNOWN", signal.detail.clone()),
        };
    }
    match snapshot.enrichment_state.as_str() {
        "pending" | "updating" => (
            "PENDING",
            "fixed recent-change projection has not completed".into(),
        ),
        "disabled" => ("DISABLED", "bounded enrichment is disabled".into()),
        "unavailable" => (
            "UNAVAILABLE",
            "bounded enrichment is permission or API limited".into(),
        ),
        _ => (
            "UNKNOWN",
            "no recent-change query state is available".into(),
        ),
    }
}

fn render_inventory(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    if area.width >= 108 {
        let rows =
            Layout::vertical([Constraint::Percentage(52), Constraint::Percentage(48)]).split(area);
        render_attention(frame, rows[0], snapshot, state);
        let columns = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);
        render_inventory_metrics(frame, columns[0], snapshot, state);
        render_operations_signals(frame, columns[1], snapshot, state);
    } else {
        let rows =
            Layout::vertical([Constraint::Percentage(65), Constraint::Percentage(35)]).split(area);
        render_attention(frame, rows[0], snapshot, state);
        render_inventory_metrics(frame, rows[1], snapshot, state);
    }
}

fn render_inventory_metrics(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    let window = window_label(state);
    let grain = interval_label(state);
    let mut lines = Vec::new();
    for (name, maximum) in [
        ("requests", false),
        ("http_5xx", false),
        ("cpu_percent", true),
        ("memory_percent", true),
    ] {
        if let Some(lane) = first_aggregate(
            snapshot,
            &[name],
            maximum,
            state.window_hours,
            state.interval_minutes,
        ) {
            let metric = &lane.series;
            lines.push(Line::from(format!(
                "{name:<18} {} {} · {}",
                sparkline(&metric.values, area.width.saturating_sub(30) as usize),
                format_value(metric.display_value(), &metric.unit, name),
                lane.family
            )));
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            format!("? no bounded aggregate series matching {window}/{grain}"),
            style_unknown(state.color),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel("bounded aggregates", None, state.color)),
        area,
    );
}

fn render_operations_signals(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    let mut lines = snapshot
        .details
        .signals
        .iter()
        .map(|signal| {
            Line::from(format!(
                "{} {} · {} [{} · {} · {}]",
                state_symbol(&signal.state),
                signal.name,
                signal.detail,
                signal.source,
                signal.window,
                if signal.scope.is_empty() {
                    "scope unknown"
                } else {
                    &signal.scope
                }
            ))
        })
        .collect::<Vec<_>>();
    lines.extend(
        snapshot
            .details
            .limitations
            .iter()
            .map(|limitation| Line::from(format!("LIM {limitation}"))),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("operations signals / limits", None, state.color))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_overlay(frame: &mut Frame, area: Rect, snapshot: &Snapshot, state: &UiState) {
    let (title, lines, width, height) = match &state.overlay {
        Overlay::None => return,
        Overlay::Help => (
            " HELP ",
            vec![
                "j/k or arrows   select resource".into(),
                "Enter           resource detail".into(),
                "d               recent changes for selected resource".into(),
                "l               aggregate log signals".into(),
                "Shift+L         raw-log safety status".into(),
                "g / s           group / subscription chooser".into(),
                "[ / ]           previous / next group".into(),
                "o / O           sort / reverse".into(),
                "f               category filter".into(),
                "t / T           session star / watchlist only".into(),
                "v               operations / inventory".into(),
                "w               1h/1m, 6h/5m, 24h/15m".into(),
                "m / r           metrics toggle / refresh".into(),
                "q / Ctrl-C      quit and restore terminal".into(),
            ],
            70,
            18,
        ),
        Overlay::Detail => {
            let resource = selected_resource(snapshot, state);
            (
                " SELECTED RESOURCE ",
                resource.map_or_else(
                    || vec!["? no selected resource".into()],
                    |resource| {
                        let mut lines = vec![
                            format!(
                                "{}{} [{}]",
                                if resource.watched { "★ " } else { "" },
                                resource.name,
                                resource.category
                            ),
                            format!("TYPE {}", resource.resource_type),
                            format!(
                                "CONTROL {} · PROVISION {}",
                                resource.control_state, resource.provisioning_state
                            ),
                            format!(
                                "AZURE HEALTH {} · APP HEALTH {}",
                                resource.resource_health_state, resource.health_state
                            ),
                            format!("DIAGNOSTICS {}", resource.diagnostic_detail),
                            format!(
                                "EVIDENCE {} · {}",
                                resource.evidence_label(),
                                resource.evidence_detail
                            ),
                            format!("VERSION {}", resource.version),
                        ];
                        if !resource.watch_expected_control.is_empty() {
                            lines.push(format!(
                                "WATCH expected control {}",
                                resource.watch_expected_control
                            ));
                        }
                        for relation in &resource.relationships {
                            lines.push(format!(
                                "REL {} {} · {}",
                                relation.direction, relation.kind, relation.resource_name
                            ));
                        }
                        for (name, metric) in &resource.metrics {
                            lines.push(format!(
                                "{} {} · {} · {} {}",
                                state_symbol(&metric.state),
                                name,
                                format_value(metric.display_value(), &metric.unit, name),
                                metric.window,
                                metric.interval
                            ));
                        }
                        lines
                    },
                ),
                90,
                24,
            )
        }
        Overlay::RecentChanges => {
            let selected = selected_resource(snapshot, state);
            let selected_name = selected
                .map(|resource| resource.name.as_str())
                .unwrap_or("");
            let selected_type = selected
                .map(|resource| resource.resource_type.as_str())
                .unwrap_or("");
            let changes = snapshot
                .details
                .recent_changes
                .iter()
                .filter(|change| {
                    !selected_name.is_empty()
                        && change.resource_name.eq_ignore_ascii_case(selected_name)
                        && (change.resource_type.eq_ignore_ascii_case(selected_type)
                            || change.resource_type == "unresolved type")
                })
                .collect::<Vec<_>>();
            let mut lines = vec![
                format!(
                    "RESOURCE {}",
                    if selected_name.is_empty() {
                        "no selected resource"
                    } else {
                        selected_name
                    }
                ),
                "WINDOW 24h · CAP 20 · metadata only".into(),
                "SAFETY no actors, resource IDs, parameters, outputs, diffs, or payloads".into(),
                "SEMANTICS UPDATE is a generic Azure resource change, not a confirmed deployment"
                    .into(),
                String::new(),
            ];
            if changes.is_empty() {
                let (state_label, detail) = recent_changes_status(snapshot);
                lines.push(if snapshot.details.recent_changes.is_empty() {
                    format!("{state_label} · {detail}")
                } else {
                    "NO MATCHING DATA · absence does not mean no deployment or change occurred"
                        .into()
                });
            } else {
                lines.push(format!(
                    "{:<9} {:<8} {:<28} {:<20} {}",
                    "AGE", "EVENT", "RESOURCE", "TYPE", "EVIDENCE"
                ));
                for change in changes {
                    lines.push(format_recent_change(change));
                }
            }
            lines.extend([
                String::new(),
                "SOURCE Azure Resource Graph fixed projection + transitions observed by aztop"
                    .into(),
                "[d/Esc/Enter] close".into(),
            ]);
            (" RECENT CHANGES — SELECTED RESOURCE ", lines, 120, 32)
        }
        Overlay::Chooser(chooser) => {
            let title = match chooser.mode {
                ChooserMode::Group => " SELECT RESOURCE GROUP — ZERO-READ BROWSING ",
                ChooserMode::Subscription => " SELECT SUBSCRIPTION — ZERO-READ BROWSING ",
            };
            let choices = chooser_choices(snapshot, chooser);
            let mut lines = vec![format!(
                "FILTER {} · {} match{}",
                if chooser.query.is_empty() {
                    "(type to filter)"
                } else {
                    &chooser.query
                },
                choices.len(),
                if choices.len() == 1 { "" } else { "es" }
            )];
            for (index, (label, detail, current)) in choices.iter().enumerate() {
                lines.push(format!(
                    "{} {:<34} {}{}",
                    if index == chooser.selected {
                        "›"
                    } else {
                        " "
                    },
                    label,
                    detail,
                    if *current { " · current" } else { "" }
                ));
            }
            lines.push("Enter loads · Esc clears then cancels · browsing performs no reads".into());
            (title, lines, 100, 28)
        }
        Overlay::LogSignals {
            loading,
            result,
            error,
        } => {
            let mut lines = vec![
                "SAFETY fixed summarize-only KQL; no messages, URLs, operation names, identifiers, or raw rows".into(),
                format!("STATE {}", if *loading { "LOADING" } else if error.is_empty() { result.as_ref().map_or("NO DATA", |result| result.state.as_str()) } else { "UNAVAILABLE" }),
            ];
            if !error.is_empty() {
                lines.push(error.clone());
            }
            if let Some(result) = result {
                lines.extend([
                    format!(
                        "SOURCE {} · {} · {} bins",
                        result.source, result.window, result.interval
                    ),
                    format!(
                        "TOTAL {}  ERRORS {}  WARNINGS {}  EXCEPTIONS {}  FAILED DEPS {}",
                        result.total,
                        result.errors,
                        result.warnings,
                        result.exceptions,
                        result.failed_dependencies
                    ),
                    format!(
                        "LAST {}  INGEST LAG {}  QUERIES {} ok / {} unavailable",
                        if result.last_seen.is_empty() {
                            "no data"
                        } else {
                            &result.last_seen
                        },
                        result
                            .ingestion_lag_seconds
                            .map_or("unknown".into(), |seconds| format!("{seconds:.1}s")),
                        result.queried_workspaces,
                        result.unavailable_workspaces
                    ),
                    format!(
                        "events   {}",
                        sparkline(
                            &result
                                .counts
                                .iter()
                                .map(|value| Some(*value))
                                .collect::<Vec<_>>(),
                            60
                        )
                    ),
                    format!(
                        "errors   {}",
                        sparkline(
                            &result
                                .error_counts
                                .iter()
                                .map(|value| Some(*value))
                                .collect::<Vec<_>>(),
                            60
                        )
                    ),
                    format!(
                        "warnings {}",
                        sparkline(
                            &result
                                .warning_counts
                                .iter()
                                .map(|value| Some(*value))
                                .collect::<Vec<_>>(),
                            60
                        )
                    ),
                    "No health verdict is inferred from zero or absent log data.".into(),
                ]);
                for table in &result.tables {
                    lines.push(format!(
                        "{:<30} {:>8} events {:>8} errors {:>8} warnings",
                        table.name, table.total, table.errors, table.warnings
                    ));
                }
            }
            lines.push(
                "[l/Esc] close  [w] window  [r] refresh  aggregate only · no raw content".into(),
            );
            (" LOG SIGNALS — AGGREGATE ONLY ", lines, 120, 32)
        }
        Overlay::RawConfirm(target) => {
            let selected = selected_resource(snapshot, state);
            let mut lines = Vec::new();
            let title = if target.is_some() {
                lines.extend([
                    "Raw service output may contain secrets, prompts, customer data, identifiers, URLs, or payloads.".into(),
                    "Content stays in a 200-line memory-only buffer for at most 15 minutes and is never exported.".into(),
                ]);
                " RAW SERVICE LOGS — SENSITIVE "
            } else {
                lines.push(
                    "Direct raw service logs are disabled; this screen is an explanation only."
                        .into(),
                );
                " RAW SERVICE LOGS — BLOCKED "
            };
            if let Some(resource) = selected {
                lines.push(format!(
                    "RESOURCE {} [{}]",
                    resource.name,
                    resource.type_label()
                ));
            }
            if let Some(target) = target {
                lines.push(format!("PROVIDER {}", target.provider));
                lines.push(format!("SOURCE {}", target.description));
                lines.push(
                    "Press y to connect. Any other answer cancels without an Azure read.".into(),
                );
            } else {
                if selected.is_some_and(|resource| {
                    matches!(
                        resource.resource_type.to_ascii_lowercase().as_str(),
                        "microsoft.web/sites"
                            | "microsoft.web/sites/slots"
                            | "microsoft.app/containerapps"
                    )
                }) {
                    lines.push(
                        "BLOCKED · Azure CLI's stream path reads credentials or full service configuration before returning logs; disabled by the safety boundary.".into(),
                    );
                } else {
                    lines.push(
                        "UNSUPPORTED · no fixed direct stream for this resource type.".into(),
                    );
                }
                lines
                    .push("No Azure read was attempted. Aggregate log signals remain on l.".into());
            }
            (title, lines, 100, 15)
        }
        Overlay::RawLogs(raw) => {
            let mut lines = vec![
                format!("{} · {}", raw.resource_name, raw.provider),
                format!("STATE {} · {}", raw.status, raw.detail),
                "SENSITIVE · memory only · never serialized".into(),
            ];
            lines.extend(raw.lines.iter().cloned());
            lines.push("[Esc/L] stop and close  [r] reconnect  [q] quit".into());
            (" RAW SERVICE LOGS — SENSITIVE ", lines, 130, 38)
        }
    };
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(style_muted(state.color))
                    .title(Line::styled(title, style_header(state.color))),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

pub fn chooser_choices(snapshot: &Snapshot, chooser: &ChooserState) -> Vec<(String, String, bool)> {
    let query = chooser.query.to_ascii_lowercase();
    match chooser.mode {
        ChooserMode::Group => snapshot
            .resource_groups
            .iter()
            .filter(|group| group.name.to_ascii_lowercase().contains(&query))
            .map(|group| {
                (
                    group.name.clone(),
                    format!("{} · {}", group.location, group.provisioning_state),
                    group.name == snapshot.selected_resource_group,
                )
            })
            .collect(),
        ChooserMode::Subscription => snapshot
            .subscriptions
            .iter()
            .filter(|subscription| subscription.name.to_ascii_lowercase().contains(&query))
            .map(|subscription| {
                (
                    subscription.name.clone(),
                    subscription.cloud.clone(),
                    subscription.subscription_id == snapshot.selected_subscription_id,
                )
            })
            .collect(),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(10);
    let height = height.min(area.height.saturating_sub(2)).max(5);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

pub fn visible_resources<'a>(snapshot: &'a Snapshot, state: &UiState) -> Vec<&'a AzureResource> {
    let category = CATEGORIES[state.category_index];
    let candidates = snapshot
        .resources
        .iter()
        .filter(|resource| {
            (category == "all" || resource.category == category)
                && (!state.watchlist_only || resource.watched)
        })
        .collect::<Vec<_>>();
    let operational_rows = state.view == ViewMode::Operations
        && candidates
            .iter()
            .any(|resource| resource_attention(resource).0 != "INV");
    let mut resources = candidates
        .into_iter()
        .filter(|resource| !operational_rows || resource_attention(resource).0 != "INV")
        .collect::<Vec<_>>();
    resources.sort_by(|left, right| resource_order(left, right, state.sort));
    if state.reverse {
        resources.reverse();
    }
    resources
}

fn table_scope_count(snapshot: &Snapshot, state: &UiState) -> usize {
    let category = CATEGORIES[state.category_index];
    snapshot
        .resources
        .iter()
        .filter(|resource| {
            (category == "all" || resource.category == category)
                && (!state.watchlist_only || resource.watched)
        })
        .count()
}

fn resource_order(left: &AzureResource, right: &AzureResource, sort: SortKey) -> Ordering {
    match sort {
        SortKey::Attention => attention_rank(left)
            .cmp(&attention_rank(right))
            .then_with(|| right.watched.cmp(&left.watched))
            .then_with(|| {
                activity_score(right)
                    .partial_cmp(&activity_score(left))
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
            }),
        SortKey::Name => left
            .name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase()),
        SortKey::Category => left
            .category
            .cmp(&right.category)
            .then_with(|| left.name.cmp(&right.name)),
        SortKey::Control => left
            .control_state
            .cmp(&right.control_state)
            .then_with(|| left.name.cmp(&right.name)),
        SortKey::Signal => left
            .signal_state()
            .cmp(right.signal_state())
            .then_with(|| left.name.cmp(&right.name)),
        SortKey::Changed => left
            .changed_at
            .cmp(&right.changed_at)
            .then_with(|| left.name.cmp(&right.name)),
    }
}

pub fn selected_resource<'a>(snapshot: &'a Snapshot, state: &UiState) -> Option<&'a AzureResource> {
    let resources = visible_resources(snapshot, state);
    resources
        .iter()
        .find(|resource| resource.resource_id == state.selected_id)
        .copied()
        .or_else(|| resources.first().copied())
}

pub fn initial_selection(snapshot: &Snapshot, state: &UiState) -> String {
    let resources = visible_resources(snapshot, state);
    if let Some(resource) = resources
        .iter()
        .find(|resource| matches!(resource_attention(resource).0, "BAD" | "WRN"))
    {
        return resource.resource_id.clone();
    }
    resources
        .iter()
        .filter(|resource| {
            resource
                .metrics
                .values()
                .any(|metric| metric.state == "available")
        })
        .max_by(|left, right| {
            activity_score(left)
                .partial_cmp(&activity_score(right))
                .unwrap_or(Ordering::Equal)
        })
        .or_else(|| resources.first())
        .map(|resource| resource.resource_id.clone())
        .unwrap_or_default()
}

pub fn resource_attention(resource: &AzureResource) -> (&'static str, String) {
    let health = resource.health_state.to_ascii_lowercase();
    let azure = resource.resource_health_state.to_ascii_lowercase();
    let provisioning = resource.provisioning_state.to_ascii_lowercase();
    if health == "unhealthy" {
        return ("BAD", "application health unhealthy".into());
    }
    if azure == "unavailable" {
        return ("BAD", "Azure Resource Health unavailable".into());
    }
    if matches!(provisioning.as_str(), "failed" | "canceled" | "cancelled") {
        return (
            "BAD",
            format!("provisioning {}", resource.provisioning_state),
        );
    }
    if resource.watched && !resource.watch_expected_control.is_empty() {
        let control = resource.control_state.to_ascii_lowercase();
        if matches!(control.as_str(), "running" | "stopped")
            && control != resource.watch_expected_control
        {
            return (
                "BAD",
                format!(
                    "watched: expected {}, is {}",
                    resource.watch_expected_control, control
                ),
            );
        }
    }
    if health == "degraded" || azure == "degraded" {
        return ("WRN", "explicit degraded health".into());
    }
    if resource.watched && resource.diagnostic_state == "not_configured" {
        return ("WRN", "watched resource has no enabled diagnostics".into());
    }
    if resource.evidence_state == EvidenceState::Signal {
        if let Some(detail) = failure_warning(resource) {
            return ("WRN", detail);
        }
    }
    if !matches!(provisioning.as_str(), "succeeded" | "unknown" | "") {
        return (
            "WRN",
            format!("provisioning {}", resource.provisioning_state),
        );
    }
    if resource.control_state.eq_ignore_ascii_case("stopped")
        && !(resource.watched
            && resource
                .watch_expected_control
                .eq_ignore_ascii_case("stopped"))
    {
        return ("STOP", "provider control state stopped".into());
    }
    if resource.watched
        && !resource.watch_expected_control.is_empty()
        && !matches!(
            resource.control_state.to_ascii_lowercase().as_str(),
            "running" | "stopped"
        )
    {
        return (
            "LIM",
            "watched control expectation cannot be evaluated for this resource".into(),
        );
    }
    if health == "unavailable"
        || resource.evidence_state == EvidenceState::Limited
        || (resource.diagnostic_state == "unavailable"
            && resource.evidence_state != EvidenceState::Signal)
    {
        return ("LIM", "permission or provider API limited".into());
    }
    if health == "healthy" {
        return ("OK", "explicit positive application-health evidence".into());
    }
    if azure == "available" {
        return (
            "SIG",
            "Azure Resource Health available; application health unknown".into(),
        );
    }
    match resource.evidence_state {
        EvidenceState::Signal => ("SIG", "bounded metric evidence".into()),
        EvidenceState::Limited => ("LIM", resource.evidence_detail.clone()),
        EvidenceState::Pending => ("PEND", resource.evidence_detail.clone()),
        EvidenceState::NoData => ("ND", resource.evidence_detail.clone()),
        EvidenceState::NotSampled => ("CAP", resource.evidence_detail.clone()),
        EvidenceState::InventoryOnly => ("INV", resource.evidence_detail.clone()),
    }
}

fn available_metric_value(resource: &AzureResource, name: &str) -> Option<f64> {
    resource
        .metrics
        .get(name)
        .filter(|metric| metric.state == "available")
        .and_then(MetricSeries::display_value)
        .filter(|value| value.is_finite())
}

fn failure_warning(resource: &AzureResource) -> Option<String> {
    for (failures_name, volume_name) in [("http_5xx", "requests"), ("total_errors", "total_calls")]
    {
        let Some(failures) = available_metric_value(resource, failures_name) else {
            continue;
        };
        if failures <= 0.0 {
            continue;
        }
        let volume = available_metric_value(resource, volume_name);
        let ratio = volume
            .filter(|volume| *volume > 0.0)
            .map(|volume| failures / volume);
        // A handful of isolated errors remains visible as evidence in the
        // SIGNAL column without promoting the whole resource to warning.
        // Promote when the bounded window has a material absolute count or
        // at least five errors at a 1%+ rate.
        if failures >= 100.0 || (failures >= 5.0 && ratio.is_none_or(|ratio| ratio >= 0.01)) {
            let ratio_detail = ratio
                .map(|ratio| format!(" · {:.2}%", ratio * 100.0))
                .unwrap_or_default();
            return Some(format!(
                "{failures_name} {}{ratio_detail}",
                format_value(Some(failures), "", failures_name)
            ));
        }
    }
    if let Some(percent) = available_metric_value(resource, "http_5xx_percent") {
        if percent >= 1.0 {
            return Some(format!("http_5xx_percent {percent:.2}%"));
        }
    }
    for name in [
        "errors",
        "runs_failed",
        "runs_throttled",
        "deadlocks",
        "evicted_keys",
    ] {
        if let Some(value) = available_metric_value(resource, name) {
            if value > 0.0 {
                return Some(format!("{name} {}", format_value(Some(value), "", name)));
            }
        }
    }
    if let Some(percent) = available_metric_value(resource, "search_throttled_percent") {
        if percent >= 1.0 {
            return Some(format!("search_throttled_percent {percent:.2}%"));
        }
    }
    None
}

fn attention_rank(resource: &AzureResource) -> usize {
    match resource_attention(resource).0 {
        "BAD" => 0,
        "WRN" => 1,
        "STOP" => 2,
        "LIM" => 3,
        "PEND" => 4,
        "ND" => 5,
        "CAP" => 6,
        "SIG" => 7,
        "OK" => 8,
        "INV" => 9,
        _ => 10,
    }
}

fn attention_counts(resources: &[AzureResource]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for resource in resources {
        *counts.entry(resource_attention(resource).0).or_default() += 1;
    }
    counts
}

fn activity_score(resource: &AzureResource) -> f64 {
    [
        "requests",
        "total_calls",
        "transactions",
        "api_hits",
        "runs_started",
        "search_qps",
    ]
    .iter()
    .filter_map(|name| resource.metrics.get(*name))
    .filter_map(MetricSeries::display_value)
    .fold(0.0, f64::max)
}

/// One coherent cross-resource aggregate: a public metric name, the single
/// provider family it was summed within, and how many resources contributed.
struct LaneAggregate {
    name: &'static str,
    family: &'static str,
    contributors: usize,
    series: MetricSeries,
}

const FAMILY_LABELS: [&str; 3] = ["edge", "app", "data-plane"];

/// Coherent provider family for cross-resource aggregation: edge delivery
/// (Front Door/CDN), application hosting (App Service), then data-plane
/// services (Cosmos/storage/search/…) as an explicitly labeled fallback.
/// Families are never mixed inside one lane: edge + app + database requests
/// are three different populations, not one "resource group traffic" total.
fn provider_family(resource_type: &str) -> usize {
    let kind = resource_type.to_ascii_lowercase();
    if kind.starts_with("microsoft.cdn/") || kind.contains("frontdoor") {
        0
    } else if kind.starts_with("microsoft.web/") {
        1
    } else {
        2
    }
}

fn window_label(state: &UiState) -> String {
    format!("{}h", state.window_hours)
}

/// Explicit hidden-evidence state for the requested window. Metric series for
/// another window are never drawn beneath the requested-window chrome.
fn window_state_message(state: &UiState) -> String {
    let window = format!("{}h/{}", state.window_hours, interval_label(state));
    if state.window_loading {
        format!("LOADING {window} · previous evidence hidden")
    } else {
        format!("NO MATCHING DATA {window} · stale evidence hidden")
    }
}

fn first_aggregate(
    snapshot: &Snapshot,
    names: &[&'static str],
    maximum: bool,
    window_hours: u64,
    interval_minutes: u64,
) -> Option<LaneAggregate> {
    if !snapshot.fleet_query.matches(window_hours, interval_minutes) {
        return None;
    }
    let window = snapshot.fleet_query.window_label();
    let interval = snapshot.fleet_query.interval_label();
    let mut candidates = Vec::new();
    for (family, family_label) in FAMILY_LABELS.iter().enumerate() {
        for (name_index, name) in names.iter().enumerate() {
            let members = snapshot
                .resources
                .iter()
                .filter(|resource| provider_family(&resource.resource_type) == family)
                .filter_map(|resource| resource.fleet_metrics.get(*name))
                .filter(|metric| {
                    metric.state == "available"
                        && metric.query.matches(window_hours, interval_minutes)
                        && metric.query.cohort == snapshot.fleet_query.cohort
                })
                .collect::<Vec<_>>();
            if members.is_empty() {
                continue;
            }
            // Prefer the requested grain; otherwise use the first accepted
            // grain, but never mix grains inside one aggregate.
            let grain = members
                .iter()
                .find(|metric| metric.interval == interval)
                .unwrap_or(&members[0])
                .interval
                .clone();
            let members = members
                .into_iter()
                .filter(|metric| metric.interval == grain)
                .collect::<Vec<_>>();
            let template = members[0];
            let cross_maximum = maximum
                || name.ends_with("percent")
                || template.aggregation.eq_ignore_ascii_case("maximum");
            // Union of timestamps; bins where no contributor reported stay
            // null instead of being dropped or zero-filled.
            let mut grouped = BTreeMap::<String, Vec<f64>>::new();
            for metric in &members {
                for (timestamp, value) in metric.timestamps.iter().zip(&metric.values) {
                    let bucket = grouped.entry(timestamp.clone()).or_default();
                    if let Some(value) = value.filter(|value| value.is_finite()) {
                        bucket.push(value);
                    }
                }
            }
            let values = grouped
                .values()
                .map(|bucket| {
                    if bucket.is_empty() {
                        None
                    } else if cross_maximum {
                        bucket.iter().copied().reduce(f64::max)
                    } else {
                        Some(bucket.iter().sum())
                    }
                })
                .collect::<Vec<_>>();
            candidates.push((
                family,
                name_index,
                LaneAggregate {
                    name,
                    family: family_label,
                    contributors: members.len(),
                    series: MetricSeries {
                        name: (*name).into(),
                        unit: template.unit.clone(),
                        source: "Azure Monitor metrics".into(),
                        window: window.clone(),
                        interval: grain,
                        state: "available".into(),
                        detail: format!(
                            "{} across {} '{}' series in the {} family only; temporal aggregation {}",
                            if cross_maximum { "maximum" } else { "sum" },
                            members.len(),
                            name,
                            family_label,
                            template.aggregation
                        ),
                        timestamps: grouped.keys().cloned().collect(),
                        values,
                        aggregation: template.aggregation.clone(),
                        query: template.query.clone(),
                    },
                },
            ));
        }
    }
    candidates
        .into_iter()
        .max_by(compare_lane_candidates)
        .map(|(_, _, lane)| lane)
}

/// Choose the most representative coherent family for a lane. A merely
/// present all-zero edge series must not hide meaningful app/data traffic.
/// Coverage and contributor count break ties before the stable edge/app/data
/// preference; families themselves are never combined.
fn compare_lane_candidates(
    left: &(usize, usize, LaneAggregate),
    right: &(usize, usize, LaneAggregate),
) -> Ordering {
    let left_nonzero = left
        .2
        .series
        .values
        .iter()
        .flatten()
        .any(|value| value.abs() > f64::EPSILON);
    let right_nonzero = right
        .2
        .series
        .values
        .iter()
        .flatten()
        .any(|value| value.abs() > f64::EPSILON);
    let coverage = |lane: &LaneAggregate| {
        (
            lane.series.values.iter().flatten().count(),
            lane.series.values.len(),
        )
    };
    let (left_filled, left_total) = coverage(&left.2);
    let (right_filled, right_total) = coverage(&right.2);
    left_nonzero
        .cmp(&right_nonzero)
        .then_with(|| {
            // Compare ratios exactly, treating an empty vector as zero
            // coverage without introducing NaN ordering.
            (left_filled as u128 * right_total.max(1) as u128)
                .cmp(&(right_filled as u128 * left_total.max(1) as u128))
        })
        .then_with(|| left_filled.cmp(&right_filled))
        .then_with(|| left.2.contributors.cmp(&right.2.contributors))
        .then_with(|| right.0.cmp(&left.0))
        .then_with(|| right.1.cmp(&left.1))
}

const WEB_SELECTED: &[(&str, &str)] = &[
    ("memory_working_set", "MEMORY — APP"),
    ("requests", "REQUESTS"),
    ("http_5xx", "HTTP 5XX"),
    ("response_time", "LATENCY"),
    ("health_check_status", "HEALTH CHECK"),
];
const PLAN_SELECTED: &[(&str, &str)] = &[("cpu_percent", "CPU"), ("memory_percent", "MEMORY")];
const POSTGRES_SELECTED: &[(&str, &str)] = &[
    ("cpu_percent", "CPU"),
    ("storage_percent", "STORAGE"),
    ("active_connections", "CONNECTIONS"),
];
const SEARCH_SELECTED: &[(&str, &str)] = &[
    ("search_qps", "SEARCH QPS"),
    ("search_latency", "SEARCH LATENCY"),
    ("search_throttled_percent", "THROTTLED"),
];
const AI_SELECTED: &[(&str, &str)] = &[
    ("total_calls", "CALLS"),
    ("total_errors", "ERRORS"),
    ("latency", "LATENCY"),
];
const AFD_SELECTED: &[(&str, &str)] = &[
    ("requests", "REQUESTS"),
    ("http_5xx_percent", "HTTP 5XX"),
    ("total_latency", "TOTAL LATENCY"),
    ("origin_health_percent", "ORIGIN HEALTH"),
];
const STORAGE_SELECTED: &[(&str, &str)] = &[
    ("transactions", "TRANSACTIONS"),
    ("e2e_latency", "E2E LATENCY"),
    ("server_latency", "SERVER LATENCY"),
    ("storage_used", "CAPACITY"),
    ("availability_percent", "AVAILABILITY"),
];
const KEYVAULT_SELECTED: &[(&str, &str)] = &[
    ("api_hits", "API HITS"),
    ("api_results", "API RESULTS"),
    ("api_latency", "API LATENCY"),
    ("saturation_percent", "SATURATION"),
    ("availability_percent", "AVAILABILITY"),
];
const ACR_SELECTED: &[(&str, &str)] = &[
    ("pulls", "PULLS"),
    ("successful_pulls", "PULL SUCCESSES"),
    ("pushes", "PUSHES"),
    ("successful_pushes", "PUSH SUCCESSES"),
    ("storage_used", "CAPACITY"),
];
const SQL_SELECTED: &[(&str, &str)] = &[
    ("cpu_percent", "CPU"),
    ("dtu_percent", "DTU"),
    ("storage_percent", "STORAGE"),
    ("workers_percent", "WORKERS"),
    ("sessions_percent", "SESSIONS"),
    ("deadlocks", "DEADLOCKS"),
];
const COSMOS_SELECTED: &[(&str, &str)] = &[
    ("requests", "REQUESTS"),
    ("request_units", "REQUEST UNITS"),
    ("ru_percent", "RU PRESSURE"),
    ("server_latency", "SERVER LATENCY"),
    ("availability_percent", "AVAILABILITY"),
];
const REDIS_SELECTED: &[(&str, &str)] = &[
    ("server_load_percent", "SERVER LOAD"),
    ("memory_percent", "MEMORY"),
    ("connected_clients", "CLIENTS"),
    ("errors", "ERRORS"),
    ("evicted_keys", "EVICTIONS"),
    ("cache_miss_percent", "CACHE MISS"),
];
const FIREWALL_SELECTED: &[(&str, &str)] = &[
    ("throughput", "THROUGHPUT"),
    ("data_processed", "DATA PROCESSED"),
    ("snat_percent", "SNAT"),
    ("firewall_latency", "LATENCY"),
    ("firewall_health_percent", "FIREWALL HEALTH"),
];
const LOGIC_SELECTED: &[(&str, &str)] = &[
    ("runs_started", "RUNS STARTED"),
    ("runs_completed", "RUNS COMPLETED"),
    ("runs_failed", "RUNS FAILED"),
    ("runs_throttled", "THROTTLED"),
    ("triggers_started", "TRIGGERS"),
];

fn provider_selected_order(resource_type: &str) -> &'static [(&'static str, &'static str)] {
    match resource_type.to_ascii_lowercase().as_str() {
        "microsoft.web/sites" | "microsoft.web/sites/slots" => WEB_SELECTED,
        "microsoft.web/serverfarms" => PLAN_SELECTED,
        "microsoft.dbforpostgresql/flexibleservers" => POSTGRES_SELECTED,
        "microsoft.search/searchservices" => SEARCH_SELECTED,
        "microsoft.cognitiveservices/accounts" => AI_SELECTED,
        "microsoft.cdn/profiles" => AFD_SELECTED,
        "microsoft.storage/storageaccounts" => STORAGE_SELECTED,
        "microsoft.keyvault/vaults" => KEYVAULT_SELECTED,
        "microsoft.containerregistry/registries" => ACR_SELECTED,
        "microsoft.sql/servers/databases" => SQL_SELECTED,
        "microsoft.documentdb/databaseaccounts" => COSMOS_SELECTED,
        "microsoft.cache/redis" => REDIS_SELECTED,
        "microsoft.network/azurefirewalls" => FIREWALL_SELECTED,
        "microsoft.logic/workflows" => LOGIC_SELECTED,
        _ => &[],
    }
}

fn selected_series<'a>(
    snapshot: &'a Snapshot,
    resource: &'a AzureResource,
    state: &UiState,
) -> Vec<(String, &'a str, &'a MetricSeries)> {
    let plan = backing_plan(snapshot, resource);
    let cohort = resource
        .metrics
        .values()
        .chain(
            plan.into_iter()
                .flat_map(|resource| resource.metrics.values()),
        )
        .filter(|metric| {
            metric.state == "available"
                && metric
                    .query
                    .matches(state.window_hours, state.interval_minutes)
        })
        .max_by(|left, right| left.query.queried_at.cmp(&right.query.queried_at))
        .map(|metric| metric.query.cohort.as_str());
    let Some(cohort) = cohort else {
        return Vec::new();
    };
    let matching = |metric: &&MetricSeries| {
        metric.state == "available"
            && metric
                .query
                .matches(state.window_hours, state.interval_minutes)
            && metric.query.cohort == cohort
    };
    let is_web_app = matches!(
        resource.resource_type.to_ascii_lowercase().as_str(),
        "microsoft.web/sites" | "microsoft.web/sites/slots"
    );
    let mut result = Vec::new();
    if is_web_app {
        if let Some(plan) = plan {
            for (name, label) in PLAN_SELECTED {
                if let Some(metric) = plan.metrics.get(*name).filter(matching) {
                    result.push((format!("SHARED PLAN {label}"), *name, metric));
                }
            }
        }
    }
    for (name, label) in provider_selected_order(&resource.resource_type) {
        if let Some(metric) = resource.metrics.get(*name).filter(matching) {
            result.push(((*label).into(), *name, metric));
        }
    }
    // A future fixed adapter or provider-returned safe aggregate should still
    // render without a release-cycle mapping update. Only available series
    // from the exact selected query cohort qualify for this fallback.
    for (name, metric) in &resource.metrics {
        if matching(&metric)
            && !result
                .iter()
                .any(|(_, selected_name, _)| *selected_name == name.as_str())
        {
            result.push((
                name.replace('_', " ").to_ascii_uppercase(),
                name.as_str(),
                metric,
            ));
        }
    }
    result
}

fn backing_plan<'a>(snapshot: &'a Snapshot, resource: &AzureResource) -> Option<&'a AzureResource> {
    if !resource.hosting_plan_id.is_empty() {
        if let Some(plan) = snapshot.resources.iter().find(|candidate| {
            candidate
                .resource_id
                .eq_ignore_ascii_case(&resource.hosting_plan_id)
        }) {
            return Some(plan);
        }
    }
    resource
        .relationships
        .iter()
        .find(|relation| relation.direction == "parent" && relation.kind == "app_service_plan")
        .and_then(|relation| {
            snapshot.resources.iter().find(|candidate| {
                candidate.name.eq_ignore_ascii_case(&relation.resource_name)
                    && candidate
                        .resource_type
                        .eq_ignore_ascii_case(&relation.resource_type)
            })
        })
}

fn metric_summary_line(
    label: &str,
    name: &str,
    metric: &MetricSeries,
    color: bool,
) -> Line<'static> {
    let count = matches!(metric.aggregation.as_str(), "total" | "count");
    let (value_label, peak_label) = if count {
        ("total", "peak-bin")
    } else {
        ("now", "max")
    };
    Line::from(vec![
        Span::styled(format!("{label} "), style_header(color)),
        Span::styled(
            format!("{:<16} ", name.replace('_', " ")),
            style_muted(color),
        ),
        Span::styled(format!("{value_label} "), style_muted(color)),
        Span::raw(format_value(
            if count {
                metric.total()
            } else {
                metric.latest()
            },
            &metric.unit,
            name,
        )),
        Span::styled(format!("  {peak_label} "), style_muted(color)),
        Span::raw(format_value(
            metric.samples().reduce(f64::max),
            &metric.unit,
            name,
        )),
    ])
}

fn interval_label(state: &UiState) -> String {
    if state.interval_minutes == 60 {
        "1h".into()
    } else {
        format!("{}m", state.interval_minutes)
    }
}

fn panel(title: &str, chip: Option<Line<'static>>, color: bool) -> Block<'static> {
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if color {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default()
        })
        .title(Line::styled(format!(" {title} "), style_header(color)));
    if let Some(chip) = chip {
        block = block.title_top(chip.right_aligned());
    }
    block
}

fn chip(text: String, color: bool) -> Line<'static> {
    Line::styled(format!(" {text} "), style_muted(color))
}

fn meter(value: f64, width: usize) -> String {
    let filled = ((value.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "▰".repeat(filled), "▱".repeat(width - filled))
}

// Bottom-up braille fill masks: [0 dots, 1, 2, 3, 4] for each cell column.
const BRAILLE_LEFT: [u8; 5] = [0x00, 0x40, 0x44, 0x46, 0x47];
const BRAILLE_RIGHT: [u8; 5] = [0x00, 0x80, 0xA0, 0xB0, 0xB8];
// Single-dot masks per cell column, indexed by dot height from the bottom.
const BRAILLE_LEFT_DOT: [u8; 4] = [0x40, 0x04, 0x02, 0x01];
const BRAILLE_RIGHT_DOT: [u8; 4] = [0x80, 0x20, 0x10, 0x08];

/// Fixed 0..=100 scale for bounded percent metrics; unbounded metrics scale
/// to their own window so peaks touch the top of the graph.
fn scale_top(name: &str) -> Option<f64> {
    name.ends_with("_percent").then_some(100.0)
}

/// Time-raster the whole input window onto `slots` columns, preserving order.
/// Downsampling aggregates each column's source bucket with maximum (peaks
/// survive); upsampling repeats each source bin across its proportional
/// x-span. A column is None only when every covered source bin is missing —
/// gaps are never interpolated and a lone real bin stays a narrow bar.
fn resample_max(values: &[Option<f64>], slots: usize) -> Vec<Option<f64>> {
    if values.is_empty() {
        return vec![None; slots];
    }
    let len = values.len();
    (0..slots)
        .map(|slot| {
            let start = slot * len / slots;
            let end = ((slot + 1) * len / slots).clamp(start + 1, len);
            values[start..end]
                .iter()
                .flatten()
                .copied()
                .filter(|value| value.is_finite())
                .reduce(f64::max)
        })
        .collect()
}

/// Dot heights (0 = missing, 1..=dots_total) for the full series rastered
/// over `slots` columns. Scale comes from the un-resampled series: a real
/// zero keeps one baseline dot; constant and negative series stay visible via
/// min(0, minimum)..maximum scaling; an all-None series has no baseline.
fn braille_levels(
    values: &[Option<f64>],
    slots: usize,
    dots_total: u16,
    minimum_top: Option<f64>,
) -> Vec<u16> {
    let window = resample_max(values, slots);
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for value in values.iter().flatten().filter(|value| value.is_finite()) {
        low = low.min(*value);
        high = high.max(*value);
    }
    let (low, high) = if high < low {
        (0.0, 1.0)
    } else {
        let low = low.min(0.0);
        let high = high.max(minimum_top.unwrap_or(high));
        (low, if high <= low { low + 1.0 } else { high })
    };
    window
        .iter()
        .map(|value| match value {
            Some(value) => {
                let ratio = ((value - low) / (high - low)).clamp(0.0, 1.0);
                ((ratio * f64::from(dots_total)).round() as u16).clamp(1, dots_total)
            }
            None => 0,
        })
        .collect()
}

/// Filled multi-row area graph using braille cells (2 columns x 4 dot levels
/// per cell). The entire input window spans the entire plot width; missing
/// spans stay blank gap columns.
pub fn braille_graph(
    values: &[Option<f64>],
    width: usize,
    height: usize,
    minimum_top: Option<f64>,
) -> Vec<String> {
    let width = width.max(1);
    let height = height.max(1);
    let levels = braille_levels(values, width * 2, (height * 4) as u16, minimum_top);
    let mut rows = Vec::with_capacity(height);
    for row in 0..height {
        let base = ((height - 1 - row) * 4) as u16;
        let mut line = String::with_capacity(width * 3);
        for cell in 0..width {
            let left = levels[cell * 2].saturating_sub(base).min(4) as usize;
            let right = levels[cell * 2 + 1].saturating_sub(base).min(4) as usize;
            let bits = BRAILLE_LEFT[left] | BRAILLE_RIGHT[right];
            if bits == 0 {
                line.push(' ');
            } else {
                line.push(char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' '));
            }
        }
        rows.push(line);
    }
    rows
}

/// Silhouette variant of `braille_graph`: only the value line is dotted, not
/// the area beneath it, so a near-constant series reads as a level line
/// instead of an opaque wall. Same rastering, scale, and gap semantics.
pub fn braille_line_graph(
    values: &[Option<f64>],
    width: usize,
    height: usize,
    minimum_top: Option<f64>,
) -> Vec<String> {
    let width = width.max(1);
    let height = height.max(1);
    let levels = braille_levels(values, width * 2, (height * 4) as u16, minimum_top);
    let mut rows = Vec::with_capacity(height);
    for row in 0..height {
        let base = ((height - 1 - row) * 4) as u16;
        let mut line = String::with_capacity(width * 3);
        for cell in 0..width {
            let mut bits = 0u8;
            for (level, masks) in [
                (levels[cell * 2], BRAILLE_LEFT_DOT),
                (levels[cell * 2 + 1], BRAILLE_RIGHT_DOT),
            ] {
                if level > 0 {
                    let dot = level - 1;
                    if (base..base + 4).contains(&dot) {
                        bits |= masks[(dot - base) as usize];
                    }
                }
            }
            if bits == 0 {
                line.push(' ');
            } else {
                line.push(char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' '));
            }
        }
        rows.push(line);
    }
    rows
}

/// Change markers on the same full-window time axis as the graphs: the first
/// series timestamp maps to the left edge and the last to the right edge.
pub fn change_timeline(series: &MetricSeries, changes: &[ChangePoint], width: usize) -> String {
    let width = width.max(1);
    let Some(first) = series
        .timestamps
        .first()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
    else {
        return " ".repeat(width);
    };
    let Some(last) = series
        .timestamps
        .last()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
    else {
        return " ".repeat(width);
    };
    let span = (last.timestamp_millis() - first.timestamp_millis()).max(1) as f64;
    let mut counts = vec![0u64; width];
    for change in changes {
        let Ok(timestamp) = DateTime::parse_from_rfc3339(&change.timestamp) else {
            continue;
        };
        if timestamp < first || timestamp > last {
            continue;
        }
        let ratio = (timestamp.timestamp_millis() - first.timestamp_millis()) as f64 / span;
        let column = ((ratio * (width - 1) as f64).round() as usize).min(width - 1);
        counts[column] = counts[column].saturating_add(change.count);
    }
    let peak = counts.iter().copied().max().unwrap_or(0);
    counts
        .into_iter()
        .map(|count| {
            if count == 0 || peak == 0 {
                ' '
            } else {
                match count as f64 / peak as f64 {
                    ratio if ratio < 0.34 => '▂',
                    ratio if ratio < 0.67 => '▄',
                    _ => '█',
                }
            }
        })
        .collect()
}

/// Single-row spark rendering with the same full-window rastering as the
/// braille graphs; missing buckets stay '·'.
pub fn sparkline(values: &[Option<f64>], width: usize) -> String {
    const SPARKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let width = width.max(1);
    if values.is_empty() {
        return "·".repeat(width.min(8));
    }
    let samples = resample_max(values, width);
    let numeric = samples.iter().flatten().copied().collect::<Vec<_>>();
    if numeric.is_empty() {
        return "·".repeat(samples.len());
    }
    let minimum = numeric.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum = numeric.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    samples
        .iter()
        .map(|value| match value {
            None => '·',
            Some(_) if maximum == 0.0 => '▁',
            Some(_) if maximum == minimum => '▄',
            Some(value) => {
                let index = ((value - minimum) / (maximum - minimum) * 7.0).round() as usize;
                SPARKS[index.min(7)]
            }
        })
        .collect()
}

pub fn format_value(value: Option<f64>, unit: &str, metric_name: &str) -> String {
    let Some(value) = value else {
        return "no data".into();
    };
    match unit.to_ascii_lowercase().as_str() {
        "milliseconds" | "millisecond" => format_duration_ms(value),
        "seconds" => format_duration_ms(value * 1000.0),
        "bytes" => format!("{}MiB", compact(value / 1024.0 / 1024.0)),
        "bytespersecond" => format!("{}MiB/s", compact(value / 1024.0 / 1024.0)),
        "bitspersecond" => format!("{}Mb/s", compact(value / 1_000_000.0)),
        "percent" | "percentage" => format!("{}%", compact(value)),
        _ if metric_name.ends_with("_percent") => format!("{}%", compact(value)),
        _ => compact(value),
    }
}

fn format_duration_ms(value: f64) -> String {
    if value.abs() >= 60_000.0 {
        format!("{}m", compact(value / 60_000.0))
    } else if value.abs() >= 1_000.0 {
        format!("{}s", compact(value / 1_000.0))
    } else {
        format!("{}ms", compact(value))
    }
}

fn compact(value: f64) -> String {
    let absolute = value.abs();
    if absolute >= 1_000_000.0 {
        format!("{:.1}m", value / 1_000_000.0)
    } else if absolute >= 1_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if absolute >= 100.0 {
        format!("{value:.0}")
    } else if absolute >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn primary_signal(resource: &AzureResource) -> String {
    if resource.evidence_state != EvidenceState::Signal {
        return resource.evidence_label().into();
    }
    for name in ["http_5xx", "total_errors", "runs_failed"] {
        if let Some(metric) = resource.metrics.get(name).filter(|metric| {
            metric.state == "available" && metric.display_value().is_some_and(|value| value > 0.0)
        }) {
            return format!(
                "{} {}",
                name.replace('_', ""),
                format_value(metric.display_value(), &metric.unit, name)
            );
        }
    }
    for name in [
        "requests",
        "total_calls",
        "cpu_percent",
        "memory_percent",
        "storage_percent",
    ] {
        if let Some(metric) = resource
            .metrics
            .get(name)
            .filter(|metric| metric.state == "available")
        {
            return format!(
                "{} {}",
                name.replace('_', ""),
                format_value(metric.display_value(), &metric.unit, name)
            );
        }
    }
    resource.evidence_label().into()
}

fn relationship_summary(resource: &AzureResource) -> String {
    let parents = resource
        .relationships
        .iter()
        .filter(|relation| relation.direction == "parent")
        .map(|relation| {
            format!(
                "{}: {}",
                relation.kind.replace('_', " "),
                relation.resource_name
            )
        })
        .collect::<Vec<_>>();
    let dependents = resource
        .relationships
        .iter()
        .filter(|relation| relation.direction == "dependent")
        .count();
    let mut parts = parents;
    if dependents > 0 {
        parts.push(format!(
            "{dependents} dependent{}",
            if dependents == 1 { "" } else { "s" }
        ));
    }
    parts.join(" · ")
}

fn short_control(state: &str) -> String {
    if state.eq_ignore_ascii_case("running") {
        "RUN".into()
    } else if state.eq_ignore_ascii_case("stopped") {
        "STOP".into()
    } else {
        short_state(state)
    }
}

fn short_state(state: &str) -> String {
    match state.to_ascii_lowercase().as_str() {
        "healthy" | "available" | "configured" | "succeeded" => "OK".into(),
        "degraded" | "unhealthy" | "failed" => "BAD".into(),
        "warning" => "WRN".into(),
        "unavailable" | "unsupported" => "LIM".into(),
        "signal" => "SIG".into(),
        "no_data" => "ND".into(),
        "pending" => "PEND".into(),
        "not_sampled" => "CAP".into(),
        "not_inspected" => "N/I".into(),
        "inventory_only" => "INV".into(),
        "stopped" => "STOP".into(),
        _ => "?".into(),
    }
}

fn state_symbol(state: &str) -> &'static str {
    match state {
        "healthy" | "available" | "configured" => "OK ",
        "degraded" | "unhealthy" => "BAD",
        "signal" => "SIG",
        "warning" => "WRN",
        "unavailable" | "unsupported" => "LIM",
        "no_data" => "ND ",
        "pending" => "PND",
        "not_sampled" => "CAP",
        "inventory_only" => "INV",
        _ => "?  ",
    }
}

fn last_seen(resource: &AzureResource) -> String {
    resource
        .metrics
        .values()
        .filter_map(|metric| {
            let timestamp = metric.latest_timestamp();
            (!timestamp.is_empty()).then_some(timestamp)
        })
        .max()
        .map(age)
        .unwrap_or_else(|| "?".into())
}

fn age(value: &str) -> String {
    let Ok(timestamp) = DateTime::parse_from_rfc3339(value) else {
        return if value.is_empty() { "unknown" } else { value }.into();
    };
    let seconds = (Utc::now() - timestamp.with_timezone(&Utc))
        .num_seconds()
        .max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    }
}

fn style_header(color: bool) -> Style {
    if color {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn style_accent(color: bool) -> Style {
    if color {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn style_muted(color: bool) -> Style {
    if color {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    }
}

fn style_unknown(color: bool) -> Style {
    if color {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

fn style_warning(color: bool) -> Style {
    if color {
        Style::default().fg(Color::LightRed)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn style_attention(attention: &str, color: bool) -> Style {
    if !color {
        return Style::default();
    }
    match attention {
        "BAD" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "WRN" => Style::default().fg(Color::Yellow),
        "STOP" => Style::default().fg(Color::LightRed),
        "LIM" => Style::default().fg(Color::Magenta),
        "PEND" | "ND" => Style::default().fg(Color::Yellow),
        "CAP" | "INV" => Style::default().fg(Color::DarkGray),
        "SIG" => Style::default().fg(Color::Cyan),
        "OK" => Style::default().fg(Color::Green),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn style_evidence(evidence: EvidenceState, color: bool) -> Style {
    if !color {
        return Style::default();
    }
    match evidence {
        EvidenceState::Signal => Style::default().fg(Color::Cyan),
        EvidenceState::NoData | EvidenceState::Pending => Style::default().fg(Color::Yellow),
        EvidenceState::Limited => Style::default().fg(Color::Magenta),
        EvidenceState::NotSampled | EvidenceState::InventoryOnly => {
            Style::default().fg(Color::DarkGray)
        }
    }
}

fn style_change(color: bool) -> Style {
    if color {
        Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn style_metric(label: &str, color: bool) -> Style {
    if !color {
        return Style::default();
    }
    match label {
        "FAILURES" | "HTTP 5XX" | "ERRORS" => Style::default().fg(Color::Red),
        "LATENCY" | "PRESSURE" | "CPU" => Style::default().fg(Color::Yellow),
        "MEMORY" | "MEMORY — APP" | "SHARED PLAN" => Style::default().fg(Color::Magenta),
        _ => Style::default().fg(Color::Green),
    }
}

pub fn render_table(snapshot: &Snapshot, width: usize) -> String {
    let mut output = vec![
        "aztop — ACCESSIBLE SNAPSHOT".into(),
        format!(
            "Cloud: {}",
            snapshot
                .subscriptions
                .iter()
                .find(|subscription| {
                    subscription.subscription_id == snapshot.selected_subscription_id
                })
                .or_else(|| {
                    snapshot.subscriptions.iter().find(|subscription| {
                        subscription.name == snapshot.selected_subscription_name
                    })
                })
                .map(|subscription| subscription.cloud.as_str())
                .unwrap_or("Unknown")
        ),
        format!("Subscription: {}", snapshot.selected_subscription_name),
        format!("Resource group: {}", snapshot.selected_resource_group),
        format!(
            "Access: {}{}",
            snapshot.access_state,
            if snapshot.access_detail.is_empty() {
                String::new()
            } else {
                format!(" · {}", snapshot.access_detail)
            }
        ),
        "Source: Azure Resource Graph fixed inventory; Azure Monitor fixed bounded aggregates"
            .into(),
        String::new(),
        format!(
            "{:<2} {:<5} {:<30} {:<18} {:<10} {:<10} {:<6} {}",
            "★", "ATTN", "RESOURCE", "TYPE", "CONTROL", "APP", "EVID", "WHY"
        ),
    ];
    let state = UiState {
        color: false,
        view: ViewMode::Inventory,
        ..UiState::default()
    };
    for resource in visible_resources(snapshot, &state) {
        let (attention, reason) = resource_attention(resource);
        output.push(format!(
            "{:<2} {:<5} {:<30} {:<18} {:<10} {:<10} {:<6} {}",
            if resource.watched { "★" } else { "" },
            attention,
            truncate(&resource.name, 30),
            truncate(resource.type_label(), 18),
            truncate(&resource.control_state, 10),
            truncate(&resource.health_state, 10),
            resource.evidence_label(),
            reason,
        ));
        output.push(format!(
            "      evidence: {} · {}",
            resource.evidence_state.as_str(),
            resource.evidence_detail
        ));
        if !resource.metrics.is_empty() {
            output.push(format!(
                "      metrics: {}",
                resource
                    .metrics
                    .iter()
                    .map(|(name, metric)| format!(
                        "{name}={} [{} {} {}]",
                        format_value(metric.display_value(), &metric.unit, name),
                        metric.state,
                        metric.window,
                        metric.interval
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        output.push(format!(
            "      resource health: {}{}",
            resource.resource_health_state,
            if resource.resource_health_reason.is_empty() {
                String::new()
            } else {
                format!(" · {}", resource.resource_health_reason)
            }
        ));
        output.push(format!(
            "      diagnostics: {} · {}",
            resource.diagnostic_state, resource.diagnostic_detail
        ));
        if !resource.relationships.is_empty() {
            output.push(format!(
                "      relationships: {}",
                relationship_summary(resource)
            ));
        }
    }
    output.push(String::new());
    output.push("RECENT CHANGES — 24H · METADATA ONLY · CAP 20".into());
    output.push(
        "Generic Azure UPDATE records are not confirmed deployments; absence is not proof of no change."
            .into(),
    );
    if snapshot.details.recent_changes.is_empty() {
        let (state_label, detail) = recent_changes_status(snapshot);
        output.push(format!("{state_label} · {detail}"));
    } else {
        for change in &snapshot.details.recent_changes {
            output.push(format_recent_change(change));
        }
    }
    output.push(String::new());
    output.push("RESOURCE HEALTH / OPERATIONS SIGNALS".into());
    for signal in &snapshot.details.signals {
        output.push(format!(
            "{} {} · {} [{} {}]",
            state_symbol(&signal.state),
            signal.name,
            signal.detail,
            signal.source,
            signal.window
        ));
    }
    for limitation in &snapshot.details.limitations {
        output.push(format!("LIM {limitation}"));
    }
    output
        .into_iter()
        .map(|line| truncate(&line, width))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.into();
    }
    value
        .chars()
        .take(width.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

pub fn render_to_string(snapshot: &Snapshot, state: &UiState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test backend");
    terminal
        .draw(|frame| draw(frame, snapshot, state))
        .expect("draw frame");
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GroupDetails, MetricQuery, Subscription};

    fn test_query(window_hours: u64, interval_minutes: u64) -> MetricQuery {
        MetricQuery {
            window_hours,
            requested_interval_minutes: interval_minutes,
            start_time: "2026-07-28T00:00:00Z".into(),
            end_time: "2026-07-28T01:00:00Z".into(),
            queried_at: "2026-07-28T01:00:01Z".into(),
            cohort: format!("test-{window_hours}h-{interval_minutes}m"),
        }
    }

    fn snapshot() -> Snapshot {
        let query = test_query(1, 1);
        let mut resource = AzureResource {
            name: "app".into(),
            resource_type: "Microsoft.Web/sites".into(),
            category: "compute/web".into(),
            control_state: "Running".into(),
            provisioning_state: "Succeeded".into(),
            health_state: "unknown".into(),
            resource_health_state: "unknown".into(),
            diagnostic_state: "unknown".into(),
            evidence_state: EvidenceState::Signal,
            evidence_detail: "sampled".into(),
            resource_id:
                "/subscriptions/private/resourceGroups/rg/providers/Microsoft.Web/sites/app".into(),
            ..AzureResource::default()
        };
        let metric = MetricSeries {
            name: "requests".into(),
            unit: "Count".into(),
            source: "Azure Monitor metrics".into(),
            window: "1h".into(),
            interval: "1m".into(),
            state: "available".into(),
            timestamps: vec!["2026-07-28T00:00:00Z".into(), "2026-07-28T00:01:00Z".into()],
            values: vec![Some(0.0), Some(10.0)],
            aggregation: "total".into(),
            query: query.clone(),
            ..MetricSeries::default()
        };
        resource.metrics.insert("requests".into(), metric.clone());
        resource.fleet_metrics.insert("requests".into(), metric);
        let mut snapshot = Snapshot::now(
            vec![Subscription {
                name: "Gov".into(),
                cloud: "AzureUSGovernment".into(),
                is_default: true,
                subscription_id: "secret".into(),
            }],
            "Gov".into(),
            "secret".into(),
            Vec::new(),
            "rg".into(),
            "available".into(),
            String::new(),
            vec![resource],
            GroupDetails::default(),
            true,
        );
        snapshot.fleet_query = query;
        snapshot.fleet_state = "current".into();
        snapshot
    }

    #[test]
    fn responsive_frames_contain_all_primary_panels() {
        let snapshot = snapshot();
        let state = UiState {
            selected_id: snapshot.resources[0].resource_id.clone(),
            color: false,
            ..UiState::default()
        };
        for (width, height) in [(160, 50), (140, 40), (110, 30), (80, 24)] {
            let output = render_to_string(&snapshot, &state, width, height);
            assert_eq!(output.lines().count(), height as usize);
            assert!(output.contains("resource group pulse"));
            assert!(output.contains("selected resource metrics"));
            assert!(output.contains("attention queue"));
            assert!(output.contains('›'));
        }
        let narrow = render_to_string(&snapshot, &state, 80, 24);
        assert!(narrow.contains("access"));
        assert!(!narrow.contains("operationsaccess"));
        assert!(narrow.contains("w 1h/1m"));
        let wide = render_to_string(&snapshot, &state, 160, 50);
        assert!(wide.contains("l logs"));
        assert!(wide.contains("? help"));
    }

    #[test]
    fn recent_changes_are_compact_safe_and_filter_to_selected_resource() {
        let mut snapshot = snapshot();
        snapshot.details.recent_changes = vec![
            RecentChange {
                timestamp: Utc::now().to_rfc3339(),
                resource_name: "app".into(),
                resource_type: "Microsoft.Web/sites".into(),
                event: "VERSION".into(),
                detail: "v1 → v2".into(),
                source: "aztop observation".into(),
            },
            RecentChange {
                timestamp: Utc::now().to_rfc3339(),
                resource_name: "other".into(),
                resource_type: "Microsoft.Web/sites".into(),
                event: "UPDATE".into(),
                detail: "cause not inferred".into(),
                source: "Azure Resource Graph".into(),
            },
        ];
        let state = UiState {
            selected_id: snapshot.resources[0].resource_id.clone(),
            color: false,
            ..UiState::default()
        };
        let compact = render_to_string(&snapshot, &state, 160, 50);
        assert!(compact.contains("recent changes"));
        assert!(compact.contains("VERSION"));
        let overlay = render_to_string(
            &snapshot,
            &UiState {
                overlay: Overlay::RecentChanges,
                ..state
            },
            140,
            40,
        );
        assert!(overlay.contains("RECENT CHANGES — SELECTED RESOURCE"));
        assert!(overlay.contains("v1 → v2"));
        assert!(!overlay.contains("cause not inferred"));
        assert!(overlay.contains("not a confirmed deployment"));
        let table = render_table(&snapshot, 180);
        assert!(table.contains("RECENT CHANGES — 24H"));
        assert!(table.contains("v1 → v2"));

        snapshot.details.recent_changes.clear();
        snapshot.details.signals.push(crate::model::Signal {
            name: "recent change events".into(),
            state: "unavailable".into(),
            detail: "permission limited".into(),
            ..crate::model::Signal::default()
        });
        let unavailable = render_to_string(&snapshot, &UiState::default(), 160, 50);
        assert!(unavailable.contains("UNAVAILABLE · permission limited"));
        assert!(render_table(&snapshot, 180).contains("UNAVAILABLE · permission limited"));
    }

    #[test]
    fn cached_startup_is_explicit_without_blank_panels() {
        let mut snapshot = snapshot();
        snapshot.origin = "cache".into();
        snapshot.cache_saved_at = Utc::now().to_rfc3339();
        snapshot.inventory_state = "cached".into();
        let state = UiState {
            selected_id: snapshot.resources[0].resource_id.clone(),
            operation: "RG rg · UPDATING INVENTORY".into(),
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot, &state, 160, 50);
        assert!(output.contains("CACHE"));
        assert!(output.contains("UPDATING INVENTORY"));
        assert!(output.contains("app"));
        assert!(output.contains("resource group pulse"));
    }

    #[test]
    fn selected_row_is_reversed_and_marked_without_color() {
        let snapshot = snapshot();
        let state = UiState {
            selected_id: snapshot.resources[0].resource_id.clone(),
            color: false,
            ..UiState::default()
        };
        let backend = TestBackend::new(160, 50);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| draw(frame, &snapshot, &state))
            .expect("draw frame");
        let buffer = terminal.backend().buffer();
        let marker = (0..50u16)
            .flat_map(|y| (0..160u16).map(move |x| (x, y)))
            .find(|&(x, y)| {
                let cell = &buffer[(x, y)];
                cell.symbol() == "›" && cell.style().add_modifier.contains(Modifier::REVERSED)
            })
            .expect("reversed selection marker rendered");
        assert_eq!(buffer[marker].symbol(), "›");
    }

    #[test]
    fn totals_are_not_labeled_as_current() {
        let output = render_to_string(&snapshot(), &UiState::default(), 140, 40);
        assert!(output.contains("total 10.0"));
        assert!(output.contains("peak-bin"));
    }

    #[test]
    fn missing_and_zero_are_visually_distinct() {
        // full-width rastering: 2 missing bins expand across all 8 columns
        assert_eq!(sparkline(&[None, None], 8), "········");
        // each source bin keeps its proportional span; gaps stay gaps
        assert_eq!(sparkline(&[Some(0.0), None, Some(0.0)], 8), "▁▁▁···▁▁");
    }

    #[test]
    fn braille_graph_scales_changing_zero_constant_negative_and_missing() {
        // changing series rises toward its own maximum
        assert_eq!(
            braille_graph(&[Some(0.0), Some(1.0), Some(2.0), Some(3.0)], 2, 1, None),
            vec!["⣀⣾"]
        );
        // real zero keeps a baseline dot instead of vanishing
        assert_eq!(
            braille_graph(&[Some(0.0), Some(0.0), Some(0.0), Some(0.0)], 2, 2, None),
            vec!["  ", "⣀⣀"]
        );
        // constant positive series stays visible (fully filled, own scale)
        assert_eq!(braille_graph(&[Some(5.0); 4], 2, 1, None), vec!["⣿⣿"]);
        // negative values scale across min..max without collapsing
        assert_eq!(
            braille_graph(&[Some(-3.0), Some(-1.0)], 1, 1, None),
            vec!["⣸"]
        );
        // missing samples are gap columns, not interpolated
        assert_eq!(
            braille_graph(&[Some(1.0), None, Some(1.0), None], 2, 1, None),
            vec!["⡇⡇"]
        );
        // a one-bin series covers the whole window, so it spans the width
        assert_eq!(braille_graph(&[Some(1.0)], 2, 1, None), vec!["⣿⣿"]);
        // bounded percent metrics pin the scale top at 100
        assert_eq!(
            braille_graph(&[Some(50.0), Some(50.0)], 1, 1, Some(100.0)),
            vec!["⣤"]
        );
    }

    #[test]
    fn braille_graph_rasters_short_series_across_full_width() {
        // 96 bins (24h/15m) on a 300-column ultrawide plot must span the
        // whole width instead of clustering as a dot pile at the right edge.
        let values = (0..96)
            .map(|index| Some(1.0 + (index % 7) as f64))
            .collect::<Vec<_>>();
        let rows = braille_graph(&values, 300, 4, None);
        assert_eq!(rows.len(), 4);
        let bottom = rows.last().unwrap().chars().collect::<Vec<_>>();
        assert_eq!(bottom.len(), 300);
        assert_ne!(bottom[0], ' ');
        assert_ne!(bottom[299], ' ');
        let filled_left = bottom[..225].iter().filter(|cell| **cell != ' ').count();
        assert!(
            filled_left > 200,
            "left three quarters must carry the series, got {filled_left} filled cells"
        );
    }

    #[test]
    fn braille_graph_upsampling_keeps_one_point_narrow_and_all_none_blank() {
        let mut single = vec![None; 96];
        single[48] = Some(5.0);
        let rows = braille_graph(&single, 300, 2, None);
        let filled = rows
            .iter()
            .flat_map(|row| {
                row.chars()
                    .enumerate()
                    .filter(|(_, cell)| *cell != ' ')
                    .map(|(index, _)| index)
            })
            .collect::<Vec<_>>();
        assert!(!filled.is_empty());
        assert!(
            filled.iter().all(|index| (148..=158).contains(index)),
            "one real bin must occupy its own time bucket, not the window: {filled:?}"
        );
        // an all-None series renders nothing — no fabricated baseline
        assert!(braille_graph(&vec![None; 96], 300, 2, None)
            .iter()
            .all(|row| row.chars().all(|cell| cell == ' ')));
        assert!(braille_line_graph(&vec![None; 96], 300, 2, None)
            .iter()
            .all(|row| row.chars().all(|cell| cell == ' ')));
    }

    #[test]
    fn braille_graph_downsampling_keeps_missing_buckets_blank() {
        let mut values = vec![Some(2.0); 600];
        for value in &mut values[300..360] {
            *value = None;
        }
        let rows = braille_graph(&values, 100, 1, None);
        let row = rows[0].chars().collect::<Vec<_>>();
        // slots 100..120 map to cells 50..60, entirely inside the missing run
        assert!(
            row[51..59].iter().all(|cell| *cell == ' '),
            "missing run must stay a gap after downsampling"
        );
        assert_ne!(row[10], ' ');
        assert_ne!(row[90], ' ');
    }

    #[test]
    fn line_graph_is_a_silhouette_not_an_area_fill() {
        // near-constant high series: the area variant fills the panel, the
        // line variant keeps a single level line with empty air beneath it
        let values = vec![Some(95.0); 32];
        let filled = braille_graph(&values, 16, 4, Some(100.0));
        let line = braille_line_graph(&values, 16, 4, Some(100.0));
        let dots = |rows: &[String]| {
            rows.iter()
                .flat_map(|row| row.chars())
                .filter(|cell| *cell != ' ')
                .count()
        };
        assert!(
            dots(&line) * 3 <= dots(&filled),
            "silhouette must carry far less ink"
        );
        // the bottom row of the line graph is pure air for a high series
        assert!(line.last().unwrap().chars().all(|cell| cell == ' '));
        // a real zero still draws a baseline line
        let zero = braille_line_graph(&[Some(0.0); 4], 4, 2, None);
        assert!(zero.last().unwrap().chars().any(|cell| cell != ' '));
    }

    #[test]
    fn change_timeline_is_separate_and_column_aligned() {
        let series = MetricSeries {
            timestamps: vec![
                "2026-07-28T00:00:00Z".into(),
                "2026-07-28T00:01:00Z".into(),
                "2026-07-28T00:02:00Z".into(),
                "2026-07-28T00:03:00Z".into(),
            ],
            values: vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
            ..MetricSeries::default()
        };
        let changes = vec![
            ChangePoint {
                timestamp: "2026-07-28T00:01:00Z".into(),
                change_type: "Update".into(),
                count: 1,
            },
            ChangePoint {
                timestamp: "2026-07-28T00:03:00Z".into(),
                change_type: "Update".into(),
                count: 3,
            },
        ];
        let graph = braille_graph(&series.values, 2, 2, None);
        assert_eq!(change_timeline(&series, &changes, 2), "▂█");
        assert_eq!(braille_graph(&series.values, 2, 2, None), graph);
    }

    #[test]
    fn watchlist_pins_within_tier_but_never_above_a_fault() {
        let mut snapshot = snapshot();
        let mut bad = snapshot.resources[0].clone();
        bad.name = "bad".into();
        bad.resource_id = "bad".into();
        bad.health_state = "unhealthy".into();
        bad.watched = false;
        let mut watched = snapshot.resources[0].clone();
        watched.name = "watched".into();
        watched.resource_id = "watched".into();
        watched.watched = true;
        snapshot.resources = vec![watched, bad];
        let resources = visible_resources(&snapshot, &UiState::default());
        assert_eq!(resources[0].name, "bad");

        snapshot.resources[1].health_state = "unknown".into();
        let resources = visible_resources(&snapshot, &UiState::default());
        assert_eq!(resources[0].name, "watched");
    }

    #[test]
    fn watch_expectations_are_bad_only_when_evaluable_and_violated() {
        let watched = AzureResource {
            watched: true,
            watch_expected_control: "running".into(),
            control_state: "stopped".into(),
            ..AzureResource::default()
        };
        assert_eq!(resource_attention(&watched).0, "BAD");
        assert!(resource_attention(&watched).1.contains("expected running"));

        let unevaluable = AzureResource {
            control_state: "unknown".into(),
            ..watched
        };
        assert_eq!(resource_attention(&unevaluable).0, "LIM");

        let expected_stopped = AzureResource {
            watched: true,
            watch_expected_control: "stopped".into(),
            control_state: "stopped".into(),
            evidence_state: EvidenceState::Signal,
            ..AzureResource::default()
        };
        assert_eq!(resource_attention(&expected_stopped).0, "SIG");
    }

    #[test]
    fn tall_wide_frames_are_chart_dense_without_color() {
        let snapshot = snapshot();
        let state = UiState {
            selected_id: snapshot.resources[0].resource_id.clone(),
            color: false,
            ..UiState::default()
        };
        for (width, height) in [(160u16, 50u16), (140, 40)] {
            let output = render_to_string(&snapshot, &state, width, height);
            let is_braille = |line: &&str| {
                line.chars()
                    .any(|character| ('\u{2800}'..='\u{28FF}').contains(&character))
            };
            let braille_rows = output.lines().filter(is_braille).count();
            assert!(
                braille_rows >= 6,
                "expected multi-row charts at {width}x{height}, got {braille_rows} braille rows"
            );
            // the selected-resource panel (lower region) draws a silhouette
            // line graph: sparse by design, but never empty
            let lower_rows = output
                .lines()
                .skip(height as usize / 2)
                .filter(is_braille)
                .count();
            assert!(
                lower_rows >= 1,
                "expected a selected-resource line graph at {width}x{height}"
            );
            // lanes without data are explicit states, never empty charts
            assert!(output.contains("no comparable series"));
        }
    }

    #[test]
    fn percentage_meter_is_fixed_width_and_clamped() {
        assert_eq!(meter(0.0, 5), "▱▱▱▱▱");
        assert_eq!(meter(50.0, 10), "▰▰▰▰▰▱▱▱▱▱");
        assert_eq!(meter(200.0, 5), "▰▰▰▰▰");
    }

    #[test]
    fn json_and_table_hide_ids() {
        let snapshot = snapshot();
        let json = snapshot.public_json().to_string();
        let table = render_table(&snapshot, 280);
        assert!(!json.contains("/subscriptions/"));
        assert!(!json.contains("\"secret\""));
        assert!(!table.contains("/subscriptions/"));
    }

    #[test]
    fn accessible_table_includes_inventory_only_resources() {
        let mut snapshot = snapshot();
        snapshot.resources.push(AzureResource {
            name: "network-inventory-only".into(),
            resource_type: "Microsoft.Network/virtualNetworks".into(),
            category: "network/edge".into(),
            evidence_state: EvidenceState::InventoryOnly,
            evidence_detail: "inventory metadata only".into(),
            ..AzureResource::default()
        });
        let table = render_table(&snapshot, 280);
        assert!(table.contains("network-inventory-only"));
        assert!(table.contains("INV"));
    }

    #[test]
    fn explicit_unhealthy_and_failed_provisioning_are_bad() {
        let unhealthy = AzureResource {
            health_state: "unhealthy".into(),
            ..AzureResource::default()
        };
        assert_eq!(resource_attention(&unhealthy).0, "BAD");
        let failed = AzureResource {
            health_state: "unknown".into(),
            resource_health_state: "unknown".into(),
            provisioning_state: "Failed".into(),
            ..AzureResource::default()
        };
        assert_eq!(resource_attention(&failed).0, "BAD");
    }

    #[test]
    fn failure_aggregate_requires_an_actionable_count_or_rate() {
        let mut resource = AzureResource {
            health_state: "unknown".into(),
            resource_health_state: "unknown".into(),
            provisioning_state: "Succeeded".into(),
            evidence_state: EvidenceState::Signal,
            ..AzureResource::default()
        };
        resource.metrics.insert(
            "http_5xx".into(),
            MetricSeries {
                state: "available".into(),
                values: vec![Some(5.0)],
                aggregation: "total".into(),
                ..MetricSeries::default()
            },
        );
        resource.metrics.insert(
            "requests".into(),
            MetricSeries {
                state: "available".into(),
                values: vec![Some(1_000.0)],
                aggregation: "total".into(),
                ..MetricSeries::default()
            },
        );
        assert_eq!(resource_attention(&resource).0, "SIG");
        resource.metrics.get_mut("http_5xx").unwrap().values = vec![Some(10.0)];
        assert_eq!(resource_attention(&resource).0, "WRN");
        assert!(resource_attention(&resource).1.contains("1.00%"));
    }

    #[test]
    fn positive_resource_health_does_not_manufacture_application_health() {
        let resource = AzureResource {
            health_state: "unknown".into(),
            resource_health_state: "available".into(),
            evidence_state: EvidenceState::InventoryOnly,
            ..AzureResource::default()
        };
        assert_eq!(resource_attention(&resource).0, "SIG");
        assert!(resource_attention(&resource)
            .1
            .contains("application health unknown"));
    }

    #[test]
    fn control_and_evidence_states_remain_distinct() {
        let base = AzureResource {
            health_state: "unknown".into(),
            resource_health_state: "unknown".into(),
            provisioning_state: "Succeeded".into(),
            diagnostic_state: "unknown".into(),
            ..AzureResource::default()
        };
        assert_eq!(
            resource_attention(&AzureResource {
                control_state: "Stopped".into(),
                ..base.clone()
            })
            .0,
            "STOP"
        );
        assert_eq!(
            resource_attention(&AzureResource {
                diagnostic_state: "unavailable".into(),
                ..base.clone()
            })
            .0,
            "LIM"
        );
        assert_eq!(resource_attention(&base).0, "INV");
        let mut signal = base;
        signal.evidence_state = EvidenceState::Signal;
        signal.evidence_detail = "sampled".into();
        signal.metrics.insert(
            "requests".into(),
            MetricSeries {
                state: "available".into(),
                values: vec![Some(0.0)],
                aggregation: "total".into(),
                ..MetricSeries::default()
            },
        );
        assert_eq!(resource_attention(&signal).0, "SIG");

        for (state, badge) in [
            (EvidenceState::NoData, "ND"),
            (EvidenceState::Pending, "PEND"),
            (EvidenceState::NotSampled, "CAP"),
            (EvidenceState::InventoryOnly, "INV"),
        ] {
            assert_eq!(
                resource_attention(&AzureResource {
                    evidence_state: state,
                    ..AzureResource::default()
                })
                .0,
                badge
            );
        }
    }

    #[test]
    fn initial_selection_uses_warning_then_most_active_metric_resource() {
        let mut snapshot = snapshot();
        let mut quiet = snapshot.resources[0].clone();
        quiet.name = "quiet".into();
        quiet.resource_id = "quiet".into();
        quiet.metrics.get_mut("requests").unwrap().values = vec![Some(1.0)];
        snapshot.resources.push(quiet);
        let state = UiState::default();
        assert_eq!(
            initial_selection(&snapshot, &state),
            snapshot.resources[0].resource_id
        );
        snapshot.resources[1].metrics.insert(
            "http_5xx".into(),
            MetricSeries {
                state: "available".into(),
                values: vec![Some(5.0)],
                aggregation: "total".into(),
                ..MetricSeries::default()
            },
        );
        assert_eq!(initial_selection(&snapshot, &state), "quiet");
    }

    #[test]
    fn fleet_aggregate_never_combines_different_metric_grains() {
        let mut snapshot = snapshot();
        let mut five_minute = snapshot.resources[0].clone();
        five_minute.name = "five-minute".into();
        five_minute.resource_id = "five-minute".into();
        let metric = five_minute.fleet_metrics.get_mut("requests").unwrap();
        metric.interval = "5m".into();
        metric.values = vec![Some(100.0), Some(100.0)];
        snapshot.resources.push(five_minute);
        let lane = first_aggregate(&snapshot, &["requests"], false, 1, 1).unwrap();
        assert_eq!(lane.series.interval, "1m");
        assert_eq!(lane.series.values, vec![Some(0.0), Some(10.0)]);
    }

    #[test]
    fn fleet_aggregate_only_uses_series_matching_the_requested_window() {
        let mut snapshot = snapshot(); // series window is 1h
        assert!(first_aggregate(&snapshot, &["requests"], false, 24, 15).is_none());
        // A provider may accept a coarser grain than requested. The query
        // cohort still matches the requested window while the series reports
        // the actually accepted grain.
        let fallback_query = test_query(1, 15);
        snapshot.fleet_query = fallback_query.clone();
        let metric = snapshot.resources[0]
            .fleet_metrics
            .get_mut("requests")
            .unwrap();
        metric.query = fallback_query;
        metric.interval = "1m".into();
        let lane = first_aggregate(&snapshot, &["requests"], false, 1, 15).unwrap();
        assert_eq!(lane.series.interval, "1m");
    }

    #[test]
    fn fleet_aggregate_preserves_bins_where_no_contributor_reported() {
        let mut snapshot = snapshot();
        let metric = snapshot.resources[0]
            .fleet_metrics
            .get_mut("requests")
            .unwrap();
        metric.timestamps.push("2026-07-28T00:02:00Z".into());
        metric.values = vec![Some(1.0), None, Some(2.0)];
        let lane = first_aggregate(&snapshot, &["requests"], false, 1, 1).unwrap();
        assert_eq!(lane.series.values, vec![Some(1.0), None, Some(2.0)]);
        assert_eq!(lane.series.timestamps.len(), 3);
    }

    #[test]
    fn pulse_traffic_prefers_one_family_over_cross_provider_sums() {
        let mut snapshot = snapshot();
        let mut cosmos = AzureResource {
            name: "cosmos".into(),
            resource_type: "Microsoft.DocumentDB/databaseAccounts".into(),
            resource_id: "cosmos".into(),
            ..AzureResource::default()
        };
        cosmos.fleet_metrics.insert(
            "requests".into(),
            MetricSeries {
                name: "requests".into(),
                unit: "Count".into(),
                window: "1h".into(),
                interval: "1m".into(),
                state: "available".into(),
                timestamps: vec!["2026-07-28T00:00:00Z".into(), "2026-07-28T00:01:00Z".into()],
                values: vec![Some(500.0), Some(500.0)],
                aggregation: "total".into(),
                query: snapshot.fleet_query.clone(),
                ..MetricSeries::default()
            },
        );
        snapshot.resources.push(cosmos.clone());
        // app traffic wins over the data-plane fallback and is never summed
        // with it into a fake "resource group traffic" total
        let lane = first_aggregate(&snapshot, &["requests"], false, 1, 1).unwrap();
        assert_eq!(lane.family, "app");
        assert_eq!(lane.contributors, 1);
        assert_eq!(lane.series.values, vec![Some(0.0), Some(10.0)]);
        // an edge profile outranks both when present
        let mut edge = cosmos;
        edge.name = "frontdoor".into();
        edge.resource_id = "frontdoor".into();
        edge.resource_type = "Microsoft.Cdn/profiles".into();
        snapshot.resources.push(edge);
        let lane = first_aggregate(&snapshot, &["requests"], false, 1, 1).unwrap();
        assert_eq!(lane.family, "edge");
        assert_eq!(lane.series.values, vec![Some(500.0), Some(500.0)]);
        // data-plane remains available as an explicitly labeled fallback
        snapshot
            .resources
            .retain(|resource| resource.name == "cosmos");
        let lane = first_aggregate(&snapshot, &["requests"], false, 1, 1).unwrap();
        assert_eq!(lane.family, "data-plane");
    }

    #[test]
    fn pulse_nonzero_app_traffic_beats_an_all_zero_edge_series() {
        let mut snapshot = snapshot();
        let mut edge = AzureResource {
            name: "frontdoor".into(),
            resource_type: "Microsoft.Cdn/profiles".into(),
            resource_id: "frontdoor".into(),
            ..AzureResource::default()
        };
        edge.fleet_metrics.insert(
            "requests".into(),
            MetricSeries {
                name: "requests".into(),
                unit: "Count".into(),
                window: "1h".into(),
                interval: "1m".into(),
                state: "available".into(),
                timestamps: vec!["2026-07-28T00:00:00Z".into(), "2026-07-28T00:01:00Z".into()],
                values: vec![Some(0.0), Some(0.0)],
                aggregation: "total".into(),
                query: snapshot.fleet_query.clone(),
                ..MetricSeries::default()
            },
        );
        snapshot.resources.push(edge);
        let lane = first_aggregate(&snapshot, &["requests"], false, 1, 1).unwrap();
        assert_eq!(lane.family, "app");
        assert_eq!(lane.series.values, vec![Some(0.0), Some(10.0)]);
    }

    #[test]
    fn pulse_failures_accepts_front_door_5xx_percentage() {
        let mut snapshot = snapshot();
        snapshot.resources.clear();
        let mut edge = AzureResource {
            name: "frontdoor".into(),
            resource_type: "Microsoft.Cdn/profiles".into(),
            resource_id: "frontdoor".into(),
            ..AzureResource::default()
        };
        edge.fleet_metrics.insert(
            "http_5xx_percent".into(),
            MetricSeries {
                name: "http_5xx_percent".into(),
                unit: "Percent".into(),
                window: "1h".into(),
                interval: "1m".into(),
                state: "available".into(),
                timestamps: vec!["2026-07-28T00:00:00Z".into(), "2026-07-28T00:01:00Z".into()],
                values: vec![Some(0.0), Some(1.25)],
                aggregation: "average".into(),
                query: snapshot.fleet_query.clone(),
                ..MetricSeries::default()
            },
        );
        snapshot.resources.push(edge);
        let lane = first_aggregate(&snapshot, PULSE_LANES[1].1, PULSE_LANES[1].2, 1, 1).unwrap();
        assert_eq!(lane.name, "http_5xx_percent");
        assert_eq!(lane.family, "edge");
        assert_eq!(lane.series.values, vec![Some(0.0), Some(1.25)]);
    }

    #[test]
    fn provider_selected_orders_cover_every_fixed_metric_adapter() {
        use crate::azure::metric_adapter;

        for resource_type in [
            "Microsoft.Web/sites",
            "Microsoft.Web/sites/slots",
            "Microsoft.Web/serverfarms",
            "Microsoft.DBforPostgreSQL/flexibleServers",
            "Microsoft.Search/searchServices",
            "Microsoft.CognitiveServices/accounts",
            "Microsoft.Cdn/profiles",
            "Microsoft.Storage/storageAccounts",
            "Microsoft.KeyVault/vaults",
            "Microsoft.ContainerRegistry/registries",
            "Microsoft.Sql/servers/databases",
            "Microsoft.DocumentDB/databaseAccounts",
            "Microsoft.Cache/Redis",
            "Microsoft.Network/azureFirewalls",
            "Microsoft.Logic/workflows",
        ] {
            let order = provider_selected_order(resource_type);
            for metric in metric_adapter(resource_type) {
                assert!(
                    order.iter().any(|(name, _)| *name == metric.public_name),
                    "{resource_type} metric {} has no selected-resource label",
                    metric.public_name
                );
            }
        }
    }

    #[test]
    fn selected_provider_metrics_keep_order_and_safe_fallback_in_exact_cohort() {
        let query = test_query(1, 1);
        let metric = |name: &str| MetricSeries {
            name: name.into(),
            unit: "Count".into(),
            window: "1h".into(),
            interval: "1m".into(),
            state: "available".into(),
            timestamps: vec!["2026-07-28T00:00:00Z".into()],
            values: vec![Some(1.0)],
            aggregation: "average".into(),
            query: query.clone(),
            ..MetricSeries::default()
        };
        let resource = AzureResource {
            name: "cache".into(),
            resource_type: "Microsoft.Cache/Redis".into(),
            resource_id: "cache".into(),
            metrics: BTreeMap::from([
                ("cache_miss_percent".into(), metric("cache_miss_percent")),
                ("connected_clients".into(), metric("connected_clients")),
                (
                    "future_safe_aggregate".into(),
                    metric("future_safe_aggregate"),
                ),
                ("server_load_percent".into(), metric("server_load_percent")),
            ]),
            ..AzureResource::default()
        };
        let mut snapshot = snapshot();
        snapshot.resources = vec![resource];
        let state = UiState {
            selected_id: "cache".into(),
            color: false,
            ..UiState::default()
        };
        let selected = selected_series(&snapshot, &snapshot.resources[0], &state);
        assert_eq!(
            selected
                .iter()
                .map(|(label, name, _)| (label.as_str(), *name))
                .collect::<Vec<_>>(),
            vec![
                ("SERVER LOAD", "server_load_percent"),
                ("CLIENTS", "connected_clients"),
                ("CACHE MISS", "cache_miss_percent"),
                ("FUTURE SAFE AGGREGATE", "future_safe_aggregate"),
            ]
        );
    }

    #[test]
    fn pulse_and_attention_report_fleet_freshness_and_operational_scope() {
        let mut snapshot = snapshot();
        snapshot.resources.push(AzureResource {
            name: "inventory-only".into(),
            resource_id: "inventory-only".into(),
            evidence_state: EvidenceState::InventoryOnly,
            ..AzureResource::default()
        });
        let state = UiState {
            selected_id: snapshot.resources[0].resource_id.clone(),
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot, &state, 240, 60);
        assert!(output.contains("fleet current · read"));
        assert!(output.contains("1/2 operational"));
    }

    #[test]
    fn requested_window_mismatch_hides_stale_evidence_and_says_so() {
        let snapshot = snapshot(); // series carry window 1h/1m
        let state = UiState {
            selected_id: snapshot.resources[0].resource_id.clone(),
            window_hours: 24,
            interval_minutes: 15,
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot, &state, 160, 50);
        assert!(output.contains("NO MATCHING DATA 24h/15m"));
        assert!(output.contains("stale evidence hidden"));
        assert!(
            !output.contains("total 10.0"),
            "stale 1h series must not render under a 24h title"
        );
        let loading = UiState {
            window_loading: true,
            ..state
        };
        let output = render_to_string(&snapshot, &loading, 160, 50);
        assert!(output.contains("LOADING 24h/15m"));
        assert!(output.contains("previous evidence hidden"));
    }

    #[test]
    fn ultrawide_frame_spreads_lanes_and_keeps_attention_columns_grouped() {
        let mut snapshot = snapshot();
        snapshot.resources[0].name = "a-very-long-resource-name-that-should-not-eat-the-row".into();
        let state = UiState {
            selected_id: snapshot.resources[0].resource_id.clone(),
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot, &state, 320, 70);
        assert_eq!(output.lines().count(), 70);
        assert!(output.contains("resource group pulse"));
        assert!(output.contains("selected resource metrics"));
        assert!(output.contains("attention queue"));
        // no-data lanes remain an explicit compact rail.
        assert!(output.contains("NO DATA"));
        // the traffic graph reaches into the left quarter of the canvas
        let braille_far_left = output.lines().take(24).any(|line| {
            line.char_indices().any(|(index, character)| {
                index < 40 && ('\u{2800}'..='\u{28FF}').contains(&character)
            })
        });
        assert!(braille_far_left, "pulse graph must use the left canvas");
        // RESOURCE is capped so operational columns stay grouped beside it
        let header = output
            .lines()
            .find(|line| line.contains("RESOURCE") && line.contains("SIGNAL"))
            .expect("attention header");
        let resource_at = header.find("RESOURCE").unwrap();
        let type_at = header.find("TYPE").unwrap();
        let age_at = header.find("DATA AGE").unwrap();
        assert!(
            type_at - resource_at <= 46,
            "TYPE drifted {} cells from RESOURCE",
            type_at - resource_at
        );
        assert!(
            age_at - resource_at <= 120,
            "operational columns must stay a dense group, DATA AGE at {age_at}"
        );
        assert!(output.contains("app · 1res"));
    }

    #[test]
    fn table_spells_out_sources_windows_and_limitations() {
        let mut snapshot = snapshot();
        snapshot
            .details
            .limitations
            .push("permission limited".into());
        let table = render_table(&snapshot, 280);
        assert!(table.contains("Azure Resource Graph fixed inventory"));
        assert!(table.contains("Azure Monitor fixed bounded aggregates"));
        assert!(table.contains("1h 1m"));
        assert!(table.contains("LIM permission limited"));
    }

    #[test]
    fn help_and_inventory_views_are_test_backend_renderable() {
        let snapshot = snapshot();
        let help = UiState {
            overlay: Overlay::Help,
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot, &help, 100, 30);
        assert!(output.contains("HELP"));
        assert!(output.contains("Shift+L"));
        let inventory = UiState {
            view: ViewMode::Inventory,
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot, &inventory, 140, 40);
        assert!(output.contains("view inventory"));
        assert!(output.contains("bounded aggregates"));
    }

    #[test]
    fn chooser_filter_is_local_and_marks_current_scope() {
        let mut snapshot = snapshot();
        snapshot.resource_groups = vec![
            crate::model::ResourceGroup {
                name: "production".into(),
                ..crate::model::ResourceGroup::default()
            },
            crate::model::ResourceGroup {
                name: "staging".into(),
                ..crate::model::ResourceGroup::default()
            },
        ];
        snapshot.selected_resource_group = "staging".into();
        let chooser = ChooserState {
            mode: ChooserMode::Group,
            query: "stag".into(),
            selected: 0,
        };
        let choices = chooser_choices(&snapshot, &chooser);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].0, "staging");
        assert!(choices[0].2);
    }

    #[test]
    fn aggregate_log_overlay_declares_safety_and_no_health_inference() {
        let state = UiState {
            overlay: Overlay::LogSignals {
                loading: false,
                result: Some(Box::new(LogSignalResult {
                    resource_name: "app".into(),
                    state: "no_data".into(),
                    source: "Azure Monitor Logs fixed aggregate".into(),
                    window: "1h".into(),
                    interval: "5m".into(),
                    ..LogSignalResult::default()
                })),
                error: String::new(),
            },
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot(), &state, 120, 35);
        assert!(output.contains("AGGREGATE ONLY"));
        assert!(output.contains("no messages"));
        assert!(output.contains("No health verdict"));
    }

    #[test]
    fn raw_confirmation_component_remains_explicitly_sensitive() {
        let state = UiState {
            overlay: Overlay::RawConfirm(Some(RawLogTarget {
                provider: "Container Apps".into(),
                description: "configured stream".into(),
                command: vec!["az".into()],
            })),
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot(), &state, 110, 30);
        assert!(output.contains("SENSITIVE"));
        assert!(output.contains("Press y to connect"));
        assert!(output.contains("never exported"));
    }

    #[test]
    fn direct_raw_confirmation_explains_credential_boundary() {
        let state = UiState {
            overlay: Overlay::RawConfirm(None),
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot(), &state, 120, 30);
        assert!(output.contains("BLOCKED"));
        assert!(output.contains("explanation only"));
        assert!(output.contains("credentials or full service configuration"));
        assert!(output.contains("No Azure read was attempted"));
        assert!(!output.contains("200-line"));
    }

    #[test]
    fn app_service_plan_metrics_are_labeled_shared() {
        let mut snapshot = snapshot();
        let query = snapshot.fleet_query.clone();
        let plan_id =
            "/subscriptions/private/resourceGroups/rg/providers/Microsoft.Web/serverfarms/plan";
        snapshot.resources[0].hosting_plan_id = plan_id.into();
        snapshot.resources.push(AzureResource {
            name: "plan".into(),
            resource_type: "Microsoft.Web/serverfarms".into(),
            resource_id: plan_id.into(),
            metrics: BTreeMap::from([(
                "cpu_percent".into(),
                MetricSeries {
                    name: "cpu_percent".into(),
                    unit: "Percent".into(),
                    window: "1h".into(),
                    interval: "1m".into(),
                    state: "available".into(),
                    timestamps: vec!["2026-07-28T00:00:00Z".into()],
                    values: vec![Some(42.0)],
                    aggregation: "average".into(),
                    query,
                    ..MetricSeries::default()
                },
            )]),
            ..AzureResource::default()
        });
        let state = UiState {
            selected_id: snapshot.resources[0].resource_id.clone(),
            color: false,
            ..UiState::default()
        };
        let output = render_to_string(&snapshot, &state, 160, 50);
        assert!(output.contains("SHARED PLAN"));
        assert!(!snapshot
            .public_json()
            .to_string()
            .contains("serverfarms/plan"));
    }

    #[test]
    fn unit_formatting_preserves_provider_units() {
        assert_eq!(
            format_value(Some(1500.0), "Milliseconds", "latency"),
            "1.50s"
        );
        assert_eq!(
            format_value(Some(66_200.0), "Milliseconds", "latency"),
            "1.10m"
        );
        assert_eq!(
            format_value(Some(1024.0 * 1024.0), "Bytes", "memory"),
            "1.00MiB"
        );
        assert_eq!(format_value(Some(42.0), "Percent", "cpu_percent"), "42.0%");
    }
}
