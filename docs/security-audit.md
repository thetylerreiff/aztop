# Open-source security audit

Audit date: 2026-07-29

Scope: the publishable `aztop` worktree, Rust dependency lockfile, local cache,
Azure CLI command construction, terminal handling, release packaging,
installer, and GitHub Actions workflows.

## Results

### Cloud and data boundary

- Azure access is restricted to typed, fixed read-only command shapes.
- Every network inventory or enrichment read is subscription-scoped without
  changing the Azure CLI default subscription.
- Resource Graph and log queries are bounded and project only the documented
  aggregate or metadata fields.
- Raw App Service and Container Apps streams remain disabled because their CLI
  paths can cross credential or full-configuration boundaries.
- Azure CLI command-file logging, telemetry, dynamic extension installation,
  stdin, and inherited interactive prompts are disabled for child processes.
- Errors, terminal text, JSON output, cache content, and recent-change records
  are sanitized and bounded. The cache is scope-bound, size-limited, and
  owner-only on Unix.

### Source and dependency review

- The publishable tree contains no detected credential patterns, private keys,
  access tokens, or organization-specific examples.
- All locked dependencies come from crates.io; there are no Git or path
  dependencies outside this crate.
- Dependency license expressions are checked by `cargo-deny`.
- Known RustSec advisories are checked by `cargo-audit`.
- The initial scan found `RUSTSEC-2026-0002` (`lru`) and
  `RUSTSEC-2024-0436` (`paste`) through Ratatui 0.29. Upgrading to Ratatui
  0.30.2 removed both affected transitive dependencies; the current locked
  graph passes `cargo audit --deny warnings`.
- `cargo deny check` passes advisories, bans, licenses, and sources. It reports
  two accepted duplicate-version warnings (`hashbrown` and `syn`) within the
  Ratatui dependency graph; neither warning is a vulnerability or policy
  violation.
- The only application `unsafe` block calls the Unix `geteuid` function to
  verify cache ownership. It has no pointer or buffer manipulation.
- Unit tests exercise command allowlisting, query projections, sanitization,
  cache permissions and bounds, process cancellation, invalid UTF-8, terminal
  restoration, and safe JSON output.

### Build and release supply chain

- Workflows use minimum permissions and immutable full-length action SHAs.
- Pull requests do not receive release permissions and no workflow uses
  `pull_request_target`.
- Release tags must exactly match the Cargo package version.
- Release binaries are built with `--locked` on native GitHub-hosted runners.
- Archives contain only the binary, version marker, changelog, README, and MIT
  license.
- Releases include SHA-256 checksums and GitHub build provenance attestations.
- The installer accepts only supported platform/architecture pairs, enforces
  HTTPS/TLS for downloads, verifies the archive checksum, avoids `sudo`, and
  atomically replaces only the target `aztop` binary.

## Public history boundary

The public `main` branch begins with the sanitized open-source tree as a new
root commit. Earlier local prototype/review commits, organization-specific
examples, and corporate commit identity are not ancestors of the public
history and must never be pushed through another ref or tag.

Before every first push to a new remote, verify that `git rev-list --all` shows
only the intended public lineage and re-run the source and history scans. A
normal deletion commit is not an acceptable substitute for removing sensitive
history.

## GitHub settings to enable after repository creation

- private vulnerability reporting;
- secret scanning and push protection;
- Dependabot alerts and security updates;
- a `main` branch ruleset requiring CI and CodeQL checks, review, and resolved
  conversations;
- tag protection or a release ruleset for `v*`;
- Actions policy requiring immutable full-length action SHAs.

These settings cannot be applied until a remote repository exists and are not
substitutes for the checked-in controls.
