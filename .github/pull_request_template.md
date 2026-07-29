## Summary

<!-- Describe the operator-facing change and why it is needed. -->

## Safety

- [ ] Azure acquisition remains fixed, allowlisted, and read-only.
- [ ] No credentials, app settings, keys, raw logs, payloads, tenant/customer
      data, internal names, or real Azure identifiers were added.
- [ ] Missing, unsupported, permission-limited, and unhealthy states remain
      semantically distinct.
- [ ] New queries are bounded, server-projected, sanitized, and covered by
      command-construction tests.
- [ ] Not applicable; this change does not touch Azure acquisition or output.

## Verification

<!-- List exact tests, platforms, and manual checks. -->
