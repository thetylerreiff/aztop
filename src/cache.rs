use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    model::{MetricQuery, MetricSeries, Snapshot},
    sanitize::error_detail,
};

const CACHE_SCHEMA_VERSION: u64 = 2;
const MAX_CACHE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CACHE_SCOPES: usize = 8;
const MAX_JSON_DEPTH: usize = 16;
const MAX_COLLECTION_ITEMS: usize = 4096;
const MAX_STRING_BYTES: usize = 16 * 1024;
const MAX_METRICS_PER_RESOURCE: usize = 128;
const MAX_RELATIONS_PER_RESOURCE: usize = 256;
const MAX_METRIC_POINTS: usize = 4096;
const MAX_RECENT_CHANGES: usize = 20;
const CACHE_CLOCK_SKEW_SECONDS: i64 = 300;

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct CacheStore {
    path: Option<PathBuf>,
    ttl_seconds: u64,
    scope: Option<ScopeBinding>,
    enabled: bool,
}

#[derive(Clone, Debug)]
struct ScopeBinding {
    cloud: String,
    group: String,
    fingerprint: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheEnvelope {
    schema_version: u64,
    scope_fingerprint: String,
    saved_at: String,
    snapshot: Snapshot,
}

impl CacheStore {
    /// Constructs an unverified cache handle.
    ///
    /// A selector is not an authenticated subscription identity, so this
    /// compatibility constructor deliberately cannot read or write a cache.
    /// Call `new_verified` after Azure has resolved the cloud, subscription ID,
    /// and resource group.
    pub fn new(
        _subscription_selector: &str,
        _group_selector: &str,
        ttl_seconds: u64,
        enabled: bool,
    ) -> Self {
        Self {
            path: None,
            ttl_seconds,
            scope: None,
            enabled,
        }
    }

