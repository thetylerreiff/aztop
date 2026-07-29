use std::{
    collections::{BTreeMap, VecDeque},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::Command,
    sync::{Mutex, RwLock},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::{
    azure::{AzureCli, AzureError, FixedLogRead},
    model::{AzureResource, LogSignalResult, LogTableSignal},
    sanitize::{clean_text, terminal_line},
};

pub const RAW_LINE_CAP: usize = 200;
pub const RAW_SESSION_SECONDS: u64 = 15 * 60;
const RAW_LINE_BYTE_CAP: usize = 8 * 1024;

const LOG_WINDOWS: [(u64, &str, &str, &str); 3] = [
    (15, "15m", "1m", "PT15M"),
    (60, "1h", "5m", "PT1H"),
    (360, "6h", "30m", "PT6H"),
];

fn window(minutes: u64) -> Option<(&'static str, &'static str, &'static str)> {
    LOG_WINDOWS
        .iter()
        .find(|entry| entry.0 == minutes)
        .map(|entry| (entry.1, entry.2, entry.3))
}

pub fn log_tables(resource_type: &str) -> &'static [&'static str] {
    match resource_type.to_ascii_lowercase().as_str() {
        "microsoft.insights/components" => &[
            "AppTraces",
            "AppExceptions",
            "AppRequests",
            "AppDependencies",
            "AppAvailabilityResults",
        ],
        "microsoft.web/sites" | "microsoft.web/sites/slots" => &[
            "AppServiceHTTPLogs",
            "AppServiceAppLogs",
            "AppServiceConsoleLogs",
            "AzureDiagnostics",
        ],
        value if value.starts_with("microsoft.app/containerapps") => &[
            "ContainerAppConsoleLogs_CL",
            "ContainerAppSystemLogs_CL",
            "ContainerLogV2",
            "AzureDiagnostics",
        ],
        _ => &["AzureDiagnostics"],
    }
}

pub fn log_aggregate_query(
    resource_id: &str,
    resource_type: &str,
    window_minutes: u64,
) -> Result<String, AzureError> {
    let (window, interval, _) =
        window(window_minutes).ok_or_else(|| invalid_window(window_minutes))?;
    let resource_id = serde_json::to_string(&resource_id.to_ascii_lowercase())
        .expect("serializing a Rust string cannot fail");
    Ok(format!(
        "union isfuzzy=true withsource=SourceTable {} | where TimeGenerated >= ago({window}) | where tolower(tostring(column_ifexists('_ResourceId', ''))) == {resource_id} | extend SafeSeverity=toint(column_ifexists('SeverityLevel', -1)), SafeLevel=tolower(tostring(column_ifexists('Level', ''))), SafeSuccess=tolower(tostring(column_ifexists('Success', ''))), SafeResult=tostring(column_ifexists('ResultCode', '')), SafeStatus=tostring(column_ifexists('ScStatus', '')) | extend IsError=SourceTable endswith 'AppExceptions' or SafeSeverity >= 3 or SafeLevel in ('error', 'critical', 'fatal') or SafeSuccess == 'false' or SafeResult startswith '5' or SafeStatus startswith '5', IsWarning=SafeSeverity == 2 or SafeLevel == 'warning' | summarize Total=count(), Errors=countif(IsError), Warnings=countif(IsWarning), Latest=max(TimeGenerated), Ingested=max(ingestion_time()) by bin(TimeGenerated, {interval}), SourceTable | order by TimeGenerated asc | project TimeGenerated, SourceTable, Total, Errors, Warnings, Latest, Ingested",
        log_tables(resource_type).join(", ")
    ))
}

