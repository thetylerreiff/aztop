#!/bin/sh
set -eu

repository=${AZTOP_REPOSITORY:-thetylerreiff/aztop}
version=${AZTOP_VERSION:-latest}
install_dir=${AZTOP_INSTALL_DIR:-"${HOME:?HOME is required}/.local/bin"}

die() {
  printf 'aztop installer: %s\n' "$*" >&2
  exit 1
}

owner=${repository%%/*}
name=${repository#*/}
if [ "$owner" = "$repository" ] || [ -z "$owner" ] || [ -z "$name" ] || [ "$name" = "$repository" ]; then
  die "AZTOP_REPOSITORY must be in owner/repository form"
fi
case "$owner$name" in
  *[!A-Za-z0-9._-]*) die "AZTOP_REPOSITORY contains unsupported characters" ;;
esac

case "$version" in
  latest) release_root="https://github.com/${repository}/releases/latest/download" ;;
  v[0-9]*)
    case "$version" in
      *[!A-Za-z0-9._-]*) die "AZTOP_VERSION contains unsupported characters" ;;
    esac
    release_root="https://github.com/${repository}/releases/download/${version}"
    ;;
  *) die "AZTOP_VERSION must be latest or a v-prefixed release tag" ;;
esac

case "$(uname -s)" in
  Darwin) operating_system=apple-darwin ;;
  Linux)
    if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
      die "prebuilt Linux releases require glibc; build from source on musl systems"
    fi
    operating_system=unknown-linux-gnu
    ;;
  *) die "unsupported operating system; aztop supports macOS and Linux" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) architecture=x86_64 ;;
  arm64 | aarch64) architecture=aarch64 ;;
  *) die "unsupported architecture; aztop supports x86-64 and arm64" ;;
esac

target="${architecture}-${operating_system}"
archive="aztop-${target}.tar.gz"

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/aztop-install.XXXXXX")
temporary_binary=
cleanup() {
  if [ -n "$temporary_binary" ]; then
    rm -f "$temporary_binary"
  fi
  rm -rf "$temporary_directory"
}
trap cleanup EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  --output "${temporary_directory}/${archive}" "${release_root}/${archive}"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  --output "${temporary_directory}/SHA256SUMS" "${release_root}/SHA256SUMS"

expected=$(
  awk -v archive="$archive" '$2 == archive { print $1; exit }' \
    "${temporary_directory}/SHA256SUMS"
)
case "$expected" in
  "" | *[!0-9a-fA-F]*) die "release checksum is missing or invalid" ;;
esac
[ "${#expected}" -eq 64 ] || die "release checksum has an invalid length"

if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "${temporary_directory}/${archive}" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "${temporary_directory}/${archive}" | awk '{ print $1 }')
else
  die "sha256sum or shasum is required"
fi

[ "$actual" = "$expected" ] || die "archive checksum verification failed"

tar -xzf "${temporary_directory}/${archive}" -C "$temporary_directory" \
  "aztop-${target}/aztop"
source_binary="${temporary_directory}/aztop-${target}/aztop"
[ -f "$source_binary" ] || die "release archive does not contain aztop"
[ ! -L "$source_binary" ] || die "release archive contains a symbolic-link binary"

umask 022
mkdir -p "$install_dir"
temporary_binary="${install_dir}/.aztop-install.$$"
cp "$source_binary" "$temporary_binary"
chmod 0755 "$temporary_binary"
mv "$temporary_binary" "${install_dir}/aztop"
temporary_binary=

printf 'Installed aztop to %s/aztop\n' "$install_dir"
case ":${PATH:-}:" in
  *":${install_dir}:"*) ;;
  *) printf 'Add %s to PATH to run aztop from any shell.\n' "$install_dir" ;;
esac
