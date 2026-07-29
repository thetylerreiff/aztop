use std::{
    collections::HashSet,
    io::{self, Stdout},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::Show,
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{stream, StreamExt};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    azure::{
        mark_metric_candidates, merge_enriched_snapshot, merge_fleet_resources, merge_resources,
        metric_query, reconcile_inventory, select_metric_candidates, Collector,
    },
    cache::CacheStore,
    logs::{raw_log_target, LogCollector, RawLogStream},
    model::{AzureResource, EvidenceState, LogSignalResult, MetricQuery, Snapshot},
    render::{
        chooser_choices, draw, initial_selection, selected_resource, visible_resources,
        ChooserMode, ChooserState, Overlay, UiState, ViewMode, CATEGORIES,
    },
};

enum WorkerEvent {
    Inventory {
        generation: u64,
        result: Result<Snapshot, String>,
    },
    Enriched {
        generation: u64,
        task_id: u64,
        snapshot: Snapshot,
    },
    Metrics {
        generation: u64,
        task_id: u64,
        subscription: String,
        group: String,
        query: MetricQuery,
        resources: Vec<AzureResource>,
        focused: bool,
        complete: bool,
        timed_out: bool,
        focus_resource_ids: Vec<String>,
    },
    Logs {
        generation: u64,
        result: LogSignalResult,
    },
}

struct WorkerTask {
    handle: JoinHandle<()>,
    collector: Option<Collector>,
}

impl WorkerTask {
    fn stop(self) {
        if let Some(collector) = self.collector {
            collector.cancel();
        }
        self.handle.abort();
    }
}

struct Tasks {
    scope: Option<WorkerTask>,
    enrichment: Option<WorkerTask>,
    focus: Option<WorkerTask>,
    fleet: Option<WorkerTask>,
    logs: Option<WorkerTask>,
}

impl Tasks {
    fn new() -> Self {
        Self {
            scope: None,
            enrichment: None,
            focus: None,
            fleet: None,
            logs: None,
        }
    }

    fn abort_all(&mut self) {
        for task in [
            &mut self.scope,
            &mut self.enrichment,
            &mut self.focus,
            &mut self.fleet,
            &mut self.logs,
        ] {
            if let Some(task) = task.take() {
                task.stop();
            }
        }
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = write_terminal_restore(self.terminal.backend_mut());
    }
}

fn write_terminal_restore(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(writer, LeaveAlternateScreen, Show)
}

pub struct App {
    collector: Collector,
    snapshot: Snapshot,
    ui: UiState,
    watch_seconds: u64,
    generation: u64,
    enrichment_serial: u64,
    active_enrichment: Option<u64>,
    metric_serial: u64,
    active_focus: Option<u64>,
    active_fleet: Option<u64>,
    active_fleet_query: Option<MetricQuery>,
    active_fleet_resource_ids: HashSet<String>,
    force_metric_refresh: bool,
    log_generation: u64,
    log_window_index: usize,
    log_windows: [u64; 3],
    tx: mpsc::UnboundedSender<WorkerEvent>,
    rx: mpsc::UnboundedReceiver<WorkerEvent>,
    tasks: Tasks,
    raw_stream: RawLogStream,
    next_focus: Option<Instant>,
    next_fleet: Option<Instant>,
    next_inventory: Option<Instant>,
    focus_debounce: Option<Instant>,
    pending_group: Option<(Instant, String)>,
    cache: Option<CacheStore>,
    cache_save: Option<JoinHandle<()>>,
    pending_cache: Option<(CacheStore, Snapshot)>,
    session_stars: HashSet<(String, String, String, String)>,
    startup_scope: Option<(String, String)>,
    user_navigated: bool,
    initial_evidence_selection_done: bool,
    dirty: bool,
}