pub fn app_insights_log_query(window_minutes: u64) -> Result<String, AzureError> {
    let (window, interval, _) =
        window(window_minutes).ok_or_else(|| invalid_window(window_minutes))?;
    Ok(format!(
        "union isfuzzy=true withsource=SourceTable traces, exceptions, requests, dependencies, availabilityResults | where timestamp >= ago({window}) | extend SafeSeverity=toint(column_ifexists('severityLevel', -1)), SafeSuccess=tolower(tostring(column_ifexists('success', ''))), SafeResult=tostring(column_ifexists('resultCode', '')) | extend IsError=SourceTable == 'exceptions' or SafeSeverity >= 3 or SafeSuccess == 'false' or SafeResult startswith '5', IsWarning=SafeSeverity == 2 | summarize Total=count(), Errors=countif(IsError), Warnings=countif(IsWarning), Latest=max(timestamp), Ingested=max(ingestion_time()) by bin(timestamp, {interval}), SourceTable | order by timestamp asc | project TimeGenerated=timestamp, SourceTable, Total, Errors, Warnings, Latest, Ingested"
    ))
}

fn invalid_window(minutes: u64) -> AzureError {
    AzureError {
        detail: format!("log window must be 15, 60, or 360 minutes; got {minutes}"),
        permission_limited: false,
        not_found: false,
    }
}

#[derive(Clone)]
pub struct LogCollector {
    azure: AzureCli,
    max_workspaces: usize,
}

impl LogCollector {
    pub fn new(azure: AzureCli) -> Self {
        Self {
            azure,
            max_workspaces: 3,
        }
    }

    pub async fn collect(
        &self,
        cloud: &str,
        subscription: &str,
        resource_group: &str,
        resource: &AzureResource,
        window_minutes: u64,
    ) -> LogSignalResult {
        if window(window_minutes).is_none() {
            return empty_log_result(
                &resource.name,
                "unavailable",
                &invalid_window(window_minutes).detail,
                60,
                0,
                1,
            );
        }
        if resource
            .resource_type
            .eq_ignore_ascii_case("microsoft.insights/components")
        {
            if resource.telemetry_query_id.is_empty() {
                return empty_log_result(
                    &resource.name,
                    "unavailable",
                    "Application Insights application query ID was not exposed by the fixed Resource Graph projection; no component metadata read attempted",
                    window_minutes,
                    0,
                    0,
                );
            }
            let result = self
                .azure
                .fixed_log_query(
                    subscription,
                    FixedLogRead::ApplicationInsights {
                        application_id: resource.telemetry_query_id.clone(),
                        offset: window(window_minutes).expect("validated").0,
                        aggregate_query: app_insights_log_query(window_minutes).expect("validated"),
                    },
                )
                .await;
            return match result {
                Ok(value) => {
                    let rows = tabular_rows(&value);
                    let mut parsed = parse_log_rows(
                        &resource.name,
                        &rows,
                        &resource.resource_type,
                        window_minutes,
                        1,
                        0,
                    );
                    parsed.source = "Application Insights fixed aggregate".into();
                    parsed
                }
                Err(error) => empty_log_result(
                    &resource.name,
                    "unavailable",
                    &format!(
                        "Application Insights aggregate unavailable: {}",
                        error.detail
                    ),
                    window_minutes,
                    0,
                    1,
                ),
            };
        }
        if !workspace_aggregate_supported(cloud) {
            return empty_log_result(
                &resource.name,
                "unsupported",
                "generic workspace aggregates are disabled in AzureUSGovernment until the Azure CLI Log Analytics extension supports sovereign endpoints; no query attempted",
                window_minutes,
                0,
                0,
            );
        }

        let workspaces = match self.azure.workspaces(subscription, resource_group).await {
            Ok(workspaces) => workspaces,
            Err(error) => {
                return empty_log_result(
                    &resource.name,
                    "unavailable",
                    &error.detail,
                    window_minutes,
                    0,
                    1,
                )
            }
        };
        if workspaces.is_empty() {
            return empty_log_result(
                &resource.name,
                "no_data",
                "no visible Log Analytics workspace in this resource group; no health inference",
                window_minutes,
                0,
                0,
            );
        }
        let selected = workspaces
            .iter()
            .take(self.max_workspaces)
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        let mut successful = 0;
        let mut first_error = String::new();
        for workspace in &selected {
            let customer_id = workspace["customerId"].as_str().unwrap_or_default();
            if customer_id.is_empty() {
                continue;
            }
            let query = log_aggregate_query(
                &resource.resource_id,
                &resource.resource_type,
                window_minutes,
            )
            .expect("validated");
            let result = self
                .azure
                .fixed_log_query(
                    subscription,
                    FixedLogRead::LogAnalytics {
                        workspace_id: customer_id.into(),
                        aggregate_query: query,
                        timespan: window(window_minutes).expect("validated").2,
                    },
                )
                .await;
            match result {
                Ok(value) => {
                    rows.extend(tabular_rows(&value));
                    successful += 1;
                }
                Err(error) => {
                    if first_error.is_empty() {
                        first_error = error.detail;
                    }
                }
            }
        }
        let unavailable = selected.len().saturating_sub(successful);
        if successful == 0 {
            return empty_log_result(
                &resource.name,
                "unavailable",
                &format!(
                    "all {} bounded workspace queries were permission/API limited: {}",
                    selected.len(),
                    first_error
                ),
                window_minutes,
                0,
                unavailable,
            );
        }
        let mut result = parse_log_rows(
            &resource.name,
            &rows,
            &resource.resource_type,
            window_minutes,
            successful,
            unavailable,
        );
        if workspaces.len() > selected.len() {
            result.detail.push_str(&format!(
                "; workspace cap {}/{}",
                selected.len(),
                workspaces.len()
            ));
        }
        result
    }
}

