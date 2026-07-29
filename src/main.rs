use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::ExitCode,
};

use aztop::{
    app::App,
    azure::{apply_watchlist, select_group, select_subscription, AzureCli, Collector},
    cache::CacheStore,
    config::{load_config, resolve_config_path},
    render::render_table,
    VERSION,
};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "aztop",
    version = VERSION,
    about = "Read-only btop-style terminal viewer for Azure resources."
)]
struct Arguments {
    /// Local TOML or JSON profile; overrides standard user profile discovery.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Subscription name or ID; does not change the Azure CLI default.
    #[arg(long, value_name = "NAME_OR_ID")]
    subscription: Option<String>,
    /// Resource group to display.
    #[arg(long, value_name = "NAME")]
    resource_group: Option<String>,
    /// Print an accessible text snapshot.
    #[arg(long, conflicts_with = "json")]
    table: bool,
    /// Print sanitized JSON.
    #[arg(long, conflicts_with = "table")]
    json: bool,
    /// Enable fixed, bounded metrics and summarize-only telemetry adapters.
    #[arg(long)]
    metrics: bool,
    /// TUI focused-resource refresh interval; 0 disables automatic reads.
    #[arg(long, value_name = "SECONDS")]
    watch: Option<u64>,
    /// Disable ANSI colors.
    #[arg(long)]
    no_color: bool,
    /// Disable the private local progressive-startup cache.
    #[arg(long)]
    no_cache: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<u8, String> {
    let arguments = Arguments::parse();
    if arguments
        .watch
        .is_some_and(|value| value != 0 && value < 10)
    {
        return Err("--watch must be 0 or at least 10 seconds".into());
    }
    let explicit_config_path = arguments.config.as_deref().map(expand_home);
    let config_path = resolve_config_path(explicit_config_path.as_deref());
    let mut config = load_config(config_path.as_deref()).map_err(|error| error.to_string())?;
    let interactive = !arguments.table
        && !arguments.json
        && io::stdin().is_terminal()
        && io::stdout().is_terminal();
    let metrics_enabled = arguments.metrics || interactive;
    let subscription = arguments
        .subscription
        .as_deref()
        .unwrap_or(&config.subscription)
        .to_string();
    let group = arguments
        .resource_group
        .as_deref()
        .unwrap_or(&config.resource_group)
        .to_string();
    config.subscription.clone_from(&subscription);
    config.resource_group.clone_from(&group);
    if let Some(watch) = arguments.watch {
        if watch != 0 {
            config.refresh_seconds = watch;
        }
    }
    let mut collector = Collector::new(config.clone(), AzureCli::new(45), metrics_enabled);
    if interactive {
        let cache_enabled = config.cache_enabled && !arguments.no_cache;
        let (mut snapshot, cache, startup_scope) = if cache_enabled {
            // Resolve the authenticated identity before deriving or reading a scope cache.
            // A blank selector means "current default" and is not a stable cache identity.
            let (cloud, subscriptions) = collector
                .azure
                .subscriptions()
                .await
                .map_err(|error| error.detail)?;
            let resolved_subscription = select_subscription(&subscriptions, &subscription)
                .map_err(|error| error.detail)?
                .clone();
            let resolved_subscription_id = resolved_subscription.subscription_id.clone();
            let groups = collector
                .azure
                .resource_groups(&resolved_subscription_id)
                .await
                .map_err(|error| error.detail)?;
            let selected_group = select_group(&groups, &group)
                .map_err(|error| error.detail)?
                .clone();
            let resolved_group = selected_group.name.clone();
            collector
                .config
                .subscription
                .clone_from(&resolved_subscription_id);
            collector.config.resource_group.clone_from(&resolved_group);
            let cache = CacheStore::new_verified(
                &cloud,
                &resolved_subscription_id,
                &resolved_group,
                config.cache_ttl_seconds,
                true,
            );
            if let Some(snapshot) = cache.load() {
                (
                    snapshot,
                    cache,
                    Some((resolved_subscription_id, resolved_group)),
                )
            } else {
                let snapshot = collector
                    .collect_resolved_inventory(
                        subscriptions,
                        resolved_subscription,
                        groups,
                        selected_group,
                    )
                    .await
                    .map_err(|error| error.detail)?;
                (snapshot, cache, None)
            }
        } else {
            let snapshot = collector
                .collect_inventory(&subscription, &group)
                .await
                .map_err(|error| error.detail)?;
            let cache = CacheStore::new("", "", config.cache_ttl_seconds, false);
            (snapshot, cache, None)
        };
        apply_watchlist(&mut snapshot.resources, &config.watchlist);
        let watch = arguments.watch.unwrap_or(config.refresh_seconds);
        App::new(collector, snapshot, watch, !arguments.no_color)
            .with_cache(cache, startup_scope)
            .run()
            .await
            .map_err(|error| error.to_string())?;
    } else {
        let snapshot = collector
            .collect(&subscription, &group)
            .await
            .map_err(|error| error.detail)?;
        if arguments.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot.public_json())
                    .map_err(|error| error.to_string())?
            );
        } else {
            println!("{}", render_table(&snapshot, 280));
        }
    }
    Ok(0)
}

fn expand_home(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if value == "~" || value.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(value.trim_start_matches("~/"));
        }
    }
    path.to_path_buf()
}
