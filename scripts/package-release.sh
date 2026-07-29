#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
  printf 'usage: %s TARGET VERSION OUTPUT_DIRECTORY\n' "$0" >&2
  exit 2
fi

target=$1
version=$2
output_directory=$3
binary="${CARGO_TARGET_DIR:-target}/${target}/release/aztop"

case "$target" in
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu | x86_64-apple-darwin | aarch64-apple-darwin) ;;
  *)
    printf 'unsupported release target: %s\n' "$target" >&2
    exit 2
    ;;
esac
case "$version" in
  [0-9]*.[0-9]*.[0-9]*)
    case "$version" in
      *[!A-Za-z0-9._-]*)
        printf 'invalid release version: %s\n' "$version" >&2
        exit 2
        ;;
    esac
    ;;
  *)
    printf 'invalid release version: %s\n' "$version" >&2
    exit 2
    ;;
esac

[ -f "$binary" ] || {
  printf 'release binary not found: %s\n' "$binary" >&2
  exit 1
}
[ -f LICENSE ] || {
  printf 'LICENSE is required for release packaging\n' >&2
  exit 1
}
[ -f README.md ] || {
  printf 'README.md is required for release packaging\n' >&2
  exit 1
}
[ -f CHANGELOG.md ] || {
  printf 'CHANGELOG.md is required for release packaging\n' >&2
  exit 1
}

temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/aztop-package.XXXXXX")
cleanup() {
  rm -rf "$temporary_directory"
}
trap cleanup EXIT HUP INT TERM

package_name="aztop-${target}"
package_directory="${temporary_directory}/${package_name}"
mkdir -p "$package_directory" "$output_directory"
cp "$binary" "${package_directory}/aztop"
chmod 0755 "${package_directory}/aztop"
cp CHANGELOG.md LICENSE README.md "$package_directory/"
printf '%s\n' "$version" >"${package_directory}/VERSION"

tar -czf "${output_directory}/${package_name}.tar.gz" \
  -C "$temporary_directory" "$package_name"
printf '%s\n' "${output_directory}/${package_name}.tar.gz"