fn tabular_rows(value: &Value) -> Vec<Value> {
    if let Some(rows) = value.as_array() {
        return rows.clone();
    }
    let Some(table) = value["tables"].as_array().and_then(|tables| tables.first()) else {
        return Vec::new();
    };
    let names = table["columns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|column| column["name"].as_str())
        .collect::<Vec<_>>();
    table["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let row = row.as_array()?;
            Some(
                names
                    .iter()
                    .zip(row)
                    .map(|(name, value)| ((*name).to_string(), value.clone()))
                    .collect::<serde_json::Map<_, _>>()
                    .into(),
            )
        })
        .collect()
}

pub fn empty_log_result(
    resource_name: &str,
    state: &str,
    detail: &str,
    window_minutes: u64,
    queried_workspaces: usize,
    unavailable_workspaces: usize,
) -> LogSignalResult {
    let (window, interval, _) = window(window_minutes).unwrap_or(("1h", "5m", "PT1H"));
    LogSignalResult {
        resource_name: clean_text(resource_name, 120),
        state: state.into(),
        detail: clean_text(detail, 300),
        source: "Azure Monitor Logs fixed aggregate".into(),
        window: window.into(),
        interval: interval.into(),
        queried_workspaces,
        unavailable_workspaces,
        ..LogSignalResult::default()
    }
}

