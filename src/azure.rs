use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, TimeZone, Utc};
use futures::{future::BoxFuture, stream, StreamExt};
use serde_json::Value;
use thiserror::Error;
use tokio::{process::Command, sync::Semaphore, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{Config, WatchRule},
    model::{
        AzureResource, ChangePoint, EvidenceState, GroupDetails, MetricQuery, MetricSeries,
        RecentChange, ResourceGroup, ResourceRelation, Signal, Snapshot, Subscription,
    },
    sanitize::{clean_text, error_detail},
};

const ACCOUNT_QUERY: &str = "[?state=='Enabled'].{id:id,name:name,isDefault:isDefault}";
const CLOUD_QUERY: &str = "name";
const METRIC_QUERY: &str = "value[].{name:name.value,unit:unit,series:timeseries[].data[].{timestamp:timeStamp,total:total,average:average,maximum:maximum,minimum:minimum,count:count}}";
const DIAGNOSTIC_CATEGORY_QUERY: &str = "value[].{name:name,categoryType:categoryType}";
const MAX_TELEMETRY_COMPONENTS: usize = 8;
const MAX_GRAPH_ROWS: usize = 1_000;
const MAX_RECENT_CHANGES: usize = 20;

const SERVICE_HEALTH_QUERY: &str = "ServiceHealthResources | where type =~ 'microsoft.resourcehealth/events' | extend eventType=tostring(properties.EventType), status=tostring(properties.Status) | summarize eventCount=count() by eventType, status | order by eventCount desc | project eventType, status, eventCount";
const TELEMETRY_KQL: &str = "union withsource=table_name isfuzzy=true requests, dependencies, exceptions, availabilityResults, customMetrics | summarize sampleCount=count(), failedCount=countif(tostring(success) == 'False'), p95Duration=percentile(duration, 95), latestAt=max(timestamp) by table_name";
const RESOURCE_GROUP_QUERY: &str = "ResourceContainers | where type =~ 'microsoft.resources/subscriptions/resourcegroups' | project name, location, state = tostring(properties.provisioningState) | order by name asc";
const POLICY_QUERY: &str = "PolicyResources | where type =~ 'microsoft.policyinsights/policystates' | where tostring(properties.complianceState) =~ 'NonCompliant' | summarize nonCompliantResources = dcount(tostring(properties.resourceId)), nonCompliantPolicies = dcount(tostring(properties.policyDefinitionId)) | project nonCompliantResources, nonCompliantPolicies";

fn resource_change_query(group: &str) -> String {
    let group = kusto_literal(group);
    format!(
        "resourcechanges | where resourceGroup =~ {group} | where todatetime(properties.changeAttributes.timestamp) > ago(24h) | extend timestamp=todatetime(properties.changeAttributes.timestamp), changeType=tostring(properties.changeType) | summarize changeCount=count() by bin(timestamp, 5m), changeType | order by timestamp asc | project timestamp, changeType, changeCount"
    )
}

fn recent_change_events_query(group: &str) -> String {
    let group = kusto_literal(group);
    format!(
        "resourcechanges | where resourceGroup =~ {group} | where todatetime(properties.changeAttributes.timestamp) > ago(24h) | extend targetResourceId=tolower(tostring(properties.targetResourceId)), timestamp=todatetime(properties.changeAttributes.timestamp), changeType=tostring(properties.changeType) | join kind=leftouter (Resources | where resourceGroup =~ {group} | project targetResourceId=tolower(id), resourceName=name, resourceType=type) on targetResourceId | top {MAX_RECENT_CHANGES} by timestamp desc | project timestamp, changeType, resourceName, resourceType"
    )
}

fn alert_instance_query(group: &str) -> String {
    let group = kusto_literal(group);
    format!(
        "AlertsManagementResources | where type =~ 'microsoft.alertsmanagement/alerts' | where tostring(properties.essentials.targetResourceGroup) =~ {group} | extend severity=tostring(properties.essentials.severity), condition=tostring(properties.essentials.monitorCondition), state=tostring(properties.essentials.alertState), started=todatetime(properties.essentials.startDateTime) | where condition =~ 'Fired' and state !~ 'Closed' | summarize total=count(), recent24h=countif(started > ago(24h)) by severity, condition, state | order by total desc | project severity, condition, state, total, recent24h"
    )
}

fn alert_rule_query(group: &str) -> String {
    let group = kusto_literal(group);
    format!(
        "Resources | where resourceGroup =~ {group} | where type in~ ('microsoft.insights/metricalerts','microsoft.insights/scheduledqueryrules','microsoft.alertsmanagement/smartdetectoralertrules','microsoft.insights/activitylogalerts') | extend enabled=coalesce(tobool(properties.enabled), tobool(properties.state =~ 'Enabled')), severity=tostring(properties.severity) | summarize ruleCount=count(), enabledCount=countif(enabled == true) by type, severity | order by ruleCount desc | project type, severity, ruleCount, enabledCount"
    )
}

fn resource_health_query(group: &str) -> String {
    let group = kusto_literal(group);
    format!(
        "HealthResources | where resourceGroup =~ {group} | where type =~ 'microsoft.resourcehealth/availabilitystatuses' | project targetResourceId=tolower(tostring(properties.targetResourceId)), availabilityState=tostring(properties.availabilityState), reasonType=tostring(properties.reasonType), occurredTime=tostring(properties.occurredTime)"
    )
}

fn resource_inventory_query(group: &str) -> String {
    let group = kusto_literal(group);
    format!(
        "Resources | where resourceGroup =~ {group} | project id, name, type, kind, location, changedTime = tostring(systemData.lastModifiedAt), createdTime = tostring(systemData.createdAt), provisioningState = tostring(properties.provisioningState), state = iff(type =~ 'microsoft.web/sites', tostring(properties.state), ''), availabilityState = iff(type =~ 'microsoft.web/sites', tostring(properties.availabilityState), ''), lastModifiedTimeUtc = iff(type =~ 'microsoft.web/sites', tostring(properties.lastModifiedTimeUtc), ''), serverFarmId = iff(type =~ 'microsoft.web/sites', tostring(properties.serverFarmId), ''), appServicePlanId = iff(type =~ 'microsoft.web/sites', tostring(properties.appServicePlanId), ''), linuxFxVersion = iff(type =~ 'microsoft.web/sites', tostring(properties.siteConfig.linuxFxVersion), ''), healthCheckConfigured = iff(type =~ 'microsoft.web/sites', isnotempty(tostring(properties.siteConfig.healthCheckPath)), false), telemetryQueryId = iff(type =~ 'microsoft.insights/components', tostring(properties.AppId), '') | order by name asc"
    )
}

fn workspace_inventory_query(group: &str) -> String {
    let group = kusto_literal(group);
    format!(
        "Resources | where resourceGroup =~ {group} | where type =~ 'microsoft.operationalinsights/workspaces' | project name, customerId = tostring(properties.customerId) | order by name asc"
    )
}

fn front_door_query(group: &str) -> String {
    let group = kusto_literal(group);
    format!(
        "Resources | where resourceGroup =~ {group} | where type =~ 'microsoft.cdn/profiles/afdendpoints' | extend enabledState = tostring(properties.enabledState), provisioningState = tostring(properties.provisioningState) | summarize total = count(), enabled = countif(enabledState =~ 'Enabled'), provisioningFailures = countif(isnotempty(provisioningState) and provisioningState !~ 'Succeeded') | project total, enabled, provisioningFailures"
    )
}

fn kusto_literal(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a Rust string cannot fail")
}

#[derive(Clone, Copy, Debug)]
pub struct MetricDef {
    pub azure_name: &'static str,
    pub aggregation: &'static str,
    pub public_name: &'static str,
}

const WEB_METRICS: &[MetricDef] = &[
    metric("Requests", "total", "requests"),
    metric("Http5xx", "total", "http_5xx"),
    metric("AverageResponseTime", "average", "response_time"),
    metric("MemoryWorkingSet", "average", "memory_working_set"),
    metric("HealthCheckStatus", "average", "health_check_status"),
];
const SLOT_METRICS: &[MetricDef] = &[
    metric("Requests", "total", "requests"),
    metric("Http5xx", "total", "http_5xx"),
    metric("AverageResponseTime", "average", "response_time"),
    metric("MemoryWorkingSet", "average", "memory_working_set"),
];
const PLAN_METRICS: &[MetricDef] = &[
    metric("CpuPercentage", "average", "cpu_percent"),
    metric("MemoryPercentage", "average", "memory_percent"),
];
const POSTGRES_METRICS: &[MetricDef] = &[
    metric("cpu_percent", "average", "cpu_percent"),
    metric("active_connections", "average", "active_connections"),
    metric("storage_percent", "average", "storage_percent"),
];
const SEARCH_METRICS: &[MetricDef] = &[
    metric("SearchLatency", "average", "search_latency"),
    metric("SearchQueriesPerSecond", "average", "search_qps"),
    metric(
        "ThrottledSearchQueriesPercentage",
        "average",
        "search_throttled_percent",
    ),
];
const AI_METRICS: &[MetricDef] = &[
    metric("TotalCalls", "total", "total_calls"),
    metric("TotalErrors", "total", "total_errors"),
    metric("Latency", "average", "latency"),
];
const AFD_METRICS: &[MetricDef] = &[
    metric("RequestCount", "total", "requests"),
    metric("Percentage5XX", "average", "http_5xx_percent"),
    metric("TotalLatency", "average", "total_latency"),
    metric("OriginHealthPercentage", "average", "origin_health_percent"),
];
const STORAGE_METRICS: &[MetricDef] = &[
    metric("UsedCapacity", "average", "storage_used"),
    metric("Availability", "average", "availability_percent"),
    metric("Transactions", "total", "transactions"),
    metric("SuccessE2ELatency", "average", "e2e_latency"),
    metric("SuccessServerLatency", "average", "server_latency"),
];
const KEYVAULT_METRICS: &[MetricDef] = &[
    metric("Availability", "average", "availability_percent"),
    metric("SaturationShoebox", "average", "saturation_percent"),
    metric("ServiceApiHit", "count", "api_hits"),
    metric("ServiceApiLatency", "average", "api_latency"),
    metric("ServiceApiResult", "count", "api_results"),
];
const ACR_METRICS: &[MetricDef] = &[
    metric("StorageUsed", "average", "storage_used"),
    metric("TotalPullCount", "total", "pulls"),
    metric("SuccessfulPullCount", "total", "successful_pulls"),
    metric("TotalPushCount", "total", "pushes"),
    metric("SuccessfulPushCount", "total", "successful_pushes"),
];
const SQL_METRICS: &[MetricDef] = &[
    metric("cpu_percent", "average", "cpu_percent"),
    metric("dtu_consumption_percent", "average", "dtu_percent"),
    metric("storage_percent", "average", "storage_percent"),
    metric("workers_percent", "average", "workers_percent"),
    metric("sessions_percent", "average", "sessions_percent"),
    metric("deadlock", "total", "deadlocks"),
];
const COSMOS_METRICS: &[MetricDef] = &[
    metric("TotalRequests", "total", "requests"),
    metric("TotalRequestUnits", "total", "request_units"),
    metric("NormalizedRUConsumption", "maximum", "ru_percent"),
    metric("ServiceAvailability", "average", "availability_percent"),
    metric("ServerSideLatency", "average", "server_latency"),
];
const REDIS_METRICS: &[MetricDef] = &[
    metric("serverLoad", "maximum", "server_load_percent"),
    metric("connectedclients", "maximum", "connected_clients"),
    metric("usedmemorypercentage", "maximum", "memory_percent"),
    metric("errors", "maximum", "errors"),
    metric("evictedkeys", "total", "evicted_keys"),
    metric("cachemissrate", "maximum", "cache_miss_percent"),
];
const FIREWALL_METRICS: &[MetricDef] = &[
    metric("FirewallHealth", "average", "firewall_health_percent"),
    metric("SNATPortUtilization", "maximum", "snat_percent"),
    metric("Throughput", "average", "throughput"),
    metric("DataProcessed", "total", "data_processed"),
    metric("FirewallLatencyPng", "average", "firewall_latency"),
];
const LOGIC_METRICS: &[MetricDef] = &[
    metric("RunsStarted", "total", "runs_started"),
    metric("RunsCompleted", "total", "runs_completed"),
    metric("RunsFailed", "total", "runs_failed"),
    metric("RunStartThrottledEvents", "total", "runs_throttled"),
    metric("TriggersStarted", "total", "triggers_started"),
];

const fn metric(
    azure_name: &'static str,
    aggregation: &'static str,
    public_name: &'static str,
) -> MetricDef {
    MetricDef {
        azure_name,
        aggregation,
        public_name,
    }
}

pub fn metric_adapter(resource_type: &str) -> &'static [MetricDef] {
    match resource_type.to_ascii_lowercase().as_str() {
        "microsoft.web/sites" => WEB_METRICS,
        "microsoft.web/sites/slots" => SLOT_METRICS,
        "microsoft.web/serverfarms" => PLAN_METRICS,
        "microsoft.dbforpostgresql/flexibleservers" => POSTGRES_METRICS,
        "microsoft.search/searchservices" => SEARCH_METRICS,
        "microsoft.cognitiveservices/accounts" => AI_METRICS,
        "microsoft.cdn/profiles" => AFD_METRICS,
        "microsoft.storage/storageaccounts" => STORAGE_METRICS,
        "microsoft.keyvault/vaults" => KEYVAULT_METRICS,
        "microsoft.containerregistry/registries" => ACR_METRICS,
        "microsoft.sql/servers/databases" => SQL_METRICS,
        "microsoft.documentdb/databaseaccounts" => COSMOS_METRICS,
        "microsoft.cache/redis" => REDIS_METRICS,
        "microsoft.network/azurefirewalls" => FIREWALL_METRICS,
        "microsoft.logic/workflows" => LOGIC_METRICS,
        _ => &[],
    }
}

#[derive(Debug, Error, Clone)]
#[error("{detail}")]
pub struct AzureError {
    pub detail: String,
    pub permission_limited: bool,
    pub not_found: bool,
}

