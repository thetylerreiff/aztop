# Releasing aztop

Releases are built by GitHub Actions from an annotated or lightweight tag.
Maintainers do not upload locally built binaries.

## Prepare

1. Update `version` in `Cargo.toml`.
2. Move the release notes in `CHANGELOG.md` out of `Unreleased`, add the
   release date, and update its comparison links.
3. Run `cargo check --locked` so `Cargo.lock` remains current.
4. Complete the checks in `CONTRIBUTING.md`.
5. Merge the release commit to `main`.

## Publish

Create and push a matching tag:

```sh
version="$(cargo metadata --no-deps --format-version 1 |
  jq -r '.packages[] | select(.name == "aztop") | .version')"
git tag -s "v${version}" -m "aztop ${version}"
git push origin "v${version}"
```

The release workflow verifies that the tag and Cargo version match, builds
native archives for macOS and Linux on x86-64 and arm64, creates
`SHA256SUMS`, attests the artifacts, and publishes a GitHub Release with
generated notes.

After the workflow succeeds, test the documented installer on at least one
macOS and one Linux host. Do not move or recreate a published tag.
