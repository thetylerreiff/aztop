# Changelog

All notable changes to `aztop` are documented here.

This project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-07-29

### Added

- A read-only, btop-style Azure resource-group cockpit built with Rust,
  Ratatui, and Tokio.
- Bounded inventory, metrics, diagnostics, alerts, policy, recent-change, and
  aggregate-log views with explicit no-data and permission-limited states.
- Keyboard-first scope selection, filtering, watchlists, responsive charts,
  accessible table output, and sanitized JSON.
- User-local TOML configuration at `~/.aztop/config.toml`.
- Native macOS and glibc Linux release artifacts for x86-64 and arm64.
- A checksum-verifying, no-`sudo` installer plus GitHub artifact provenance.

[Unreleased]: https://github.com/thetylerreiff/aztop/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/thetylerreiff/aztop/releases/tag/v1.0.0
