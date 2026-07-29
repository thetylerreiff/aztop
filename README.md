# aztop

[![CI](https://github.com/thetylerreiff/aztop/actions/workflows/ci.yml/badge.svg)](https://github.com/thetylerreiff/aztop/actions/workflows/ci.yml)
[![CodeQL](https://github.com/thetylerreiff/aztop/actions/workflows/codeql.yml/badge.svg)](https://github.com/thetylerreiff/aztop/actions/workflows/codeql.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

aztop is a local, **read-only** terminal viewer for Azure, implemented as a single Rust application with Ratatui and Tokio. Its default operations view is visualization-first: a full-width resource-group pulse, selected-resource utilization charts, and a btop-style attention queue. It keeps keyboard navigation and intentional unknown/limited states without pretending to replace Azure Portal, Azure Monitor, Application Insights, Workbooks, alerts, or incident response.

It is generic: the tool discovers enabled subscriptions and resource groups
from the current Azure CLI session. It does not contain an environment-specific
service model.

## Install

The release installer supports macOS and glibc-based Linux on x86-64 and arm64.
It downloads the native release archive, verifies its SHA-256 checksum, and
installs only the `aztop` binary to `~/.local/bin` by default. It does not use
`sudo`. Musl-based systems can build from source.

One-line install:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/thetylerreiff/aztop/releases/latest/download/install.sh |
  sh
```

For the inspect-before-running path:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/thetylerreiff/aztop/releases/latest/download/install.sh \
  -o /tmp/install-aztop.sh
less /tmp/install-aztop.sh
sh /tmp/install-aztop.sh
```

Pin a release or choose another install directory:

```sh
AZTOP_VERSION=v1.0.0 AZTOP_INSTALL_DIR="$HOME/bin" \
  sh /tmp/install-aztop.sh
```

If `~/.local/bin` is not already on `PATH`, add it to your shell profile:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Release archives, `SHA256SUMS`, and build provenance attestations are attached
to every GitHub Release. With GitHub CLI installed, an archive can also be
verified against its workflow provenance:

```sh
gh attestation verify aztop-aarch64-apple-darwin.tar.gz \
  --repo thetylerreiff/aztop
```

## Requirements

- macOS or Linux
- Azure CLI (`az`)
- An existing authenticated Azure CLI session
- Rust 1.88+ only when building from source or using the repository launcher

The tool does not log in, change the Azure CLI default subscription, install extensions, or write Azure configuration.
Resource Graph, Application Insights, and Front Door enrichments use their
corresponding Azure CLI extensions when already installed. A missing extension,
permission, provider API, or Azure Government feature is rendered as
`unavailable`; the viewer never installs anything on the user's behalf.
Every child process sets `AZURE_EXTENSION_USE_DYNAMIC_INSTALL=no`.

## Run

Authenticate Azure CLI first, then start `aztop`:

```sh
az login
aztop
```

For a predictable initial scope:

```sh
aztop --subscription "My Subscription" --resource-group my-resource-group
```

`--subscription` is applied to every fixed read-only Azure call. `aztop` never
runs `az account set`.

## Build from source

For development from the repository:

```sh
cd /path/to/aztop
cargo build --release --locked
./aztop
```

`./aztop` is a Rust development launcher. It runs the locked Cargo project
and recompiles only when source files changed. There is no Python fallback.

Install the standalone `aztop` command with:

```sh
cargo install --path . --locked
aztop
```

The installed executable does not require Python or a checkout of this
repository.

### Keyboard

| Key | Action |
| --- | --- |
| `j` / `↓`, `k` / `↑` | Select a resource |
| `Enter` | Toggle selected-resource drill-down |
| `d` | Open the selected resource's 24-hour recent-change timeline |
| `l` | Open selected-resource aggregate log signals |
| `Shift+L` | Explain why direct raw service streams are blocked or unsupported (no Azure read) |
| `g` | Open the zero-read resource-group chooser |
| `s` | Open the zero-read subscription chooser |
| `[` / `]` | Expert previous / next resource group, debounced |
| `o`, `O` | Cycle sort field / reverse sort |
| `f` | Cycle category filter |
| `v` | Toggle operations and inventory layouts |
| `w` | Cycle dashboard windows: `1h/1m`, `6h/5m`, `24h/15m` |
| `m` | Toggle bounded metrics and telemetry aggregates |
| `t` | Star or unstar the selected resource for this session |
| `Shift+T` | Toggle the operator watchlist-only view |
| `r` | Refresh |
| `?` | In-product help |
| `q` / `Ctrl-C` | Quit and restore the terminal |

In a scope chooser, type immediately to filter, use arrows or
`Page Up`/`Page Down`/`Home`/`End` to select, and press `Enter` to load.
`Esc` clears a non-empty filter, then cancels. Printable characters—including
`j`, `k`, and `q`—are filter text inside the chooser. Opening, searching, and
canceling a chooser perform no Azure reads. `Tab` and left/right no longer
change scope.

The common staging flow is:

```text
g → type staging → ↑/↓ if needed → Enter
```

Confirmed scope changes keep the current snapshot visibly labeled while
inventory loads in a background worker. Fresh target inventory is reconciled
into the visible model, then bounded evidence fills in without clearing charts,
selection, or known evidence. Rows explicitly distinguish `PEND` (a query is
running), `ND` (a query succeeded with no samples), `CAP` (outside the current
bounded sample), `INV` (inventory only), and `LIM` (permission/API limited).
Failed refreshes retain the last successful snapshot as `STALE`; failed or
superseded loads never relabel old rows as the requested scope.

The layout reflows on PTY resize. At normal desktop widths, the top 30% is a
full-width resource-group pulse. The lower area dedicates roughly 40% to
selected-resource charts and 60% to operations navigation. That right-hand
area keeps the attention-ranked resource table dominant and reserves its lower
portion for a compact 24-hour recent-change timeline. Narrow terminals stack
the same information when there is enough height; very small terminals retain
the table and expose the full timeline with `d`. Press `v` for the
inventory-oriented layout.

At wide sizes, the resource-group pulse gives its primary available signal a
large, multi-row braille chart and places the remaining independently scaled
signals in a compact right rail. The selected-resource panel divides all
available vertical space among its provider-native metrics; compact terminals
fall back to one-row sparklines. Missing lanes remain explicitly labeled. No
view combines incompatible units into a synthetic health score. For an App
Service, app-scoped requests, 5xx, latency, and memory working set remain
separate from CPU and memory percentage on its visible backing App Service
Plan; plan values are labeled `SHARED PLAN`.

The dominant pulse chart reserves a separate aligned change rail for safe,
resource-group-scoped change counts. It never overlays change markers on the
metric series or implies causality. aztop displays only 5-minute
count/type aggregates; it does not retrieve change actors, targets, property
diffs, or raw change records.

### Recent changes, not deployment claims

The compact `recent changes` panel shows at most 20 safe records from the last
24 hours. Press `d` to open the timeline filtered to the selected resource.
Azure Resource Graph contributes only a fixed server-side projection of
timestamp, `Create`/`Update`/`Delete` type, visible resource name, and visible
resource type. For resources that no longer resolve after deletion, the name
and type remain explicitly unresolved; aztop never reconstructs them from an
ARM ID.

aztop also records version and control-state transitions that it observes
between successful inventory refreshes, labeled `aztop observation`. These are
point-in-time observations, not a complete audit trail. A generic Azure
`UPDATE` can represent many kinds of control-plane changes and is never called
a deployment. Empty results do not prove that no deployment occurred.
Confirmed CI/CD workflow, actor, commit, and rollout status require a separate
provider integration and are intentionally outside this Azure-only surface.

Interactive refresh is split by cost. `refresh_seconds` or `--watch` controls
the focused-resource scheduling check (30 seconds by default). Fleet checks run
no faster than 60 seconds, and inventory/Resource Graph/Policy/diagnostic
coverage no faster than five minutes. A check reuses the exact aligned metric
cohort until the selected bin advances, avoiding a duplicate Azure read that
could not add a new point. `r` deliberately bypasses that reuse and refreshes
every layer. `--watch 0` disables automatic reads after the initial snapshot.

### Operator watchlist

Profiles can identify the few resources an operator owns. A watch rule uses an
exact resource name, with an optional exact resource type, short alias, and
expected control-plane state:

```json
{
  "watchlist": [
    {
      "name": "api-production",
      "type": "Microsoft.Web/sites",
      "alias": "API",
      "expect_control": "running"
    },
    "production-search"
  ]
}
```

Watched resources are starred, prioritized inside the hard bounded metric
sample, and ranked first within the same attention tier. The cap is never
expanded: if the watchlist itself exceeds it, overflow rows say `CAP` and remain
available for a focused read. An explicit expected-state mismatch is `BAD`; an
expectation that cannot be evaluated is `LIM`. Merely being watched never
manufactures a health verdict. Press `t` for a session-only star and `Shift+T`
to isolate the watchlist. Session stars do not modify the profile or cache.

### Private progressive cache

Interactive runs use a bounded, user-private local cache by default so a known
resource group appears immediately while Azure refreshes in the background.
The header labels cached evidence and its age. A successful refresh reconciles
fresh inventory into the cached snapshot; a failed refresh retains the prior
view as `STALE`. Cache lookup starts only after Azure resolves the current
cloud, authenticated subscription ID, and canonical resource group.

Only the already-sanitized operational model is stored: no subscription IDs,
ARM IDs, hosting-plan IDs, diagnostic destinations, workspace IDs, raw logs,
or credentials. Files are atomically replaced, capped to eight scopes and
4 MiB each, expire after 24 hours by default, and use owner-only permissions
on Unix. The default location is
`~/Library/Caches/aztop` on macOS or
`$XDG_CACHE_HOME/aztop` (falling back to
`~/.cache/aztop`) on Linux. Configure `cache_enabled` and
`cache_ttl_seconds` in a profile, or pass `--no-cache` for a run.

aztop also disables Azure CLI command-file logging and CLI
telemetry for every spawned process. This prevents new `~/.azure/commands`
files from retaining internal subscription, resource, workspace, or fixed-query
arguments. Azure CLI logs created by older versions/runs are outside this
application's cache and are not deleted automatically.

### Aggregate log signals

Press `l` on a selected resource to open its safe log-signals view. `w` cycles
the fixed `15m`, `1h`, and `6h` windows, `r` refreshes that view, and `l` or
`Esc` closes it. Queries run in an independent worker so the main inventory and
metric refresh remain responsive.

The panel retrieves only server-side aggregates: event, error, warning,
exception, and failed-dependency counts; time buckets; latest telemetry time;
and ingestion lag when the source exposes it. It never requests or returns raw
rows, messages, URLs, operation names, exception details, user identifiers,
request/response bodies, or arbitrary columns. Zero events is rendered
`no_data`, never healthy.

Application Insights components use a component-scoped fixed aggregate.
Other resource types can use up to three visible Log Analytics workspaces in
the selected resource group and filter on the selected ARM resource ID inside a
fixed query. Workspace query IDs are held only in memory for the CLI call and
are never rendered or included in aztop table/JSON output. On
`AzureUSGovernment`, generic workspace aggregates are disabled before any
workspace or query read is attempted; the panel reports `unsupported`.
Missing extensions, tables, permissions, or other API support are shown as
`unavailable`.

### Direct raw service streams are disabled

`Shift+L` is an explanatory safety surface, not a connection mode. It performs
no Azure read. App Service CLI log tailing can fetch publishing credentials;
Container Apps CLI log streaming can fetch a stream token and the full resource
configuration. Neither path meets this viewer's aggregate-only boundary, so
the tool constructs and runs no direct raw-stream command.

For supported resource types, the overlay explains the blocked boundary. For
other types, it reports unsupported. Platform services such as Azure AI Search
remain aggregate-only: raw diagnostics may contain user query strings and are
not queried.

### Accessible table and JSON

```sh
./aztop --table --no-color --resource-group my-resource-group
./aztop --json --resource-group my-resource-group
./aztop --table --metrics --resource-group my-resource-group
```

Redirected stdout automatically uses a one-shot uncolored table. Table mode
spells out source and time window for aggregate signals, includes the sanitized
recent-change timeline, and lists the complete selected-group inventory,
including `INV` rows that the interactive operations view intentionally
suppresses. JSON uses schema version 2 and deliberately omits subscription IDs
and Azure resource IDs. Recent changes in JSON contain only timestamp, visible
resource name/type, event label, bounded detail, and source.

## Scope selection and optional profiles

With no selector, aztop uses the current default enabled subscription and the
first visible resource group alphabetically. Set persistent startup defaults in
`~/.aztop/config.toml`:

```sh
mkdir -p ~/.aztop
cp config.toml ~/.aztop/config.toml
```

The three primary settings are:

```toml
subscription = "My Subscription"
resource_group = "my-resource-group"
window = "1h/1m"
```

`window` uses `<hours>h/<minutes>m`; the normal interactive presets are
`1h/1m`, `6h/5m`, and `24h/15m`. Other bounded combinations from 1–24 hours
and 1–60 minutes remain valid. Do not combine `window` with the older
`metric_window_hours` or `metric_interval_minutes` fields.

Command-line `--subscription` and `--resource-group` values override the file
for one run. Inside the TUI, `g` chooses among the current subscription's
already-discovered groups and `s` chooses a subscription; selecting a new
subscription then loads its first visible group.

Without `--config`, the first existing profile in this order is loaded:

1. `~/.aztop/config.toml`;
2. `$XDG_CONFIG_HOME/aztop/config.toml`, when `XDG_CONFIG_HOME` is set;
3. `$XDG_CONFIG_HOME/aztop/config.json`;
4. `~/Library/Application Support/aztop/config.toml` on macOS;
5. `~/Library/Application Support/aztop/config.json` on macOS;
6. `~/.config/aztop.json`.

TOML is the preferred format; existing JSON profiles remain supported.
Profiles are not merged. `--config PATH` accepts a `.toml` or `.json` file and
always wins, including when the explicit file is missing or invalid. If no
discovered profile exists, generic defaults are used. Unknown profile or
watch-rule fields and conflicting overlapping watch rules are rejected before
Azure is read; identical duplicate watch rules are collapsed.

The tool never writes profiles. [`config.toml`](config.toml) is
the copy-ready generic example. [`aztop.example.json`](aztop.example.json)
documents the compatible JSON shape. Watch rules are profile data, not product
defaults.

## Resource model

Current ARM resources are grouped into practical categories:

- compute/web
- data
- network/edge
- AI
- storage
- monitoring
- security
- other

Every resource has inventory metadata, provisioning state, category, type, location, and changed age when Azure exposes it. Type-specific adapters add only supported state and aggregate metrics. Unsupported types remain visibly `inventory only`; the viewer does not offer a generic metric name, KQL, or Azure-command field.

Relationships are intentionally narrow and explicit. App Services and slots
show their backing App Service Plan, plans show visible dependent apps, and a
slot shows its parent app. aztop does not infer dependencies from
similar names or guess Application Insights ownership.

### Fixed metric adapters

| Azure resource type | Bounded aggregate metrics |
| --- | --- |
| App Service | requests, HTTP 5xx, average response time, memory working set, configured Health Check status |
| App Service slot | requests, HTTP 5xx, average response time, memory working set |
| App Service plan | CPU and memory percentage |
| PostgreSQL Flexible Server | CPU, active connections, storage percentage |
| Azure AI/Cognitive Services | total calls, total errors, latency |
| Azure AI Search | search latency, query rate, throttled-query percentage |
| Azure Front Door profile | requests, 5xx percentage, total latency, origin-health percentage |
| Storage account | used capacity, availability, transactions, end-to-end and server latency |
| Key Vault | availability, saturation, API hits/results, API latency |
| Container Registry | storage used, pull/push totals and success counts |
| Azure SQL database | CPU, DTU, storage, workers, sessions, deadlocks |
| Cosmos DB | requests, request units, normalized RU use, availability, server latency |
| Azure Cache for Redis | server load, clients, memory, errors, evictions, miss rate |
| Azure Firewall | health, SNAT utilization, throughput, data processed, latency |
| Logic App | run and trigger totals, failures, throttles |

Metrics use a configurable 1–24 hour window (1 hour by default), 1–60 minute
bins (1 minute by default), no dimensions, and a hard resource-count cap.
Interactive `w` provides the fixed `1h/1m`, `6h/5m`, and `24h/15m` presets.
When a provider rejects the requested grain, the fixed adapter retries only the
bounded *coarser* `5m`, `15m`, and `1h` grains, remembers that provider result
for the process lifetime, and labels the series with the grain Azure accepted.
Missing samples are `no_data`, not zero or healthy.

Background Application Insights enrichment uses one fixed, summarize-only KQL
query over a 24-hour window for at most eight prioritized visible components;
overflow is an explicit limited/capped signal. The selected-resource log panel
uses a second fixed aggregate query over one of three short windows. Both return only counts,
durations/timestamps, time buckets, and built-in table labels. User KQL, raw
rows, operation names, URLs, traces, messages, and identifiers are not
supported.

When enrichment is enabled, fixed subscription-scoped Resource Graph queries
add active fired alert-instance counts, Service Health advisory/maintenance
counts, resource-group-scoped 24-hour resource-change counts in 5-minute bins,
a capped metadata-only recent-change projection, alert-rule coverage, and
Resource Health availability records. Azure Policy
contributes only subscription-level noncompliant resource and policy counts.
Diagnostic Settings contributes only supported-category counts for a bounded,
provider-balanced sample. The tool calls only the categories endpoint; it does
not inspect diagnostic settings or destinations.

## State semantics

aztop keeps these evidence classes separate:

| State | Meaning |
| --- | --- |
| Control | Provider-specific state such as App Service `Running` or `Stopped`. |
| Provision | ARM provisioning metadata such as `Succeeded`. It is not application health. |
| Availability | Provider control-plane availability or an actual availability-result aggregate. |
| Resource Health | Azure Resource Health availability state for that ARM resource, when a record exists. |
| App Health | Only a configured health-check metric or explicit availability result. |
| Signal | A bounded numeric aggregate such as requests, failures, latency, CPU, or connections. |
| Attention | A navigation priority (`BAD`, `WRN`, `STOP`, `SIG`, `OK`, `LIM`, `PEND`, `ND`, `CAP`, `INV`), not a synthetic health verdict. |
| `warning` / `WRN` | Operator attention signal such as retained alerts, maintenance, or noncompliance; not an application-health verdict. |
| `SIG` | A bounded numeric signal is available. It is evidence, not necessarily health. |
| `ND` / `no_data` | The bounded query worked but returned no numeric samples. |
| `PEND` | The fixed bounded query is running; prior evidence remains visible. |
| `CAP` | The resource is metric-capable but was not sampled in this bounded pass. |
| `INV` | Inventory metadata only; no fixed safe metric adapter exists. |
| `unsupported` | No fixed safe adapter exists for that provider/type/API. |
| `unavailable` / `LIM` | Permission, API, or Azure Government limitation. |
| `unknown` | No trustworthy verdict is available. |

Running, successful provisioning, zero traffic, and zero returned failures never become a manufactured “healthy” verdict.

## Strict read-only boundary

The code has no generic Azure command passthrough. Its complete cloud-read allowlist is:

```text
az cloud show
az account list                              # existing CLI subscription cache; fixed output projection
az graph query --graph-query ...             # fixed server-side inventory, workspace, status, and aggregate projections
az monitor metrics list --resource ...        # fixed metric adapters only
az monitor diagnostic-settings categories list --resource ... # supported categories only
az monitor app-insights query --apps ...       # private server-projected app GUID; fixed summarize-only KQL
az monitor log-analytics query --workspace ... # fixed 15m/1h/6h aggregate KQL only; never on AzureUSGovernment
```

Aggregate log calls are represented by typed Application Insights and Log
Analytics variants; no crate-generic command vector is exposed. Every spawned
Azure CLI child disables command-file logging, CLI telemetry, and dynamic
extension installation.

`az account list` reads Azure CLI's already-authenticated local account cache
without `--refresh`; aztop emits and persists only subscription
name, selected/default state, and its private runtime scope ID. Network
inventory and enrichment use server-side Resource Graph projections so richer
resource, workspace, policy, and Front Door objects never reach the process.

It never reads or executes:

- app settings, connection strings, access keys, credentials, publishing profiles, Kudu/SCM credentials, arbitrary SCM endpoints, or deployment credentials;
- generic `az rest`, user-provided KQL, arbitrary metric names, or arbitrary Azure subcommands;
- user-provided Resource Graph queries, raw resource-change records, resource
  change actors/IDs/diffs, Resource Health details beyond state/reason/time, or
  raw policy state rows;
- raw application or Log Analytics rows, messages, request/response bodies, URLs, operation names, traces, prompts, tenant/user datasets, job payloads, or exception text; authenticated tenant/user fields already present in Azure CLI's local account cache are not emitted to, received by, or persisted by aztop;
- diagnostic settings or destinations, storage IDs, rendered/serialized workspace query IDs, or Action Group receivers;
- create/update/delete/restart/start/stop/deploy/account-set operations.

Direct App Service and Container Apps raw streams are disabled: their Azure CLI
paths may obtain publishing credentials, a stream token, or full service
configuration before returning output. `Shift+L` only reports that blocked or
unsupported state and performs no Azure read.

Azure Activity Log and confirmed deployment history are intentionally excluded.
Azure CLI downloads individual activity events before applying a local JMESPath
projection, which does not meet this viewer's server-projection boundary.
Recent Changes is therefore a narrower Resource Graph metadata timeline, not an
Activity Log replacement. Action Group receiver configuration is also excluded
because it may contain personal routing details.

## Rust architecture

The Cargo project exposes a reusable `aztop` library and the `aztop`
binary. Typed domain models and sanitized schema output are separate from the
private Azure CLI adapter. Collection, refresh scheduling, Ratatui rendering,
aggregate logs, direct-stream safety status, configuration, and sanitization
are isolated modules.

Tokio tasks keep inventory, focused-resource metrics, fleet aggregates, and
aggregate-log collection independent. Scope, generation, and aligned metric
cohort tokens reject stale results. Superseded Azure CLI children are cancelled
and killed with bounded timeouts. Fixed metric names and aggregations are
batched into one Azure CLI read per resource behind a shared process gate:
four children by default, configurable from one to sixteen. Focused work has a
20-second deadline; fleet work has a 60-second deadline. Cold cached startup
resolves authenticated cloud, subscription, and group before cache lookup, then
reuses the sanitized snapshot while inventory refreshes. The TUI redraws every
second and never blocks keyboard input on an Azure read. The private cache
stores up to eight verified scopes, each capped at 4 MiB.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
shellcheck aztop scripts/*.sh scripts/testdata/*
cargo audit --deny warnings
cargo deny check
```

See the [changelog](CHANGELOG.md), checked-in
[security audit](docs/security-audit.md),
[contribution guide](CONTRIBUTING.md), and
[release process](RELEASING.md).
CI repeats the Rust, shell, dependency, installer, and packaging gates on every
pull request. CodeQL scans both Rust and GitHub Actions workflows.

## License

`aztop` is available under the [MIT License](LICENSE).
