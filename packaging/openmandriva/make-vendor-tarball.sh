#!/usr/bin/env bash
# Build the deterministic Rust dependency source used by ABF Source1.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
package_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$project_root/Cargo.toml" | head -n1)"
[ -n "$package_version" ] || { echo "Could not read workspace version" >&2; exit 1; }

vendor_tmp="$(mktemp -d -t bigiris-vendor.XXXXXX)"
trap 'rm -rf -- "$vendor_tmp"' EXIT
mkdir -p "$vendor_tmp/.cargo"

( cd "$project_root" && cargo vendor --locked "$vendor_tmp/vendor" ) \
    > "$vendor_tmp/.cargo/config.toml"

archive="$project_root/packaging/openmandriva/bigiris-${package_version}-vendor.tar.xz"
tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner \
    -C "$vendor_tmp" -cJf "$archive" .cargo vendor
sha256sum "$archive"