    pub fn new_verified(
        cloud: &str,
        subscription_id: &str,
        group: &str,
        ttl_seconds: u64,
        enabled: bool,
    ) -> Self {
        let scope = ScopeBinding::new(cloud, subscription_id, group);
        let path = (enabled)
            .then_some(scope.as_ref())
            .flatten()
            .and_then(|scope| {
                default_cache_dir()
                    .map(|directory| directory.join(format!("scope-{}.json", scope.fingerprint)))
            });
        Self {
            path,
            ttl_seconds,
            scope,
            enabled,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn matches_scope(
        &self,
        _subscription_name: &str,
        subscription_id: &str,
        group: &str,
    ) -> bool {
        let Some(scope) = &self.scope else {
            return false;
        };
        scope_fingerprint(&scope.cloud, subscription_id, group)
            .is_some_and(|fingerprint| fingerprint == scope.fingerprint)
    }

    /// Retargets using an authenticated subscription ID in the already-bound
    /// cloud. A subscription name is intentionally rejected as unverifiable.
    pub fn retarget(&self, subscription_id: &str, group: &str) -> Self {
        let Some(scope) = &self.scope else {
            return Self::new(subscription_id, group, self.ttl_seconds, self.enabled);
        };
        Self::new_verified(
            &scope.cloud,
            subscription_id,
            group,
            self.ttl_seconds,
            self.enabled,
        )
    }

    pub fn retarget_verified(&self, cloud: &str, subscription_id: &str, group: &str) -> Self {
        Self::new_verified(
            cloud,
            subscription_id,
            group,
            self.ttl_seconds,
            self.enabled,
        )
    }

    pub fn load(&self) -> Option<Snapshot> {
        if !self.enabled {
            return None;
        }
        let path = self.path.as_ref()?;
        let scope = self.scope.as_ref()?;
        let (mut file, metadata) = open_private_cache(path).ok()?;
        if metadata.len() > MAX_CACHE_BYTES {
            discard_cache(path);
            return None;
        }

        let mut data = Vec::with_capacity(metadata.len().min(MAX_CACHE_BYTES) as usize);
        if (&mut file)
            .take(MAX_CACHE_BYTES + 1)
            .read_to_end(&mut data)
            .is_err()
            || data.len() as u64 > MAX_CACHE_BYTES
        {
            discard_cache(path);
            return None;
        }

        let envelope = match serde_json::from_slice::<CacheEnvelope>(&data) {
            Ok(envelope) => envelope,
            Err(_) => {
                discard_cache(path);
                return None;
            }
        };
        if envelope.schema_version != CACHE_SCHEMA_VERSION
            || envelope.scope_fingerprint != scope.fingerprint
            || normalize_scope_component(&envelope.snapshot.selected_resource_group)
                != normalize_scope_component(&error_detail(&scope.group))
            || cache_age_seconds(&envelope.saved_at).is_none_or(|age| age > self.ttl_seconds)
            || envelope.snapshot.access_state != "available"
            || !snapshot_within_bounds(&envelope.snapshot)
        {
            discard_cache(path);
            return None;
        }

        // Cache bytes are untrusted on every read, even when this process wrote
        // them previously. Redact and strip terminal controls again before any
        // value can reach the renderer.
        let Some(mut snapshot) = sanitized_snapshot(&envelope.snapshot) else {
            discard_cache(path);
            return None;
        };
        if snapshot.access_state != "available" || !snapshot_within_bounds(&snapshot) {
            discard_cache(path);
            return None;
        }
        snapshot.origin = "cache".into();
        snapshot.cache_saved_at = envelope.saved_at;
        snapshot.inventory_state = "cached".into();
        snapshot.enrichment_state = "cached".into();
        restore_private_runtime_identity(&mut snapshot);
        Some(snapshot)
    }

    pub fn save(&self, snapshot: &Snapshot) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let (Some(path), Some(scope)) = (&self.path, &self.scope) else {
            // Unverified selectors must never create a cache entry.
            return Ok(());
        };
        if snapshot.access_state != "available" {
            return Ok(());
        }
        if !snapshot_matches_scope(snapshot, scope) {
            return Err(io::Error::other(
                "cache snapshot does not match authenticated scope",
            ));
        }

        let directory = path
            .parent()
            .ok_or_else(|| io::Error::other("cache path has no parent"))?;
        prepare_private_directory(directory)?;

        let saved_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let snapshot = sanitized_snapshot(snapshot)
            .ok_or_else(|| io::Error::other("cache snapshot could not be sanitized"))?;
        if !snapshot_within_bounds(&snapshot) {
            return Err(io::Error::other("cache snapshot exceeds structural limits"));
        }
        let envelope = CacheEnvelope {
            schema_version: CACHE_SCHEMA_VERSION,
            scope_fingerprint: scope.fingerprint.clone(),
            saved_at,
            snapshot,
        };
        let data = serde_json::to_vec(&envelope).map_err(io::Error::other)?;
        if data.len() as u64 > MAX_CACHE_BYTES {
            return Err(io::Error::other("sanitized cache exceeds 4 MiB limit"));
        }

        let temporary = write_private_temporary(path, &data)?;
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        restrict_file(path)?;
        sync_directory(directory)?;
        prune(directory, path)?;
        Ok(())
    }

    #[cfg(test)]
    fn at(path: PathBuf, ttl_seconds: u64) -> Self {
        Self::at_scope(
            path,
            "AzureUSGovernment",
            "12345678-1234-1234-1234-123456789abc",
            "staging",
            ttl_seconds,
        )
    }

    #[cfg(test)]
    fn at_scope(
        path: PathBuf,
        cloud: &str,
        subscription_id: &str,
        group: &str,
        ttl_seconds: u64,
    ) -> Self {
        Self {
            path: Some(path),
            ttl_seconds,
            scope: ScopeBinding::new(cloud, subscription_id, group),
            enabled: true,
        }
    }
}

impl ScopeBinding {
    fn new(cloud: &str, subscription_id: &str, group: &str) -> Option<Self> {
        Some(Self {
            cloud: normalize_required_component(cloud)?,
            group: normalize_required_component(group)?,
            fingerprint: scope_fingerprint(cloud, subscription_id, group)?,
        })
    }
}

fn snapshot_matches_scope(snapshot: &Snapshot, scope: &ScopeBinding) -> bool {
    if normalize_scope_component(&snapshot.selected_resource_group) != scope.group {
        return false;
    }
    let Some(subscription) = snapshot.subscriptions.iter().find(|subscription| {
        subscription
            .subscription_id
            .eq_ignore_ascii_case(&snapshot.selected_subscription_id)
    }) else {
        return false;
    };
    scope_fingerprint(
        &subscription.cloud,
        &snapshot.selected_subscription_id,
        &snapshot.selected_resource_group,
    )
    .is_some_and(|fingerprint| fingerprint == scope.fingerprint)
}

fn sanitized_snapshot(snapshot: &Snapshot) -> Option<Snapshot> {
    // Round-tripping through the serialized form makes this exhaustive for
    // current and future cache fields. `serde(skip)` fields (raw Azure IDs)
    // disappear, and every serialized string is redacted before reconstruction.
    let mut value = serde_json::to_value(snapshot).ok()?;
    sanitize_json_value(&mut value);
    serde_json::from_value(value).ok()
}

fn sanitize_json_value(value: &mut Value) {
    match value {
        Value::String(value) => *value = error_detail(&*value),
        Value::Array(values) => {
            for value in values {
                sanitize_json_value(value);
            }
        }
        Value::Object(values) => {
            let mut sanitized = serde_json::Map::with_capacity(values.len());
            for (key, mut value) in std::mem::take(values) {
                sanitize_json_value(&mut value);
                sanitized.insert(error_detail(&key), value);
            }
            *values = sanitized;
        }
        _ => {}
    }
}

fn restore_private_runtime_identity(snapshot: &mut Snapshot) {
    for (index, subscription) in snapshot.subscriptions.iter_mut().enumerate() {
        subscription.subscription_id = format!("cache-subscription-{index}");
        if subscription.name == snapshot.selected_subscription_name {
            snapshot
                .selected_subscription_id
                .clone_from(&subscription.subscription_id);
        }
    }
    for (index, resource) in snapshot.resources.iter_mut().enumerate() {
        resource.resource_id = format!(
            "cache:{}:{}:{index}",
            resource.resource_type.to_ascii_lowercase(),
            resource.name.to_ascii_lowercase()
        );
    }
}

fn snapshot_within_bounds(snapshot: &Snapshot) -> bool {
    if snapshot.resources.iter().any(|resource| {
        resource.metrics.len() > MAX_METRICS_PER_RESOURCE
            || resource.fleet_metrics.len() > MAX_METRICS_PER_RESOURCE
            || resource.relationships.len() > MAX_RELATIONS_PER_RESOURCE
            || resource
                .metrics
                .values()
                .chain(resource.fleet_metrics.values())
                .any(|metric| !metric_series_within_bounds(metric))
    }) || snapshot.details.recent_changes.len() > MAX_RECENT_CHANGES
        || !metric_query_within_bounds(&snapshot.fleet_query)
    {
        return false;
    }
    serde_json::to_value(snapshot)
        .ok()
        .is_some_and(|value| json_value_within_bounds(&value, 0))
}

fn metric_series_within_bounds(metric: &MetricSeries) -> bool {
    if metric.timestamps.len() > MAX_METRIC_POINTS
        || metric.values.len() > MAX_METRIC_POINTS
        || metric.timestamps.len() != metric.values.len()
        || metric
            .values
            .iter()
            .flatten()
            .any(|value| !value.is_finite())
        || !metric_query_within_bounds(&metric.query)
    {
        return false;
    }
    if metric.timestamps.is_empty() {
        return true;
    }
    let (Some(start), Some(end)) = (
        parse_query_time(&metric.query.start_time),
        parse_query_time(&metric.query.end_time),
    ) else {
        return false;
    };
    let maximum_points = ((end - start).num_minutes().max(0) as u64
        / metric.query.requested_interval_minutes.max(1))
    .max(1) as usize;
    metric.timestamps.len() <= maximum_points
        && metric.timestamps.iter().all(|timestamp| {
            parse_query_time(timestamp)
                .is_some_and(|timestamp| timestamp >= start && timestamp <= end)
        })
}

fn metric_query_within_bounds(query: &MetricQuery) -> bool {
    let empty = query.window_hours == 0
        && query.requested_interval_minutes == 0
        && query.start_time.is_empty()
        && query.end_time.is_empty()
        && query.queried_at.is_empty()
        && query.cohort.is_empty();
    if empty {
        return true;
    }
    if !(1..=24).contains(&query.window_hours)
        || !(1..=60).contains(&query.requested_interval_minutes)
    {
        return false;
    }
    let (Some(start), Some(end), Some(_queried_at)) = (
        parse_query_time(&query.start_time),
        parse_query_time(&query.end_time),
        parse_query_time(&query.queried_at),
    ) else {
        return false;
    };
    if end <= start || (end - start).num_seconds() != query.window_hours as i64 * 3_600 {
        return false;
    }
    query.cohort
        == format!(
            "{}h/{}m:{}..{}",
            query.window_hours, query.requested_interval_minutes, query.start_time, query.end_time
        )
}

fn parse_query_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn json_value_within_bounds(value: &Value, depth: usize) -> bool {
    if depth > MAX_JSON_DEPTH {
        return false;
    }
    match value {
        Value::String(value) => value.len() <= MAX_STRING_BYTES,
        Value::Array(values) => {
            values.len() <= MAX_COLLECTION_ITEMS
                && values
                    .iter()
                    .all(|value| json_value_within_bounds(value, depth + 1))
        }
        Value::Object(values) => {
            values.len() <= MAX_COLLECTION_ITEMS
                && values.iter().all(|(key, value)| {
                    key.len() <= MAX_STRING_BYTES && json_value_within_bounds(value, depth + 1)
                })
        }
        _ => true,
    }
}

fn cache_age_seconds(value: &str) -> Option<u64> {
    let timestamp = DateTime::parse_from_rfc3339(value)
        .ok()?
        .with_timezone(&Utc);
    let delta = Utc::now().signed_duration_since(timestamp).num_seconds();
    if delta < -CACHE_CLOCK_SKEW_SECONDS {
        return None;
    }
    Some(delta.max(0) as u64)
}

fn scope_fingerprint(cloud: &str, subscription_id: &str, group: &str) -> Option<String> {
    let cloud = normalize_required_component(cloud)?;
    let subscription_id = normalize_subscription_id(subscription_id)?;
    let group = normalize_required_component(group)?;
    let mut material = b"aztop-cache-scope-v2\0".to_vec();
    material.extend_from_slice(cloud.as_bytes());
    material.push(0);
    material.extend_from_slice(subscription_id.as_bytes());
    material.push(0);
    material.extend_from_slice(group.as_bytes());
    Some(hex(&sha256(&material)))
}

fn normalize_required_component(value: &str) -> Option<String> {
    let normalized = normalize_scope_component(value);
    (!normalized.is_empty()).then_some(normalized)
}

fn normalize_scope_component(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalize_subscription_id(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(['{', '}']).to_ascii_lowercase();
    if value.len() != 36 {
        return None;
    }
    for (index, byte) in value.bytes().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return None;
            }
        } else if !byte.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(value)
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut state = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut words = [0u32; 64];
        for (index, bytes) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(bytes.try_into().expect("four-byte chunk"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (target, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *target = target.wrapping_add(value);
        }
    }

    let mut digest = [0u8; 32];
    for (chunk, word) in digest.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn default_cache_dir() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(value).join("aztop"));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Caches/aztop"))
    } else {
        Some(home.join(".cache/aztop"))
    }
}

