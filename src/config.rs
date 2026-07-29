use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
};

use serde::{
    de::{value::MapAccessDeserializer, MapAccess, Visitor},
    Deserialize, Serialize,
};
use thiserror::Error;

use crate::sanitize::is_unsafe_control;

pub const DEFAULT_CATEGORIES: [&str; 8] = [
    "compute/web",
    "data",
    "network/edge",
    "ai",
    "storage",
    "monitoring",
    "security",
    "other",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Config {
    pub subscription: String,
    pub resource_group: String,
    pub refresh_seconds: u64,
    pub metric_window_hours: u64,
    pub metric_interval_minutes: u64,
    pub max_workers: usize,
    pub max_metric_resources: usize,
    pub category_order: Vec<String>,
    pub watchlist: Vec<WatchRule>,
    pub cache_enabled: bool,
    pub cache_ttl_seconds: u64,
    #[serde(skip)]
    pub profile: ProfileMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileMetadata {
    pub active_path: Option<PathBuf>,
    pub watch_rule_count: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchRule {
    pub name: String,
    #[serde(default, rename = "type")]
    pub resource_type: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub expect_control: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            subscription: String::new(),
            resource_group: String::new(),
            refresh_seconds: 30,
            metric_window_hours: 1,
            metric_interval_minutes: 1,
            max_workers: 4,
            max_metric_resources: 16,
            category_order: DEFAULT_CATEGORIES.iter().map(|s| (*s).into()).collect(),
            watchlist: Vec::new(),
            cache_enabled: true,
            cache_ttl_seconds: 86_400,
            profile: ProfileMetadata::default(),
        }
    }
}

#[derive(Debug)]
enum RawWatchRule {
    Name(String),
    Rule(WatchRule),
}

impl<'de> Deserialize<'de> for RawWatchRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RawWatchRuleVisitor;

        impl<'de> Visitor<'de> for RawWatchRuleVisitor {
            type Value = RawWatchRule;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a resource name or watch-rule object")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawWatchRule::Name(value.to_string()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawWatchRule::Name(value))
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                WatchRule::deserialize(MapAccessDeserializer::new(map)).map(RawWatchRule::Rule)
            }
        }

        deserializer.deserialize_any(RawWatchRuleVisitor)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    subscription: Option<String>,
    resource_group: Option<String>,
    window: Option<String>,
    refresh_seconds: Option<u64>,
    metric_window_hours: Option<u64>,
    metric_interval_minutes: Option<u64>,
    max_workers: Option<usize>,
    max_metric_resources: Option<usize>,
    watchlist: Option<Vec<RawWatchRule>>,
    cache_enabled: Option<bool>,
    cache_ttl_seconds: Option<u64>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file does not exist: {0}")]
    Missing(String),
    #[error("could not read config {0}: {1}")]
    Read(String, String),
    #[error("invalid JSON config {0}: {1}")]
    Json(String, String),
    #[error("invalid TOML config {0}: {1}")]
    Toml(String, String),
    #[error("unsupported config format for {0}; use .toml or .json")]
    Format(String),
    #[error("{0}")]
    Invalid(String),
}

pub fn resolve_config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    let xdg_config_home = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    resolve_config_path_from(
        explicit,
        xdg_config_home.as_deref(),
        home.as_deref(),
        cfg!(target_os = "macos"),
    )
}