impl AzureError {
    fn new(detail: impl Into<String>) -> Self {
        let raw = detail.into();
        let lower = raw.to_ascii_lowercase();
        let permission_limited = [
            "authorizationfailed",
            "forbidden",
            "does not have authorization",
            "permission",
            "access denied",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern));
        let not_found = [
            "resourcegroupnotfound",
            "could not be found",
            "was not found",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern));
        let detail = if permission_limited {
            "permission limited".into()
        } else if not_found {
            "resource not found or not visible".into()
        } else if lower.contains("timed out") || lower.contains("timeout") {
            "Azure CLI read timed out".into()
        } else if lower.contains("cancelled") || lower.contains("canceled") {
            "Azure CLI read cancelled".into()
        } else {
            error_detail(&raw)
        };
        Self {
            permission_limited,
            not_found,
            detail,
        }
    }
}

#[derive(Clone)]
pub struct AzureCli {
    timeout: Duration,
    cancel: CancellationToken,
    semaphore: Arc<Semaphore>,
    accepted_grains: Arc<Mutex<HashMap<String, u64>>>,
    runner: Option<Arc<FixedRunner>>,
    program: PathBuf,
}

type FixedRunner =
    dyn Fn(Vec<String>) -> BoxFuture<'static, Result<Value, AzureError>> + Send + Sync;

pub(crate) enum FixedLogRead {
    ApplicationInsights {
        application_id: String,
        offset: &'static str,
        aggregate_query: String,
    },
    LogAnalytics {
        workspace_id: String,
        timespan: &'static str,
        aggregate_query: String,
    },
}