fn open_private_cache(path: &Path) -> io::Result<(File, Metadata)> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("cache path has no parent"))?;
    validate_private_directory(directory)?;

    let before = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&before)?;
    let mut options = OpenOptions::new();
    options.read(true);
    apply_no_follow(&mut options);
    let file = options.open(path)?;
    let after = file.metadata()?;
    validate_private_file_metadata(&after)?;
    ensure_same_file(&before, &after)?;
    Ok((file, after))
}

fn prepare_private_directory(path: &Path) -> io::Result<()> {
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(
            "cache directory is not a regular directory",
        ));
    }
    validate_owner(&metadata)?;
    restrict_directory(path)?;
    validate_private_directory(path)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(
            "cache directory is not a regular directory",
        ));
    }
    validate_owner(&metadata)?;
    validate_private_permissions(&metadata, true)
}

fn validate_private_file_metadata(metadata: &Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::other("cache is not a regular file"));
    }
    validate_owner(metadata)?;
    validate_private_permissions(metadata, false)
}

#[cfg(unix)]
fn validate_owner(metadata: &Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != effective_uid() {
        return Err(io::Error::other("cache is not owned by the current user"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_owner(_metadata: &Metadata) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_private_permissions(metadata: &Metadata, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode();
    if mode & 0o077 != 0 || mode & 0o7000 != 0 {
        return Err(io::Error::other("cache permissions are not private"));
    }
    if directory && mode & 0o700 != 0o700 {
        return Err(io::Error::other(
            "cache directory owner permissions are incomplete",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_permissions(_metadata: &Metadata, _directory: bool) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn ensure_same_file(before: &Metadata, after: &Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if before.dev() != after.dev() || before.ino() != after.ino() {
        return Err(io::Error::other("cache changed while it was opened"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_file(_before: &Metadata, _after: &Metadata) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid takes no arguments and has no preconditions on macOS or
    // Linux. uid_t is an unsigned 32-bit integer on both supported platforms.
    unsafe { geteuid() }
}

fn write_private_temporary(destination: &Path, data: &[u8]) -> io::Result<PathBuf> {
    let directory = destination
        .parent()
        .ok_or_else(|| io::Error::other("cache path has no parent"))?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("cache path has no UTF-8 file name"))?;

    for _ in 0..32 {
        let temporary = directory.join(format!("{file_name}.tmp-{:016x}", temporary_nonce()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        apply_no_follow(&mut options);
        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = file.write_all(data).and_then(|_| file.sync_all());
        drop(file);
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        return Ok(temporary);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique cache temporary file",
    ))
}

fn temporary_nonce() -> u64 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let counter = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let mut value = time ^ counter.rotate_left(19) ^ u64::from(std::process::id()).rotate_left(41);
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

#[cfg(target_os = "linux")]
fn apply_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    const O_NOFOLLOW: i32 = 0o400000;
    options.custom_flags(O_NOFOLLOW);
}

#[cfg(target_os = "macos")]
fn apply_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    const O_NOFOLLOW: i32 = 0x0000_0100;
    options.custom_flags(O_NOFOLLOW);
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn apply_no_follow(_options: &mut OpenOptions) {}

fn discard_cache(path: &Path) {
    let _ = fs::remove_file(path);
}

fn prune(directory: &Path, current: &Path) -> io::Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)?.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path == current || !name.starts_with("scope-") {
            continue;
        }
        if name.contains(".tmp-") {
            let _ = fs::remove_file(path);
        } else if name.ends_with(".json")
            && entry.file_type().is_ok_and(|file_type| file_type.is_file())
        {
            files.push(entry);
        }
    }
    files.sort_by_key(|entry| {
        fs::symlink_metadata(entry.path())
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    let remove = files.len().saturating_sub(MAX_CACHE_SCOPES - 1);
    for entry in files.into_iter().take(remove) {
        let _ = fs::remove_file(entry.path());
    }
    Ok(())
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AzureResource, MetricQuery, MetricSeries, RecentChange, ResourceRelation, Signal,
        Subscription,
    };

    const SUBSCRIPTION_A: &str = "12345678-1234-1234-1234-123456789abc";
    const SUBSCRIPTION_B: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const CLOUD: &str = "AzureUSGovernment";
    const GROUP: &str = "staging";

    fn snapshot_for(subscription_id: &str, subscription_name: &str) -> Snapshot {
        Snapshot {
            generated_at: Utc::now().to_rfc3339(),
            subscriptions: vec![Subscription {
                name: subscription_name.into(),
                cloud: CLOUD.into(),
                subscription_id: subscription_id.into(),
                is_default: true,
            }],
            selected_subscription_name: subscription_name.into(),
            selected_subscription_id: subscription_id.into(),
            selected_resource_group: GROUP.into(),
            access_state: "available".into(),
            resources: vec![AzureResource {
                name: "api".into(),
                resource_type: "Microsoft.Web/sites".into(),
                resource_id: format!(
                    "/subscriptions/{subscription_id}/resourceGroups/{GROUP}/providers/Microsoft.Web/sites/api"
                ),
                hosting_plan_id: format!(
                    "/subscriptions/{subscription_id}/resourceGroups/{GROUP}/providers/Microsoft.Web/serverFarms/plan"
                ),
                relationships: vec![ResourceRelation {
                    kind: "app_service_plan".into(),
                    direction: "parent".into(),
                    resource_name: "plan".into(),
                    resource_type: "Microsoft.Web/serverFarms".into(),
                }],
                ..AzureResource::default()
            }],
            origin: "live".into(),
            ..Snapshot::default()
        }
    }

    fn snapshot() -> Snapshot {
        snapshot_for(SUBSCRIPTION_A, "Gov")
    }

    fn metric_query() -> MetricQuery {
        MetricQuery {
            window_hours: 1,
            requested_interval_minutes: 30,
            start_time: "2026-07-29T00:00:00Z".into(),
            end_time: "2026-07-29T01:00:00Z".into(),
            queried_at: "2026-07-29T01:00:01Z".into(),
            cohort: "1h/30m:2026-07-29T00:00:00Z..2026-07-29T01:00:00Z".into(),
        }
    }

    fn write_fixture(path: &Path, data: &[u8]) {
        fs::write(path, data).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn raw_envelope(store: &CacheStore, snapshot: Snapshot) -> CacheEnvelope {
        CacheEnvelope {
            schema_version: CACHE_SCHEMA_VERSION,
            scope_fingerprint: store.scope.as_ref().unwrap().fingerprint.clone(),
            saved_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            snapshot,
        }
    }

    #[test]
    fn cache_round_trip_omits_private_ids_and_restores_runtime_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scope-test.json");
        let store = CacheStore::at(path.clone(), 60);
        store.save(&snapshot()).unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(!text.contains("/subscriptions/"));
        assert!(!text.contains(SUBSCRIPTION_A));
        let loaded = store.load().unwrap();
        assert_eq!(loaded.origin, "cache");
        assert!(loaded.resources[0].resource_id.starts_with("cache:"));
        assert_eq!(loaded.resources[0].relationships[0].resource_name, "plan");
    }

    #[test]
    fn cache_privacy_redacts_authorization_identifiers_in_every_string_surface() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scope-test.json");
        let store = CacheStore::at(path.clone(), 60);
        let mut snapshot = snapshot();
        let failure = concat!(
            "AuthorizationFailed: client 'admin@contoso.example' object id ",
            "'11111111-2222-3333-4444-555555555555' over scope ",
            "'/subscriptions/12345678-1234-1234-1234-123456789abc/",
            "resourceGroups/secret/providers/Microsoft.Web/sites/private'. ",
            "See https://portal.azure.us/private"
        );
        snapshot.access_detail = failure.into();
        snapshot.resources[0].health_detail = failure.into();
        snapshot.resources[0].name = format!("\u{1b}[31mapi\u{1b}[0m\u{202e} {failure}");
        snapshot.details.signals.push(Signal {
            name: "permission".into(),
            detail: failure.into(),
            ..Signal::default()
        });
        snapshot.details.recent_changes.push(RecentChange {
            timestamp: Utc::now().to_rfc3339(),
            resource_name: failure.into(),
            resource_type: "Microsoft.Web/sites".into(),
            event: "VERSION".into(),
            detail: failure.into(),
            source: "aztop observation".into(),
        });
        snapshot
            .category_counts
            .insert("admin@contoso.example".into(), 1);

        store.save(&snapshot).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        for private in [
            "admin@contoso.example",
            "11111111-2222",
            SUBSCRIPTION_A,
            "/subscriptions/",
            "secret/providers",
            "portal.azure.us",
            "\u{1b}",
            "\u{202e}",
        ] {
            assert!(
                !text.contains(private),
                "cache retained private input {private:?}"
            );
        }
    }

    #[test]
    fn two_subscriptions_with_the_same_group_have_distinct_scope_bindings() {
        let first = ScopeBinding::new(CLOUD, SUBSCRIPTION_A, GROUP).unwrap();
        let second = ScopeBinding::new(CLOUD, SUBSCRIPTION_B, GROUP).unwrap();
        assert_ne!(first.fingerprint, second.fingerprint);

        let directory = tempfile::tempdir().unwrap();
        let first_path = directory
            .path()
            .join(format!("scope-{}.json", first.fingerprint));
        let second_path = directory
            .path()
            .join(format!("scope-{}.json", second.fingerprint));
        let first_store = CacheStore::at_scope(first_path, CLOUD, SUBSCRIPTION_A, GROUP, 60);
        let second_store = CacheStore::at_scope(second_path, CLOUD, SUBSCRIPTION_B, GROUP, 60);
        first_store
            .save(&snapshot_for(SUBSCRIPTION_A, "First"))
            .unwrap();
        second_store
            .save(&snapshot_for(SUBSCRIPTION_B, "Second"))
            .unwrap();
        assert_eq!(
            first_store.load().unwrap().selected_subscription_name,
            "First"
        );
        assert_eq!(
            second_store.load().unwrap().selected_subscription_name,
            "Second"
        );
    }

    #[test]
    fn swapped_envelope_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("scope-first.json");
        let second_path = directory.path().join("scope-second.json");
        let first = CacheStore::at_scope(first_path.clone(), CLOUD, SUBSCRIPTION_A, GROUP, 60);
        let second = CacheStore::at_scope(second_path.clone(), CLOUD, SUBSCRIPTION_B, GROUP, 60);
        first.save(&snapshot_for(SUBSCRIPTION_A, "First")).unwrap();
        fs::copy(first_path, &second_path).unwrap();
        assert!(second.load().is_none());
    }

    #[test]
    fn implicit_or_unverified_subscription_cannot_load_or_save() {
        let store = CacheStore::new("", GROUP, 60, true);
        assert!(store.load().is_none());
        assert!(store.path.is_none());
        store.save(&snapshot()).unwrap();

        let named_selector = CacheStore::new("Gov", GROUP, 60, true);
        assert!(named_selector.load().is_none());
        assert!(named_selector.path.is_none());
    }

    #[test]
    fn cache_scope_matching_uses_resolved_subscription_id_and_group() {
        let store = CacheStore::at(
            tempfile::tempdir().unwrap().path().join("scope-test.json"),
            60,
        );
        assert!(store.matches_scope("arbitrary display name", SUBSCRIPTION_A, GROUP));
        assert!(!store.matches_scope("Gov", SUBSCRIPTION_B, GROUP));
        assert!(!store.matches_scope("Gov", SUBSCRIPTION_A, "production"));
    }

    #[test]
    fn successful_save_atomically_replaces_cache_and_prunes_abandoned_temps() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scope-test.json");
        let abandoned = directory.path().join("scope-old.json.tmp-999");
        fs::write(&abandoned, "partial").unwrap();
        let store = CacheStore::at(path, 60);
        store.save(&snapshot()).unwrap();
        let mut replacement = snapshot();
        replacement.resources[0].name = "replacement".into();
        store.save(&replacement).unwrap();
        assert_eq!(store.load().unwrap().resources[0].name, "replacement");
        assert!(!abandoned.exists());
    }

    #[test]
    fn corrupt_schema_wrong_scope_and_expired_cache_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scope-test.json");
        let store = CacheStore::at(path.clone(), 60);
        write_fixture(&path, b"{");
        assert!(store.load().is_none());
        assert!(!path.exists());

        let mut wrong_schema = raw_envelope(&store, snapshot());
        wrong_schema.schema_version += 1;
        write_fixture(&path, &serde_json::to_vec(&wrong_schema).unwrap());
        assert!(store.load().is_none());

        let mut wrong_scope = raw_envelope(&store, snapshot());
        wrong_scope.scope_fingerprint = scope_fingerprint(CLOUD, SUBSCRIPTION_B, GROUP).unwrap();
        write_fixture(&path, &serde_json::to_vec(&wrong_scope).unwrap());
        assert!(store.load().is_none());

        let mut expired = raw_envelope(&store, snapshot());
        expired.saved_at = "2020-01-01T00:00:00Z".into();
        write_fixture(&path, &serde_json::to_vec(&expired).unwrap());
        assert!(store.load().is_none());
    }

    #[test]
    fn oversized_cache_and_oversized_metric_series_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scope-test.json");
        let store = CacheStore::at(path.clone(), 60);
        write_fixture(&path, &vec![b'x'; MAX_CACHE_BYTES as usize + 1]);
        assert!(store.load().is_none());

        let mut hostile = snapshot();
        hostile.resources[0].metrics.insert(
            "requests".into(),
            MetricSeries {
                timestamps: vec!["2026-01-01T00:00:00Z".into(); MAX_METRIC_POINTS + 1],
                values: vec![Some(1.0); MAX_METRIC_POINTS + 1],
                ..MetricSeries::default()
            },
        );
        let envelope = raw_envelope(&store, hostile);
        write_fixture(&path, &serde_json::to_vec(&envelope).unwrap());
        assert!(store.load().is_none());

        let mut too_many_changes = snapshot();
        too_many_changes.details.recent_changes =
            vec![RecentChange::default(); MAX_RECENT_CHANGES + 1];
        let envelope = raw_envelope(&store, too_many_changes);
        write_fixture(&path, &serde_json::to_vec(&envelope).unwrap());
        assert!(store.load().is_none());
    }

    #[test]
    fn cached_metric_queries_require_bounded_parseable_points() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scope-test.json");
        let store = CacheStore::at(path.clone(), 60);
        let mut valid = snapshot();
        valid.resources[0].fleet_metrics.insert(
            "requests".into(),
            MetricSeries {
                timestamps: vec!["2026-07-29T00:00:00Z".into(), "2026-07-29T00:30:00Z".into()],
                values: vec![Some(1.0), Some(2.0)],
                query: metric_query(),
                ..MetricSeries::default()
            },
        );
        valid.fleet_query = metric_query();
        store.save(&valid).unwrap();
        assert_eq!(
            store.load().unwrap().resources[0].fleet_metrics["requests"]
                .values
                .len(),
            2
        );

        let mut invalid_timestamp = valid.clone();
        invalid_timestamp.resources[0]
            .fleet_metrics
            .get_mut("requests")
            .unwrap()
            .timestamps[1] = "not-a-timestamp".into();
        let envelope = raw_envelope(&store, invalid_timestamp);
        write_fixture(&path, &serde_json::to_vec(&envelope).unwrap());
        assert!(store.load().is_none());

        let mut too_many_points = valid;
        let metric = too_many_points.resources[0]
            .fleet_metrics
            .get_mut("requests")
            .unwrap();
        metric.timestamps.push("2026-07-29T01:00:00Z".into());
        metric.values.push(Some(4.0));
        let envelope = raw_envelope(&store, too_many_points);
        write_fixture(&path, &serde_json::to_vec(&envelope).unwrap());
        assert!(store.load().is_none());
    }

    #[test]
    fn loaded_strings_are_sanitized_again_after_deserialization() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scope-test.json");
        let store = CacheStore::at(path.clone(), 60);
        let mut hostile = snapshot();
        hostile.resources[0].name = concat!(
            "\u{1b}[31mapi\u{1b}[0m\u{202e} admin@contoso.example ",
            "11111111-2222-3333-4444-555555555555"
        )
        .into();
        let envelope = raw_envelope(&store, hostile);
        write_fixture(&path, &serde_json::to_vec(&envelope).unwrap());

        let loaded = store.load().unwrap();
        let name = &loaded.resources[0].name;
        assert!(name.starts_with("api"));
        assert!(!name.contains('\u{1b}'));
        assert!(!name.contains('\u{202e}'));
        assert!(!name.contains("contoso.example"));
        assert!(!name.contains("11111111"));
    }

    #[cfg(unix)]
    #[test]
    fn cache_permissions_are_private_and_permissive_files_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let cache_directory = directory.path().join("cache");
        let path = cache_directory.join("scope-test.json");
        let store = CacheStore::at(path.clone(), 60);
        store.save(&snapshot()).unwrap();
        assert_eq!(
            fs::metadata(&cache_directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::set_permissions(&cache_directory, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(store.load().is_none());
        fs::set_permissions(&cache_directory, fs::Permissions::from_mode(0o700)).unwrap();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load().is_none());
        assert!(
            path.exists(),
            "an insecure file must not be followed or removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_symlinks_and_non_regular_files_are_rejected() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        let link = directory.path().join("scope-link.json");
        let store = CacheStore::at(link.clone(), 60);
        write_fixture(&target, b"{}");
        symlink(&target, &link).unwrap();
        assert!(store.load().is_none());
        assert!(link.is_symlink());

        let non_regular = directory.path().join("scope-directory.json");
        fs::create_dir(&non_regular).unwrap();
        let store = CacheStore::at(non_regular, 60);
        assert!(store.load().is_none());
    }

    #[test]
    fn sha256_matches_published_empty_input_vector() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb924\
             27ae41e4649b934ca495991b7852b855"
        );
    }
}
