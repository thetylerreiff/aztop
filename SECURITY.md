# Security policy

## Supported versions

Security fixes are made on the latest released version of `aztop`. Update to
the newest release before reporting an issue that may already be fixed.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use
[GitHub private vulnerability reporting](https://github.com/thetylerreiff/aztop/security/advisories/new)
and include:

- the affected `aztop` version and platform;
- a minimal reproduction;
- the security impact;
- whether Azure CLI output, local cache data, terminal rendering, the installer,
  or a release artifact is involved.

Please do not include real credentials, tenant data, customer data, raw logs,
or other sensitive Azure output. Use synthetic identifiers and redacted
samples.

You should receive an acknowledgement within five business days. Validated
issues will be coordinated privately through a fix and release.

## Security boundary

`aztop` is a local viewer, not an Azure authorization boundary. It runs the
`az` executable found on `PATH` and inherits the permissions of the existing
Azure CLI session. The Azure CLI installation, installed extensions, terminal,
operating system account, and release binary are trusted dependencies.

The application itself exposes no generic Azure-command or KQL passthrough. Its
fixed cloud operations are documented in the README. It does not log in,
change subscriptions, mutate Azure resources, retrieve secrets, or enable
dynamic extension installation.

Release archives include SHA-256 checksums and GitHub build provenance
attestations. The installer verifies the archive checksum before replacing an
existing binary.
