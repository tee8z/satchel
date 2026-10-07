#!/usr/bin/env bash
# package-release.sh VERSION SYSTEM REVISION BINARY OPENSSL_PREFIX OUTPUT_DIR
set -euo pipefail

fail() {
  printf '%s\n' "$*" >&2
  exit 1
}

validate_inputs() {
  if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    fail "Invalid release version: $version"
  fi
  if [[ ! $revision =~ ^[0-9a-f]{40}$ ]]; then
    fail "Invalid source revision: $revision"
  fi
  case $system in
    x86_64-linux) machine="x86-64" ;;
    aarch64-linux) machine="ARM aarch64" ;;
    *) fail "Unsupported release system: $system" ;;
  esac
  if ! file -b "$binary" | grep -q "ELF 64-bit LSB .*, ${machine},"; then
    fail "Binary does not match $system"
  fi
  for license in LICENSE-MIT LICENSE-APACHE; do
    if [ ! -f "$license" ]; then
      fail "Missing license: $license"
    fi
  done
  if [ ! -f "$openssl_prefix/share/licenses/openssl/LICENSE.txt" ]; then
    fail "Missing OpenSSL license from the verified source"
  fi
}

verify_static_tls() {
  local needed
  needed=$(objdump -p "$binary" | awk '$1 == "NEEDED" { print $2 }')
  if grep -Eq '^lib(ssl|crypto)\.so' <<< "$needed"; then
    fail "Release binary must link OpenSSL statically"
  fi
  if ! LC_ALL=C strings "$binary" | grep -Fx 'OpenSSL 3.5.9 29 Sep 2026'; then
    fail "Release binary does not contain the verified OpenSSL version"
  fi
}

assemble_archive() {
  local package_root share
  package_root="$work/$name"
  share="$package_root/share/satchel"
  mkdir -p "$package_root/bin" "$share/licenses"
  install -m 0755 "$binary" "$package_root/bin/satchel"
  printf '%s\n' "$revision" > "$share/REVISION"
  install -m 0644 LICENSE-MIT LICENSE-APACHE "$share/licenses/"
  install -m 0644 "$openssl_prefix/share/licenses/openssl/LICENSE.txt" \
    "$share/licenses/LICENSE-OPENSSL.txt"
  find "$package_root" -type d -exec chmod 0755 {} +
  chmod 0644 "$share/REVISION"
  mkdir -p "$output"
  tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --format=gnu \
    -C "$work" -cf - "$name" | gzip -9 -n > "$output/$name.tar.gz"
}

write_checksum() {
  cd "$output"
  sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
  sha256sum --check "$name.tar.gz.sha256"
}

main() {
  if [ "$#" -ne 6 ]; then
    fail "Usage: $0 VERSION SYSTEM REVISION BINARY OPENSSL_PREFIX OUTPUT_DIR"
  fi
  version=$1
  system=$2
  revision=$3
  binary=$4
  openssl_prefix=$5
  output=$6
  validate_inputs
  verify_static_tls
  name="satchel-$version-$system"
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  assemble_archive
  write_checksum
}

main "$@"