impl AzureCli {
    pub fn new(timeout_seconds: u64) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_seconds),
            cancel: CancellationToken::new(),
            semaphore: Arc::new(Semaphore::new(4)),
            accepted_grains: Arc::new(Mutex::new(HashMap::new())),
            runner: None,
            program: PathBuf::from("az"),
        }
    }

    pub fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.semaphore = Arc::new(Semaphore::new(max_concurrency.clamp(1, 16)));
        self
    }

    pub fn child(&self) -> Self {
        Self {
            timeout: self.timeout,
            cancel: self.cancel.child_token(),
            semaphore: self.semaphore.clone(),
            accepted_grains: self.accepted_grains.clone(),
            runner: self.runner.clone(),
            program: self.program.clone(),
        }
    }

    #[cfg(test)]
    fn with_runner(
        runner: impl Fn(Vec<String>) -> BoxFuture<'static, Result<Value, AzureError>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            timeout: Duration::from_secs(1),
            cancel: CancellationToken::new(),
            semaphore: Arc::new(Semaphore::new(4)),
            accepted_grains: Arc::new(Mutex::new(HashMap::new())),
            runner: Some(Arc::new(runner)),
            program: PathBuf::from("az"),
        }
    }

    #[cfg(test)]
    fn with_program(program: PathBuf, timeout: Duration) -> Self {
        Self {
            timeout,
            cancel: CancellationToken::new(),
            semaphore: Arc::new(Semaphore::new(4)),
            accepted_grains: Arc::new(Mutex::new(HashMap::new())),
            runner: None,
            program,
        }
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    async fn run_json(
        &self,
        mut args: Vec<String>,
        subscription: Option<&str>,
    ) -> Result<Value, AzureError> {
        if let Some(subscription) = subscription {
            args.extend(["--subscription".into(), subscription.into()]);
        }
        args.extend([
            "--output".into(),
            "json".into(),
            "--only-show-errors".into(),
        ]);
        let _permit = tokio::select! {
            _ = self.cancel.cancelled() => {
                return Err(AzureError::new("Azure CLI command cancelled"));
            }
            permit = self.semaphore.clone().acquire_owned() => {
                permit.map_err(|_| AzureError::new("Azure CLI concurrency gate unavailable"))?
            }
        };
        if let Some(runner) = &self.runner {
            return runner(args).await;
        }
        let mut command = Command::new(&self.program);
        command
            .args(&args)
            .env("AZURE_CORE_COLLECT_TELEMETRY", "false")
            .env("AZURE_CORE_ONLY_SHOW_ERRORS", "true")
            .env("AZURE_LOGGING_ENABLE_LOG_FILE", "false")
            .env("AZURE_EXTENSION_USE_DYNAMIC_INSTALL", "no")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command
            .spawn()
            .map_err(|error| AzureError::new(format!("could not run Azure CLI: {error}")))?;
        let output = tokio::select! {
            _ = self.cancel.cancelled() => return Err(AzureError::new("Azure CLI command cancelled")),
            result = timeout(self.timeout, child.wait_with_output()) => {
                result
                    .map_err(|_| AzureError::new(format!("Azure CLI command timed out after {}s", self.timeout.as_secs())))?
                    .map_err(|error| AzureError::new(format!("could not run Azure CLI: {error}")))?
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(AzureError::new(if stderr.trim().is_empty() {
                stdout.as_ref()
            } else {
                stderr.as_ref()
            }));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|_| AzureError::new("Azure CLI returned malformed JSON"))
    }

    pub async fn subscriptions(&self) -> Result<(String, Vec<Subscription>), AzureError> {
        let cloud = self
            .run_json(strings(&["cloud", "show", "--query", CLOUD_QUERY]), None)
            .await?
            .as_str()
            .map(|value| clean_text(value, 80))
            .unwrap_or_else(|| "Unknown".into());
        let value = self
            .run_json(
                strings(&["account", "list", "--query", ACCOUNT_QUERY]),
                None,
            )
            .await?;
        let mut subscriptions = array(&value)?
            .iter()
            .filter_map(|item| {
                Some(Subscription {
                    name: text(item, "name", "Unknown", 120),
                    cloud: cloud.clone(),
                    is_default: item.get("isDefault")?.as_bool().unwrap_or(false),
                    subscription_id: item.get("id")?.as_str()?.to_string(),
                })
            })
            .collect::<Vec<_>>();
        subscriptions.sort_by_key(|item| (!item.is_default, item.name.to_ascii_lowercase()));
        Ok((cloud, subscriptions))
    }

    pub async fn resource_groups(
        &self,
        subscription: &str,
    ) -> Result<Vec<ResourceGroup>, AzureError> {
        let value = self
            .run_json(
                vec![
                    "graph".into(),
                    "query".into(),
                    "-q".into(),
                    RESOURCE_GROUP_QUERY.into(),
                    "--first".into(),
                    "1000".into(),
                    "--query".into(),
                    "data".into(),
                ],
                Some(subscription),
            )
            .await?;
        let mut groups = array(&value)?
            .iter()
            .map(|item| ResourceGroup {
                name: text(item, "name", "(unnamed)", 120),
                location: text(item, "location", "Unknown", 80),
                provisioning_state: text(item, "state", "Unknown", 80),
            })
            .collect::<Vec<_>>();
        if groups.len() == MAX_GRAPH_ROWS {
            return Err(AzureError::new(
                "fixed Resource Graph resource-group cap reached; refusing a silently partial chooser",
            ));
        }
        groups.sort_by_key(|group| group.name.to_ascii_lowercase());
        Ok(groups)
    }

    pub async fn resources(
        &self,
        subscription: &str,
        group: &str,
    ) -> Result<Vec<Value>, AzureError> {
        let rows = self
            .list(
                vec![
                    "graph".into(),
                    "query".into(),
                    "-q".into(),
                    resource_inventory_query(group),
                    "--first".into(),
                    "1000".into(),
                    "--query".into(),
                    "data".into(),
                ],
                subscription,
            )
            .await?;
        if rows.len() == MAX_GRAPH_ROWS {
            return Err(AzureError::new(
                "fixed Resource Graph inventory cap reached; refusing a silently partial resource group",
            ));
        }
        Ok(rows)
    }

    async fn list(&self, args: Vec<String>, subscription: &str) -> Result<Vec<Value>, AzureError> {
        let value = self.run_json(args, Some(subscription)).await?;
        Ok(array(&value)?.clone())
    }

    pub async fn metrics(
        &self,
        subscription: &str,
        resource: &AzureResource,
        query: &MetricQuery,
    ) -> Result<BTreeMap<String, MetricSeries>, AzureError> {
        let definitions = metric_adapter(&resource.resource_type)
            .iter()
            .filter(|definition| {
                definition.public_name != "health_check_status" || resource.health_check_configured
            })
            .collect::<Vec<_>>();
        if definitions.is_empty() {
            return Ok(BTreeMap::new());
        }
        let names = definitions
            .iter()
            .map(|definition| definition.azure_name)
            .collect::<Vec<_>>();
        let mut aggregations = definitions
            .iter()
            .map(|definition| definition.aggregation)
            .collect::<Vec<_>>();
        aggregations.sort_unstable();
        aggregations.dedup();
        let (rows, accepted_interval) = self
            .metric_batch(
                subscription,
                &resource.resource_id,
                &names,
                &aggregations,
                query,
            )
            .await?;
        let mut parsed = BTreeMap::new();
        for aggregation in aggregations {
            let matching = definitions
                .iter()
                .copied()
                .filter(|definition| definition.aggregation == aggregation)
                .collect::<Vec<_>>();
            parse_metrics(
                &mut parsed,
                &matching,
                &rows,
                aggregation,
                query,
                accepted_interval,
            );
        }
        Ok(parsed)
    }

    async fn metric_batch(
        &self,
        subscription: &str,
        resource_id: &str,
        names: &[&str],
        aggregations: &[&str],
        query: &MetricQuery,
    ) -> Result<(Vec<Value>, u64), AzureError> {
        let grain_key = format!(
            "{}:{}",
            resource_id.to_ascii_lowercase(),
            query.requested_interval_minutes
        );
        let remembered = self
            .accepted_grains
            .lock()
            .ok()
            .and_then(|grains| grains.get(&grain_key).copied())
            .filter(|interval| *interval >= query.requested_interval_minutes);
        let mut candidates = remembered.into_iter().collect::<Vec<_>>();
        for interval in [query.requested_interval_minutes, 5, 15, 60] {
            if interval >= query.requested_interval_minutes && !candidates.contains(&interval) {
                candidates.push(interval);
            }
        }
        let mut last_error = None;
        for interval in candidates {
            let iso_interval = if interval == 60 {
                "PT1H".to_string()
            } else {
                format!("PT{interval}M")
            };
            let mut args = vec![
                "monitor".into(),
                "metrics".into(),
                "list".into(),
                "--resource".into(),
                resource_id.into(),
                "--metrics".into(),
            ];
            args.extend(names.iter().map(|name| (*name).into()));
            args.push("--aggregation".into());
            args.extend(aggregations.iter().map(|aggregation| (*aggregation).into()));
            args.extend([
                "--interval".into(),
                iso_interval,
                "--start-time".into(),
                query.start_time.clone(),
                "--end-time".into(),
                query.end_time.clone(),
                "--query".into(),
                METRIC_QUERY.into(),
            ]);
            let value = self.run_json(args, Some(subscription)).await;
            match value {
                Ok(value) => {
                    if let Ok(mut grains) = self.accepted_grains.lock() {
                        grains.insert(grain_key.clone(), interval);
                    }
                    return Ok((array(&value)?.clone(), interval));
                }
                Err(error)
                    if error.detail.to_ascii_lowercase().contains("time grain")
                        || error.detail.to_ascii_lowercase().contains("interval") =>
                {
                    if remembered == Some(interval) {
                        if let Ok(mut grains) = self.accepted_grains.lock() {
                            grains.remove(&grain_key);
                        }
                    }
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| AzureError::new("metric interval unavailable")))
    }

    pub async fn diagnostic_categories(
        &self,
        subscription: &str,
        resource_id: &str,
    ) -> Result<usize, AzureError> {
        let categories = self
            .run_json(
                strings(&[
                    "monitor",
                    "diagnostic-settings",
                    "categories",
                    "list",
                    "--resource",
                    resource_id,
                    "--query",
                    DIAGNOSTIC_CATEGORY_QUERY,
                ]),
                Some(subscription),
            )
            .await?;
        Ok(array(&categories)?.len())
    }

    pub async fn aggregate_signals(
        &self,
        subscription: &str,
        group: &str,
        resources: &[AzureResource],
        max_workers: usize,
    ) -> (
        Vec<Signal>,
        Vec<String>,
        HashMap<String, (String, String, String)>,
        Vec<ChangePoint>,
        Vec<RecentChange>,
    ) {
        let specs = vec![
            (
                "alert instances",
                "resource_group",
                alert_instance_query(group),
            ),
            (
                "Azure Service Health",
                "subscription",
                SERVICE_HEALTH_QUERY.to_string(),
            ),
            (
                "resource changes",
                "resource_group",
                resource_change_query(group),
            ),
            (
                "recent change events",
                "resource_group",
                recent_change_events_query(group),
            ),
            (
                "alert-rule coverage",
                "resource_group",
                alert_rule_query(group),
            ),
            (
                "Resource Health availability",
                "resource_group",
                resource_health_query(group),
            ),
            (
                "Front Door endpoints",
                "resource_group",
                front_door_query(group),
            ),
            ("Azure Policy", "subscription", POLICY_QUERY.to_string()),
        ];
        let graph_futures = specs.into_iter().map(|(name, scope, query)| async move {
            (
                name,
                scope,
                self.run_json(
                    vec![
                        "graph".into(),
                        "query".into(),
                        "-q".into(),
                        query,
                        "--first".into(),
                        "1000".into(),
                        "--query".into(),
                        "data".into(),
                    ],
                    Some(subscription),
                )
                .await,
            )
        });
        let mut component_resources = resources
            .iter()
            .filter(|resource| {
                resource
                    .resource_type
                    .eq_ignore_ascii_case("microsoft.insights/components")
            })
            .collect::<Vec<_>>();
        component_resources.sort_by(|left, right| {
            right.watched.cmp(&left.watched).then_with(|| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
            })
        });
        let component_total = component_resources.len();
        let missing_component_ids = component_resources
            .iter()
            .filter(|resource| resource.telemetry_query_id.is_empty())
            .count();
        component_resources.retain(|resource| !resource.telemetry_query_id.is_empty());
        let queryable_components = component_resources.len();
        component_resources.truncate(MAX_TELEMETRY_COMPONENTS);
        let components = component_resources
            .into_iter()
            .map(|resource| resource.telemetry_query_id.clone())
            .collect::<Vec<_>>();
        let capped_components = queryable_components.saturating_sub(components.len());
        let telemetry_future = async {
            let subscription = subscription.to_string();
            let results = stream::iter(components.iter().cloned())
                .map(|application_id| {
                    let azure = self.child();
                    let subscription = subscription.clone();
                    async move {
                        azure
                            .run_json(
                                vec![
                                    "monitor".into(),
                                    "app-insights".into(),
                                    "query".into(),
                                    "--apps".into(),
                                    application_id,
                                    "--analytics-query".into(),
                                    TELEMETRY_KQL.into(),
                                    "--offset".into(),
                                    "24h".into(),
                                ],
                                Some(&subscription),
                            )
                            .await
                    }
                })
                .buffer_unordered(max_workers.max(1))
                .collect::<Vec<_>>()
                .await;
            let mut active = 0;
            let mut unavailable = 0;
            for result in results {
                match result {
                    Ok(value) if tabular_has_rows(&value) => active += 1,
                    Ok(_) => {}
                    Err(_) => unavailable += 1,
                }
            }
            (active, unavailable)
        };
        let (graph_results, telemetry) =
            tokio::join!(futures::future::join_all(graph_futures), telemetry_future);
        let mut signals = Vec::new();
        let mut limitations = vec![
            "Confirmed deployment history and Activity Log actors intentionally excluded; Azure CLI downloads raw records before projection.".into(),
            "Action Group receivers intentionally excluded.".into(),
        ];
        if missing_component_ids > 0 {
            limitations.push(format!(
                "Application Insights telemetry: {missing_component_ids} components lacked a server-projected application query ID; no component GET was attempted."
            ));
        }
        if capped_components > 0 {
            limitations.push(format!(
                "Application Insights telemetry: {capped_components} components outside the fixed {MAX_TELEMETRY_COMPONENTS}-component aggregate cap."
            ));
            signals.push(Signal {
                name: "Application Insights telemetry sampling".into(),
                state: "limited".into(),
                detail: format!(
                    "{}/{} components queried; {capped_components} capped",
                    components.len(),
                    component_total
                ),
                source: "fixed provider-balanced cap".into(),
                window: "24h".into(),
                scope: "resource_group".into(),
            });
        }
        let mut health = HashMap::new();
        let mut changes = Vec::new();
        let mut recent_changes = Vec::new();
        for (name, scope, result) in graph_results {
            match result {
                Ok(value) if name == "Resource Health availability" => {
                    if let Ok(rows) = array(&value) {
                        for row in rows {
                            if let Some(id) = row["targetResourceId"].as_str() {
                                health.insert(
                                    id.to_ascii_lowercase(),
                                    (
                                        text(row, "availabilityState", "unknown", 40)
                                            .to_ascii_lowercase(),
                                        text(row, "reasonType", "", 120),
                                        text(row, "occurredTime", "", 60),
                                    ),
                                );
                            }
                        }
                        signals.push(Signal {
                            name: name.into(),
                            state: if rows.is_empty() {
                                "no_data"
                            } else {
                                "available"
                            }
                            .into(),
                            detail: format!("{} availability records", rows.len()),
                            source: "Azure Resource Graph HealthResources".into(),
                            window: "current".into(),
                            scope: scope.into(),
                        });
                    }
                }
                Ok(value) if name == "resource changes" => {
                    changes = parse_change_points(&value);
                    let total = changes.iter().map(|point| point.count).sum::<u64>();
                    signals.push(Signal {
                        name: name.into(),
                        state: if total == 0 { "no_data" } else { "signal" }.into(),
                        detail: format!(
                            "{total} resource changes in {} safe aggregate bins; actors and diffs excluded",
                            changes.len()
                        ),
                        source: "Azure Resource Graph fixed aggregate".into(),
                        window: "24h · 5m bins".into(),
                        scope: "resource_group".into(),
                    });
                }
                Ok(value) if name == "recent change events" => {
                    recent_changes = parse_recent_changes(&value);
                    signals.push(Signal {
                        name: name.into(),
                        state: if recent_changes.is_empty() {
                            "no_data"
                        } else {
                            "signal"
                        }
                        .into(),
                        detail: format!(
                            "{} fixed metadata-only change records; no actors, IDs, or diffs",
                            recent_changes.len()
                        ),
                        source: "Azure Resource Graph fixed projection".into(),
                        window: "24h · 20 record cap".into(),
                        scope: scope.into(),
                    });
                }
                Ok(value) => {
                    signals.push(graph_signal(name, scope, &value));
                }
                Err(error) => {
                    limitations.push(format!("{name}: {}", error.detail));
                    signals.push(Signal {
                        name: name.into(),
                        state: "unavailable".into(),
                        detail: error.detail,
                        source: "Azure Resource Graph fixed aggregate".into(),
                        window: "current".into(),
                        scope: scope.into(),
                    });
                }
            }
        }
        if component_total == 0 {
            signals.push(Signal {
                name: "Application Insights telemetry".into(),
                state: "unsupported".into(),
                detail: "no components in selected resource group".into(),
                source: "Application Insights aggregate query".into(),
                window: "24h".into(),
                scope: "resource_group".into(),
            });
        } else if components.is_empty() {
            signals.push(Signal {
                name: "Application Insights telemetry".into(),
                state: "unavailable".into(),
                detail: format!(
                    "0/{component_total} components queryable from server-projected IDs; no component GET attempted"
                ),
                source: "Application Insights aggregate query".into(),
                window: "24h".into(),
                scope: "resource_group".into(),
            });
        } else {
            let (active, unavailable) = telemetry;
            signals.push(Signal {
                name: "Application Insights telemetry".into(),
                state: if unavailable == components.len() {
                    "unavailable"
                } else if active == 0 {
                    "no_data"
                } else {
                    "available"
                }
                .into(),
                detail: format!(
                    "{active}/{} sampled components active; {unavailable} query failures; {missing_component_ids} IDs unavailable; {component_total} total visible",
                    components.len(),
                ),
                source: "Application Insights fixed aggregate".into(),
                window: "24h".into(),
                scope: "resource_group".into(),
            });
        }
        (signals, limitations, health, changes, recent_changes)
    }

    pub async fn workspaces(
        &self,
        subscription: &str,
        group: &str,
    ) -> Result<Vec<Value>, AzureError> {
        let rows = self
            .list(
                vec![
                    "graph".into(),
                    "query".into(),
                    "-q".into(),
                    workspace_inventory_query(group),
                    "--first".into(),
                    "1000".into(),
                    "--query".into(),
                    "data".into(),
                ],
                subscription,
            )
            .await?;
        if rows.len() == MAX_GRAPH_ROWS {
            return Err(AzureError::new(
                "fixed Resource Graph workspace cap reached; refusing silently partial log discovery",
            ));
        }
        Ok(rows)
    }

    pub(crate) async fn fixed_log_query(
        &self,
        subscription: &str,
        read: FixedLogRead,
    ) -> Result<Value, AzureError> {
        let args = match read {
            FixedLogRead::ApplicationInsights {
                application_id,
                offset,
                aggregate_query,
            } => vec![
                "monitor".into(),
                "app-insights".into(),
                "query".into(),
                "--apps".into(),
                application_id,
                "--offset".into(),
                offset.into(),
                "--analytics-query".into(),
                aggregate_query,
            ],
            FixedLogRead::LogAnalytics {
                workspace_id,
                timespan,
                aggregate_query,
            } => vec![
                "monitor".into(),
                "log-analytics".into(),
                "query".into(),
                "--workspace".into(),
                workspace_id,
                "--analytics-query".into(),
                aggregate_query,
                "--timespan".into(),
                timespan.into(),
            ],
        };
        self.run_json(args, Some(subscription)).await
    }
}

#[derive(Clone)]
pub struct Collector {
    pub config: Config,
    pub azure: AzureCli,
    pub metrics_enabled: bool,
}

pub fn metric_query(window_hours: u64, interval_minutes: u64) -> MetricQuery {
    let interval_minutes = interval_minutes.max(1);
    let grain_seconds = (interval_minutes * 60) as i64;
    let now = Utc::now();
    let end_seconds = now.timestamp() - now.timestamp().rem_euclid(grain_seconds);
    let end = Utc.timestamp_opt(end_seconds, 0).single().unwrap_or(now);
    let start = end - ChronoDuration::hours(window_hours.max(1) as i64);
    let start_time = start.to_rfc3339_opts(SecondsFormat::Secs, true);
    let end_time = end.to_rfc3339_opts(SecondsFormat::Secs, true);
    MetricQuery {
        window_hours: window_hours.max(1),
        requested_interval_minutes: interval_minutes,
        start_time: start_time.clone(),
        end_time: end_time.clone(),
        queried_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        cohort: format!(
            "{}h/{}m:{}..{}",
            window_hours.max(1),
            interval_minutes,
            start_time,
            end_time
        ),
    }
}

impl Collector {
    pub fn new(config: Config, azure: AzureCli, metrics_enabled: bool) -> Self {
        let azure = azure.with_max_concurrency(config.max_workers);
        Self {
            config,
            azure,
            metrics_enabled,
        }
    }

    pub fn child(&self) -> Self {
        Self {
            config: self.config.clone(),
            azure: self.azure.child(),
            metrics_enabled: self.metrics_enabled,
        }
    }

    pub fn cancel(&self) {
        self.azure.cancel();
    }

    pub async fn collect_inventory(
        &self,
        subscription_selector: &str,
        group_selector: &str,
    ) -> Result<Snapshot, AzureError> {
        let (_, subscriptions) = self.azure.subscriptions().await?;
        let selected = select_subscription(&subscriptions, subscription_selector)?.clone();
        let groups = self
            .azure
            .resource_groups(&selected.subscription_id)
            .await?;
        let group = select_group(&groups, group_selector)?.clone();
        self.collect_resolved_inventory(subscriptions, selected, groups, group)
            .await
    }

    pub async fn collect_resolved_inventory(
        &self,
        subscriptions: Vec<Subscription>,
        selected: Subscription,
        groups: Vec<ResourceGroup>,
        group: ResourceGroup,
    ) -> Result<Snapshot, AzureError> {
        let inventory = self
            .azure
            .resources(&selected.subscription_id, &group.name)
            .await;
        let mut details = GroupDetails {
            limitations: vec![
                "Confirmed deployment history and Activity Log actors intentionally excluded."
                    .into(),
                "Raw logs excluded from JSON and table output.".into(),
            ],
            ..GroupDetails::default()
        };
        let (access_state, access_detail, resources) = match inventory {
            Ok(inventory) => {
                let mut resources = build_resources(&inventory, &inventory);
                apply_watchlist(&mut resources, &self.config.watchlist);
                mark_metric_candidates(
                    &mut resources,
                    self.config.max_metric_resources,
                    self.metrics_enabled,
                );
                build_relationships(&mut resources);
                details.signals.push(Signal {
                    name: "Azure resource inventory".into(),
                    state: "available".into(),
                    detail: format!("{} server-projected resources", resources.len()),
                    source: "Azure Resource Graph fixed inventory projection".into(),
                    window: "current".into(),
                    scope: "resource_group".into(),
                });
                ("available".into(), String::new(), resources)
            }
            Err(error) => {
                details.signals.push(Signal {
                    name: "Azure resource inventory".into(),
                    state: "unavailable".into(),
                    detail: error.detail.clone(),
                    source: "Azure Resource Graph fixed inventory projection".into(),
                    window: "current".into(),
                    scope: "resource_group".into(),
                });
                ("unavailable".into(), error.detail, Vec::new())
            }
        };
        Ok(Snapshot::now(
            subscriptions,
            selected.name,
            selected.subscription_id,
            groups,
            group.name,
            access_state,
            access_detail,
            resources,
            details,
            self.metrics_enabled,
        ))
    }

    pub async fn refresh_current_inventory(
        &self,
        current: &Snapshot,
    ) -> Result<Snapshot, AzureError> {
        let subscription = current.selected_subscription_id.clone();
        let group = current.selected_resource_group.clone();
        let inventory = self.azure.resources(&subscription, &group).await?;
        let details = current.details.clone();
        let mut resources = build_resources(&inventory, &inventory);
        apply_watchlist(&mut resources, &self.config.watchlist);
        mark_metric_candidates(
            &mut resources,
            self.config.max_metric_resources,
            self.metrics_enabled,
        );
        build_relationships(&mut resources);
        Ok(Snapshot::now(
            current.subscriptions.clone(),
            current.selected_subscription_name.clone(),
            subscription,
            current.resource_groups.clone(),
            group,
            "available".into(),
            String::new(),
            resources,
            details,
            self.metrics_enabled,
        ))
    }

    pub async fn enrich_metadata(&self, mut snapshot: Snapshot) -> Snapshot {
        if !self.metrics_enabled || snapshot.access_state != "available" {
            return snapshot;
        }
        let subscription = snapshot.selected_subscription_id.clone();
        let group = snapshot.selected_resource_group.clone();
        let resources = snapshot.resources.clone();
        let diagnostics_resources = resources.clone();
        let azure = self.azure.clone();
        let diagnostic_workers = self.config.max_workers;
        let diagnostics = async move {
            let diagnostic_candidates = select_diagnostic_candidates(&diagnostics_resources, 12);
            stream::iter(diagnostic_candidates)
                .map(|resource| {
                    let azure = azure.child();
                    let subscription = subscription.clone();
                    async move {
                        let result = azure
                            .diagnostic_categories(&subscription, &resource.resource_id)
                            .await;
                        (resource.resource_id, result)
                    }
                })
                .buffer_unordered(diagnostic_workers)
                .collect::<Vec<_>>()
                .await
        };
        let aggregate = self.azure.aggregate_signals(
            &snapshot.selected_subscription_id,
            &group,
            &resources,
            self.config.max_workers,
        );
        let ((signals, limitations, health, changes, cloud_changes), diagnostic_results) =
            tokio::join!(aggregate, diagnostics);
        for resource in &mut snapshot.resources {
            if let Some((state, reason, observed)) =
                health.get(&resource.resource_id.to_ascii_lowercase())
            {
                resource.resource_health_state.clone_from(state);
                resource.resource_health_reason.clone_from(reason);
                resource.resource_health_observed_at.clone_from(observed);
            }
        }
        for (resource_id, result) in diagnostic_results {
            if let Some(resource) = snapshot
                .resources
                .iter_mut()
                .find(|resource| resource.resource_id == resource_id)
            {
                match result {
                    Ok(supported) => {
                        resource.diagnostic_state = "not_inspected".into();
                        resource.diagnostic_detail = format!(
                            "{supported} supported categories; configuration intentionally not retrieved because Azure CLI exposes destination metadata before projection"
                        );
                    }
                    Err(error) => {
                        resource.diagnostic_state = "unavailable".into();
                        resource.diagnostic_detail = error.detail;
                    }
                }
            }
        }
        snapshot.details.signals = signals;
        snapshot.details.limitations = limitations;
        snapshot.details.changes = changes;
        snapshot.details.recent_changes =
            merge_recent_changes(cloud_changes, snapshot.details.recent_changes);
        snapshot.generated_at = chrono::Utc::now().to_rfc3339();
        snapshot.enrichment_state = "current".into();
        snapshot
    }

    pub async fn enrich_snapshot(&self, snapshot: Snapshot) -> Snapshot {
        if !self.metrics_enabled || snapshot.access_state != "available" {
            return snapshot;
        }
        let query = metric_query(
            self.config.metric_window_hours,
            self.config.metric_interval_minutes,
        );
        let subscription = snapshot.selected_subscription_id.clone();
        let candidates =
            select_metric_candidates(&snapshot.resources, self.config.max_metric_resources);
        let metadata = self.enrich_metadata(snapshot.clone());
        let metrics = self.refresh_metrics(&subscription, &candidates, &query);
        let (mut snapshot, refreshed) = tokio::join!(metadata, metrics);
        merge_resources(&mut snapshot.resources, refreshed.clone());
        merge_fleet_resources(&mut snapshot.resources, refreshed);
        snapshot.fleet_query = query;
        snapshot.fleet_state = "current".into();
        snapshot
    }

    pub async fn collect(
        &self,
        subscription_selector: &str,
        group_selector: &str,
    ) -> Result<Snapshot, AzureError> {
        let snapshot = self
            .collect_inventory(subscription_selector, group_selector)
            .await?;
        Ok(self.enrich_snapshot(snapshot).await)
    }

    pub async fn refresh_metrics(
        &self,
        subscription: &str,
        resources: &[AzureResource],
        query: &MetricQuery,
    ) -> Vec<AzureResource> {
        let collector = Arc::new(self.clone());
        let query = query.clone();
        stream::iter(resources.iter().cloned())
            .map(|resource| {
                let collector = collector.clone();
                let subscription = subscription.to_string();
                let query = query.clone();
                async move {
                    collector
                        .refresh_metric(&subscription, resource, &query)
                        .await
                }
            })
            .buffer_unordered(self.config.max_workers)
            .collect()
            .await
    }

    pub async fn refresh_metric(
        &self,
        subscription: &str,
        mut resource: AzureResource,
        query: &MetricQuery,
    ) -> AzureResource {
        match self.azure.metrics(subscription, &resource, query).await {
            Ok(metrics) => {
                resource.metrics = metrics;
                resource.evidence_state = if resource
                    .metrics
                    .values()
                    .any(|metric| metric.state == "available")
                {
                    EvidenceState::Signal
                } else {
                    EvidenceState::NoData
                };
                resource.evidence_detail = if resource.evidence_state == EvidenceState::Signal {
                    format!(
                        "fixed metric read returned aggregate samples · read {}",
                        query.queried_at
                    )
                } else {
                    format!(
                        "fixed metric read succeeded with no samples · read {}",
                        query.queried_at
                    )
                };
                apply_metric_health(&mut resource);
            }
            Err(error) => {
                resource.metrics = metric_adapter(&resource.resource_type)
                    .iter()
                    .map(|definition| {
                        (
                            definition.public_name.into(),
                            MetricSeries {
                                name: definition.public_name.into(),
                                state: "unavailable".into(),
                                detail: error.detail.clone(),
                                source: "Azure Monitor metrics".into(),
                                window: query.window_label(),
                                interval: query.interval_label(),
                                aggregation: definition.aggregation.into(),
                                query: query.clone(),
                                ..MetricSeries::default()
                            },
                        )
                    })
                    .collect();
                resource.evidence_state = EvidenceState::Limited;
                resource.evidence_detail = error.detail;
            }
        }
        resource
    }
}

pub fn select_subscription<'a>(
    subscriptions: &'a [Subscription],
    selector: &str,
) -> Result<&'a Subscription, AzureError> {
    if subscriptions.is_empty() {
        return Err(AzureError::new(
            "no enabled Azure subscriptions are visible",
        ));
    }
    if selector.trim().is_empty() {
        return Ok(subscriptions
            .iter()
            .find(|item| item.is_default)
            .unwrap_or(&subscriptions[0]));
    }
    subscriptions
        .iter()
        .find(|item| {
            item.subscription_id.eq_ignore_ascii_case(selector)
                || item.name.eq_ignore_ascii_case(selector)
        })
        .ok_or_else(|| AzureError::new(format!("subscription not found: {selector}")))
}

