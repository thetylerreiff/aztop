use std::collections::BTreeMap;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub name: String,
    pub cloud: String,
    pub is_default: bool,
    #[serde(skip)]
    pub subscription_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceGroup {
    pub name: String,
    pub location: String,
    pub provisioning_state: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceState {
    Signal,
    NoData,
    Limited,
    Pending,
    NotSampled,
    #[default]
    InventoryOnly,
}

impl EvidenceState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Signal => "SIG",
            Self::NoData => "ND",
            Self::Limited => "LIM",
            Self::Pending => "PEND",
            Self::NotSampled => "CAP",
            Self::InventoryOnly => "INV",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Signal => "signal",
            Self::NoData => "no_data",
            Self::Limited => "limited",
            Self::Pending => "pending",
            Self::NotSampled => "not_sampled",
            Self::InventoryOnly => "inventory_only",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRelation {
    pub kind: String,
    pub direction: String,
    pub resource_name: String,
    pub resource_type: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricQuery {
    pub window_hours: u64,
    pub requested_interval_minutes: u64,
    pub start_time: String,
    pub end_time: String,
    pub queried_at: String,
    pub cohort: String,
}

impl MetricQuery {
    pub fn window_label(&self) -> String {
        format!("{}h", self.window_hours)
    }

    pub fn interval_label(&self) -> String {
        if self.requested_interval_minutes == 60 {
            "1h".into()
        } else {
            format!("{}m", self.requested_interval_minutes)
        }
    }

    pub fn matches(&self, window_hours: u64, interval_minutes: u64) -> bool {
        self.window_hours == window_hours
            && self.requested_interval_minutes == interval_minutes
            && !self.cohort.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricSeries {
    pub name: String,
    pub unit: String,
    pub source: String,
    pub window: String,
    pub interval: String,
    pub state: String,
    pub detail: String,
    pub timestamps: Vec<String>,
    pub values: Vec<Option<f64>>,
    pub aggregation: String,
    #[serde(default)]
    pub query: MetricQuery,
}

impl MetricSeries {
    pub fn samples(&self) -> impl Iterator<Item = f64> + '_ {
        self.values
            .iter()
            .flatten()
            .copied()
            .filter(|v| v.is_finite())
    }

    pub fn total(&self) -> Option<f64> {
        let values = self.samples().collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.into_iter().sum())
    }

    pub fn average(&self) -> Option<f64> {
        let values = self.samples().collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    }

    pub fn latest(&self) -> Option<f64> {
        self.values
            .iter()
            .rev()
            .flatten()
            .copied()
            .find(|v| v.is_finite())
    }

    pub fn latest_timestamp(&self) -> &str {
        self.timestamps
            .iter()
            .zip(&self.values)
            .rev()
            .find_map(|(timestamp, value)| value.map(|_| timestamp.as_str()))
            .unwrap_or("")
    }

    pub fn display_value(&self) -> Option<f64> {
        match self.aggregation.to_ascii_lowercase().as_str() {
            "total" | "count" => self.total(),
            "maximum" => self.samples().reduce(f64::max),
            "minimum" => self.samples().reduce(f64::min),
            _ => self.average(),
        }
    }

    pub fn public_json(&self) -> Value {
        json!({
            "name": self.name,
            "unit": self.unit,
            "aggregation": if self.aggregation.is_empty() { "unknown" } else { &self.aggregation },
            "source": self.source,
            "window": self.window,
            "interval": self.interval,
            "state": self.state,
            "detail": self.detail,
            "query": {
                "window_hours": self.query.window_hours,
                "requested_interval_minutes": self.query.requested_interval_minutes,
                "start_time": self.query.start_time,
                "end_time": self.query.end_time,
                "queried_at": self.query.queried_at,
                "cohort": self.query.cohort,
            },
            "points": self.timestamps.iter().zip(&self.values)
                .map(|(timestamp, value)| json!({"timestamp": timestamp, "value": value}))
                .collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AzureResource {
    pub name: String,
    pub resource_type: String,
    pub category: String,
    pub kind: String,
    pub location: String,
    pub control_state: String,
    pub availability_state: String,
    pub provisioning_state: String,
    pub version: String,
    pub changed_at: String,
    pub health_check_configured: bool,
    pub health_state: String,
    pub health_detail: String,
    pub metrics: BTreeMap<String, MetricSeries>,
    #[serde(default)]
    pub fleet_metrics: BTreeMap<String, MetricSeries>,
    pub resource_health_state: String,
    pub resource_health_reason: String,
    pub resource_health_observed_at: String,
    pub diagnostic_state: String,
    pub diagnostic_detail: String,
    pub evidence_state: EvidenceState,
    pub evidence_detail: String,
    #[serde(skip)]
    pub watched: bool,
    #[serde(skip)]
    pub profile_watched: bool,
    #[serde(skip)]
    pub session_starred: bool,
    #[serde(skip)]
    pub watch_alias: String,
    #[serde(skip)]
    pub watch_expected_control: String,
    pub relationships: Vec<ResourceRelation>,
    #[serde(skip)]
    pub resource_id: String,
    #[serde(skip)]
    pub hosting_plan_id: String,
    #[serde(skip)]
    pub telemetry_query_id: String,
}

impl AzureResource {
    pub fn refresh_watched(&mut self) {
        self.watched = self.profile_watched || self.session_starred;
    }

    pub fn type_label(&self) -> &str {
        self.resource_type
            .split_once('/')
            .map_or(self.resource_type.as_str(), |(_, label)| label)
    }

    pub fn signal_state(&self) -> &str {
        if !matches!(self.health_state.as_str(), "unknown" | "unsupported") {
            return &self.health_state;
        }
        if self.metrics.values().any(|m| m.state == "available") {
            "signal"
        } else if self.metrics.values().any(|m| m.state == "unavailable") {
            "unavailable"
        } else if self.metrics.values().any(|m| m.state == "no_data") {
            "no_data"
        } else {
            &self.health_state
        }
    }

    pub fn evidence_label(&self) -> &'static str {
        self.evidence_state.label()
    }

    pub fn public_json(&self) -> Value {
        let metrics = self
            .metrics
            .iter()
            .map(|(name, metric)| (name.clone(), metric.public_json()))
            .collect::<serde_json::Map<_, _>>();
        json!({
            "name": self.name,
            "type": self.resource_type,
            "category": self.category,
            "kind": self.kind,
            "location": self.location,
            "control_state": self.control_state,
            "availability_state": self.availability_state,
            "provisioning_state": self.provisioning_state,
            "version": self.version,
            "changed_at": self.changed_at,
            "health_check_configured": self.health_check_configured,
            "health_state": self.health_state,
            "health_detail": self.health_detail,
            "resource_health_state": self.resource_health_state,
            "resource_health_reason": self.resource_health_reason,
            "resource_health_observed_at": self.resource_health_observed_at,
            "diagnostic_state": self.diagnostic_state,
            "diagnostic_detail": self.diagnostic_detail,
            "signal_state": self.signal_state(),
            "evidence_state": self.evidence_state.as_str(),
            "evidence_detail": self.evidence_detail,
            "watched": self.watched,
            "watch_alias": self.watch_alias,
            "watch_expected_control": self.watch_expected_control,
            "relationships": self.relationships,
            "metrics": metrics,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub timestamp: String,
    pub status: String,
    pub operation: String,
    pub resource: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangePoint {
    pub timestamp: String,
    pub change_type: String,
    pub count: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentChange {
    pub timestamp: String,
    pub resource_name: String,
    pub resource_type: String,
    pub event: String,
    pub detail: String,
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    pub name: String,
    pub state: String,
    pub detail: String,
    pub source: String,
    pub window: String,
    #[serde(default)]
    pub scope: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogTableSignal {
    pub name: String,
    pub total: u64,
    pub errors: u64,
    pub warnings: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogSignalResult {
    pub resource_name: String,
    pub state: String,
    pub detail: String,
    pub source: String,
    pub window: String,
    pub interval: String,
    pub total: u64,
    pub errors: u64,
    pub warnings: u64,
    pub exceptions: u64,
    pub failed_dependencies: u64,
    pub last_seen: String,
    pub ingestion_lag_seconds: Option<f64>,
    pub timestamps: Vec<String>,
    pub counts: Vec<f64>,
    pub error_counts: Vec<f64>,
    pub warning_counts: Vec<f64>,
    pub tables: Vec<LogTableSignal>,
    pub queried_workspaces: usize,
    pub unavailable_workspaces: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupDetails {
    pub activity: Vec<ActivityEvent>,
    pub changes: Vec<ChangePoint>,
    #[serde(default)]
    pub recent_changes: Vec<RecentChange>,
    pub signals: Vec<Signal>,
    pub limitations: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub generated_at: String,
    pub subscriptions: Vec<Subscription>,
    pub selected_subscription_name: String,
    #[serde(skip)]
    pub selected_subscription_id: String,
    pub resource_groups: Vec<ResourceGroup>,
    pub selected_resource_group: String,
    pub access_state: String,
    pub access_detail: String,
    pub resources: Vec<AzureResource>,
    pub category_counts: BTreeMap<String, usize>,
    pub details: GroupDetails,
    pub metrics_enabled: bool,
    pub origin: String,
    pub cache_saved_at: String,
    pub inventory_state: String,
    pub enrichment_state: String,
    #[serde(default)]
    pub fleet_query: MetricQuery,
    #[serde(default)]
    pub fleet_state: String,
}

impl Snapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn now(
        subscriptions: Vec<Subscription>,
        selected_subscription_name: String,
        selected_subscription_id: String,
        resource_groups: Vec<ResourceGroup>,
        selected_resource_group: String,
        access_state: String,
        access_detail: String,
        resources: Vec<AzureResource>,
        details: GroupDetails,
        metrics_enabled: bool,
    ) -> Self {
        let mut category_counts = BTreeMap::new();
        for resource in &resources {
            *category_counts
                .entry(resource.category.clone())
                .or_default() += 1;
        }
        Self {
            generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            subscriptions,
            selected_subscription_name,
            selected_subscription_id,
            resource_groups,
            selected_resource_group,
            access_state,
            access_detail,
            resources,
            category_counts,
            details,
            metrics_enabled,
            origin: "live".into(),
            cache_saved_at: String::new(),
            inventory_state: "current".into(),
            enrichment_state: if metrics_enabled {
                "pending".into()
            } else {
                "disabled".into()
            },
            fleet_query: MetricQuery::default(),
            fleet_state: if metrics_enabled {
                "pending".into()
            } else {
                "disabled".into()
            },
        }
    }

    pub fn running_count(&self) -> usize {
        self.resources
            .iter()
            .filter(|r| r.control_state.eq_ignore_ascii_case("running"))
            .count()
    }

    pub fn stopped_count(&self) -> usize {
        self.resources
            .iter()
            .filter(|r| r.control_state.eq_ignore_ascii_case("stopped"))
            .count()
    }

    pub fn public_json(&self) -> Value {
        json!({
            "schema_version": 2,
            "generated_at": self.generated_at,
            "selected_subscription": self.selected_subscription_name,
            "selected_resource_group": self.selected_resource_group,
            "access_state": self.access_state,
            "access_detail": self.access_detail,
            "metrics_enabled": self.metrics_enabled,
            "origin": self.origin,
            "cache_saved_at": self.cache_saved_at,
            "inventory_state": self.inventory_state,
            "enrichment_state": self.enrichment_state,
            "fleet_state": self.fleet_state,
            "fleet_query": self.fleet_query,
            "subscriptions": self.subscriptions.iter().map(|subscription| json!({
                "name": subscription.name,
                "cloud": subscription.cloud,
                "is_default": subscription.is_default,
                "selected": subscription.subscription_id == self.selected_subscription_id,
            })).collect::<Vec<_>>(),
            "resource_groups": self.resource_groups.iter().map(|group| json!({
                "name": group.name,
                "location": group.location,
                "provisioning_state": group.provisioning_state,
                "selected": group.name == self.selected_resource_group,
            })).collect::<Vec<_>>(),
            "category_counts": self.category_counts,
            "resources": self.resources.iter().map(AzureResource::public_json).collect::<Vec<_>>(),
            "details": self.details,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(aggregation: &str) -> MetricSeries {
        MetricSeries {
            name: "requests".into(),
            unit: "Count".into(),
            source: "Azure Monitor metrics".into(),
            window: "1h".into(),
            interval: "1m".into(),
            state: "available".into(),
            detail: "aggregate".into(),
            timestamps: vec!["a".into(), "b".into(), "c".into()],
            values: vec![Some(1.0), None, Some(3.0)],
            aggregation: aggregation.into(),
            query: MetricQuery::default(),
        }
    }

    #[test]
    fn metric_total_average_and_latest_are_distinct() {
        let metric = metric("average");
        assert_eq!(metric.total(), Some(4.0));
        assert_eq!(metric.average(), Some(2.0));
        assert_eq!(metric.latest(), Some(3.0));
        assert_eq!(metric.latest_timestamp(), "c");
    }

    #[test]
    fn metric_display_honors_azure_aggregation() {
        assert_eq!(metric("total").display_value(), Some(4.0));
        assert_eq!(metric("count").display_value(), Some(4.0));
        assert_eq!(metric("maximum").display_value(), Some(3.0));
        assert_eq!(metric("minimum").display_value(), Some(1.0));
        assert_eq!(metric("average").display_value(), Some(2.0));
    }

    #[test]
    fn metric_public_json_preserves_missing_points() {
        let value = metric("total").public_json();
        assert_eq!(value["points"][1]["value"], Value::Null);
        assert_eq!(value["aggregation"], "total");
    }

    #[test]
    fn resource_signal_prefers_explicit_health() {
        let resource = AzureResource {
            health_state: "degraded".into(),
            ..AzureResource::default()
        };
        assert_eq!(resource.signal_state(), "degraded");
    }

    #[test]
    fn resource_signal_distinguishes_available_unavailable_and_no_data() {
        for (state, expected) in [
            ("available", "signal"),
            ("unavailable", "unavailable"),
            ("no_data", "no_data"),
        ] {
            let mut resource = AzureResource {
                health_state: "unknown".into(),
                ..AzureResource::default()
            };
            resource.metrics.insert(
                "metric".into(),
                MetricSeries {
                    state: state.into(),
                    ..MetricSeries::default()
                },
            );
            assert_eq!(resource.signal_state(), expected);
        }
    }

    #[test]
    fn resource_public_json_omits_all_private_ids() {
        let resource = AzureResource {
            resource_id: "/subscriptions/secret/resource".into(),
            hosting_plan_id: "/subscriptions/secret/plan".into(),
            telemetry_query_id: "private-app-query-id".into(),
            ..AzureResource::default()
        };
        let output = resource.public_json().to_string();
        assert!(!output.contains("secret"));
        assert!(!output.contains("hosting_plan"));
        assert!(!output.contains("resource_id"));
        assert!(!output.contains("private-app-query-id"));
    }

    #[test]
    fn snapshot_counts_control_plane_state_only() {
        let snapshot = Snapshot {
            resources: vec![
                AzureResource {
                    control_state: "Running".into(),
                    health_state: "unknown".into(),
                    ..AzureResource::default()
                },
                AzureResource {
                    control_state: "Stopped".into(),
                    health_state: "healthy".into(),
                    ..AzureResource::default()
                },
            ],
            ..Snapshot::default()
        };
        assert_eq!(snapshot.running_count(), 1);
        assert_eq!(snapshot.stopped_count(), 1);
    }

    #[test]
    fn snapshot_json_schema_and_selected_markers_are_stable() {
        let snapshot = Snapshot {
            selected_subscription_id: "private".into(),
            selected_subscription_name: "Gov".into(),
            selected_resource_group: "staging".into(),
            subscriptions: vec![Subscription {
                name: "Gov".into(),
                subscription_id: "private".into(),
                ..Subscription::default()
            }],
            resource_groups: vec![ResourceGroup {
                name: "staging".into(),
                ..ResourceGroup::default()
            }],
            ..Snapshot::default()
        };
        let value = snapshot.public_json();
        assert_eq!(value["schema_version"], 2);
        assert_eq!(value["subscriptions"][0]["selected"], true);
        assert_eq!(value["resource_groups"][0]["selected"], true);
        assert!(!value.to_string().contains("private"));
    }

    #[test]
    fn snapshot_now_builds_sorted_category_counts() {
        let snapshot = Snapshot::now(
            Vec::new(),
            String::new(),
            String::new(),
            Vec::new(),
            String::new(),
            "available".into(),
            String::new(),
            vec![
                AzureResource {
                    category: "storage".into(),
                    ..AzureResource::default()
                },
                AzureResource {
                    category: "compute/web".into(),
                    ..AzureResource::default()
                },
                AzureResource {
                    category: "storage".into(),
                    ..AzureResource::default()
                },
            ],
            GroupDetails::default(),
            true,
        );
        assert_eq!(snapshot.category_counts["storage"], 2);
        assert!(snapshot.generated_at.ends_with('Z'));
    }
}