fn resolve_config_path_from(
    explicit: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
    macos: bool,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }

    let mut candidates = Vec::new();
    if let Some(directory) = home {
        candidates.push(directory.join(".aztop/config.toml"));
    }
    if let Some(directory) = xdg_config_home {
        candidates.push(directory.join("aztop/config.toml"));
        candidates.push(directory.join("aztop/config.json"));
    }
    if let Some(directory) = home {
        if macos {
            candidates.push(directory.join("Library/Application Support/aztop/config.toml"));
            candidates.push(directory.join("Library/Application Support/aztop/config.json"));
        }
        candidates.push(directory.join(".config/aztop.json"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

pub fn load_config(path: Option<&Path>) -> Result<Config, ConfigError> {
    let raw = match path {
        None => RawConfig::default(),
        Some(path) => {
            let label = path.display().to_string();
            let data = fs::read_to_string(path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    ConfigError::Missing(label.clone())
                } else {
                    ConfigError::Read(label.clone(), error.to_string())
                }
            })?;
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("toml") => toml::from_str(&data)
                    .map_err(|error| ConfigError::Toml(label, error.to_string()))?,
                Some("json") => serde_json::from_str(&data)
                    .map_err(|error| ConfigError::Json(label, error.to_string()))?,
                _ => return Err(ConfigError::Format(label)),
            }
        }
    };
    let mut config = Config::default();
    if let Some(value) = raw.subscription {
        config.subscription = clean_selector(value, "subscription")?;
    }
    if let Some(value) = raw.resource_group {
        config.resource_group = clean_selector(value, "resource_group")?;
    }
    if let Some(value) = raw.refresh_seconds {
        config.refresh_seconds = if value == 0 {
            0
        } else {
            bounded(value, "refresh_seconds", 10, 3600)?
        };
    }
    if let Some(value) = raw.window {
        if raw.metric_window_hours.is_some() || raw.metric_interval_minutes.is_some() {
            return Err(ConfigError::Invalid(
                "window cannot be combined with metric_window_hours or metric_interval_minutes"
                    .into(),
            ));
        }
        (config.metric_window_hours, config.metric_interval_minutes) = parse_window(&value)?;
    } else {
        if let Some(value) = raw.metric_window_hours {
            config.metric_window_hours = bounded(value, "metric_window_hours", 1, 24)?;
        }
        if let Some(value) = raw.metric_interval_minutes {
            config.metric_interval_minutes = bounded(value, "metric_interval_minutes", 1, 60)?;
        }
    }
    if let Some(value) = raw.max_workers {
        config.max_workers = bounded(value as u64, "max_workers", 1, 6)? as usize;
    }
    if let Some(value) = raw.max_metric_resources {
        config.max_metric_resources =
            bounded(value as u64, "max_metric_resources", 1, 24)? as usize;
    }
    if let Some(value) = raw.watchlist {
        config.watchlist = validate_watchlist(value)?;
    }
    if let Some(value) = raw.cache_enabled {
        config.cache_enabled = value;
    }
    if let Some(value) = raw.cache_ttl_seconds {
        config.cache_ttl_seconds = bounded(value, "cache_ttl_seconds", 60, 604_800)?;
    }
    config.profile.active_path = path.map(Path::to_path_buf);
    config.profile.watch_rule_count = config.watchlist.len();
    Ok(config)
}

fn validate_watchlist(rules: Vec<RawWatchRule>) -> Result<Vec<WatchRule>, ConfigError> {
    let mut validated = Vec::<WatchRule>::with_capacity(rules.len());
    for raw in rules {
        let rule = validate_watch_rule(match raw {
            RawWatchRule::Name(name) => WatchRule {
                name,
                ..WatchRule::default()
            },
            RawWatchRule::Rule(rule) => rule,
        })?;

        if let Some(existing) = validated.iter().find(|existing| {
            overlapping_watch_selectors(existing, &rule) && !same_watch_directives(existing, &rule)
        }) {
            return Err(conflicting_watch_rules(existing, &rule));
        }
        if validated
            .iter()
            .any(|existing| same_watch_selector(existing, &rule))
        {
            continue;
        }
        if validated.iter().any(|existing| {
            existing.name.eq_ignore_ascii_case(&rule.name) && existing.resource_type.is_empty()
        }) {
            continue;
        }
        if rule.resource_type.is_empty() {
            validated.retain(|existing| !existing.name.eq_ignore_ascii_case(&rule.name));
        }
        validated.push(rule);
    }
    Ok(validated)
}

fn same_watch_selector(left: &WatchRule, right: &WatchRule) -> bool {
    left.name.eq_ignore_ascii_case(&right.name)
        && left
            .resource_type
            .eq_ignore_ascii_case(&right.resource_type)
}

fn overlapping_watch_selectors(left: &WatchRule, right: &WatchRule) -> bool {
    left.name.eq_ignore_ascii_case(&right.name)
        && (left.resource_type.is_empty()
            || right.resource_type.is_empty()
            || left
                .resource_type
                .eq_ignore_ascii_case(&right.resource_type))
}

fn same_watch_directives(left: &WatchRule, right: &WatchRule) -> bool {
    left.alias == right.alias && left.expect_control == right.expect_control
}

fn conflicting_watch_rules(left: &WatchRule, right: &WatchRule) -> ConfigError {
    let left_type = if left.resource_type.is_empty() {
        "*"
    } else {
        &left.resource_type
    };
    let right_type = if right.resource_type.is_empty() {
        "*"
    } else {
        &right.resource_type
    };
    ConfigError::Invalid(format!(
        "watchlist contains conflicting rules for {} ({left_type} and {right_type})",
        left.name
    ))
}