pub fn parse_log_rows(
    resource_name: &str,
    rows: &[Value],
    resource_type: &str,
    window_minutes: u64,
    queried_workspaces: usize,
    unavailable_workspaces: usize,
) -> LogSignalResult {
    let allowed = log_tables(resource_type);
    let mut buckets = BTreeMap::<String, [u64; 3]>::new();
    let mut tables = BTreeMap::<String, [u64; 3]>::new();
    let mut latest = String::new();
    let mut ingested = String::new();
    for row in rows {
        let lower = row
            .as_object()
            .into_iter()
            .flatten()
            .map(|(key, value)| (key.to_ascii_lowercase(), value))
            .collect::<BTreeMap<_, _>>();
        let mut table = lower
            .get("sourcetable")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .rsplit('.')
            .next()
            .unwrap_or("unknown")
            .to_string();
        table = match table.to_ascii_lowercase().as_str() {
            "traces" => "AppTraces",
            "exceptions" => "AppExceptions",
            "requests" => "AppRequests",
            "dependencies" => "AppDependencies",
            "availabilityresults" => "AppAvailabilityResults",
            _ => &table,
        }
        .into();
        if !allowed.contains(&table.as_str()) {
            continue;
        }
        let timestamp = lower
            .get("timegenerated")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let values = [
            number(lower.get("total").copied()),
            number(lower.get("errors").copied()),
            number(lower.get("warnings").copied()),
        ];
        if !timestamp.is_empty() {
            let bucket = buckets.entry(timestamp.clone()).or_default();
            for index in 0..3 {
                bucket[index] += values[index];
            }
        }
        let table_values = tables.entry(table).or_default();
        for index in 0..3 {
            table_values[index] += values[index];
        }
        latest = latest.max(
            lower
                .get("latest")
                .and_then(|value| value.as_str())
                .unwrap_or(&timestamp)
                .to_string(),
        );
        ingested = ingested.max(
            lower
                .get("ingested")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
        );
    }
    let timestamps = buckets.keys().cloned().collect::<Vec<_>>();
    let counts = buckets
        .values()
        .map(|values| values[0] as f64)
        .collect::<Vec<_>>();
    let error_counts = buckets
        .values()
        .map(|values| values[1] as f64)
        .collect::<Vec<_>>();
    let warning_counts = buckets
        .values()
        .map(|values| values[2] as f64)
        .collect::<Vec<_>>();
    let table_signals = tables
        .into_iter()
        .map(|(name, values)| LogTableSignal {
            name,
            total: values[0],
            errors: values[1],
            warnings: values[2],
        })
        .collect::<Vec<_>>();
    let total = counts.iter().sum::<f64>() as u64;
    let errors = error_counts.iter().sum::<f64>() as u64;
    let warnings = warning_counts.iter().sum::<f64>() as u64;
    let exceptions = table_signals
        .iter()
        .filter(|table| table.name == "AppExceptions")
        .map(|table| table.total)
        .sum();
    let failed_dependencies = table_signals
        .iter()
        .filter(|table| table.name == "AppDependencies")
        .map(|table| table.errors)
        .sum();
    let (window, interval, _) = window(window_minutes).unwrap_or(("1h", "5m", "PT1H"));
    LogSignalResult {
        resource_name: clean_text(resource_name, 120),
        state: if total == 0 { "no_data" } else { "available" }.into(),
        detail: if total == 0 {
            "query succeeded with no matching aggregate events; no health inference".into()
        } else {
            format!(
                "{total} aggregate events across {} fixed tables",
                table_signals.len()
            )
        },
        source: "Azure Monitor Logs fixed aggregate".into(),
        window: window.into(),
        interval: interval.into(),
        total,
        errors,
        warnings,
        exceptions,
        failed_dependencies,
        last_seen: latest.clone(),
        ingestion_lag_seconds: ingestion_lag(&latest, &ingested),
        timestamps,
        counts,
        error_counts,
        warning_counts,
        tables: table_signals,
        queried_workspaces,
        unavailable_workspaces,
    }
}

fn workspace_aggregate_supported(cloud: &str) -> bool {
    !cloud.eq_ignore_ascii_case("AzureUSGovernment")
}

fn number(value: Option<&Value>) -> u64 {
    value
        .and_then(|value| value.as_u64().or_else(|| value.as_f64().map(|v| v as u64)))
        .unwrap_or_default()
}

