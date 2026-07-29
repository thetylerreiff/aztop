# Contributing to aztop

Thanks for helping improve `aztop`.

## Before opening a change

- Search existing issues and pull requests.
- Keep Azure acquisition inside the typed, fixed read-only adapter.
- Do not add generic Azure CLI passthrough, arbitrary KQL, secret-bearing
  reads, raw tenant data, or cloud mutation.
- Preserve the distinction between control state, health evidence, no data,
  unsupported APIs, and permission-limited reads.
- Never include real subscription IDs, ARM IDs, credentials, customer data, or
  internal resource names in tests, fixtures, screenshots, or documentation.

For a vulnerability, follow [SECURITY.md](SECURITY.md) instead of opening a
public issue.

## Development

Requirements:

- Rust 1.88 or newer;
- macOS or Linux;
- Azure CLI only for optional live, read-only smoke checks.

Run the local quality gate:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
shellcheck aztop scripts/*.sh scripts/testdata/*
```

Unit tests must mock Azure CLI process execution. A pull request must not
require Azure credentials or a live subscription to pass.

## Pull requests

Keep changes focused and document:

- the user-visible behavior;
- any new Azure CLI command shape and why it remains read-only;
- query bounds, projections, and data-sensitivity decisions;
- the tests and platforms used for verification.

New Azure reads need tests proving that forbidden command families, sensitive
fields, and unbounded queries cannot be constructed.