fn validate_watch_rule(mut rule: WatchRule) -> Result<WatchRule, ConfigError> {
    rule.name = clean_selector(rule.name, "watchlist.name")?;
    if rule.name.is_empty() {
        return Err(ConfigError::Invalid(
            "watchlist.name must not be empty".into(),
        ));
    }
    rule.resource_type = clean_selector(rule.resource_type, "watchlist.type")?;
    rule.alias = clean_selector(rule.alias, "watchlist.alias")?;
    if rule.alias.chars().count() > 12 {
        return Err(ConfigError::Invalid(
            "watchlist.alias must be at most 12 characters".into(),
        ));
    }
    rule.expect_control =
        clean_selector(rule.expect_control, "watchlist.expect_control")?.to_ascii_lowercase();
    if !matches!(rule.expect_control.as_str(), "" | "running" | "stopped") {
        return Err(ConfigError::Invalid(
            "watchlist.expect_control must be running or stopped".into(),
        ));
    }
    Ok(rule)
}

fn clean_selector(value: String, label: &str) -> Result<String, ConfigError> {
    let value = value.trim().to_string();
    if value.chars().any(is_unsafe_control) {
        return Err(ConfigError::Invalid(format!(
            "{label} contains unsafe control or direction characters"
        )));
    }
    Ok(value)
}

fn parse_window(value: &str) -> Result<(u64, u64), ConfigError> {
    let value = clean_selector(value.to_string(), "window")?.to_ascii_lowercase();
    let Some((hours, interval)) = value.split_once('/') else {
        return Err(invalid_window());
    };
    let Some(hours) = hours.trim().strip_suffix('h') else {
        return Err(invalid_window());
    };
    let Some(interval) = interval.trim().strip_suffix('m') else {
        return Err(invalid_window());
    };
    let hours = hours.parse::<u64>().map_err(|_| invalid_window())?;
    let interval = interval.parse::<u64>().map_err(|_| invalid_window())?;
    Ok((
        bounded(hours, "window hours", 1, 24)?,
        bounded(interval, "window interval", 1, 60)?,
    ))
}

fn invalid_window() -> ConfigError {
    ConfigError::Invalid(
        "window must use <hours>h/<minutes>m, for example 1h/1m, 6h/5m, or 24h/15m".into(),
    )
}