fn ingestion_lag(latest: &str, ingested: &str) -> Option<f64> {
    let latest = DateTime::parse_from_rfc3339(latest).ok()?;
    let ingested = DateTime::parse_from_rfc3339(ingested).ok()?;
    Some(
        (ingested.with_timezone(&Utc) - latest.with_timezone(&Utc))
            .num_milliseconds()
            .max(0) as f64
            / 1000.0,
    )
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawLogTarget {
    pub provider: String,
    pub description: String,
    pub(crate) command: Vec<String>,
}

pub fn raw_log_target(
    _subscription: &str,
    _resource_group: &str,
    _resource: &AzureResource,
) -> Option<RawLogTarget> {
    // Both otherwise-plausible Azure CLI streams cross the viewer's strict
    // boundary before emitting logs: App Service tail can obtain publishing
    // credentials, while Container Apps obtains a stream token and reads the
    // full resource document. Keep Shift+L as an explanatory surface, but
    // construct no cloud command until Azure exposes a metadata-safe stream.
    None
}

#[derive(Clone, Debug)]
pub struct RawLogSnapshot {
    pub resource_name: String,
    pub provider: String,
    pub status: String,
    pub detail: String,
    pub lines: Vec<String>,
    pub started_at: Option<Instant>,
    pub version: u64,
}

impl Default for RawLogSnapshot {
    fn default() -> Self {
        Self {
            resource_name: String::new(),
            provider: String::new(),
            status: "idle".into(),
            detail: String::new(),
            lines: Vec::new(),
            started_at: None,
            version: 0,
        }
    }
}

#[derive(Default)]
struct RawLogControl {
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

#[derive(Clone, Default)]
pub struct RawLogStream {
    state: Arc<RwLock<RawLogSnapshot>>,
    control: Arc<Mutex<RawLogControl>>,
    lifecycle: Arc<Mutex<()>>,
}

impl RawLogStream {
    pub async fn start(&self, resource_name: &str, target: RawLogTarget) {
        let _lifecycle = self.lifecycle.lock().await;
        self.stop_current().await;
        let cancel = CancellationToken::new();
        self.control.lock().await.cancel = cancel.clone();
        {
            let mut state = self.state.write().await;
            *state = RawLogSnapshot {
                resource_name: clean_text(resource_name, 120),
                provider: target.provider.clone(),
                status: "connecting".into(),
                detail: target.description.clone(),
                lines: Vec::new(),
                started_at: Some(Instant::now()),
                version: state.version + 1,
            };
        }
        let state = self.state.clone();
        let task = tokio::spawn(async move {
            run_raw_stream(state, cancel, target).await;
        });
        self.control.lock().await.task = Some(task);
    }

    pub async fn stop(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        self.stop_current().await;
    }

    async fn stop_current(&self) {
        let (cancel, task) = {
            let mut control = self.control.lock().await;
            (control.cancel.clone(), control.task.take())
        };
        cancel.cancel();
        if let Some(mut task) = task {
            if timeout(Duration::from_secs(2), &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
        let mut state = self.state.write().await;
        if state.status != "idle" {
            state.status = "stopped".into();
            state.detail = "local stream stopped; raw content cleared".into();
            state.lines.clear();
            state.started_at = None;
            state.version += 1;
        }
    }

    pub async fn snapshot(&self) -> RawLogSnapshot {
        self.state.read().await.clone()
    }
}

async fn run_raw_stream(
    state: Arc<RwLock<RawLogSnapshot>>,
    cancel: CancellationToken,
    target: RawLogTarget,
) {
    let Some((program, arguments)) = target.command.split_first() else {
        set_raw_state(&state, "unavailable", "empty fixed command").await;
        return;
    };
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env("AZURE_CORE_COLLECT_TELEMETRY", "false")
        .env("AZURE_CORE_ONLY_SHOW_ERRORS", "true")
        .env("AZURE_LOGGING_ENABLE_LOG_FILE", "false")
        .env("AZURE_EXTENSION_USE_DYNAMIC_INSTALL", "no")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            set_raw_state(
                &state,
                "unavailable",
                &format!("could not start Azure CLI: {error}"),
            )
            .await;
            return;
        }
    };
    let Some(stdout) = child.stdout.take() else {
        set_raw_state(&state, "unavailable", "Azure CLI stream exposed no output").await;
        return;
    };
    set_raw_state(
        &state,
        "streaming",
        "connected; waiting for provider output",
    )
    .await;
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::with_capacity(2_048);
    let mut dropping_overflow = false;
    let mut chunk = [0_u8; 4_096];
    let deadline = tokio::time::sleep(Duration::from_secs(RAW_SESSION_SECONDS));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                set_raw_state(&state, "stopped", "local stream stopped").await;
                return;
            }
            _ = &mut deadline => {
                let _ = child.kill().await;
                set_raw_state(&state, "ended", "15-minute local session limit reached").await;
                return;
            }
            read = reader.read(&mut chunk) => match read {
                Ok(0) => break,
                Ok(count) => {
                    for byte in &chunk[..count] {
                        if *byte == b'\n' {
                            append_raw_bytes(&state, &line, dropping_overflow).await;
                            line.clear();
                            dropping_overflow = false;
                        } else if !dropping_overflow {
                            if line.len() < RAW_LINE_BYTE_CAP {
                                line.push(*byte);
                            } else {
                                dropping_overflow = true;
                            }
                        }
                    }
                }
                Err(error) => {
                    let _ = child.kill().await;
                    set_raw_state(&state, "unavailable", &format!("stream failed: {error}")).await;
                    return;
                }
            }
        }
    }
    if !line.is_empty() || dropping_overflow {
        append_raw_bytes(&state, &line, dropping_overflow).await;
    }
    match timeout(Duration::from_secs(1), child.wait()).await {
        Ok(Ok(status)) if status.success() => {
            set_raw_state(&state, "ended", "remote stream ended").await
        }
        Ok(Ok(status)) => {
            set_raw_state(
                &state,
                "unavailable",
                &format!(
                    "Azure CLI stream exited with code {}",
                    status.code().unwrap_or(-1)
                ),
            )
            .await
        }
        _ => {
            let _ = child.kill().await;
            set_raw_state(&state, "unavailable", "stream did not terminate cleanly").await
        }
    }
}

