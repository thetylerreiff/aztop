#!/bin/sh
set -eu

project_directory=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/aztop-installer-test.XXXXXX")
cleanup() {
  rm -rf "$temporary_directory"
}
trap cleanup EXIT HUP INT TERM

case "$(uname -s)" in
  Darwin) target_suffix=apple-darwin ;;
  Linux) target_suffix=unknown-linux-gnu ;;
  *) printf 'unsupported test operating system\n' >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) architecture=x86_64 ;;
  arm64 | aarch64) architecture=aarch64 ;;
  *) printf 'unsupported test architecture\n' >&2; exit 1 ;;
esac

target="${architecture}-${target_suffix}"
archive="aztop-${target}.tar.gz"
release_directory="${temporary_directory}/release"
payload_directory="${temporary_directory}/payload/aztop-${target}"
fake_bin="${temporary_directory}/bin"
install_directory="${temporary_directory}/install"
mkdir -p "$release_directory" "$payload_directory" "$fake_bin"

printf '#!/bin/sh\nprintf "aztop test binary\\n"\n' >"${payload_directory}/aztop"
chmod 0755 "${payload_directory}/aztop"
tar -czf "${release_directory}/${archive}" \
  -C "${temporary_directory}/payload" "aztop-${target}"

if command -v sha256sum >/dev/null 2>&1; then
  checksum=$(sha256sum "${release_directory}/${archive}" | awk '{ print $1 }')
else
  checksum=$(shasum -a 256 "${release_directory}/${archive}" | awk '{ print $1 }')
fi
printf '%s  %s\n' "$checksum" "$archive" >"${release_directory}/SHA256SUMS"
cp "${project_directory}/scripts/testdata/curl" "${fake_bin}/curl"
chmod 0755 "${fake_bin}/curl"

PATH="${fake_bin}:${PATH}" \
  AZTOP_TEST_RELEASE_DIRECTORY="$release_directory" \
  AZTOP_INSTALL_DIR="$install_directory" \
  AZTOP_VERSION="v0.1.0" \
  sh "${project_directory}/scripts/install.sh"

output=$("${install_directory}/aztop")
[ "$output" = "aztop test binary" ] || {
  printf 'installed binary produced unexpected output\n' >&2
  exit 1
}

printf 'tamper' >>"${release_directory}/${archive}"
if PATH="${fake_bin}:${PATH}" \
  AZTOP_TEST_RELEASE_DIRECTORY="$release_directory" \
  AZTOP_INSTALL_DIR="${temporary_directory}/tampered-install" \
  AZTOP_VERSION="v0.1.0" \
  sh "${project_directory}/scripts/install.sh" >/dev/null 2>&1; then
  printf 'installer accepted an archive with an invalid checksum\n' >&2
  exit 1
fi

if [ -e "${temporary_directory}/tampered-install/aztop" ]; then
  printf 'installer left a binary behind after checksum verification failed\n' >&2
  exit 1
fi

printf 'installer integration test passed for %s\n' "$target"