fn bounded(value: u64, label: &str, minimum: u64, maximum: u64) -> Result<u64, ConfigError> {
    if !(minimum..=maximum).contains(&value) {
        return Err(ConfigError::Invalid(format!(
            "{label} must be between {minimum} and {maximum}"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_generic_and_bounded() {
        let config = load_config(None).unwrap();
        assert!(config.subscription.is_empty());
        assert!(config.resource_group.is_empty());
        assert_eq!(
            (config.refresh_seconds, config.metric_window_hours),
            (30, 1)
        );
        assert_eq!(config.metric_interval_minutes, 1);
        assert_eq!(config.profile, ProfileMetadata::default());
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(clean_selector("bad\u{1b}".into(), "subscription").is_err());
        assert!(bounded(0, "metric_window_hours", 1, 24).is_err());
    }

    #[test]
    fn profile_fields_load_without_product_specific_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        fs::write(
            &path,
            r#"{"subscription":"Gov","resource_group":"staging","refresh_seconds":45,"metric_window_hours":6,"metric_interval_minutes":5,"max_workers":2,"max_metric_resources":8,"watchlist":[{"name":"api","type":"microsoft.web/sites","alias":"API","expect_control":"running"}],"cache_enabled":true,"cache_ttl_seconds":3600}"#,
        )
        .unwrap();
        let config = load_config(Some(&path)).unwrap();
        assert_eq!(config.subscription, "Gov");
        assert_eq!(config.resource_group, "staging");
        assert_eq!(
            (
                config.refresh_seconds,
                config.metric_window_hours,
                config.metric_interval_minutes,
            ),
            (45, 6, 5)
        );
        assert_eq!((config.max_workers, config.max_metric_resources), (2, 8));
        assert_eq!(
            config.watchlist,
            vec![WatchRule {
                name: "api".into(),
                resource_type: "microsoft.web/sites".into(),
                alias: "API".into(),
                expect_control: "running".into(),
            }]
        );
        assert!(config.cache_enabled);
        assert_eq!(config.cache_ttl_seconds, 3600);
        assert_eq!(config.profile.active_path.as_deref(), Some(path.as_path()));
        assert_eq!(config.profile.watch_rule_count, 1);
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(!serialized.contains("profile.json"));
        assert!(!serialized.contains("\"profile\""));
    }

    #[test]
    fn toml_profile_sets_default_scope_and_window() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
subscription = "Example Subscription"
resource_group = "example-staging"
window = "6h/5m"
refresh_seconds = 45
"#,
        )
        .unwrap();

        let config = load_config(Some(&path)).unwrap();
        assert_eq!(config.subscription, "Example Subscription");
        assert_eq!(config.resource_group, "example-staging");
        assert_eq!(
            (
                config.metric_window_hours,
                config.metric_interval_minutes,
                config.refresh_seconds,
            ),
            (6, 5, 45)
        );
        assert_eq!(config.profile.active_path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn window_is_validated_and_cannot_conflict_with_numeric_fields() {
        assert_eq!(parse_window("24H / 15M").unwrap(), (24, 15));
        assert!(parse_window("6 hours").is_err());
        assert!(parse_window("25h/5m").is_err());
        assert!(parse_window("6h/0m").is_err());

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "window = \"6h/5m\"\nmetric_window_hours = 6\n").unwrap();
        assert!(matches!(
            load_config(Some(&path)),
            Err(ConfigError::Invalid(message)) if message.contains("cannot be combined")
        ));
    }

    #[test]
    fn zero_refresh_disables_automatic_reads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        fs::write(&path, r#"{"refresh_seconds":0}"#).unwrap();
        assert_eq!(load_config(Some(&path)).unwrap().refresh_seconds, 0);
    }

    #[test]
    fn unknown_profile_fields_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        fs::write(&path, r#"{"future_option":true}"#).unwrap();
        assert!(matches!(
            load_config(Some(&path)),
            Err(ConfigError::Json(_, message)) if message.contains("future_option")
        ));

        fs::write(
            &path,
            r#"{"watchlist":[{"name":"api","watch_alis":"API"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            load_config(Some(&path)),
            Err(ConfigError::Json(_, message)) if message.contains("watch_alis")
        ));

        let toml_path = directory.path().join("config.toml");
        fs::write(&toml_path, "future_option = true\n").unwrap();
        assert!(matches!(
            load_config(Some(&toml_path)),
            Err(ConfigError::Toml(_, message)) if message.contains("future_option")
        ));
    }

    #[test]
    fn missing_and_malformed_profiles_are_explicit() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.json");
        assert!(matches!(
            load_config(Some(&missing)),
            Err(ConfigError::Missing(_))
        ));
        let malformed = directory.path().join("bad.json");
        fs::write(&malformed, "{").unwrap();
        assert!(matches!(
            load_config(Some(&malformed)),
            Err(ConfigError::Json(_, _))
        ));

        let malformed_toml = directory.path().join("bad.toml");
        fs::write(&malformed_toml, "window = [").unwrap();
        assert!(matches!(
            load_config(Some(&malformed_toml)),
            Err(ConfigError::Toml(_, _))
        ));

        let unsupported = directory.path().join("config.yaml");
        fs::write(&unsupported, "window: 1h/1m").unwrap();
        assert!(matches!(
            load_config(Some(&unsupported)),
            Err(ConfigError::Format(_))
        ));
    }

    #[test]
    fn category_order_is_fixed_and_generic() {
        let config = Config::default();
        assert_eq!(config.category_order.len(), 8);
        assert_eq!(config.category_order[0], "compute/web");
        assert_eq!(config.category_order[7], "other");
    }

    #[test]
    fn watchlist_accepts_shorthand_and_rejects_ambiguous_expectations() {
        let directory = tempfile::tempdir().unwrap();
        let shorthand = directory.path().join("shorthand.json");
        fs::write(&shorthand, r#"{"watchlist":["api"]}"#).unwrap();
        assert_eq!(
            load_config(Some(&shorthand)).unwrap().watchlist[0].name,
            "api"
        );

        let invalid = directory.path().join("invalid.json");
        fs::write(
            &invalid,
            r#"{"watchlist":[{"name":"api","expect_control":"healthy"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            load_config(Some(&invalid)),
            Err(ConfigError::Invalid(_))
        ));

        fs::write(
            &invalid,
            "{\"watchlist\":[{\"name\":\"api\",\"alias\":\"safe\\u202Espoof\"}]}",
        )
        .unwrap();
        assert!(matches!(
            load_config(Some(&invalid)),
            Err(ConfigError::Invalid(message)) if message.contains("unsafe control")
        ));
        fs::write(
            &invalid,
            "{\"watchlist\":[{\"name\":\"api\",\"alias\":\"safe\\u206Fspoof\"}]}",
        )
        .unwrap();
        assert!(matches!(
            load_config(Some(&invalid)),
            Err(ConfigError::Invalid(message)) if message.contains("unsafe control")
        ));
    }

    #[test]
    fn config_discovery_honors_precedence_and_explicit_override() {
        let directory = tempfile::tempdir().unwrap();
        let xdg = directory.path().join("xdg");
        let home = directory.path().join("home");
        let home_toml = home.join(".aztop/config.toml");
        let xdg_toml = xdg.join("aztop/config.toml");
        let xdg_json = xdg.join("aztop/config.json");
        let macos_toml = home.join("Library/Application Support/aztop/config.toml");
        let macos_json = home.join("Library/Application Support/aztop/config.json");
        let home_json = home.join(".config/aztop.json");
        for path in [
            &home_toml,
            &xdg_toml,
            &xdg_json,
            &macos_toml,
            &macos_json,
            &home_json,
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "{}").unwrap();
        }

        let explicit = directory.path().join("explicit.json");
        assert_eq!(
            resolve_config_path_from(Some(&explicit), Some(&xdg), Some(&home), true),
            Some(explicit)
        );
        assert_eq!(
            resolve_config_path_from(None, Some(&xdg), Some(&home), true),
            Some(home_toml.clone())
        );
        fs::remove_file(&home_toml).unwrap();
        assert_eq!(
            resolve_config_path_from(None, Some(&xdg), Some(&home), true),
            Some(xdg_toml.clone())
        );
        fs::remove_file(&xdg_toml).unwrap();
        assert_eq!(
            resolve_config_path_from(None, Some(&xdg), Some(&home), true),
            Some(xdg_json)
        );

        let no_xdg_home = directory.path().join("no-xdg-home");
        let no_xdg_macos = no_xdg_home.join("Library/Application Support/aztop/config.toml");
        let no_xdg_home_json = no_xdg_home.join(".config/aztop.json");
        for path in [&no_xdg_macos, &no_xdg_home_json] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "{}").unwrap();
        }
        assert_eq!(
            resolve_config_path_from(None, None, Some(&no_xdg_home), true),
            Some(no_xdg_macos)
        );
        assert_eq!(
            resolve_config_path_from(None, None, Some(&no_xdg_home), false),
            Some(no_xdg_home_json)
        );
    }

    #[test]
    fn absent_discovered_config_uses_generic_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let xdg = directory.path().join("xdg");
        let home = directory.path().join("home");
        assert_eq!(
            resolve_config_path_from(None, Some(&xdg), Some(&home), true),
            None
        );
        assert_eq!(load_config(None).unwrap(), Config::default());
    }

    #[test]
    fn duplicate_watch_rules_are_collapsed_and_conflicts_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        fs::write(
            &path,
            r#"{"watchlist":[
                {"name":"Api","type":"Microsoft.Web/sites","alias":"API","expect_control":"RUNNING"},
                {"name":"api","type":"microsoft.web/SITES","alias":"API","expect_control":"running"}
            ]}"#,
        )
        .unwrap();
        let config = load_config(Some(&path)).unwrap();
        assert_eq!(config.watchlist.len(), 1);
        assert_eq!(config.profile.watch_rule_count, 1);

        fs::write(
            &path,
            r#"{"watchlist":[
                {"name":"api","type":"microsoft.web/sites","alias":"API"},
                {"name":"API","type":"Microsoft.Web/sites","alias":"OTHER"}
            ]}"#,
        )
        .unwrap();
        assert!(matches!(
            load_config(Some(&path)),
            Err(ConfigError::Invalid(message)) if message.contains("conflicting rules")
        ));
    }

    #[test]
    fn overlapping_watch_rules_are_order_independent_or_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        fs::write(
            &path,
            r#"{"watchlist":[
                {"name":"api","type":"microsoft.web/sites","alias":"API"},
                {"name":"API","alias":"API"}
            ]}"#,
        )
        .unwrap();
        let config = load_config(Some(&path)).unwrap();
        assert_eq!(
            config.watchlist,
            vec![WatchRule {
                name: "API".into(),
                alias: "API".into(),
                ..WatchRule::default()
            }]
        );

        fs::write(
            &path,
            r#"{"watchlist":[
                {"name":"api","alias":"ALL"},
                {"name":"api","type":"microsoft.web/sites","alias":"WEB"}
            ]}"#,
        )
        .unwrap();
        assert!(matches!(
            load_config(Some(&path)),
            Err(ConfigError::Invalid(message)) if message.contains("conflicting rules")
        ));

        fs::write(
            &path,
            r#"{"watchlist":[
                {"name":"api","type":"microsoft.web/sites","alias":"WEB"},
                {"name":"api","type":"microsoft.dbforpostgresql/flexibleservers","alias":"DB"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(load_config(Some(&path)).unwrap().watchlist.len(), 2);
    }
}