impl App {
    pub fn new(collector: Collector, snapshot: Snapshot, watch_seconds: u64, color: bool) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut ui = UiState {
            window_hours: collector.config.metric_window_hours,
            interval_minutes: collector.config.metric_interval_minutes,
            color,
            ..UiState::default()
        };
        ui.window_loading = collector.metrics_enabled
            && !snapshot
                .fleet_query
                .matches(ui.window_hours, ui.interval_minutes);
        ui.selected_id = initial_selection(&snapshot, &ui);
        let now = Instant::now();
        let (focus, fleet, inventory) = refresh_cadences(watch_seconds);
        let session_stars = snapshot
            .resources
            .iter()
            .filter(|resource| resource.session_starred)
            .map(|resource| session_star_key(&snapshot, resource))
            .collect();
        Self {
            collector,
            snapshot,
            ui,
            watch_seconds,
            generation: 0,
            enrichment_serial: 0,
            active_enrichment: None,
            metric_serial: 0,
            active_focus: None,
            active_fleet: None,
            active_fleet_query: None,
            active_fleet_resource_ids: HashSet::new(),
            force_metric_refresh: false,
            log_generation: 0,
            log_window_index: 1,
            log_windows: [15, 60, 360],
            tx,
            rx,
            tasks: Tasks::new(),
            raw_stream: RawLogStream::default(),
            next_focus: focus.map(|seconds| now + Duration::from_secs(seconds)),
            next_fleet: fleet.map(|seconds| now + Duration::from_secs(seconds)),
            next_inventory: inventory.map(|seconds| now + Duration::from_secs(seconds)),
            focus_debounce: None,
            pending_group: None,
            cache: None,
            cache_save: None,
            pending_cache: None,
            session_stars,
            startup_scope: None,
            user_navigated: false,
            initial_evidence_selection_done: false,
            dirty: true,
        }
    }

    pub fn with_cache(
        mut self,
        cache: CacheStore,
        startup_scope: Option<(String, String)>,
    ) -> Self {
        self.cache = Some(cache);
        self.startup_scope = startup_scope;
        self
    }

    pub async fn run(mut self) -> io::Result<()> {
        let mut terminal = TerminalGuard::enter()?;
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let mut redraw = tokio::time::interval(Duration::from_secs(1));
        if let Some((subscription, group)) = self.startup_scope.take() {
            self.spawn_scope(subscription, group);
        } else {
            self.spawn_enrichment();
        }
        let result = loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.poll_raw().await;
                    self.poll_cache_save();
                    self.run_schedules();
                }
                _ = redraw.tick() => self.dirty = true,
                Some(event) = self.rx.recv() => self.handle_worker(event),
                event = events.next() => {
                    match event {
                        Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press => {
                            if !self.handle_key(key).await {
                                break Ok(());
                            }
                        }
                        Some(Ok(Event::Resize(_, _))) => self.dirty = true,
                        Some(Err(error)) => break Err(error),
                        _ => {}
                    }
                }
            }
            if self.dirty {
                if let Err(error) = terminal
                    .terminal
                    .draw(|frame| draw(frame, &self.snapshot, &self.ui))
                {
                    break Err(error);
                }
                self.dirty = false;
            }
        };
        self.shutdown().await;
        result
    }

    async fn shutdown(&mut self) {
        self.tasks.abort_all();
        self.raw_stream.stop().await;
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn spawn_enrichment(&mut self) {
        if !self.collector.metrics_enabled || self.snapshot.access_state != "available" {
            return;
        }
        let generation = self.generation;
        self.enrichment_serial += 1;
        let task_id = self.enrichment_serial;
        self.active_enrichment = Some(task_id);
        if let Some(task) = self.tasks.enrichment.take() {
            task.stop();
        }
        let collector = self.collector.child();
        let cancellation = collector.clone();
        let snapshot = self.snapshot.clone();
        let tx = self.tx.clone();
        self.snapshot.enrichment_state = "updating".into();
        self.ui.window_loading = !self
            .snapshot
            .fleet_query
            .matches(self.ui.window_hours, self.ui.interval_minutes);
        self.ui.operation = format!(
            "RG {} · LOADING OPERATOR SIGNALS",
            self.snapshot.selected_resource_group
        );
        let handle = tokio::spawn(async move {
            // Give the selected/fleet metric scheduler first access to the
            // shared Azure CLI gate so the initial useful chart is not queued
            // behind lower-frequency metadata reads.
            tokio::time::sleep(Duration::from_millis(350)).await;
            let snapshot = collector.enrich_metadata(snapshot).await;
            let _ = tx.send(WorkerEvent::Enriched {
                generation,
                task_id,
                snapshot,
            });
        });
        self.tasks.enrichment = Some(WorkerTask {
            handle,
            collector: Some(cancellation),
        });
        self.next_focus = Some(Instant::now());
        self.next_fleet = Some(Instant::now());
    }

    fn spawn_scope(&mut self, subscription: String, group: String) {
        self.spawn_scope_request(subscription, group, false);
    }

    fn spawn_scope_forced(&mut self, subscription: String, group: String) {
        self.spawn_scope_request(subscription, group, true);
    }

    fn spawn_scope_request(
        &mut self,
        subscription: String,
        group: String,
        force_metric_refresh: bool,
    ) {
        let generation = self.next_generation();
        self.force_metric_refresh = force_metric_refresh;
        if let Some(task) = self.tasks.scope.take() {
            task.stop();
        }
        if let Some(task) = self.tasks.enrichment.take() {
            task.stop();
        }
        self.active_enrichment = None;
        if let Some(task) = self.tasks.focus.take() {
            task.stop();
        }
        self.active_focus = None;
        if let Some(task) = self.tasks.fleet.take() {
            task.stop();
        }
        self.active_fleet = None;
        self.active_fleet_query = None;
        self.active_fleet_resource_ids.clear();
        self.next_focus = None;
        self.next_fleet = None;
        self.focus_debounce = None;
        let collector = self.collector.child();
        let cancellation = collector.clone();
        let current = self.snapshot.clone();
        let same_scope = current.origin != "cache"
            && subscription == current.selected_subscription_id
            && group == current.selected_resource_group;
        let tx = self.tx.clone();
        self.snapshot.inventory_state = "updating".into();
        self.ui.window_loading = self.collector.metrics_enabled;
        self.ui.operation = if group == self.snapshot.selected_resource_group {
            format!("RG {group} · UPDATING INVENTORY")
        } else {
            format!(
                "{} → {} · LOADING INVENTORY",
                self.snapshot.selected_resource_group, group
            )
        };
        let handle = tokio::spawn(async move {
            let result = if same_scope {
                collector.refresh_current_inventory(&current).await
            } else {
                collector.collect_inventory(&subscription, &group).await
            }
            .map_err(|error| error.detail);
            let _ = tx.send(WorkerEvent::Inventory { generation, result });
        });
        self.tasks.scope = Some(WorkerTask {
            handle,
            collector: Some(cancellation),
        });
    }

    fn spawn_metrics(
        &mut self,
        resources: Vec<AzureResource>,
        focused: bool,
        focus_resource_ids: Vec<String>,
        query: MetricQuery,
    ) {
        if !self.collector.metrics_enabled || self.snapshot.origin == "cache" {
            return;
        }
        if resources.is_empty() {
            let now = Instant::now();
            if focused {
                self.next_focus = refresh_cadences(self.watch_seconds)
                    .0
                    .map(|seconds| now + Duration::from_secs(seconds));
            } else {
                self.active_fleet = None;
                self.active_fleet_query = None;
                self.active_fleet_resource_ids.clear();
                self.snapshot.fleet_query = query;
                self.snapshot.fleet_state = "no_data".into();
                self.ui.window_loading = false;
                self.ui.operation =
                    "NO METRIC-CAPABLE RESOURCES IN THE CURRENT FILTERED SCOPE".into();
                self.next_fleet = refresh_cadences(self.watch_seconds)
                    .1
                    .map(|seconds| now + Duration::from_secs(seconds));
            }
            self.dirty = true;
            return;
        }
        self.metric_serial += 1;
        let task_id = self.metric_serial;
        if focused {
            self.active_focus = Some(task_id);
        } else {
            self.active_fleet = Some(task_id);
            self.active_fleet_query = Some(query.clone());
            self.active_fleet_resource_ids = resources
                .iter()
                .map(|resource| resource.resource_id.to_ascii_lowercase())
                .collect();
        }
        let generation = self.generation;
        for target in &resources {
            if let Some(resource) = self.snapshot.resources.iter_mut().find(|resource| {
                resource
                    .resource_id
                    .eq_ignore_ascii_case(&target.resource_id)
            }) {
                if resource.metrics.is_empty()
                    || matches!(
                        resource.evidence_state,
                        EvidenceState::Pending
                            | EvidenceState::NotSampled
                            | EvidenceState::InventoryOnly
                    )
                {
                    resource.evidence_state = EvidenceState::Pending;
                    resource.evidence_detail = if focused {
                        "focused metric refresh in flight"
                    } else {
                        "fleet metric refresh in flight"
                    }
                    .into();
                }
            }
        }
        if !focused {
            self.snapshot.fleet_state = "updating".into();
            self.ui.window_loading = true;
        }
        let collector = self.collector.child();
        let cancellation = collector.clone();
        let max_workers = collector.config.max_workers;
        let subscription = self.snapshot.selected_subscription_id.clone();
        let group = self.snapshot.selected_resource_group.clone();
        let tx = self.tx.clone();
        self.ui.operation = format!(
            "WINDOW {}h/{} · LOADING {}",
            query.window_hours,
            query.interval_label(),
            if focused {
                "SELECTED RESOURCE"
            } else {
                "BOUNDED FLEET SAMPLE"
            }
        );
        let task = tokio::spawn(async move {
            let mut pending = stream::iter(resources)
                .map(|resource| {
                    let collector = collector.clone();
                    let subscription = subscription.clone();
                    let query = query.clone();
                    async move {
                        collector
                            .refresh_metric(&subscription, resource, &query)
                            .await
                    }
                })
                .buffer_unordered(max_workers);
            let mut collected = Vec::new();
            let deadline = tokio::time::sleep(Duration::from_secs(if focused { 20 } else { 60 }));
            tokio::pin!(deadline);
            let mut timed_out = false;
            loop {
                tokio::select! {
                    resource = pending.next() => {
                        let Some(resource) = resource else {
                            break;
                        };
                        collected.push(resource.clone());
                        let _ = tx.send(WorkerEvent::Metrics {
                            generation,
                            task_id,
                            subscription: subscription.clone(),
                            group: group.clone(),
                            query: query.clone(),
                            resources: vec![resource],
                            focused,
                            complete: false,
                            timed_out: false,
                            focus_resource_ids: focus_resource_ids.clone(),
                        });
                    }
                    _ = &mut deadline => {
                        timed_out = true;
                        break;
                    }
                }
            }
            drop(pending);
            let _ = tx.send(WorkerEvent::Metrics {
                generation,
                task_id,
                subscription,
                group,
                query,
                resources: collected,
                focused,
                complete: true,
                timed_out,
                focus_resource_ids,
            });
        });
        let slot = if focused {
            &mut self.tasks.focus
        } else {
            &mut self.tasks.fleet
        };
        if let Some(previous) = slot.replace(WorkerTask {
            handle: task,
            collector: Some(cancellation),
        }) {
            previous.stop();
        }
    }

    fn spawn_log_query(&mut self) {
        if self.snapshot.origin == "cache" {
            self.ui.overlay = Overlay::LogSignals {
                loading: false,
                result: None,
                error: "cached snapshot · live scope refresh still pending".into(),
            };
            return;
        }
        let Some(resource) = selected_resource(&self.snapshot, &self.ui).cloned() else {
            return;
        };
        self.log_generation += 1;
        let generation = self.log_generation;
        if let Some(task) = self.tasks.logs.take() {
            task.stop();
        }
        let collector = LogCollector::new(self.collector.azure.child());
        let subscription = self.snapshot.selected_subscription_id.clone();
        let cloud = self
            .snapshot
            .subscriptions
            .iter()
            .find(|candidate| candidate.subscription_id == subscription)
            .map(|candidate| candidate.cloud.clone())
            .unwrap_or_default();
        let group = self.snapshot.selected_resource_group.clone();
        let window = self.log_windows[self.log_window_index];
        let tx = self.tx.clone();
        self.ui.overlay = Overlay::LogSignals {
            loading: true,
            result: None,
            error: String::new(),
        };
        let handle = tokio::spawn(async move {
            let result = collector
                .collect(&cloud, &subscription, &group, &resource, window)
                .await;
            let _ = tx.send(WorkerEvent::Logs { generation, result });
        });
        self.tasks.logs = Some(WorkerTask {
            handle,
            collector: None,
        });
    }

    fn handle_worker(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Inventory { generation, result } if generation == self.generation => {
                self.tasks.scope.take();
                match result {
                    Ok(snapshot) => {
                        let selected = selected_identity(&self.snapshot, &self.ui);
                        if self.cache.as_ref().is_some_and(|cache| {
                            !cache.matches_scope(
                                &snapshot.selected_subscription_name,
                                &snapshot.selected_subscription_id,
                                &snapshot.selected_resource_group,
                            )
                        }) {
                            self.cache = self.cache.as_ref().map(|cache| {
                                cache.retarget(
                                    &snapshot.selected_subscription_id,
                                    &snapshot.selected_resource_group,
                                )
                            });
                        }
                        self.snapshot = reconcile_inventory(&self.snapshot, snapshot);
                        apply_session_stars(&mut self.snapshot, &self.session_stars);
                        restore_selection(&self.snapshot, &mut self.ui, selected);
                        if !self.snapshot.access_state.eq_ignore_ascii_case("available") {
                            self.force_metric_refresh = false;
                            self.active_enrichment = None;
                            self.active_focus = None;
                            self.active_fleet = None;
                            self.active_fleet_query = None;
                            self.active_fleet_resource_ids.clear();
                            self.next_focus = None;
                            self.next_fleet = None;
                            self.next_inventory = refresh_cadences(self.watch_seconds)
                                .2
                                .map(|seconds| Instant::now() + Duration::from_secs(seconds));
                            self.snapshot.enrichment_state = if self.collector.metrics_enabled {
                                "unavailable"
                            } else {
                                "disabled"
                            }
                            .into();
                            self.snapshot.fleet_state = if self.collector.metrics_enabled {
                                "unavailable"
                            } else {
                                "disabled"
                            }
                            .into();
                            self.snapshot.fleet_query = MetricQuery::default();
                            self.ui.window_loading = false;
                            self.ui.operation = format!(
                                "ACCESS {} · {}",
                                self.snapshot.access_state.to_ascii_uppercase(),
                                if self.snapshot.access_detail.is_empty() {
                                    "inventory unavailable in this scope"
                                } else {
                                    &self.snapshot.access_detail
                                }
                            );
                        } else if self.collector.metrics_enabled {
                            self.spawn_enrichment();
                        } else {
                            self.force_metric_refresh = false;
                            self.snapshot.enrichment_state = "disabled".into();
                            self.ui.operation.clear();
                            self.reset_schedules();
                            self.save_cache();
                        }
                    }
                    Err(error) => {
                        self.force_metric_refresh = false;
                        self.snapshot.inventory_state = "stale".into();
                        self.ui.operation = format!("STALE · refresh unavailable · {error}");
                        self.reset_schedules();
                    }
                }
                self.dirty = true;
            }
            WorkerEvent::Enriched {
                generation,
                task_id,
                snapshot,
            } if generation == self.generation && self.active_enrichment == Some(task_id) => {
                self.tasks.enrichment.take();
                self.active_enrichment = None;
                let selected = selected_identity(&self.snapshot, &self.ui);
                self.snapshot = merge_enriched_snapshot(&self.snapshot, snapshot);
                apply_session_stars(&mut self.snapshot, &self.session_stars);
                restore_selection(&self.snapshot, &mut self.ui, selected);
                self.snapshot.enrichment_state = "current".into();
                if self.snapshot.fleet_state != "updating" {
                    self.ui.operation.clear();
                }
                self.next_inventory = refresh_cadences(self.watch_seconds)
                    .2
                    .map(|seconds| Instant::now() + Duration::from_secs(seconds));
                self.save_cache();
                self.dirty = true;
            }
            WorkerEvent::Metrics {
                generation,
                task_id,
                subscription,
                group,
                query,
                resources,
                focused,
                complete,
                timed_out,
                focus_resource_ids,
            } if generation == self.generation
                && subscription == self.snapshot.selected_subscription_id
                && group == self.snapshot.selected_resource_group
                && query.matches(self.ui.window_hours, self.ui.interval_minutes)
                && if focused {
                    self.active_focus == Some(task_id)
                } else {
                    self.active_fleet == Some(task_id)
                } =>
            {
                let selected = self.ui.selected_id.clone();
                // Both layers feed the operational table, but merge_resources
                // rejects an older query so a fleet result can never regress a
                // newer focused chart.
                merge_resources(&mut self.snapshot.resources, resources.clone());
                if self
                    .snapshot
                    .resources
                    .iter()
                    .any(|resource| resource.resource_id == selected)
                {
                    self.ui.selected_id = selected;
                }
                if complete {
                    if focused {
                        self.tasks.focus.take();
                        self.active_focus = None;
                    } else {
                        self.tasks.fleet.take();
                        self.active_fleet = None;
                        self.active_fleet_query = None;
                        self.active_fleet_resource_ids.clear();
                    }
                    let now = Instant::now();
                    if focused {
                        self.next_focus = refresh_cadences(self.watch_seconds)
                            .0
                            .map(|s| now + Duration::from_secs(s));
                    } else {
                        merge_fleet_resources(&mut self.snapshot.resources, resources);
                        self.snapshot.fleet_query = query.clone();
                        self.snapshot.fleet_state = if timed_out {
                            "partial".into()
                        } else {
                            "current".into()
                        };
                        self.ui.window_loading = false;
                        self.next_fleet = refresh_cadences(self.watch_seconds)
                            .1
                            .map(|s| now + Duration::from_secs(s));
                        if !focus_resource_ids.is_empty() {
                            self.next_focus = refresh_cadences(self.watch_seconds)
                                .0
                                .map(|s| now + Duration::from_secs(s));
                        }
                        if !self.user_navigated && !self.initial_evidence_selection_done {
                            self.ui.selected_id = initial_selection(&self.snapshot, &self.ui);
                            self.initial_evidence_selection_done = true;
                        }
                        self.save_cache();
                    }
                    self.ui.operation = if timed_out {
                        format!(
                            "{} METRICS PARTIAL · layer deadline reached; completed reads retained",
                            if focused { "SELECTED" } else { "FLEET" }
                        )
                    } else if self.snapshot.enrichment_state == "updating" {
                        "METRICS CURRENT · operator signals still loading".into()
                    } else {
                        String::new()
                    };
                }
                self.dirty = true;
            }
            WorkerEvent::Logs { generation, result } if generation == self.log_generation => {
                self.ui.overlay = Overlay::LogSignals {
                    loading: false,
                    result: Some(Box::new(result)),
                    error: String::new(),
                };
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn save_cache(&mut self) {
        let Some(cache) = self.cache.clone().filter(CacheStore::is_enabled) else {
            return;
        };
        let request = (cache, self.snapshot.clone());
        if self
            .cache_save
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            self.pending_cache = Some(request);
            return;
        }
        self.cache_save.take();
        self.start_cache_save(request);
    }

    fn start_cache_save(&mut self, (cache, snapshot): (CacheStore, Snapshot)) {
        self.cache_save = Some(tokio::task::spawn_blocking(move || {
            let _ = cache.save(&snapshot);
        }));
    }

    fn poll_cache_save(&mut self) {
        if self
            .cache_save
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            self.cache_save.take();
            if let Some(request) = self.pending_cache.take() {
                self.start_cache_save(request);
            }
        }
    }

    fn reset_schedules(&mut self) {
        let now = Instant::now();
        let (focus, fleet, inventory) = refresh_cadences(self.watch_seconds);
        self.next_focus = focus.map(|seconds| now + Duration::from_secs(seconds));
        self.next_fleet = fleet.map(|seconds| now + Duration::from_secs(seconds));
        self.next_inventory = inventory.map(|seconds| now + Duration::from_secs(seconds));
    }

    fn run_schedules(&mut self) {
        if !matches!(
            self.ui.overlay,
            Overlay::None | Overlay::Help | Overlay::Detail
        ) {
            return;
        }
        let now = Instant::now();
        if self
            .pending_group
            .as_ref()
            .is_some_and(|(deadline, _)| now >= *deadline)
        {
            if let Some((_, group)) = self.pending_group.take() {
                self.spawn_scope(self.scope_subscription(), group);
                return;
            }
        }
        if self.tasks.scope.is_some() {
            return;
        }
        if self.next_inventory.is_some_and(|deadline| now >= deadline)
            && self.tasks.enrichment.is_none()
            && self.tasks.focus.is_none()
            && self.tasks.fleet.is_none()
        {
            self.next_inventory = None;
            self.spawn_scope(
                self.scope_subscription(),
                self.snapshot.selected_resource_group.clone(),
            );
            return;
        }
        let focus_due = self.focus_debounce.is_some_and(|deadline| now >= deadline)
            || self.next_focus.is_some_and(|deadline| now >= deadline);
        let fleet_due = self.next_fleet.is_some_and(|deadline| now >= deadline);
        let query = metric_query(self.ui.window_hours, self.ui.interval_minutes);
        if fleet_due && self.tasks.fleet.is_none() {
            self.next_fleet = None;
            if !self.force_metric_refresh && fleet_cohort_is_current(&self.snapshot, &query) {
                self.next_fleet = refresh_cadences(self.watch_seconds)
                    .1
                    .map(|seconds| now + Duration::from_secs(seconds));
            } else {
                let candidates = select_metric_candidates(
                    &self.snapshot.resources,
                    self.collector.config.max_metric_resources,
                );
                let focused = focused_resources(&self.snapshot, &self.ui);
                let mut resources = focused.clone();
                for resource in candidates {
                    if !resources.iter().any(|candidate| {
                        candidate
                            .resource_id
                            .eq_ignore_ascii_case(&resource.resource_id)
                    }) {
                        resources.push(resource);
                    }
                }
                let focus_resource_ids = if focus_due {
                    focused
                        .iter()
                        .map(|resource| resource.resource_id.clone())
                        .collect()
                } else {
                    Vec::new()
                };
                if focus_due {
                    self.focus_debounce = None;
                    self.next_focus = None;
                    if let Some(task) = self.tasks.focus.take() {
                        task.stop();
                    }
                    self.active_focus = None;
                }
                self.force_metric_refresh = false;
                self.spawn_metrics(resources, false, focus_resource_ids, query);
                return;
            }
        }
        if focus_due && self.tasks.focus.is_none() {
            if active_fleet_covers_focus(
                self.active_fleet_query.as_ref(),
                &self.active_fleet_resource_ids,
                &self.snapshot,
                &self.ui,
                &query,
            ) || (!self.force_metric_refresh
                && focused_metrics_match_cohort(&self.snapshot, &self.ui, &query))
            {
                self.focus_debounce = None;
                self.next_focus = refresh_cadences(self.watch_seconds)
                    .0
                    .map(|seconds| now + Duration::from_secs(seconds));
            } else {
                self.focus_debounce = None;
                self.next_focus = None;
                let resources = focused_resources(&self.snapshot, &self.ui);
                self.force_metric_refresh = false;
                self.spawn_metrics(resources, true, Vec::new(), query);
            }
        }
    }

    async fn poll_raw(&mut self) {
        if matches!(self.ui.overlay, Overlay::RawLogs(_)) {
            self.ui.overlay = Overlay::RawLogs(Box::new(self.raw_stream.snapshot().await));
            self.dirty = true;
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if key.code == KeyCode::Char('q') && !matches!(self.ui.overlay, Overlay::Chooser(_)) {
            return false;
        }
        match &mut self.ui.overlay {
            Overlay::Help | Overlay::Detail | Overlay::RecentChanges => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('d') | KeyCode::Enter
                ) {
                    self.ui.overlay = Overlay::None;
                    self.dirty = true;
                }
                return true;
            }
            Overlay::Chooser(chooser) => {
                let choices = chooser_choices(&self.snapshot, chooser);
                match key.code {
                    KeyCode::Esc if !chooser.query.is_empty() => {
                        chooser.query.clear();
                        chooser.selected = 0;
                    }
                    KeyCode::Esc => self.ui.overlay = Overlay::None,
                    KeyCode::Backspace => {
                        chooser.query.pop();
                        chooser.selected = 0;
                    }
                    KeyCode::Up => chooser.selected = chooser.selected.saturating_sub(1),
                    KeyCode::Down => {
                        chooser.selected =
                            (chooser.selected + 1).min(choices.len().saturating_sub(1))
                    }
                    KeyCode::PageUp => chooser.selected = chooser.selected.saturating_sub(10),
                    KeyCode::PageDown => {
                        chooser.selected =
                            (chooser.selected + 10).min(choices.len().saturating_sub(1))
                    }
                    KeyCode::Home => chooser.selected = 0,
                    KeyCode::End => chooser.selected = choices.len().saturating_sub(1),
                    KeyCode::Char('/') => {
                        chooser.query.clear();
                        chooser.selected = 0;
                    }
                    KeyCode::Char(character)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        chooser.query.push(character);
                        chooser.selected = 0;
                    }
                    KeyCode::Enter => {
                        if let Some((label, _, current)) = choices.get(chooser.selected).cloned() {
                            let mode = chooser.mode;
                            self.ui.overlay = Overlay::None;
                            if !current {
                                match mode {
                                    ChooserMode::Group => self.spawn_scope(
                                        self.snapshot.selected_subscription_id.clone(),
                                        label,
                                    ),
                                    ChooserMode::Subscription => {
                                        if let Some(subscription) = self
                                            .snapshot
                                            .subscriptions
                                            .iter()
                                            .find(|subscription| subscription.name == label)
                                        {
                                            self.spawn_scope(
                                                subscription.subscription_id.clone(),
                                                String::new(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                self.dirty = true;
                return true;
            }
            Overlay::LogSignals { .. } => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('l') => {
                        if let Some(task) = self.tasks.logs.take() {
                            task.stop();
                        }
                        self.ui.overlay = Overlay::None;
                    }
                    KeyCode::Char('w') => {
                        self.log_window_index =
                            (self.log_window_index + 1) % self.log_windows.len();
                        self.spawn_log_query();
                    }
                    KeyCode::Char('r') => self.spawn_log_query(),
                    _ => {}
                }
                self.dirty = true;
                return true;
            }
            Overlay::RawConfirm(target) => {
                if key.code == KeyCode::Char('y') {
                    if let Some(target) = target.clone() {
                        let name = selected_resource(&self.snapshot, &self.ui)
                            .map(|resource| resource.name.clone())
                            .unwrap_or_default();
                        self.raw_stream.start(&name, target).await;
                        self.ui.overlay =
                            Overlay::RawLogs(Box::new(self.raw_stream.snapshot().await));
                    } else {
                        self.ui.overlay = Overlay::None;
                    }
                } else {
                    self.ui.overlay = Overlay::None;
                }
                self.dirty = true;
                return true;
            }
            Overlay::RawLogs(_) => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('L') => {
                        self.raw_stream.stop().await;
                        self.ui.overlay = Overlay::None;
                    }
                    KeyCode::Char('r') => {
                        if let Some(resource) = selected_resource(&self.snapshot, &self.ui) {
                            if let Some(target) = raw_log_target(
                                &self.snapshot.selected_subscription_id,
                                &self.snapshot.selected_resource_group,
                                resource,
                            ) {
                                self.raw_stream.start(&resource.name, target).await;
                            }
                        }
                    }
                    _ => {}
                }
                self.dirty = true;
                return true;
            }
            Overlay::None => {}
        }

        match key.code {
            KeyCode::Char('?') => self.ui.overlay = Overlay::Help,
            KeyCode::Enter => self.ui.overlay = Overlay::Detail,
            KeyCode::Char('d') => self.ui.overlay = Overlay::RecentChanges,
            KeyCode::Char('g') => {
                if self.snapshot.origin == "cache" {
                    self.ui.operation =
                        "CACHED · scope chooser available after live refresh".into();
                    self.dirty = true;
                    return true;
                }
                let selected = self
                    .snapshot
                    .resource_groups
                    .iter()
                    .position(|group| group.name == self.snapshot.selected_resource_group)
                    .unwrap_or(0);
                self.ui.overlay = Overlay::Chooser(ChooserState {
                    mode: ChooserMode::Group,
                    query: String::new(),
                    selected,
                });
            }
            KeyCode::Char('s') => {
                if self.snapshot.origin == "cache" {
                    self.ui.operation =
                        "CACHED · scope chooser available after live refresh".into();
                    self.dirty = true;
                    return true;
                }
                let selected = self
                    .snapshot
                    .subscriptions
                    .iter()
                    .position(|subscription| {
                        subscription.subscription_id == self.snapshot.selected_subscription_id
                    })
                    .unwrap_or(0);
                self.ui.overlay = Overlay::Chooser(ChooserState {
                    mode: ChooserMode::Subscription,
                    query: String::new(),
                    selected,
                });
            }
            KeyCode::Char('l') => self.spawn_log_query(),
            KeyCode::Char('L') => {
                let target = selected_resource(&self.snapshot, &self.ui).and_then(|resource| {
                    raw_log_target(
                        &self.snapshot.selected_subscription_id,
                        &self.snapshot.selected_resource_group,
                        resource,
                    )
                });
                self.ui.overlay = Overlay::RawConfirm(target);
            }
            KeyCode::Char('r') => self.spawn_scope_forced(
                self.scope_subscription(),
                if self.snapshot.origin == "cache" {
                    self.collector.config.resource_group.clone()
                } else {
                    self.snapshot.selected_resource_group.clone()
                },
            ),
            KeyCode::Down | KeyCode::Char('j') => {
                move_selection(&self.snapshot, &mut self.ui, 1);
                self.user_navigated = true;
                self.focus_debounce = Some(Instant::now() + Duration::from_millis(300));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                move_selection(&self.snapshot, &mut self.ui, -1);
                self.user_navigated = true;
                self.focus_debounce = Some(Instant::now() + Duration::from_millis(300));
            }
            KeyCode::Char('v') => {
                self.ui.view = match self.ui.view {
                    ViewMode::Operations => ViewMode::Inventory,
                    ViewMode::Inventory => ViewMode::Operations,
                }
            }
            KeyCode::Char('w') if self.collector.metrics_enabled => {
                let presets = [(1, 1), (6, 5), (24, 15)];
                let current = presets
                    .iter()
                    .position(|preset| *preset == (self.ui.window_hours, self.ui.interval_minutes))
                    .unwrap_or(0);
                let next = presets[(current + 1) % presets.len()];
                self.ui.window_hours = next.0;
                self.ui.interval_minutes = next.1;
                if let Some(task) = self.tasks.focus.take() {
                    task.stop();
                }
                self.active_focus = None;
                if let Some(task) = self.tasks.fleet.take() {
                    task.stop();
                }
                self.active_fleet = None;
                self.active_fleet_query = None;
                self.active_fleet_resource_ids.clear();
                self.force_metric_refresh = false;
                self.snapshot.fleet_state = "stale".into();
                self.snapshot.fleet_query = MetricQuery::default();
                for resource in &mut self.snapshot.resources {
                    resource.metrics.clear();
                    resource.fleet_metrics.clear();
                }
                self.ui.window_loading = true;
                mark_metric_candidates(
                    &mut self.snapshot.resources,
                    self.collector.config.max_metric_resources,
                    true,
                );
                self.ui.operation = format!(
                    "WINDOW {}h/{} · loading; previous-window charts hidden",
                    next.0,
                    if next.1 == 60 {
                        "1h".into()
                    } else {
                        format!("{}m", next.1)
                    }
                );
                self.focus_debounce = Some(Instant::now());
                self.next_fleet = Some(Instant::now());
            }
            KeyCode::Char('o') => {
                self.ui.sort = self.ui.sort.next();
                self.ui.selected_id = initial_selection(&self.snapshot, &self.ui);
            }
            KeyCode::Char('O') => {
                self.ui.reverse = !self.ui.reverse;
                self.ui.selected_id = initial_selection(&self.snapshot, &self.ui);
            }
            KeyCode::Char('f') => {
                self.ui.category_index = (self.ui.category_index + 1) % CATEGORIES.len();
                self.ui.selected_id = initial_selection(&self.snapshot, &self.ui);
            }
            KeyCode::Char('t') => {
                if self.snapshot.origin == "cache" {
                    self.ui.operation = "CACHED · stars available after live refresh".into();
                    self.dirty = true;
                    return true;
                }
                let selected = self.ui.selected_id.clone();
                let star = self
                    .snapshot
                    .resources
                    .iter()
                    .find(|resource| resource.resource_id == selected)
                    .map(|resource| {
                        (
                            session_star_key(&self.snapshot, resource),
                            !resource.session_starred,
                        )
                    });
                if let Some((key, add)) = star {
                    if add {
                        self.session_stars.insert(key);
                    } else {
                        self.session_stars.remove(&key);
                    }
                }
                if let Some(resource) = self
                    .snapshot
                    .resources
                    .iter_mut()
                    .find(|resource| resource.resource_id == selected)
                {
                    resource.session_starred = !resource.session_starred;
                    resource.refresh_watched();
                }
                preserve_visible_selection(&self.snapshot, &mut self.ui, selected);
            }
            KeyCode::Char('T') => {
                let selected = self.ui.selected_id.clone();
                self.ui.watchlist_only = !self.ui.watchlist_only;
                preserve_visible_selection(&self.snapshot, &mut self.ui, selected);
            }
            KeyCode::Char('m') => {
                self.collector.metrics_enabled = !self.collector.metrics_enabled;
                self.snapshot.metrics_enabled = self.collector.metrics_enabled;
                mark_metric_candidates(
                    &mut self.snapshot.resources,
                    self.collector.config.max_metric_resources,
                    self.collector.metrics_enabled,
                );
                if self.collector.metrics_enabled {
                    self.force_metric_refresh = true;
                    self.ui.window_loading = true;
                    self.spawn_enrichment();
                } else {
                    self.force_metric_refresh = false;
                    if let Some(task) = self.tasks.focus.take() {
                        task.stop();
                    }
                    self.active_focus = None;
                    if let Some(task) = self.tasks.fleet.take() {
                        task.stop();
                    }
                    self.active_fleet = None;
                    self.active_fleet_query = None;
                    self.active_fleet_resource_ids.clear();
                    self.ui.window_loading = false;
                }
            }
            KeyCode::Char(']') => self.step_group(1),
            KeyCode::Char('[') => self.step_group(-1),
            _ => {}
        }
        self.dirty = true;
        true
    }

    fn step_group(&mut self, delta: isize) {
        let groups = &self.snapshot.resource_groups;
        if groups.is_empty() {
            return;
        }
        let base = self
            .pending_group
            .as_ref()
            .map(|(_, group)| group.as_str())
            .unwrap_or(&self.snapshot.selected_resource_group);
        let current = groups
            .iter()
            .position(|group| group.name == base)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(groups.len() as isize) as usize;
        let target = groups[next].name.clone();
        self.ui.operation = format!(
            "{} → {} · SETTLING",
            self.snapshot.selected_resource_group, target
        );
        self.pending_group = Some((Instant::now() + Duration::from_millis(300), target));
    }

    fn scope_subscription(&self) -> String {
        if self.snapshot.origin == "cache" {
            self.collector.config.subscription.clone()
        } else {
            self.snapshot.selected_subscription_id.clone()
        }
    }
}

fn session_star_key(
    snapshot: &Snapshot,
    resource: &AzureResource,
) -> (String, String, String, String) {
    (
        snapshot.selected_subscription_id.to_ascii_lowercase(),
        snapshot.selected_resource_group.to_ascii_lowercase(),
        resource.name.to_ascii_lowercase(),
        resource.resource_type.to_ascii_lowercase(),
    )
}

fn apply_session_stars(snapshot: &mut Snapshot, stars: &HashSet<(String, String, String, String)>) {
    let subscription = snapshot.selected_subscription_id.to_ascii_lowercase();
    let group = snapshot.selected_resource_group.to_ascii_lowercase();
    for resource in &mut snapshot.resources {
        resource.session_starred = stars.contains(&(
            subscription.clone(),
            group.clone(),
            resource.name.to_ascii_lowercase(),
            resource.resource_type.to_ascii_lowercase(),
        ));
        resource.refresh_watched();
    }
}

fn selected_identity(snapshot: &Snapshot, ui: &UiState) -> Option<(String, String, String)> {
    selected_resource(snapshot, ui).map(|resource| {
        (
            resource.resource_id.clone(),
            resource.name.clone(),
            resource.resource_type.clone(),
        )
    })
}

fn restore_selection(
    snapshot: &Snapshot,
    ui: &mut UiState,
    identity: Option<(String, String, String)>,
) {
    ui.selected_id = identity
        .and_then(|(resource_id, name, resource_type)| {
            snapshot
                .resources
                .iter()
                .find(|resource| {
                    resource.resource_id == resource_id
                        || (resource.name.eq_ignore_ascii_case(&name)
                            && resource.resource_type.eq_ignore_ascii_case(&resource_type))
                })
                .map(|resource| resource.resource_id.clone())
        })
        .unwrap_or_else(|| initial_selection(snapshot, ui));
}

fn focused_resources(snapshot: &Snapshot, ui: &UiState) -> Vec<AzureResource> {
    let Some(resource) = selected_resource(snapshot, ui) else {
        return Vec::new();
    };
    let mut result = vec![resource.clone()];
    if !resource.hosting_plan_id.is_empty() {
        if let Some(plan) = snapshot.resources.iter().find(|candidate| {
            candidate
                .resource_id
                .eq_ignore_ascii_case(&resource.hosting_plan_id)
        }) {
            result.push(plan.clone());
        }
    }
    result
}

fn fleet_cohort_is_current(snapshot: &Snapshot, query: &MetricQuery) -> bool {
    snapshot.fleet_state == "current"
        && snapshot
            .fleet_query
            .matches(query.window_hours, query.requested_interval_minutes)
        && snapshot.fleet_query.cohort == query.cohort
}

fn focused_metrics_match_cohort(snapshot: &Snapshot, ui: &UiState, query: &MetricQuery) -> bool {
    if !query.matches(ui.window_hours, ui.interval_minutes) {
        return false;
    }
    let resources = focused_resources(snapshot, ui);
    !resources.is_empty()
        && resources.iter().all(|resource| {
            resource.metrics.values().any(|metric| {
                metric
                    .query
                    .matches(query.window_hours, query.requested_interval_minutes)
                    && metric.query.cohort == query.cohort
            })
        })
}

fn active_fleet_covers_focus(
    active_query: Option<&MetricQuery>,
    active_resource_ids: &HashSet<String>,
    snapshot: &Snapshot,
    ui: &UiState,
    query: &MetricQuery,
) -> bool {
    active_query.is_some_and(|active| {
        query.matches(ui.window_hours, ui.interval_minutes)
            && active.cohort == query.cohort
            && active.matches(query.window_hours, query.requested_interval_minutes)
            && {
                let focused = focused_resources(snapshot, ui);
                !focused.is_empty()
                    && focused.iter().all(|resource| {
                        active_resource_ids.contains(&resource.resource_id.to_ascii_lowercase())
                    })
            }
    })
}

fn move_selection(snapshot: &Snapshot, ui: &mut UiState, delta: isize) {
    let resources = visible_resources(snapshot, ui);
    if resources.is_empty() {
        ui.selected_id.clear();
        return;
    }
    let current = resources
        .iter()
        .position(|resource| resource.resource_id == ui.selected_id)
        .unwrap_or(0);
    let next = (current as isize + delta).clamp(0, resources.len() as isize - 1) as usize;
    ui.selected_id = resources[next].resource_id.clone();
}

fn preserve_visible_selection(snapshot: &Snapshot, ui: &mut UiState, selected: String) {
    if visible_resources(snapshot, ui)
        .iter()
        .any(|resource| resource.resource_id == selected)
    {
        ui.selected_id = selected;
    } else {
        ui.selected_id = initial_selection(snapshot, ui);
    }
}

pub fn refresh_cadences(watch_seconds: u64) -> (Option<u64>, Option<u64>, Option<u64>) {
    if watch_seconds == 0 {
        return (None, None, None);
    }
    (
        Some(watch_seconds),
        Some((watch_seconds * 2).max(60)),
        Some((watch_seconds * 10).max(300)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        azure::AzureCli,
        config::Config,
        model::{MetricSeries, Subscription},
    };
    use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

    fn snapshot(group: &str, resource_id: &str) -> Snapshot {
        Snapshot {
            generated_at: "2026-07-28T00:00:00Z".into(),
            subscriptions: vec![Subscription {
                name: "Subscription".into(),
                subscription_id: "sub".into(),
                cloud: "AzureUSGovernment".into(),
                is_default: true,
            }],
            selected_subscription_name: "Subscription".into(),
            selected_subscription_id: "sub".into(),
            selected_resource_group: group.into(),
            access_state: "available".into(),
            resources: vec![AzureResource {
                name: "app".into(),
                resource_id: resource_id.into(),
                resource_type: "Microsoft.Web/sites".into(),
                category: "compute/web".into(),
                ..AzureResource::default()
            }],
            ..Snapshot::default()
        }
    }

    fn app(snapshot: Snapshot) -> App {
        App::new(
            Collector::new(Config::default(), AzureCli::new(1), true),
            snapshot,
            30,
            false,
        )
    }

    fn available_metric(query: MetricQuery, value: f64) -> MetricSeries {
        MetricSeries {
            state: "available".into(),
            values: vec![Some(value)],
            query,
            ..MetricSeries::default()
        }
    }

    #[test]
    fn independent_refresh_cadences_match_contract() {
        assert_eq!(refresh_cadences(30), (Some(30), Some(60), Some(300)));
        assert_eq!(refresh_cadences(15), (Some(15), Some(60), Some(300)));
        assert_eq!(refresh_cadences(0), (None, None, None));
    }

    #[test]
    fn stale_generation_and_scope_results_are_rejected() {
        let mut app = app(snapshot("current", "id"));
        app.generation = 4;
        app.handle_worker(WorkerEvent::Enriched {
            generation: 3,
            task_id: 1,
            snapshot: snapshot("stale", "other"),
        });
        assert_eq!(app.snapshot.selected_resource_group, "current");

        app.handle_worker(WorkerEvent::Metrics {
            generation: 4,
            task_id: 1,
            subscription: "sub".into(),
            group: "wrong".into(),
            query: metric_query(1, 1),
            resources: vec![AzureResource {
                resource_id: "id".into(),
                metrics: BTreeMap::from([(
                    "requests".into(),
                    MetricSeries {
                        state: "available".into(),
                        values: vec![Some(9.0)],
                        ..MetricSeries::default()
                    },
                )]),
                ..AzureResource::default()
            }],
            focused: true,
            complete: false,
            timed_out: false,
            focus_resource_ids: Vec::new(),
        });
        assert!(app.snapshot.resources[0].metrics.is_empty());
    }

    #[test]
    fn matching_metric_refresh_preserves_selection_identity() {
        let mut app = app(snapshot("rg", "id"));
        app.ui.selected_id = "id".into();
        app.active_focus = Some(1);
        app.handle_worker(WorkerEvent::Metrics {
            generation: 0,
            task_id: 1,
            subscription: "sub".into(),
            group: "rg".into(),
            query: metric_query(1, 1),
            resources: vec![AzureResource {
                resource_id: "ID".into(),
                metrics: BTreeMap::from([(
                    "requests".into(),
                    MetricSeries {
                        state: "available".into(),
                        values: vec![Some(3.0)],
                        ..MetricSeries::default()
                    },
                )]),
                ..AzureResource::default()
            }],
            focused: true,
            complete: false,
            timed_out: false,
            focus_resource_ids: Vec::new(),
        });
        assert_eq!(app.ui.selected_id, "id");
        assert_eq!(
            app.snapshot.resources[0].metrics["requests"].latest(),
            Some(3.0)
        );
    }

    #[test]
    fn superseded_same_window_metric_event_cannot_overwrite_active_request() {
        let mut app = app(snapshot("rg", "id"));
        app.active_focus = Some(2);
        let query = metric_query(1, 1);
        app.handle_worker(WorkerEvent::Metrics {
            generation: 0,
            task_id: 1,
            subscription: "sub".into(),
            group: "rg".into(),
            query,
            resources: vec![AzureResource {
                resource_id: "id".into(),
                metrics: BTreeMap::from([(
                    "requests".into(),
                    MetricSeries {
                        state: "available".into(),
                        values: vec![Some(99.0)],
                        ..MetricSeries::default()
                    },
                )]),
                ..AzureResource::default()
            }],
            focused: true,
            complete: true,
            timed_out: false,
            focus_resource_ids: Vec::new(),
        });
        assert!(app.snapshot.resources[0].metrics.is_empty());
        assert_eq!(app.active_focus, Some(2));
    }

    #[test]
    fn fleet_refresh_never_overwrites_selected_resource_metrics() {
        let mut snapshot = snapshot("rg", "id");
        let mut focused_query = metric_query(1, 1);
        focused_query.queried_at = "2026-07-28T00:02:00Z".into();
        snapshot.resources[0].metrics.insert(
            "requests".into(),
            MetricSeries {
                state: "available".into(),
                values: vec![Some(9.0)],
                query: focused_query,
                ..MetricSeries::default()
            },
        );
        let mut app = app(snapshot);
        app.active_fleet = Some(1);
        let mut query = metric_query(1, 1);
        query.queried_at = "2026-07-28T00:01:00Z".into();
        let update = AzureResource {
            resource_id: "id".into(),
            metrics: BTreeMap::from([(
                "requests".into(),
                MetricSeries {
                    state: "available".into(),
                    values: vec![Some(3.0)],
                    query: query.clone(),
                    ..MetricSeries::default()
                },
            )]),
            ..AzureResource::default()
        };
        app.handle_worker(WorkerEvent::Metrics {
            generation: 0,
            task_id: 1,
            subscription: "sub".into(),
            group: "rg".into(),
            query,
            resources: vec![update],
            focused: false,
            complete: true,
            timed_out: false,
            focus_resource_ids: Vec::new(),
        });
        assert_eq!(
            app.snapshot.resources[0].metrics["requests"].latest(),
            Some(9.0)
        );
        assert_eq!(
            app.snapshot.resources[0].fleet_metrics["requests"].latest(),
            Some(3.0)
        );
    }

    #[test]
    fn failed_inventory_refresh_keeps_visible_rows_and_marks_them_stale() {
        let mut app = app(snapshot("rg", "id"));
        app.next_inventory = None;
        app.handle_worker(WorkerEvent::Inventory {
            generation: 0,
            result: Err("permission limited".into()),
        });
        assert_eq!(app.snapshot.resources.len(), 1);
        assert_eq!(app.snapshot.inventory_state, "stale");
        assert!(app.ui.operation.contains("STALE"));
        assert!(app.ui.operation.contains("permission limited"));
        assert!(app.next_inventory.is_some());
    }

    #[test]
    fn inventory_refresh_completes_and_rearms_when_metrics_are_disabled() {
        let mut app = app(snapshot("rg", "id"));
        app.collector.metrics_enabled = false;
        app.next_inventory = None;
        app.ui.operation = "UPDATING INVENTORY".into();
        app.handle_worker(WorkerEvent::Inventory {
            generation: 0,
            result: Ok(snapshot("rg", "fresh-id")),
        });
        assert_eq!(app.snapshot.enrichment_state, "disabled");
        assert!(app.ui.operation.is_empty());
        assert!(app.next_inventory.is_some());
    }

    #[test]
    fn permission_limited_inventory_finishes_loading_without_caching() {
        let mut app = app(snapshot("current", "id"));
        app.next_inventory = None;
        app.ui.window_loading = true;
        app.force_metric_refresh = true;
        let mut limited = snapshot("restricted", "unused");
        limited.resources.clear();
        limited.access_state = "unavailable".into();
        limited.access_detail = "permission limited".into();
        limited.inventory_state = "current".into();

        app.handle_worker(WorkerEvent::Inventory {
            generation: 0,
            result: Ok(limited),
        });

        assert_eq!(app.snapshot.selected_resource_group, "restricted");
        assert_eq!(app.snapshot.inventory_state, "current");
        assert_eq!(app.snapshot.enrichment_state, "unavailable");
        assert_eq!(app.snapshot.fleet_state, "unavailable");
        assert!(!app.ui.window_loading);
        assert!(app.ui.operation.contains("ACCESS UNAVAILABLE"));
        assert!(app.ui.operation.contains("permission limited"));
        assert!(app.next_focus.is_none());
        assert!(app.next_fleet.is_none());
        assert!(app.next_inventory.is_some());
        assert!(app.cache_save.is_none());
        assert!(app.pending_cache.is_none());
        assert!(!app.force_metric_refresh);
    }

    #[test]
    fn permission_limited_inventory_respects_watch_zero() {
        let collector = Collector::new(Config::default(), AzureCli::new(1), true);
        let mut app = App::new(collector, snapshot("current", "id"), 0, false);
        let mut limited = snapshot("restricted", "unused");
        limited.resources.clear();
        limited.access_state = "unavailable".into();
        limited.access_detail = "permission limited".into();
        limited.inventory_state = "current".into();

        app.handle_worker(WorkerEvent::Inventory {
            generation: 0,
            result: Ok(limited),
        });

        assert!(app.next_focus.is_none());
        assert!(app.next_fleet.is_none());
        assert!(app.next_inventory.is_none());
        assert!(!app.ui.window_loading);
    }

    #[test]
    fn exact_metric_cohort_controls_automatic_reuse() {
        let query = metric_query(24, 15);
        let mut snapshot = snapshot("rg", "id");
        snapshot.fleet_state = "current".into();
        snapshot.fleet_query = query.clone();
        snapshot.resources[0]
            .metrics
            .insert("requests".into(), available_metric(query.clone(), 3.0));
        let mut ui = UiState {
            selected_id: "id".into(),
            window_hours: 24,
            interval_minutes: 15,
            ..UiState::default()
        };

        assert!(fleet_cohort_is_current(&snapshot, &query));
        assert!(focused_metrics_match_cohort(&snapshot, &ui, &query));

        let mut next_query = query.clone();
        next_query.cohort.push_str(":next");
        assert!(!fleet_cohort_is_current(&snapshot, &next_query));
        assert!(!focused_metrics_match_cohort(&snapshot, &ui, &next_query));

        ui.interval_minutes = 5;
        assert!(!focused_metrics_match_cohort(&snapshot, &ui, &query));
    }

    #[test]
    fn active_fleet_suppresses_duplicate_focused_read_only_for_same_cohort() {
        let query = metric_query(1, 1);
        let snapshot = snapshot("rg", "ID");
        let ui = UiState {
            selected_id: "ID".into(),
            window_hours: 1,
            interval_minutes: 1,
            ..UiState::default()
        };
        let ids = HashSet::from(["id".to_string()]);
        assert!(active_fleet_covers_focus(
            Some(&query),
            &ids,
            &snapshot,
            &ui,
            &query
        ));

        let mut other = query.clone();
        other.cohort.push_str(":next");
        assert!(!active_fleet_covers_focus(
            Some(&query),
            &ids,
            &snapshot,
            &ui,
            &other
        ));
    }

    #[test]
    fn empty_fleet_refresh_is_terminal_and_rearmed_without_a_task() {
        let mut app = app(snapshot("rg", "id"));
        app.ui.window_loading = true;
        let query = metric_query(1, 1);
        app.spawn_metrics(Vec::new(), false, Vec::new(), query.clone());

        assert!(app.tasks.fleet.is_none());
        assert!(app.active_fleet.is_none());
        assert_eq!(app.snapshot.fleet_state, "no_data");
        assert_eq!(app.snapshot.fleet_query.cohort, query.cohort);
        assert!(!app.ui.window_loading);
        assert!(app.next_fleet.is_some());
    }

    #[tokio::test]
    async fn forced_schedule_does_not_reuse_the_current_cohort() {
        let query = metric_query(24, 15);
        let mut snapshot = snapshot("rg", "id");
        snapshot.resources[0].resource_type = "Microsoft.Unknown/widgets".into();
        snapshot.fleet_query = query.clone();
        snapshot.fleet_state = "current".into();
        snapshot.resources[0]
            .metrics
            .insert("requests".into(), available_metric(query, 1.0));
        let mut app = app(snapshot);
        app.ui.window_hours = 24;
        app.ui.interval_minutes = 15;
        app.force_metric_refresh = true;
        app.next_fleet = Some(Instant::now());
        app.next_focus = Some(Instant::now());

        app.run_schedules();

        assert!(app.active_fleet.is_some());
        assert!(app.tasks.fleet.is_some());
        assert!(app.active_focus.is_none());
        assert!(!app.force_metric_refresh);
        app.tasks.abort_all();
    }

    #[test]
    fn automatic_schedule_reuses_current_aligned_cohort() {
        let query = metric_query(24, 15);
        let mut snapshot = snapshot("rg", "id");
        snapshot.fleet_query = query.clone();
        snapshot.fleet_state = "current".into();
        snapshot.resources[0]
            .metrics
            .insert("requests".into(), available_metric(query, 1.0));
        let mut app = app(snapshot);
        app.ui.window_hours = 24;
        app.ui.interval_minutes = 15;
        app.next_fleet = Some(Instant::now());
        app.next_focus = Some(Instant::now());

        app.run_schedules();

        assert!(app.tasks.fleet.is_none());
        assert!(app.tasks.focus.is_none());
        assert!(app.next_fleet.is_some());
        assert!(app.next_focus.is_some());
    }

    #[tokio::test]
    async fn manual_refresh_key_forces_the_next_metric_cohort_read() {
        let mut app = app(snapshot("rg", "id"));

        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
                .await
        );

        assert!(app.force_metric_refresh);
        assert!(app.tasks.scope.is_some());
        app.tasks.abort_all();
    }

    #[tokio::test]
    async fn inventory_wins_when_all_refresh_deadlines_coincide() {
        let mut app = app(snapshot("rg", "id"));
        let due = Instant::now();
        app.next_inventory = Some(due);
        app.next_fleet = Some(due);
        app.next_focus = Some(due);

        app.run_schedules();

        assert!(app.tasks.scope.is_some());
        assert!(app.tasks.fleet.is_none());
        assert!(app.tasks.focus.is_none());
        app.tasks.abort_all();
    }

    #[test]
    fn selection_restores_by_type_and_name_after_cached_identity_is_replaced() {
        let cached = snapshot("rg", "cache:type:app");
        let mut ui = UiState {
            selected_id: "cache:type:app".into(),
            ..UiState::default()
        };
        let identity = selected_identity(&cached, &ui);
        let fresh = snapshot("rg", "live-arm-id");
        restore_selection(&fresh, &mut ui, identity);
        assert_eq!(ui.selected_id, "live-arm-id");
    }

    #[tokio::test]
    async fn session_star_and_watchlist_filter_are_keyboard_driven() {
        let mut app = app(snapshot("rg", "id"));
        app.ui.selected_id = "id".into();
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
                .await
        );
        assert!(app.snapshot.resources[0].watched);
        assert_eq!(app.ui.selected_id, "id");
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT))
                .await
        );
        assert!(app.ui.watchlist_only);
        assert_eq!(app.ui.selected_id, "id");

        let mut same_scope = snapshot("rg", "replacement");
        same_scope.resources[0].name = "app".into();
        apply_session_stars(&mut same_scope, &app.session_stars);
        assert!(same_scope.resources[0].session_starred);

        let mut other_scope = snapshot("other-rg", "replacement");
        other_scope.resources[0].name = "app".into();
        apply_session_stars(&mut other_scope, &app.session_stars);
        assert!(!other_scope.resources[0].session_starred);
    }

    #[tokio::test]
    async fn recent_changes_overlay_is_keyboard_driven_and_closable() {
        let mut app = app(snapshot("rg", "id"));
        app.ui.selected_id = "id".into();
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
                .await
        );
        assert!(matches!(app.ui.overlay, Overlay::RecentChanges));
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .await
        );
        assert!(matches!(app.ui.overlay, Overlay::None));
    }

    #[tokio::test]
    async fn cached_snapshot_can_explain_the_zero_read_raw_stream_boundary() {
        let mut cached = snapshot("rg", "cache:type:app");
        cached.origin = "cache".into();
        let mut app = app(cached);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT))
                .await
        );
        assert!(matches!(app.ui.overlay, Overlay::RawConfirm(None)));
    }

    #[tokio::test]
    async fn shutdown_cancels_raw_child_and_clears_sensitive_ring_on_every_exit_path() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("raw-stream");
        fs::write(&program, "#!/bin/sh\nprintf 'sensitive\\n'\nsleep 2\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let mut app = app(snapshot("rg", "id"));
        app.raw_stream
            .start(
                "app",
                crate::logs::RawLogTarget {
                    provider: "test".into(),
                    description: "test".into(),
                    command: vec![program.to_string_lossy().into_owned()],
                },
            )
            .await;
        for _ in 0..300 {
            if !app.raw_stream.snapshot().await.lines.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            app.raw_stream.snapshot().await.lines,
            vec!["sensitive".to_string()]
        );
        app.shutdown().await;
        let stopped = app.raw_stream.snapshot().await;
        assert_eq!(stopped.status, "stopped");
        assert!(stopped.lines.is_empty());
        assert!(stopped.started_at.is_none());
    }

    #[test]
    fn terminal_restore_sequence_leaves_alt_screen_and_shows_cursor() {
        let mut output = Vec::new();
        write_terminal_restore(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\u{1b}[?1049l"));
        assert!(output.contains("\u{1b}[?25h"));
    }
}