pub fn select_group<'a>(
    groups: &'a [ResourceGroup],
    selector: &str,
) -> Result<&'a ResourceGroup, AzureError> {
    if groups.is_empty() {
        return Err(AzureError::new("no resource groups are visible"));
    }
    if selector.trim().is_empty() {
        return Ok(&groups[0]);
    }
    groups
        .iter()
        .find(|item| item.name.eq_ignore_ascii_case(selector))
        .ok_or_else(|| AzureError::new(format!("resource group not found: {selector}")))
}

pub fn build_resources(inventory: &[Value], webapps: &[Value]) -> Vec<AzureResource> {
    let web_by_id = webapps
        .iter()
        .filter_map(|item| {
            item["id"]
                .as_str()
                .map(|id| (id.to_ascii_lowercase(), item))
        })
        .collect::<HashMap<_, _>>();
    let mut resources = inventory
        .iter()
        .map(|item| {
            let resource_id = item["id"].as_str().unwrap_or_default().to_string();
            let resource_type = text(item, "type", "unknown", 160);
            let metrics_supported = !metric_adapter(&resource_type).is_empty();
            let web = web_by_id.get(&resource_id.to_ascii_lowercase()).copied();
            let version = web
                .map(|web| compact_version(web["linuxFxVersion"].as_str().unwrap_or_default()))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown".into());
            AzureResource {
                name: text(item, "name", "(unnamed)", 160),
                category: resource_category(&resource_type).into(),
                kind: text(item, "kind", "", 120),
                location: text(item, "location", "Unknown", 80),
                control_state: web
                    .map(|web| text(web, "state", "unknown", 40))
                    .unwrap_or_else(|| "unknown".into()),
                availability_state: web
                    .map(|web| text(web, "availabilityState", "unknown", 40))
                    .unwrap_or_else(|| "unknown".into()),
                provisioning_state: text(item, "provisioningState", "Unknown", 80),
                changed_at: web
                    .map(|web| text(web, "lastModifiedTimeUtc", "", 80))
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| {
                        text(
                            item,
                            if item["changedTime"].is_null() {
                                "createdTime"
                            } else {
                                "changedTime"
                            },
                            "",
                            80,
                        )
                    }),
                health_check_configured: web
                    .and_then(|web| web["healthCheckConfigured"].as_bool())
                    .unwrap_or(false),
                health_state: "unknown".into(),
                health_detail: if web.is_some() {
                    "unknown: no positive application-health evidence".into()
                } else {
                    "unsupported: inventory metadata only".into()
                },
                resource_health_state: "unknown".into(),
                diagnostic_state: "not_inspected".into(),
                diagnostic_detail:
                    "diagnostic configuration intentionally not retrieved; category capability is sampled only during bounded enrichment"
                        .into(),
                evidence_state: if metrics_supported {
                    EvidenceState::Pending
                } else {
                    EvidenceState::InventoryOnly
                },
                evidence_detail: if metrics_supported {
                    "fixed metric adapter awaiting schedule".into()
                } else {
                    "inventory metadata only; no fixed metric adapter".into()
                },
                resource_id,
                hosting_plan_id: web
                    .and_then(|web| {
                        web["appServicePlanId"]
                            .as_str()
                            .or_else(|| web["serverFarmId"].as_str())
                    })
                    .unwrap_or_default()
                    .to_string(),
                telemetry_query_id: text(item, "telemetryQueryId", "", 80),
                resource_type,
                version,
                ..AzureResource::default()
            }
        })
        .collect::<Vec<_>>();
    resources.sort_by_key(|resource| resource.name.to_ascii_lowercase());
    resources
}

pub fn resource_category(resource_type: &str) -> &'static str {
    let resource_type = resource_type.to_ascii_lowercase();
    let prefixes: &[(&str, &[&str])] = &[
        (
            "compute/web",
            &[
                "microsoft.web/",
                "microsoft.compute/",
                "microsoft.containerservice/",
                "microsoft.app/",
                "microsoft.desktopvirtualization/",
            ],
        ),
        (
            "data",
            &[
                "microsoft.dbforpostgresql/",
                "microsoft.sql/",
                "microsoft.documentdb/",
                "microsoft.cache/",
                "microsoft.search/",
            ],
        ),
        (
            "network/edge",
            &[
                "microsoft.network/",
                "microsoft.cdn/",
                "microsoft.networkfunction/",
            ],
        ),
        (
            "ai",
            &[
                "microsoft.cognitiveservices/",
                "microsoft.machinelearningservices/",
                "microsoft.botservice/",
            ],
        ),
        ("storage", &["microsoft.storage/"]),
        (
            "monitoring",
            &[
                "microsoft.insights/",
                "microsoft.operationalinsights/",
                "microsoft.alertsmanagement/",
                "microsoft.portal/",
            ],
        ),
        (
            "security",
            &[
                "microsoft.keyvault/",
                "microsoft.security/",
                "microsoft.recoveryservices/",
                "microsoft.dataprotection/",
            ],
        ),
    ];
    prefixes
        .iter()
        .find_map(|(category, prefixes)| {
            prefixes
                .iter()
                .any(|prefix| resource_type.starts_with(prefix))
                .then_some(*category)
        })
        .unwrap_or("other")
}

pub fn select_metric_candidates(resources: &[AzureResource], limit: usize) -> Vec<AzureResource> {
    let mut by_type = BTreeMap::<String, Vec<AzureResource>>::new();
    for resource in resources {
        if !metric_adapter(&resource.resource_type).is_empty() {
            by_type
                .entry(resource.resource_type.to_ascii_lowercase())
                .or_default()
                .push(resource.clone());
        }
    }
    let mut selected = Vec::new();
    while selected.len() < limit && by_type.values().any(|values| !values.is_empty()) {
        for values in by_type.values_mut() {
            if let Some(resource) = (!values.is_empty()).then(|| values.remove(0)) {
                selected.push(resource);
                if selected.len() == limit {
                    break;
                }
            }
        }
    }
    let mut profile_watched = resources
        .iter()
        .filter(|resource| {
            resource.profile_watched && !metric_adapter(&resource.resource_type).is_empty()
        })
        .cloned()
        .collect::<Vec<_>>();
    profile_watched.sort_by_key(|resource| resource.name.to_ascii_lowercase());
    let mut session_starred = resources
        .iter()
        .filter(|resource| {
            resource.session_starred
                && !resource.profile_watched
                && !metric_adapter(&resource.resource_type).is_empty()
        })
        .cloned()
        .collect::<Vec<_>>();
    session_starred.sort_by_key(|resource| resource.name.to_ascii_lowercase());
    let mut legacy_watched = resources
        .iter()
        .filter(|resource| {
            resource.watched
                && !resource.profile_watched
                && !resource.session_starred
                && !metric_adapter(&resource.resource_type).is_empty()
        })
        .cloned()
        .collect::<Vec<_>>();
    legacy_watched.sort_by_key(|resource| resource.name.to_ascii_lowercase());
    let mut prioritized = profile_watched;
    prioritized.extend(session_starred);
    prioritized.extend(legacy_watched);
    for resource in selected {
        if !prioritized
            .iter()
            .any(|candidate| same_resource(candidate, &resource))
        {
            prioritized.push(resource);
        }
    }
    prioritized.truncate(limit);
    prioritized
}

pub fn apply_watchlist(resources: &mut [AzureResource], watchlist: &[WatchRule]) {
    for resource in resources {
        resource.profile_watched = false;
        resource.watch_alias.clear();
        resource.watch_expected_control.clear();
        let rule = watchlist.iter().find(|rule| {
            rule.name.eq_ignore_ascii_case(&resource.name)
                && (rule.resource_type.is_empty()
                    || rule
                        .resource_type
                        .eq_ignore_ascii_case(&resource.resource_type))
        });
        if let Some(rule) = rule {
            resource.profile_watched = true;
            resource.watch_alias.clone_from(&rule.alias);
            resource
                .watch_expected_control
                .clone_from(&rule.expect_control);
        }
        resource.refresh_watched();
    }
}

pub fn mark_metric_candidates(
    resources: &mut [AzureResource],
    limit: usize,
    metrics_enabled: bool,
) {
    let selected = if metrics_enabled {
        select_metric_candidates(resources, limit)
    } else {
        Vec::new()
    };
    for resource in resources {
        if metric_adapter(&resource.resource_type).is_empty() {
            resource.evidence_state = EvidenceState::InventoryOnly;
            resource.evidence_detail = "inventory metadata only; no fixed metric adapter".into();
        } else if selected
            .iter()
            .any(|candidate| same_resource(candidate, resource))
        {
            resource.evidence_state = EvidenceState::Pending;
            resource.evidence_detail = "fixed metric read scheduled".into();
        } else {
            resource.evidence_state = EvidenceState::NotSampled;
            resource.evidence_detail = if metrics_enabled {
                if resource.watched {
                    "watch priority exceeded the hard fleet metric cap; select for a focused read"
                        .into()
                } else {
                    "fixed metric adapter exists; outside provider-balanced cap".into()
                }
            } else {
                "fixed metric adapter exists; metrics disabled".into()
            };
        }
    }
}