async fn append_raw_bytes(state: &Arc<RwLock<RawLogSnapshot>>, bytes: &[u8], truncated: bool) {
    let decoded = String::from_utf8_lossy(bytes);
    let mut line = terminal_line(&decoded, if truncated { 1_970 } else { 2_000 });
    if truncated {
        line.push_str(" …[line truncated]");
    }
    append_raw_line(state, &line).await;
}

async fn append_raw_line(state: &Arc<RwLock<RawLogSnapshot>>, line: &str) {
    let line = terminal_line(line, 2_000);
    if line.is_empty() {
        return;
    }
    let mut state = state.write().await;
    let mut lines = VecDeque::from(std::mem::take(&mut state.lines));
    if lines.len() == RAW_LINE_CAP {
        lines.pop_front();
    }
    lines.push_back(line);
    state.lines = lines.into();
    state.status = "streaming".into();
    state.detail = "raw content in memory only; 200-line ring buffer".into();
    state.version += 1;
}

async fn set_raw_state(state: &Arc<RwLock<RawLogSnapshot>>, status: &str, detail: &str) {
    let mut state = state.write().await;
    state.status = status.into();
    state.detail = terminal_line(detail, 300);
    state.version += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn executable_stream(body: &str) -> (tempfile::TempDir, String) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fake-stream");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        (directory, path.to_string_lossy().to_string())
    }

    #[test]
    fn aggregate_queries_are_fixed_and_bounded() {
        let query = log_aggregate_query(
            "/subscriptions/private/resourceGroups/rg/providers/Microsoft.Web/sites/app",
            "Microsoft.Web/sites",
            60,
        )
        .unwrap();
        assert!(query.contains("summarize Total=count()"));
        assert!(query.contains("project TimeGenerated, SourceTable, Total"));
        assert!(!query.contains("Message"));
        assert!(log_aggregate_query("id", "type", 61).is_err());
    }

    #[test]
    fn aggregate_resource_id_is_a_backslash_escaped_kusto_literal() {
        let resource_id = r#"/subscriptions/s\evil' | project Message //"#;
        let query = log_aggregate_query(resource_id, "Microsoft.Web/sites", 60).unwrap();
        let literal = serde_json::to_string(&resource_id.to_ascii_lowercase()).unwrap();
        assert!(query.contains(&format!("== {literal}")));
        assert!(!query.contains("== '/subscriptions"));
    }

    #[test]
    fn raw_targets_are_strictly_allowlisted() {
        let app = AzureResource {
            name: "app".into(),
            resource_type: "Microsoft.Web/sites".into(),
            resource_id: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Web/sites/app"
                .into(),
            ..AzureResource::default()
        };
        assert!(raw_log_target("sub", "rg", &app).is_none());
        let search = AzureResource {
            resource_type: "Microsoft.Search/searchServices".into(),
            ..AzureResource::default()
        };
        assert!(raw_log_target("sub", "rg", &search).is_none());
    }

    #[test]
    fn parses_only_allowed_aggregate_columns() {
        let rows = vec![json!({
            "TimeGenerated": "2026-07-28T00:00:00Z",
            "SourceTable": "requests",
            "Total": 4,
            "Errors": 1,
            "Warnings": 0,
            "Latest": "2026-07-28T00:00:00Z",
            "Ingested": "2026-07-28T00:00:02Z",
            "Message": "must never be retained"
        })];
        let result = parse_log_rows("app", &rows, "Microsoft.Insights/components", 60, 1, 0);
        assert_eq!((result.total, result.errors), (4, 1));
        assert_eq!(result.tables[0].name, "AppRequests");
        assert_eq!(result.ingestion_lag_seconds, Some(2.0));
    }

    #[test]
    fn table_families_are_provider_specific() {
        assert!(log_tables("Microsoft.Web/sites").contains(&"AppServiceAppLogs"));
        assert!(log_tables("Microsoft.App/containerApps").contains(&"ContainerLogV2"));
        assert_eq!(
            log_tables("Microsoft.Search/searchServices"),
            &["AzureDiagnostics"]
        );
    }

    #[test]
    fn government_workspace_aggregates_fail_closed_before_a_query() {
        assert!(!workspace_aggregate_supported("AzureUSGovernment"));
        assert!(workspace_aggregate_supported("AzureCloud"));
    }

    #[test]
    fn app_insights_query_is_bounded_and_never_projects_content() {
        let query = app_insights_log_query(15).unwrap();
        assert!(query.contains("ago(15m)"));
        assert!(query.contains("summarize Total=count()"));
        for forbidden in ["message", "url", "operation_Name", "user_Id"] {
            assert!(!query.contains(forbidden));
        }
        assert!(app_insights_log_query(30).is_err());
    }

    #[test]
    fn empty_log_result_keeps_unavailable_separate_from_zero() {
        let result = empty_log_result("app", "unavailable", "forbidden", 60, 0, 2);
        assert_eq!(result.state, "unavailable");
        assert_eq!(result.total, 0);
        assert_eq!(result.unavailable_workspaces, 2);
        assert!(result.counts.is_empty());
    }

    #[test]
    fn successful_empty_query_is_no_data_not_healthy() {
        let result = parse_log_rows("app", &[], "Microsoft.Web/sites", 60, 1, 0);
        assert_eq!(result.state, "no_data");
        assert!(result.detail.contains("no health inference"));
    }

    #[test]
    fn unexpected_tables_are_discarded() {
        let rows = vec![json!({
            "TimeGenerated": "2026-07-28T00:00:00Z",
            "SourceTable": "SecretCustomerPayloads",
            "Total": 99
        })];
        let result = parse_log_rows("app", &rows, "Microsoft.Web/sites", 60, 1, 0);
        assert_eq!(result.total, 0);
        assert!(result.tables.is_empty());
    }

    #[test]
    fn app_service_and_slot_raw_tail_are_blocked_by_the_credential_boundary() {
        let slot = AzureResource {
            name: "app/slot".into(),
            resource_type: "Microsoft.Web/sites/slots".into(),
            resource_id:
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Web/sites/app/slots/blue"
                    .into(),
            ..AzureResource::default()
        };
        assert!(raw_log_target("sub", "rg", &slot).is_none());
    }

    #[test]
    fn container_stream_is_blocked_by_the_credential_and_config_boundary() {
        let container = AzureResource {
            name: "worker".into(),
            resource_type: "Microsoft.App/containerApps".into(),
            ..AzureResource::default()
        };
        assert!(raw_log_target("sub", "rg", &container).is_none());
    }

    #[test]
    fn ingestion_lag_never_becomes_negative() {
        assert_eq!(
            ingestion_lag("2026-07-28T00:00:02Z", "2026-07-28T00:00:00Z"),
            Some(0.0)
        );
        assert_eq!(ingestion_lag("", ""), None);
    }

    #[test]
    fn tabular_application_insights_shape_maps_columns_without_raw_retention() {
        let value = json!({
            "tables": [{
                "columns": [{"name":"SourceTable"},{"name":"Total"}],
                "rows": [["requests", 4]]
            }]
        });
        let rows = tabular_rows(&value);
        assert_eq!(rows[0]["SourceTable"], "requests");
        assert_eq!(rows[0]["Total"], 4);
        assert!(rows[0].get("Message").is_none());
    }

    #[tokio::test]
    async fn raw_ring_is_capped_and_sanitized() {
        let state = Arc::new(RwLock::new(RawLogSnapshot::default()));
        for index in 0..250 {
            append_raw_line(&state, &format!("\u{1b}[31mline-{index}\u{1b}[0m")).await;
        }
        let state = state.read().await;
        assert_eq!(state.lines.len(), RAW_LINE_CAP);
        assert_eq!(state.lines.first().unwrap(), "line-50");
        assert!(!state.lines.last().unwrap().contains('\u{1b}'));
    }

    #[tokio::test]
    async fn raw_process_replaces_invalid_utf8_and_cancels_cleanly() {
        let (_directory, program) =
            executable_stream("printf '\\033[31m\\377raw-line\\033[0m\\n'\nsleep 2");
        let stream = RawLogStream::default();
        stream
            .start(
                "app",
                RawLogTarget {
                    provider: "test".into(),
                    description: "fixed test stream".into(),
                    command: vec![program],
                },
            )
            .await;
        let mut snapshot = RawLogSnapshot::default();
        for _ in 0..300 {
            snapshot = stream.snapshot().await;
            if !snapshot.lines.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(snapshot.lines, vec!["�raw-line"]);
        assert!(!snapshot.lines[0].contains('\u{1b}'));
        stream.stop().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(stream.snapshot().await.status, "stopped");
    }

    #[tokio::test]
    async fn raw_stream_disables_cli_file_logging_and_dynamic_install() {
        let (_directory, program) = executable_stream(
            "printf '%s|%s|%s\\n' \"$AZURE_LOGGING_ENABLE_LOG_FILE\" \"$AZURE_EXTENSION_USE_DYNAMIC_INSTALL\" \"$AZURE_CORE_COLLECT_TELEMETRY\"",
        );
        let stream = RawLogStream::default();
        stream
            .start(
                "app",
                RawLogTarget {
                    provider: "test".into(),
                    description: "fixed test stream".into(),
                    command: vec![program],
                },
            )
            .await;
        let mut snapshot = RawLogSnapshot::default();
        for _ in 0..300 {
            snapshot = stream.snapshot().await;
            if !snapshot.lines.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(snapshot.lines, vec!["false|no|false"]);
        stream.stop().await;
    }

    #[tokio::test]
    async fn raw_stream_reopen_cannot_mix_prior_resource_content() {
        let (_old_directory, old_program) =
            executable_stream("printf 'old-resource\\n'\nsleep 2\nprintf 'stale-line\\n'");
        let (_new_directory, new_program) = executable_stream("printf 'new-resource\\n'\nsleep 2");
        let stream = RawLogStream::default();
        stream
            .start(
                "old-app",
                RawLogTarget {
                    provider: "test".into(),
                    description: "old".into(),
                    command: vec![old_program],
                },
            )
            .await;
        for _ in 0..300 {
            if !stream.snapshot().await.lines.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stream
            .start(
                "new-app",
                RawLogTarget {
                    provider: "test".into(),
                    description: "new".into(),
                    command: vec![new_program],
                },
            )
            .await;
        let mut snapshot = RawLogSnapshot::default();
        for _ in 0..300 {
            snapshot = stream.snapshot().await;
            if !snapshot.lines.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(snapshot.resource_name, "new-app");
        assert_eq!(snapshot.lines, vec!["new-resource"]);
        stream.stop().await;
        assert!(stream.snapshot().await.lines.is_empty());
    }

    #[tokio::test]
    async fn raw_stream_bounds_a_newline_free_record_before_sanitizing() {
        let (_directory, program) = executable_stream(
            "awk 'BEGIN { for (i = 0; i < 12000; i++) printf \"x\"; print \"\" }'",
        );
        let stream = RawLogStream::default();
        stream
            .start(
                "app",
                RawLogTarget {
                    provider: "test".into(),
                    description: "long-line".into(),
                    command: vec![program],
                },
            )
            .await;
        let mut snapshot = RawLogSnapshot::default();
        for _ in 0..300 {
            snapshot = stream.snapshot().await;
            if !snapshot.lines.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(snapshot.lines.len(), 1);
        assert!(snapshot.lines[0].len() <= 2_000);
        assert!(snapshot.lines[0].contains("[line truncated]"));
        stream.stop().await;
    }
}