pub fn build_relationships(resources: &mut [AzureResource]) {
    for resource in resources.iter_mut() {
        resource.relationships.clear();
    }
    let index = resources
        .iter()
        .map(|resource| (resource.resource_id.to_ascii_lowercase(), resource))
        .collect::<HashMap<_, _>>();
    let mut relations = Vec::new();
    for resource in resources.iter() {
        if !resource.hosting_plan_id.is_empty() {
            if let Some(plan) = index.get(&resource.hosting_plan_id.to_ascii_lowercase()) {
                relations.push((
                    resource.resource_id.clone(),
                    ResourceRelation {
                        kind: "app_service_plan".into(),
                        direction: "parent".into(),
                        resource_name: plan.name.clone(),
                        resource_type: plan.resource_type.clone(),
                    },
                ));
                relations.push((
                    plan.resource_id.clone(),
                    ResourceRelation {
                        kind: "hosts".into(),
                        direction: "dependent".into(),
                        resource_name: resource.name.clone(),
                        resource_type: resource.resource_type.clone(),
                    },
                ));
            }
        }
        if resource
            .resource_type
            .eq_ignore_ascii_case("microsoft.web/sites/slots")
        {
            let mut parts = resource.resource_id.rsplitn(3, '/');
            let _slot_name = parts.next();
            let _slots_segment = parts.next();
            if let Some(parent_id) = parts.next() {
                if let Some(parent) = index.get(&parent_id.to_ascii_lowercase()) {
                    relations.push((
                        resource.resource_id.clone(),
                        ResourceRelation {
                            kind: "slot_of".into(),
                            direction: "parent".into(),
                            resource_name: parent.name.clone(),
                            resource_type: parent.resource_type.clone(),
                        },
                    ));
                    relations.push((
                        parent.resource_id.clone(),
                        ResourceRelation {
                            kind: "slot".into(),
                            direction: "dependent".into(),
                            resource_name: resource.name.clone(),
                            resource_type: resource.resource_type.clone(),
                        },
                    ));
                }
            }
        }
    }
    drop(index);
    for (resource_id, relation) in relations {
        if let Some(resource) = resources
            .iter_mut()
            .find(|resource| resource.resource_id == resource_id)
        {
            resource.relationships.push(relation);
        }
    }
}

fn same_resource(left: &AzureResource, right: &AzureResource) -> bool {
    (!left.resource_id.is_empty()
        && !right.resource_id.is_empty()
        && left.resource_id.eq_ignore_ascii_case(&right.resource_id))
        || (left.name.eq_ignore_ascii_case(&right.name)
            && left
                .resource_type
                .eq_ignore_ascii_case(&right.resource_type))
}

pub fn select_diagnostic_candidates(
    resources: &[AzureResource],
    limit: usize,
) -> Vec<AzureResource> {
    let mut by_type = BTreeMap::<String, Vec<AzureResource>>::new();
    for resource in resources {
        let lowered = resource.resource_type.to_ascii_lowercase();
        if !metric_adapter(&lowered).is_empty()
            || matches!(
                lowered.as_str(),
                "microsoft.insights/components"
                    | "microsoft.operationalinsights/workspaces"
                    | "microsoft.app/containerapps"
            )
        {
            by_type.entry(lowered).or_default().push(resource.clone());
        }
    }
    let mut selected = Vec::new();
    while selected.len() < limit && by_type.values().any(|values| !values.is_empty()) {
        for values in by_type.values_mut() {
            if !values.is_empty() {
                selected.push(values.remove(0));
                if selected.len() == limit {
                    break;
                }
            }
        }
    }
    selected
}

fn parse_metrics(
    parsed: &mut BTreeMap<String, MetricSeries>,
    definitions: &[&MetricDef],
    rows: &[Value],
    aggregation: &str,
    query: &MetricQuery,
    interval_minutes: u64,
) {
    let start = chrono::DateTime::parse_from_rfc3339(&query.start_time)
        .ok()
        .map(|value| value.with_timezone(&Utc));
    let end = chrono::DateTime::parse_from_rfc3339(&query.end_time)
        .ok()
        .map(|value| value.with_timezone(&Utc));
    let step_seconds = (interval_minutes.max(1) * 60) as i64;
    let bucket_count = start
        .zip(end)
        .map(|(start, end)| {
            let duration = (end.timestamp() - start.timestamp()).max(1);
            ((duration + step_seconds - 1) / step_seconds) as usize
        })
        .unwrap_or_else(|| {
            ((query.window_hours.max(1) * 60) / interval_minutes.max(1)).max(1) as usize
        })
        .clamp(1, 1_440);
    let canonical_timestamps = start
        .map(|start| {
            (0..bucket_count)
                .map(|index| {
                    (start + ChronoDuration::seconds(step_seconds * index as i64))
                        .to_rfc3339_opts(SecondsFormat::Secs, true)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let by_name = rows
        .iter()
        .filter_map(|row| row["name"].as_str().map(|name| (name, row)))
        .collect::<HashMap<_, _>>();
    for definition in definitions {
        let row = by_name.get(definition.azure_name).copied();
        let unit = row
            .and_then(|row| row["unit"].as_str())
            .map(|value| clean_text(value, 32))
            .unwrap_or_default();
        let raw_points = row
            .and_then(|row| row["series"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|point| {
                let timestamp = point["timestamp"].as_str()?.to_string();
                let value = point[aggregation].as_f64();
                Some((timestamp, value))
            })
            .collect::<Vec<_>>();
        let mut grouped = vec![Vec::<f64>::new(); bucket_count];
        if let Some(start) = start {
            for (timestamp, value) in raw_points {
                let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(&timestamp) else {
                    continue;
                };
                let timestamp = timestamp.with_timezone(&Utc);
                if end.is_some_and(|end| timestamp >= end) {
                    continue;
                }
                let offset = timestamp.timestamp() - start.timestamp();
                if offset < 0 {
                    continue;
                }
                let index = (offset / step_seconds) as usize;
                if index < bucket_count {
                    if let Some(value) = value.filter(|value| value.is_finite()) {
                        grouped[index].push(value);
                    }
                }
            }
        }
        let values = grouped
            .into_iter()
            .map(|values| {
                (!values.is_empty()).then(|| match aggregation {
                    "total" | "count" => values.iter().sum(),
                    "maximum" => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                    "minimum" => values.iter().copied().fold(f64::INFINITY, f64::min),
                    _ => values.iter().sum::<f64>() / values.len() as f64,
                })
            })
            .collect::<Vec<_>>();
        let available = values.iter().flatten().count();
        let state = if available == 0 {
            "no_data"
        } else {
            "available"
        };
        parsed.insert(
            definition.public_name.into(),
            MetricSeries {
                name: definition.public_name.into(),
                unit,
                source: "Azure Monitor metrics".into(),
                window: query.window_label(),
                interval: if interval_minutes == 60 {
                    "1h".into()
                } else {
                    format!("{interval_minutes}m")
                },
                state: state.into(),
                detail: format!(
                    "aggregate across metric dimensions; {available}/{bucket_count} bins have data; no dimension values requested"
                ),
                timestamps: canonical_timestamps.clone(),
                values,
                aggregation: definition.aggregation.into(),
                query: query.clone(),
            },
        );
    }
}

pub fn apply_metric_health(resource: &mut AzureResource) {
    if resource.health_check_configured {
        resource.health_state = "unknown".into();
        resource.health_detail =
            "configured App Service Health Check has no fresh terminal-bin sample".into();
        if let Some(value) = resource
            .metrics
            .get("health_check_status")
            .and_then(|metric| {
                (metric.state == "available")
                    .then(|| metric.values.last().copied().flatten())
                    .flatten()
            })
        {
            resource.health_state = if value <= 0.0 {
                "unhealthy"
            } else if value >= 99.5 || (0.999..=1.0).contains(&value) {
                "healthy"
            } else {
                "degraded"
            }
            .into();
            resource.health_detail =
                "configured App Service Health Check aggregate; partial values are degraded".into();
        }
    }
}

pub fn merge_resources(target: &mut [AzureResource], updates: Vec<AzureResource>) {
    let updates = updates
        .into_iter()
        .map(|resource| (resource.resource_id.to_ascii_lowercase(), resource))
        .collect::<HashMap<_, _>>();
    for resource in target {
        if let Some(update) = updates.get(&resource.resource_id.to_ascii_lowercase()) {
            let current_read = resource
                .metrics
                .values()
                .map(|metric| metric.query.queried_at.as_str())
                .max()
                .unwrap_or("");
            let update_read = update
                .metrics
                .values()
                .map(|metric| metric.query.queried_at.as_str())
                .max()
                .unwrap_or("");
            if !current_read.is_empty() && (update_read.is_empty() || update_read < current_read) {
                continue;
            }
            resource.metrics.clone_from(&update.metrics);
            resource.health_state.clone_from(&update.health_state);
            resource.health_detail.clone_from(&update.health_detail);
            resource.evidence_state = update.evidence_state;
            resource.evidence_detail.clone_from(&update.evidence_detail);
        }
    }
}

pub fn merge_fleet_resources(target: &mut [AzureResource], updates: Vec<AzureResource>) {
    let updates = updates
        .into_iter()
        .map(|resource| (resource.resource_id.to_ascii_lowercase(), resource))
        .collect::<HashMap<_, _>>();
    for resource in target {
        if let Some(update) = updates.get(&resource.resource_id.to_ascii_lowercase()) {
            resource.fleet_metrics.clone_from(&update.metrics);
        }
    }
}

pub fn reconcile_inventory(current: &Snapshot, mut fresh: Snapshot) -> Snapshot {
    if current.selected_subscription_id != fresh.selected_subscription_id
        || current.selected_resource_group != fresh.selected_resource_group
    {
        return fresh;
    }
    for resource in &mut fresh.resources {
        if let Some(previous) = current
            .resources
            .iter()
            .find(|previous| same_resource(previous, resource))
        {
            resource.metrics.clone_from(&previous.metrics);
            resource.fleet_metrics.clone_from(&previous.fleet_metrics);
            resource.health_state.clone_from(&previous.health_state);
            resource.health_detail.clone_from(&previous.health_detail);
            resource
                .resource_health_state
                .clone_from(&previous.resource_health_state);
            resource
                .resource_health_reason
                .clone_from(&previous.resource_health_reason);
            resource
                .resource_health_observed_at
                .clone_from(&previous.resource_health_observed_at);
            resource
                .diagnostic_state
                .clone_from(&previous.diagnostic_state);
            resource
                .diagnostic_detail
                .clone_from(&previous.diagnostic_detail);
            resource.watched = previous.watched;
            resource.profile_watched = previous.profile_watched;
            resource.session_starred = previous.session_starred;
            resource.watch_alias.clone_from(&previous.watch_alias);
            resource
                .watch_expected_control
                .clone_from(&previous.watch_expected_control);
            if !previous.metrics.is_empty()
                || !matches!(
                    previous.evidence_state,
                    EvidenceState::Pending | EvidenceState::InventoryOnly
                )
            {
                resource.evidence_state = previous.evidence_state;
                resource
                    .evidence_detail
                    .clone_from(&previous.evidence_detail);
            }
        }
    }
    fresh.details.clone_from(&current.details);
    record_observed_transitions(current, &fresh.resources, &mut fresh.details.recent_changes);
    fresh.origin = "live".into();
    fresh.inventory_state = "current".into();
    fresh.enrichment_state = "updating".into();
    fresh.fleet_query.clone_from(&current.fleet_query);
    fresh.fleet_state.clone_from(&current.fleet_state);
    build_relationships(&mut fresh.resources);
    fresh
}

fn record_observed_transitions(
    current: &Snapshot,
    fresh_resources: &[AzureResource],
    recent_changes: &mut Vec<RecentChange>,
) {
    let observed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    for fresh in fresh_resources {
        let Some(previous) = current
            .resources
            .iter()
            .find(|previous| same_resource(previous, fresh))
        else {
            continue;
        };
        if known_value(&previous.version)
            && known_value(&fresh.version)
            && previous.version != fresh.version
        {
            recent_changes.push(RecentChange {
                timestamp: observed_at.clone(),
                resource_name: clean_text(&fresh.name, 120),
                resource_type: clean_text(&fresh.resource_type, 120),
                event: "VERSION".into(),
                detail: format!(
                    "{} → {}",
                    clean_text(&previous.version, 80),
                    clean_text(&fresh.version, 80)
                ),
                source: "aztop observation".into(),
            });
        }
        if known_value(&previous.control_state)
            && known_value(&fresh.control_state)
            && !previous
                .control_state
                .eq_ignore_ascii_case(&fresh.control_state)
        {
            recent_changes.push(RecentChange {
                timestamp: observed_at.clone(),
                resource_name: clean_text(&fresh.name, 120),
                resource_type: clean_text(&fresh.resource_type, 120),
                event: "STATE".into(),
                detail: format!(
                    "{} → {}",
                    clean_text(&previous.control_state, 40),
                    clean_text(&fresh.control_state, 40)
                ),
                source: "aztop observation".into(),
            });
        }
    }
    normalize_recent_changes(recent_changes);
}

fn known_value(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "unknown" | "unavailable" | "not_applicable" | "n/a" | "?"
        )
}

pub fn merge_enriched_snapshot(current: &Snapshot, mut enriched: Snapshot) -> Snapshot {
    if current.selected_subscription_id != enriched.selected_subscription_id
        || current.selected_resource_group != enriched.selected_resource_group
    {
        return enriched;
    }
    for resource in &mut enriched.resources {
        let Some(previous) = current
            .resources
            .iter()
            .find(|previous| same_resource(previous, resource))
        else {
            continue;
        };
        resource.metrics.clone_from(&previous.metrics);
        resource.fleet_metrics.clone_from(&previous.fleet_metrics);
        resource.evidence_state = previous.evidence_state;
        resource
            .evidence_detail
            .clone_from(&previous.evidence_detail);
        resource.watched = previous.watched;
        resource.profile_watched = previous.profile_watched;
        resource.session_starred = previous.session_starred;
        resource.watch_alias.clone_from(&previous.watch_alias);
        resource
            .watch_expected_control
            .clone_from(&previous.watch_expected_control);
    }
    enriched.origin = "live".into();
    enriched.inventory_state = "current".into();
    enriched.enrichment_state = "current".into();
    enriched.fleet_query.clone_from(&current.fleet_query);
    enriched.fleet_state.clone_from(&current.fleet_state);
    build_relationships(&mut enriched.resources);
    enriched
}

fn compact_version(value: &str) -> String {
    let value = value
        .split_once('|')
        .map_or(value, |(_, image)| image)
        .split('@')
        .next()
        .unwrap_or(value);
    clean_text(value.rsplit('/').next().unwrap_or(value), 80)
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn array(value: &Value) -> Result<&Vec<Value>, AzureError> {
    value
        .as_array()
        .ok_or_else(|| AzureError::new("Azure CLI returned an unexpected response"))
}

fn text(value: &Value, key: &str, default: &str, limit: usize) -> String {
    clean_text(value[key].as_str().unwrap_or(default), limit)
}

fn tabular_has_rows(value: &Value) -> bool {
    value.as_array().is_some_and(|rows| !rows.is_empty())
        || value["tables"]
            .as_array()
            .and_then(|tables| tables.first())
            .and_then(|table| table["rows"].as_array())
            .is_some_and(|rows| !rows.is_empty())
}

fn parse_change_points(value: &Value) -> Vec<ChangePoint> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let timestamp = row["timestamp"].as_str()?;
            Some(ChangePoint {
                timestamp: clean_text(timestamp, 60),
                change_type: text(row, "changeType", "unknown", 40),
                count: row["changeCount"].as_u64().unwrap_or(0),
            })
        })
        .filter(|point| point.count > 0)
        .collect()
}

fn parse_recent_changes(value: &Value) -> Vec<RecentChange> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let timestamp = clean_text(row["timestamp"].as_str()?, 60);
            let event = text(row, "changeType", "unknown", 40).to_ascii_uppercase();
            if timestamp.is_empty() || !matches!(event.as_str(), "CREATE" | "UPDATE" | "DELETE") {
                return None;
            }
            let resource_name = text(row, "resourceName", "unresolved resource", 120);
            let resource_type = text(row, "resourceType", "unresolved type", 120);
            Some(RecentChange {
                timestamp,
                resource_name: if resource_name.is_empty() {
                    "unresolved resource".into()
                } else {
                    resource_name
                },
                resource_type: if resource_type.is_empty() {
                    "unresolved type".into()
                } else {
                    resource_type
                },
                event,
                detail: "Azure metadata change observed; cause not inferred".into(),
                source: "Azure Resource Graph".into(),
            })
        })
        .take(MAX_RECENT_CHANGES)
        .collect()
}

fn merge_recent_changes(
    cloud_changes: Vec<RecentChange>,
    existing: Vec<RecentChange>,
) -> Vec<RecentChange> {
    let mut merged = cloud_changes;
    merged.extend(
        existing
            .into_iter()
            .filter(|change| change.source == "aztop observation"),
    );
    normalize_recent_changes(&mut merged);
    merged
}

fn normalize_recent_changes(changes: &mut Vec<RecentChange>) {
    let cutoff = Utc::now() - ChronoDuration::hours(24);
    changes.retain(|change| {
        DateTime::parse_from_rfc3339(&change.timestamp)
            .map(|timestamp| timestamp.with_timezone(&Utc) >= cutoff)
            .unwrap_or(false)
    });
    changes.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    changes.dedup_by(|left, right| {
        left.timestamp == right.timestamp
            && left
                .resource_name
                .eq_ignore_ascii_case(&right.resource_name)
            && left
                .resource_type
                .eq_ignore_ascii_case(&right.resource_type)
            && left.event == right.event
            && left.detail == right.detail
            && left.source == right.source
    });
    changes.truncate(MAX_RECENT_CHANGES);
}
fn graph_signal(name: &str, scope: &str, value: &Value) -> Signal {
    let rows = value.as_array().map(Vec::as_slice).unwrap_or_default();
    let (detail, state) = match name {
        "alert instances" => {
            let total: u64 = rows
                .iter()
                .map(|row| row["total"].as_u64().unwrap_or(0))
                .sum();
            let recent: u64 = rows
                .iter()
                .map(|row| row["recent24h"].as_u64().unwrap_or(0))
                .sum();
            (
                format!("{total} fired and non-closed; {recent} started in 24h"),
                if total == 0 { "no_data" } else { "warning" },
            )
        }
        "Azure Service Health" => {
            let active = rows
                .iter()
                .filter(|row| {
                    row["status"]
                        .as_str()
                        .is_some_and(|status| status.eq_ignore_ascii_case("active"))
                })
                .map(|row| row["eventCount"].as_u64().unwrap_or(0))
                .sum::<u64>();
            let active_maintenance = rows
                .iter()
                .filter(|row| {
                    row["eventType"]
                        .as_str()
                        .is_some_and(|kind| kind.to_ascii_lowercase().contains("maintenance"))
                        && row["status"]
                            .as_str()
                            .is_some_and(|status| status.eq_ignore_ascii_case("active"))
                })
                .map(|row| row["eventCount"].as_u64().unwrap_or(0))
                .sum::<u64>();
            (
                format!("{active} active issues; {active_maintenance} active maintenance events"),
                if active == 0 { "no_data" } else { "warning" },
            )
        }
        "alert-rule coverage" => {
            let rules: u64 = rows
                .iter()
                .map(|row| row["ruleCount"].as_u64().unwrap_or(0))
                .sum();
            let enabled: u64 = rows
                .iter()
                .map(|row| row["enabledCount"].as_u64().unwrap_or(0))
                .sum();
            let disabled = rules.saturating_sub(enabled);
            (
                format!("{enabled}/{rules} alert rules enabled; {disabled} disabled"),
                if rules == 0 {
                    "no_data"
                } else if disabled > 0 {
                    "warning"
                } else {
                    "available"
                },
            )
        }
        "Front Door endpoints" => {
            let total = rows
                .iter()
                .map(|row| row["total"].as_u64().unwrap_or(0))
                .sum::<u64>();
            let enabled = rows
                .iter()
                .map(|row| row["enabled"].as_u64().unwrap_or(0))
                .sum::<u64>();
            let failures = rows
                .iter()
                .map(|row| row["provisioningFailures"].as_u64().unwrap_or(0))
                .sum::<u64>();
            (
                format!(
                    "{enabled}/{total} endpoint resources enabled; {failures} provisioning failures; origins not inspected"
                ),
                if total == 0 {
                    "no_data"
                } else if enabled < total || failures > 0 {
                    "warning"
                } else {
                    "available"
                },
            )
        }
        "Azure Policy" => {
            let resources = rows
                .iter()
                .map(|row| row["nonCompliantResources"].as_u64().unwrap_or(0))
                .sum::<u64>();
            let policies = rows
                .iter()
                .map(|row| row["nonCompliantPolicies"].as_u64().unwrap_or(0))
                .sum::<u64>();
            (
                format!("{resources} noncompliant resources; {policies} noncompliant policies"),
                if resources > 0 {
                    "warning"
                } else {
                    "available"
                },
            )
        }
        _ => (
            format!("{} aggregate rows", rows.len()),
            if rows.is_empty() { "no_data" } else { "signal" },
        ),
    };
    Signal {
        name: name.into(),
        state: state.into(),
        detail,
        source: "Azure Resource Graph fixed aggregate".into(),
        window: "current".into(),
        scope: scope.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
        time::Duration,
    };

    fn executable_script(body: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fake-az");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        (directory, path)
    }

    #[test]
    fn all_provider_adapters_are_fixed() {
        assert_eq!(metric_adapter("Microsoft.Web/sites").len(), 5);
        assert_eq!(metric_adapter("Microsoft.Logic/workflows").len(), 5);
        assert!(metric_adapter("Microsoft.Unknown/value").is_empty());
    }

    #[test]
    fn category_model_is_generic() {
        assert_eq!(resource_category("Microsoft.Web/sites"), "compute/web");
        assert_eq!(resource_category("Microsoft.Search/searchServices"), "data");
        assert_eq!(resource_category("Contoso/widgets"), "other");
    }

    #[test]
    fn private_relationship_ids_do_not_serialize() {
        let rows = vec![json!({
            "id": "/subscriptions/secret/resourceGroups/rg/providers/Microsoft.Web/sites/app",
            "name": "app",
            "type": "Microsoft.Web/sites"
        })];
        let web = vec![json!({
            "id": "/subscriptions/secret/resourceGroups/rg/providers/Microsoft.Web/sites/app",
            "appServicePlanId": "/subscriptions/secret/resourceGroups/rg/providers/Microsoft.Web/serverfarms/plan"
        })];
        let resource = build_resources(&rows, &web).remove(0);
        assert!(resource.hosting_plan_id.ends_with("/serverfarms/plan"));
        let output = resource.public_json().to_string();
        assert!(!output.contains("/subscriptions/"));
        assert!(!output.contains("serverfarms"));
    }

    #[test]
    fn candidate_cap_is_provider_balanced() {
        let resources = ["Microsoft.Web/sites", "Microsoft.Storage/storageAccounts"]
            .into_iter()
            .flat_map(|resource_type| {
                (0..3).map(move |index| AzureResource {
                    name: format!("{resource_type}-{index}"),
                    resource_type: resource_type.into(),
                    resource_id: format!("{resource_type}/{index}"),
                    ..AzureResource::default()
                })
            })
            .collect::<Vec<_>>();
        let selected = select_metric_candidates(&resources, 4);
        assert_eq!(
            selected
                .iter()
                .filter(|resource| resource.resource_type.contains("Web"))
                .count(),
            2
        );
    }

    #[test]
    fn subscription_selection_uses_default_then_exact_name_or_id() {
        let subscriptions = vec![
            Subscription {
                name: "One".into(),
                subscription_id: "1".into(),
                ..Subscription::default()
            },
            Subscription {
                name: "Two".into(),
                subscription_id: "2".into(),
                is_default: true,
                ..Subscription::default()
            },
        ];
        assert_eq!(select_subscription(&subscriptions, "").unwrap().name, "Two");
        assert_eq!(
            select_subscription(&subscriptions, "one")
                .unwrap()
                .subscription_id,
            "1"
        );
        assert_eq!(
            select_subscription(&subscriptions, "2").unwrap().name,
            "Two"
        );
    }

    #[test]
    fn missing_subscription_is_not_silently_substituted() {
        let subscriptions = vec![Subscription {
            name: "One".into(),
            subscription_id: "1".into(),
            ..Subscription::default()
        }];
        assert!(select_subscription(&subscriptions, "missing").is_err());
        assert!(select_subscription(&[], "").is_err());
    }

    #[test]
    fn group_selection_is_explicit_and_alphabetical_default_compatible() {
        let groups = vec![
            ResourceGroup {
                name: "alpha".into(),
                ..ResourceGroup::default()
            },
            ResourceGroup {
                name: "staging".into(),
                ..ResourceGroup::default()
            },
        ];
        assert_eq!(select_group(&groups, "").unwrap().name, "alpha");
        assert_eq!(select_group(&groups, "STAGING").unwrap().name, "staging");
        assert!(select_group(&groups, "production").is_err());
    }

    #[test]
    fn web_inventory_merges_control_state_version_and_health_configuration() {
        let rows = vec![json!({
            "id": "/subscriptions/private/resourceGroups/rg/providers/Microsoft.Web/sites/app",
            "name": "app",
            "type": "Microsoft.Web/sites",
            "location": "usgovvirginia",
            "provisioningState": "Succeeded"
        })];
        let web = vec![json!({
            "id": "/subscriptions/private/resourceGroups/rg/providers/Microsoft.Web/sites/app",
            "state": "Running",
            "availabilityState": "Normal",
            "linuxFxVersion": "DOCKER|registry.example/example-api:v1",
            "healthCheckConfigured": true
        })];
        let resource = build_resources(&rows, &web).remove(0);
        assert_eq!(resource.control_state, "Running");
        assert_eq!(resource.availability_state, "Normal");
        assert_eq!(resource.version, "example-api:v1");
        assert!(resource.health_check_configured);
        assert_eq!(resource.health_state, "unknown");
    }

    #[test]
    fn provider_version_is_terminal_sanitized_and_bounded() {
        assert_eq!(
            compact_version("DOCKER|registry.example/path/\u{1b}[31mapp:v1\u{2066}"),
            "app:v1"
        );
        assert!(compact_version(&"x".repeat(200)).chars().count() <= 80);
    }

    #[test]
    fn metric_parser_combines_dimensions_by_azure_aggregation() {
        let definitions = [MetricDef {
            azure_name: "Requests",
            aggregation: "total",
            public_name: "requests",
        }];
        let rows = vec![json!({
            "name": "Requests",
            "unit": "\u{1b}[31mCount\u{2066}",
            "series": [
                {"timestamp":"2026-01-01T00:00:00Z","total":2.0},
                {"timestamp":"2026-01-01T00:00:00Z","total":3.0},
                {"timestamp":"2026-01-01T00:01:00Z","total":4.0}
            ]
        })];
        let mut parsed = BTreeMap::new();
        let query = MetricQuery {
            window_hours: 1,
            requested_interval_minutes: 1,
            start_time: "2026-01-01T00:00:00Z".into(),
            end_time: "2026-01-01T01:00:00Z".into(),
            queried_at: "2026-01-01T01:00:01Z".into(),
            cohort: "test".into(),
        };
        parse_metrics(&mut parsed, &[&definitions[0]], &rows, "total", &query, 1);
        assert_eq!(&parsed["requests"].values[..2], &[Some(5.0), Some(4.0)]);
        assert_eq!(parsed["requests"].values.len(), 60);
        assert_eq!(parsed["requests"].display_value(), Some(9.0));
        assert_eq!(parsed["requests"].unit, "Count");
    }

    #[test]
    fn metric_parser_preserves_no_data_without_zero_inference() {
        let definition = MetricDef {
            azure_name: "Requests",
            aggregation: "total",
            public_name: "requests",
        };
        let mut parsed = BTreeMap::new();
        let query = MetricQuery {
            window_hours: 1,
            requested_interval_minutes: 5,
            start_time: "2026-01-01T00:00:00Z".into(),
            end_time: "2026-01-01T01:00:00Z".into(),
            queried_at: "2026-01-01T01:00:01Z".into(),
            cohort: "test".into(),
        };
        parse_metrics(&mut parsed, &[&definition], &[], "total", &query, 5);
        assert_eq!(parsed["requests"].state, "no_data");
        assert_eq!(parsed["requests"].values, vec![None; 12]);
        assert_eq!(parsed["requests"].interval, "5m");
    }

    #[test]
    fn configured_health_metric_is_the_only_app_health_inference() {
        let mut resource = AzureResource {
            health_check_configured: true,
            health_state: "unknown".into(),
            ..AzureResource::default()
        };
        resource.metrics.insert(
            "health_check_status".into(),
            MetricSeries {
                state: "available".into(),
                values: vec![Some(1.0)],
                aggregation: "average".into(),
                ..MetricSeries::default()
            },
        );
        apply_metric_health(&mut resource);
        assert_eq!(resource.health_state, "healthy");
        resource
            .metrics
            .get_mut("health_check_status")
            .unwrap()
            .values = vec![Some(0.0)];
        apply_metric_health(&mut resource);
        assert_eq!(resource.health_state, "unhealthy");
        resource
            .metrics
            .get_mut("health_check_status")
            .unwrap()
            .values = vec![Some(100.0), None];
        apply_metric_health(&mut resource);
        assert_eq!(resource.health_state, "unknown");
        assert!(resource.health_detail.contains("no fresh terminal-bin"));
    }

    #[test]
    fn absent_health_check_does_not_manufacture_health() {
        let mut resource = AzureResource {
            health_check_configured: false,
            health_state: "unknown".into(),
            ..AzureResource::default()
        };
        resource.metrics.insert(
            "health_check_status".into(),
            MetricSeries {
                values: vec![Some(1.0)],
                ..MetricSeries::default()
            },
        );
        apply_metric_health(&mut resource);
        assert_eq!(resource.health_state, "unknown");
    }

    #[test]
    fn diagnostic_cap_is_provider_balanced() {
        let resources = [
            "Microsoft.Web/sites",
            "Microsoft.Insights/components",
            "Microsoft.Storage/storageAccounts",
        ]
        .into_iter()
        .flat_map(|resource_type| {
            (0..3).map(move |index| AzureResource {
                name: format!("{resource_type}-{index}"),
                resource_type: resource_type.into(),
                resource_id: format!("{resource_type}/{index}"),
                ..AzureResource::default()
            })
        })
        .collect::<Vec<_>>();
        let selected = select_diagnostic_candidates(&resources, 6);
        for resource_type in [
            "Microsoft.Web/sites",
            "Microsoft.Insights/components",
            "Microsoft.Storage/storageAccounts",
        ] {
            assert_eq!(
                selected
                    .iter()
                    .filter(|resource| resource.resource_type == resource_type)
                    .count(),
                2
            );
        }
    }

    #[tokio::test]
    async fn fixed_reads_scope_subscription_without_account_set() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let recorded = calls.clone();
        let cli = AzureCli::with_runner(move |args| {
            recorded.lock().unwrap().push(args);
            Box::pin(async { Ok(json!([])) })
        });
        cli.resource_groups("private-sub").await.unwrap();
        let calls = calls.lock().unwrap();
        assert!(calls[0]
            .windows(2)
            .any(|pair| pair == ["--subscription", "private-sub"]));
        assert_eq!(&calls[0][..2], ["graph", "query"]);
        assert!(calls[0]
            .iter()
            .any(|argument| argument == RESOURCE_GROUP_QUERY));
        assert!(!calls[0].windows(2).any(|pair| pair == ["account", "set"]));
    }

    #[tokio::test]
    async fn inventory_and_workspace_discovery_never_construct_full_object_list_commands() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let recorded = calls.clone();
        let cli = AzureCli::with_runner(move |args| {
            recorded.lock().unwrap().push(args);
            Box::pin(async { Ok(json!([])) })
        });
        cli.resource_groups("private").await.unwrap();
        cli.resources("private", "rg").await.unwrap();
        cli.workspaces("private", "rg").await.unwrap();
        let calls = calls.lock().unwrap();
        assert!(calls
            .iter()
            .all(|call| call.starts_with(&["graph".into(), "query".into()])));
        for forbidden in [
            ["group", "list"],
            ["resource", "list"],
            ["webapp", "list"],
            ["policy", "state"],
            ["afd", "endpoint"],
            ["afd", "origin"],
        ] {
            assert!(!calls
                .iter()
                .any(|call| call.starts_with(&forbidden.map(str::to_string))));
        }
        assert!(!calls.iter().any(|call| {
            call.starts_with(&[
                "monitor".into(),
                "log-analytics".into(),
                "workspace".into(),
                "list".into(),
            ])
        }));
    }

    #[tokio::test]
    async fn fixed_resource_graph_cap_never_silently_returns_partial_inventory() {
        let rows = (0..MAX_GRAPH_ROWS)
            .map(|index| json!({"id": format!("id-{index}")}))
            .collect::<Vec<_>>();
        let cli = AzureCli::with_runner(move |_| {
            let rows = rows.clone();
            Box::pin(async move { Ok(Value::Array(rows)) })
        });
        let error = cli.resources("private", "rg").await.unwrap_err();
        assert!(error.detail.contains("cap reached"));
        assert!(error.detail.contains("partial"));
    }

    #[tokio::test]
    async fn resolved_cold_start_does_not_repeat_account_or_group_discovery() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let recorded = calls.clone();
        let collector = Collector::new(
            Config::default(),
            AzureCli::with_runner(move |args| {
                recorded.lock().unwrap().push(args);
                Box::pin(async { Ok(json!([])) })
            }),
            false,
        );
        let selected = Subscription {
            subscription_id: "private".into(),
            name: "Gov".into(),
            is_default: true,
            cloud: "AzureUSGovernment".into(),
        };
        let group = ResourceGroup {
            name: "staging".into(),
            ..ResourceGroup::default()
        };
        let snapshot = collector
            .collect_resolved_inventory(
                vec![selected.clone()],
                selected,
                vec![group.clone()],
                group,
            )
            .await
            .unwrap();
        assert_eq!(snapshot.access_state, "available");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with(&["graph".into(), "query".into()]));
        assert!(calls[0]
            .iter()
            .any(|argument| argument.contains("project id, name, type")));
        assert!(!calls.iter().any(|call| {
            call.starts_with(&["account".into()])
                || call.starts_with(&["group".into(), "list".into()])
                || call.starts_with(&["resource".into(), "list".into()])
                || call.starts_with(&["webapp".into(), "list".into()])
        }));
    }

    #[tokio::test]
    async fn shared_cli_gate_bounds_child_process_fanout() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let runner_active = active.clone();
        let runner_maximum = maximum.clone();
        let cli = AzureCli::with_runner(move |_| {
            let active = runner_active.clone();
            let maximum = runner_maximum.clone();
            Box::pin(async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(15)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(json!([]))
            })
        })
        .with_max_concurrency(2);
        futures::future::join_all((0..8).map(|_| {
            let child = cli.child();
            async move { child.resource_groups("private").await.unwrap() }
        }))
        .await;
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn account_discovery_is_unscoped_and_projected() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let recorded = calls.clone();
        let cli = AzureCli::with_runner(move |args| {
            recorded.lock().unwrap().push(args.clone());
            Box::pin(async move {
                if args.starts_with(&["cloud".into(), "show".into()]) {
                    Ok(json!("AzureUSGovernment"))
                } else {
                    Ok(json!([{"id":"private","name":"Gov","isDefault":true}]))
                }
            })
        });
        let (cloud, subscriptions) = cli.subscriptions().await.unwrap();
        assert_eq!(cloud, "AzureUSGovernment");
        assert_eq!(subscriptions[0].name, "Gov");
        for call in calls.lock().unwrap().iter() {
            assert!(!call.iter().any(|argument| argument == "--subscription"));
        }
    }

    #[tokio::test]
    async fn metric_reader_retries_only_bounded_provider_grains() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let recorded = calls.clone();
        let cli = AzureCli::with_runner(move |args| {
            recorded.lock().unwrap().push(args.clone());
            Box::pin(async move {
                if args.iter().any(|argument| argument == "PT1M") {
                    Err(AzureError::new("unsupported time grain interval"))
                } else {
                    Ok(json!([]))
                }
            })
        });
        let resource = AzureResource {
            resource_type: "Microsoft.Storage/storageAccounts".into(),
            resource_id: "/subscriptions/private/resource".into(),
            ..AzureResource::default()
        };
        let query = metric_query(1, 1);
        let metrics = cli.metrics("private", &resource, &query).await.unwrap();
        assert_eq!(metrics["storage_used"].interval, "5m");
        let calls = calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|call| call.iter().any(|arg| arg == "PT1M")));
        assert!(calls
            .iter()
            .any(|call| call.iter().any(|arg| arg == "PT5M")));
    }

    #[tokio::test]
    async fn metric_reader_never_falls_back_to_a_finer_grain_and_remembers_success() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let recorded = calls.clone();
        let cli = AzureCli::with_runner(move |args| {
            recorded.lock().unwrap().push(args.clone());
            Box::pin(async move {
                if args.iter().any(|argument| argument == "PT15M") {
                    Err(AzureError::new("unsupported time grain interval"))
                } else {
                    Ok(json!([]))
                }
            })
        });
        let resource = AzureResource {
            resource_type: "Microsoft.Storage/storageAccounts".into(),
            resource_id: "/subscriptions/private/resource".into(),
            ..AzureResource::default()
        };
        let query = metric_query(24, 15);
        let first = cli.metrics("private", &resource, &query).await.unwrap();
        assert_eq!(first["storage_used"].interval, "1h");
        let first_call_count = calls.lock().unwrap().len();
        let second = cli.metrics("private", &resource, &query).await.unwrap();
        assert_eq!(second["storage_used"].interval, "1h");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), first_call_count + 1);
        assert!(!calls
            .iter()
            .any(|call| call.iter().any(|arg| arg == "PT1M" || arg == "PT5M")));
        assert!(calls
            .last()
            .is_some_and(|call| call.iter().any(|arg| arg == "PT1H")));
    }

    #[tokio::test]
    async fn metric_reader_batches_fixed_names_and_aggregations_in_one_cli_read() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let recorded = calls.clone();
        let cli = AzureCli::with_runner(move |args| {
            recorded.lock().unwrap().push(args);
            Box::pin(async { Ok(json!([])) })
        });
        let resource = AzureResource {
            resource_type: "Microsoft.Web/sites".into(),
            resource_id: "/subscriptions/private/resource".into(),
            ..AzureResource::default()
        };
        cli.metrics("private", &resource, &metric_query(1, 1))
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        for expected in [
            "Requests",
            "Http5xx",
            "AverageResponseTime",
            "MemoryWorkingSet",
            "total",
            "average",
        ] {
            assert!(call.iter().any(|argument| argument == expected));
        }
        assert!(call.iter().any(|argument| argument == "--start-time"));
        assert!(call.iter().any(|argument| argument == "--end-time"));
    }

    #[tokio::test]
    async fn application_insights_enrichment_has_a_hard_visible_cap() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let recorded = calls.clone();
        let cli = AzureCli::with_runner(move |args| {
            recorded.lock().unwrap().push(args.clone());
            Box::pin(async move { Ok(json!([])) })
        });
        let resources = (0..12)
            .map(|index| AzureResource {
                name: format!("component-{index:02}"),
                resource_type: "Microsoft.Insights/components".into(),
                resource_id: format!("/subscriptions/private/components/{index}"),
                telemetry_query_id: format!("00000000-0000-0000-0000-{index:012}"),
                watched: index == 11,
                ..AzureResource::default()
            })
            .collect::<Vec<_>>();
        let (signals, limitations, _, _, _) =
            cli.aggregate_signals("private", "rg", &resources, 4).await;
        let app_insights_calls = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| {
                call.starts_with(&["monitor".into(), "app-insights".into(), "query".into()])
            })
            .count();
        assert_eq!(app_insights_calls, MAX_TELEMETRY_COMPONENTS);
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| {
                call.starts_with(&["monitor".into(), "app-insights".into(), "query".into()])
            })
            .all(|call| call.iter().any(|argument| argument == "--apps")
                && !call.iter().any(|argument| argument == "--ids")));
        assert!(signals.iter().any(|signal| {
            signal.name == "Application Insights telemetry sampling"
                && signal.state == "limited"
                && signal.detail.contains("4 capped")
        }));
        assert!(limitations
            .iter()
            .any(|limitation| limitation.contains("4 components outside")));
    }

    #[test]
    fn metric_parser_uses_ceil_buckets_and_excludes_the_end_boundary() {
        let definition = MetricDef {
            azure_name: "Requests",
            aggregation: "total",
            public_name: "requests",
        };
        let rows = vec![json!({
            "name": "Requests",
            "unit": "Count",
            "series": [
                {"timestamp":"2026-01-01T01:00:00Z","total":2.0},
                {"timestamp":"2026-01-01T01:05:00Z","total":99.0}
            ]
        })];
        let query = MetricQuery {
            window_hours: 2,
            requested_interval_minutes: 60,
            start_time: "2026-01-01T00:00:00Z".into(),
            end_time: "2026-01-01T01:05:00Z".into(),
            queried_at: "2026-01-01T01:05:01Z".into(),
            cohort: "test".into(),
        };
        let mut parsed = BTreeMap::new();
        parse_metrics(&mut parsed, &[&definition], &rows, "total", &query, 60);
        assert_eq!(parsed["requests"].values, vec![None, Some(2.0)]);
        assert_eq!(parsed["requests"].timestamps.len(), 2);
    }

    #[test]
    fn allowlisted_queries_exclude_sensitive_surfaces() {
        let change_query = resource_change_query("safe-rg");
        let change_events = recent_change_events_query("safe-rg");
        let alert_instances = alert_instance_query("safe-rg");
        let alert_rules = alert_rule_query("safe-rg");
        let resource_health = resource_health_query("safe-rg");
        let inventory = resource_inventory_query("safe-rg");
        let mut projections = [
            ACCOUNT_QUERY.to_string(),
            RESOURCE_GROUP_QUERY.to_string(),
            inventory.clone(),
            workspace_inventory_query("safe-rg"),
            front_door_query("safe-rg"),
            METRIC_QUERY.to_string(),
            alert_instances.clone(),
            SERVICE_HEALTH_QUERY.to_string(),
            POLICY_QUERY.to_string(),
            alert_rules,
            resource_health,
            TELEMETRY_KQL.to_string(),
        ]
        .join(" ");
        projections.push_str(&change_query);
        projections.push_str(&change_events);
        let projections = projections.to_ascii_lowercase();
        for forbidden_field in [
            "appsettings",
            "publishing",
            "receiver",
            "message",
            "requestbody",
            "responsebody",
            "userid",
        ] {
            assert!(!projections.contains(forbidden_field));
        }
        assert!(!change_query.contains("changedBy"));
        assert!(!change_query.contains("targetResourceId"));
        assert!(change_query.contains("resourceGroup =~ \"safe-rg\""));
        assert!(change_query.ends_with("project timestamp, changeType, changeCount"));
        assert!(
            change_events.ends_with("project timestamp, changeType, resourceName, resourceType")
        );
        for forbidden_field in [
            "changedby",
            "clientrequestid",
            "correlationid",
            "subscriptionid",
            "targetresourceid,",
            "changes",
        ] {
            assert!(!change_events
                .to_ascii_lowercase()
                .ends_with(forbidden_field));
        }
        assert!(alert_instances.contains("condition =~ 'Fired'"));
        assert!(alert_instances.contains("state !~ 'Closed'"));
        assert!(inventory.contains("healthCheckConfigured = iff"));
        assert!(!inventory.contains("healthCheckPath ="));
    }

    #[test]
    fn resource_graph_scope_is_a_backslash_escaped_kusto_literal() {
        let group = r#"rg\evil" | project properties //"#;
        let literal = serde_json::to_string(group).unwrap();
        for query in [
            resource_change_query(group),
            recent_change_events_query(group),
            alert_instance_query(group),
            alert_rule_query(group),
            resource_health_query(group),
            resource_inventory_query(group),
            workspace_inventory_query(group),
            front_door_query(group),
        ] {
            assert!(query.contains(&literal));
            assert!(!query.contains("resourceGroup =~ 'rg"));
        }
    }

    #[test]
    fn alert_signal_describes_only_fired_non_closed_aggregates() {
        let signal = graph_signal(
            "alert instances",
            "resource_group",
            &json!([{"total": 7, "recent24h": 2}]),
        );
        assert_eq!(signal.state, "warning");
        assert_eq!(signal.detail, "7 fired and non-closed; 2 started in 24h");
    }

    #[test]
    fn merge_metrics_preserves_resource_identity_and_private_relationships() {
        let mut target = vec![AzureResource {
            name: "app".into(),
            resource_id: "id".into(),
            hosting_plan_id: "plan".into(),
            health_state: "unknown".into(),
            ..AzureResource::default()
        }];
        let update = AzureResource {
            resource_id: "ID".into(),
            health_state: "healthy".into(),
            metrics: BTreeMap::from([(
                "requests".into(),
                MetricSeries {
                    state: "available".into(),
                    ..MetricSeries::default()
                },
            )]),
            ..AzureResource::default()
        };
        merge_resources(&mut target, vec![update]);
        assert_eq!(target[0].name, "app");
        assert_eq!(target[0].hosting_plan_id, "plan");
        assert_eq!(target[0].health_state, "healthy");
        assert!(target[0].metrics.contains_key("requests"));
    }

    #[test]
    fn reconcile_inventory_never_reuses_metrics_across_subscription_ids() {
        let current = Snapshot {
            selected_subscription_id: "sub-a".into(),
            selected_subscription_name: "same display".into(),
            selected_resource_group: "rg".into(),
            resources: vec![AzureResource {
                resource_id: "id".into(),
                metrics: BTreeMap::from([(
                    "requests".into(),
                    MetricSeries {
                        state: "available".into(),
                        ..MetricSeries::default()
                    },
                )]),
                ..AzureResource::default()
            }],
            ..Snapshot::default()
        };
        let fresh = Snapshot {
            selected_subscription_id: "sub-b".into(),
            selected_subscription_name: "same display".into(),
            selected_resource_group: "rg".into(),
            resources: vec![AzureResource {
                resource_id: "id".into(),
                ..AzureResource::default()
            }],
            ..Snapshot::default()
        };
        let reconciled = reconcile_inventory(&current, fresh);
        assert!(reconciled.resources[0].metrics.is_empty());
    }

    #[test]
    fn watched_resources_are_pinned_inside_the_metric_cap() {
        let resources = (0..4)
            .map(|index| AzureResource {
                name: format!("app-{index}"),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id: format!("id-{index}"),
                watched: index == 3,
                ..AzureResource::default()
            })
            .collect::<Vec<_>>();
        let selected = select_metric_candidates(&resources, 2);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].name, "app-3");
    }

    #[test]
    fn candidate_marking_distinguishes_pending_cap_and_inventory_only() {
        let mut resources = vec![
            AzureResource {
                name: "watched".into(),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id: "watched".into(),
                watched: true,
                ..AzureResource::default()
            },
            AzureResource {
                name: "capped".into(),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id: "capped".into(),
                ..AzureResource::default()
            },
            AzureResource {
                name: "network".into(),
                resource_type: "Microsoft.Network/virtualNetworks".into(),
                resource_id: "network".into(),
                ..AzureResource::default()
            },
        ];
        mark_metric_candidates(&mut resources, 1, true);
        assert_eq!(resources[0].evidence_state, EvidenceState::Pending);
        assert_eq!(resources[1].evidence_state, EvidenceState::NotSampled);
        assert_eq!(resources[2].evidence_state, EvidenceState::InventoryOnly);
    }

    #[test]
    fn relationships_use_only_explicit_inventory_links() {
        let plan_id =
            "/subscriptions/private/resourceGroups/rg/providers/Microsoft.Web/serverFarms/plan";
        let mut resources = vec![
            AzureResource {
                name: "api".into(),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id:
                    "/subscriptions/private/resourceGroups/rg/providers/Microsoft.Web/sites/api"
                        .into(),
                hosting_plan_id: plan_id.into(),
                ..AzureResource::default()
            },
            AzureResource {
                name: "plan".into(),
                resource_type: "Microsoft.Web/serverFarms".into(),
                resource_id: plan_id.into(),
                ..AzureResource::default()
            },
        ];
        build_relationships(&mut resources);
        assert_eq!(resources[0].relationships[0].resource_name, "plan");
        assert_eq!(resources[0].relationships[0].direction, "parent");
        assert_eq!(resources[1].relationships[0].resource_name, "api");
        assert_eq!(resources[1].relationships[0].direction, "dependent");
        let public = resources[0].public_json().to_string();
        assert!(!public.contains("/subscriptions/"));
    }

    #[test]
    fn inventory_reconcile_preserves_evidence_and_removes_only_after_success() {
        let current = Snapshot {
            selected_subscription_name: "sub".into(),
            selected_resource_group: "rg".into(),
            resources: vec![
                AzureResource {
                    name: "api".into(),
                    resource_type: "Microsoft.Web/sites".into(),
                    resource_id: "old-api".into(),
                    evidence_state: EvidenceState::Signal,
                    watched: true,
                    watch_alias: "API".into(),
                    watch_expected_control: "running".into(),
                    metrics: BTreeMap::from([(
                        "requests".into(),
                        MetricSeries {
                            state: "available".into(),
                            timestamps: vec!["2026-07-28T00:00:00Z".into()],
                            values: vec![Some(4.0)],
                            ..MetricSeries::default()
                        },
                    )]),
                    ..AzureResource::default()
                },
                AzureResource {
                    name: "removed".into(),
                    resource_type: "Microsoft.Web/sites".into(),
                    resource_id: "removed".into(),
                    ..AzureResource::default()
                },
            ],
            ..Snapshot::default()
        };
        let fresh = Snapshot {
            selected_subscription_name: "sub".into(),
            selected_resource_group: "rg".into(),
            resources: vec![AzureResource {
                name: "api".into(),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id: "fresh-api".into(),
                ..AzureResource::default()
            }],
            ..Snapshot::default()
        };
        let reconciled = reconcile_inventory(&current, fresh);
        assert_eq!(reconciled.resources.len(), 1);
        assert_eq!(reconciled.resources[0].resource_id, "fresh-api");
        assert_eq!(
            reconciled.resources[0].metrics["requests"].latest(),
            Some(4.0)
        );
        assert_eq!(
            reconciled.resources[0].evidence_state,
            EvidenceState::Signal
        );
        assert!(reconciled.resources[0].watched);
        assert_eq!(reconciled.resources[0].watch_alias, "API");
        assert_eq!(reconciled.resources[0].watch_expected_control, "running");
    }

    #[test]
    fn enriched_merge_does_not_clobber_a_newer_focused_metric_result() {
        let metric = |timestamp: &str, value: f64| MetricSeries {
            state: "available".into(),
            timestamps: vec![timestamp.into()],
            values: vec![Some(value)],
            ..MetricSeries::default()
        };
        let current = Snapshot {
            selected_subscription_name: "sub".into(),
            selected_resource_group: "rg".into(),
            resources: vec![AzureResource {
                name: "api".into(),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id: "id".into(),
                metrics: BTreeMap::from([("requests".into(), metric("2026-07-28T00:02:00Z", 9.0))]),
                evidence_state: EvidenceState::Signal,
                ..AzureResource::default()
            }],
            ..Snapshot::default()
        };
        let enriched = Snapshot {
            selected_subscription_name: "sub".into(),
            selected_resource_group: "rg".into(),
            resources: vec![AzureResource {
                name: "api".into(),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id: "id".into(),
                metrics: BTreeMap::from([("requests".into(), metric("2026-07-28T00:01:00Z", 1.0))]),
                evidence_state: EvidenceState::Signal,
                ..AzureResource::default()
            }],
            ..Snapshot::default()
        };
        let merged = merge_enriched_snapshot(&current, enriched);
        assert_eq!(merged.resources[0].metrics["requests"].latest(), Some(9.0));
    }

    #[test]
    fn change_points_parse_only_projected_safe_aggregate_columns() {
        let points = parse_change_points(&json!([
            {
                "timestamp": "2026-07-28T00:05:00Z",
                "changeType": "Update",
                "changeCount": 3,
                "changedBy": "discarded"
            }
        ]));
        assert_eq!(
            points,
            vec![ChangePoint {
                timestamp: "2026-07-28T00:05:00Z".into(),
                change_type: "Update".into(),
                count: 3,
            }]
        );
    }

    #[test]
    fn recent_changes_parse_only_fixed_safe_columns_and_known_events() {
        let changes = parse_recent_changes(&json!([
            {
                "timestamp": "2026-07-29T00:05:00Z",
                "changeType": "Update",
                "resourceName": "api",
                "resourceType": "Microsoft.Web/sites",
                "targetResourceId": "/subscriptions/private/resourceGroups/private/providers/Microsoft.Web/sites/api",
                "changedBy": "discarded@example.test",
                "changes": {"secret": "discarded"}
            },
            {
                "timestamp": "2026-07-29T00:06:00Z",
                "changeType": "UnknownFutureEvent",
                "resourceName": "ignored"
            }
        ]));
        assert_eq!(
            changes,
            vec![RecentChange {
                timestamp: "2026-07-29T00:05:00Z".into(),
                resource_name: "api".into(),
                resource_type: "Microsoft.Web/sites".into(),
                event: "UPDATE".into(),
                detail: "Azure metadata change observed; cause not inferred".into(),
                source: "Azure Resource Graph".into(),
            }]
        );
        let serialized = serde_json::to_string(&changes).unwrap();
        assert!(!serialized.contains("/subscriptions/"));
        assert!(!serialized.contains("discarded"));
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn inventory_reconcile_records_known_version_and_state_transitions_without_ids() {
        let current = Snapshot {
            selected_subscription_id: "sub".into(),
            selected_resource_group: "rg".into(),
            resources: vec![AzureResource {
                name: "api".into(),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id: "/subscriptions/private/old".into(),
                version: "image:v1".into(),
                control_state: "Running".into(),
                ..AzureResource::default()
            }],
            ..Snapshot::default()
        };
        let fresh = Snapshot {
            selected_subscription_id: "sub".into(),
            selected_resource_group: "rg".into(),
            resources: vec![AzureResource {
                name: "api".into(),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id: "/subscriptions/private/old".into(),
                version: "image:v2".into(),
                control_state: "Stopped".into(),
                ..AzureResource::default()
            }],
            ..Snapshot::default()
        };
        let reconciled = reconcile_inventory(&current, fresh);
        assert_eq!(reconciled.details.recent_changes.len(), 2);
        assert!(reconciled
            .details
            .recent_changes
            .iter()
            .any(|change| change.event == "VERSION" && change.detail == "image:v1 → image:v2"));
        assert!(reconciled
            .details
            .recent_changes
            .iter()
            .any(|change| change.event == "STATE" && change.detail == "Running → Stopped"));
        let serialized = serde_json::to_string(&reconciled.details.recent_changes).unwrap();
        assert!(!serialized.contains("/subscriptions/"));
        assert!(reconciled
            .details
            .recent_changes
            .iter()
            .all(|change| change.source == "aztop observation"));
    }

    #[tokio::test]
    async fn process_timeout_is_bounded_and_explicit() {
        let (_directory, program) = executable_script("sleep 2\nprintf '{}'");
        let cli = AzureCli::with_program(program, Duration::from_millis(25));
        let error = cli
            .run_json(strings(&["cloud", "show"]), None)
            .await
            .unwrap_err();
        assert!(error.detail.contains("timed out"));
    }

    #[tokio::test]
    async fn cancellation_terminates_a_superseded_process_wait() {
        let (_directory, program) = executable_script("sleep 2\nprintf '{}'");
        let cli = AzureCli::with_program(program, Duration::from_secs(5));
        let worker = cli.clone();
        let task =
            tokio::spawn(async move { worker.run_json(strings(&["cloud", "show"]), None).await });
        tokio::time::sleep(Duration::from_millis(25)).await;
        cli.cancel();
        let error = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancelled process wait should finish")
            .unwrap()
            .unwrap_err();
        assert!(error.detail.contains("cancelled"));
    }

    #[tokio::test]
    async fn azure_children_disable_cli_file_logging_and_dynamic_install() {
        let (_directory, program) = executable_script(
            "printf '\"%s|%s|%s\"' \"$AZURE_LOGGING_ENABLE_LOG_FILE\" \"$AZURE_EXTENSION_USE_DYNAMIC_INSTALL\" \"$AZURE_CORE_COLLECT_TELEMETRY\"",
        );
        let cli = AzureCli::with_program(program, Duration::from_secs(1));
        let value = cli.run_json(Vec::new(), None).await.unwrap();
        assert_eq!(value, json!("false|no|false"));
    }

    #[tokio::test]
    async fn invalid_utf8_from_azure_cli_is_lossy_and_sanitized() {
        let (_directory, program) = executable_script("printf '\\377broken\\n' >&2\nexit 1");
        let cli = AzureCli::with_program(program, Duration::from_secs(1));
        let error = cli
            .run_json(strings(&["cloud", "show"]), None)
            .await
            .unwrap_err();
        assert!(error.detail.contains("broken"));
        assert!(error.detail.contains('\u{fffd}'));
        assert!(!error.detail.contains('\u{1b}'));
    }
}
