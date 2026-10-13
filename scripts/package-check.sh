#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Packaging containers may forbid nested namespaces. Runtime isolation and
# native CI tests remain mandatory; never retry codecs without their sandbox.
set -euo pipefail

in_container() {
  [[ -f /.dockerenv || -f /run/.containerenv ]]
}

namespace_unavailable() {
  grep -Fq \
    -e 'bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted' \
    -e 'bwrap: Creating new namespace failed: Operation not permitted' \
    -e 'bwrap: No permissions to create a new namespace' \
    -e 'bwrap: setting up uid map: Permission denied' \
    "$1"
}

run_package_checks() (
  local target_dir=${1:?usage: package-check.sh CARGO_TARGET_DIR}
  local probe_log
  local -a filters=()
  probe_log=$(mktemp)
  trap 'rm -f -- "$probe_log"' EXIT

  if "$target_dir/release/bigiris" --self-test >"$probe_log" 2>&1; then
    cat "$probe_log"
  elif in_container && namespace_unavailable "$probe_log"; then
    cat "$probe_log" >&2
    printf '%s\n' \
      'WARNING: packaging container forbids the codec sandbox.' \
      'The three sandbox-dependent CLI tests are filtered in this container only.' \
      'All other tests still run. Native CI must validate sandboxed conversion.' >&2
    filters=(
      --skip self_test_and_help_exit_successfully
      --skip conversion_transform_and_animation_commands_produce_readable_outputs
      --skip batch_profile_converts_folder_and_skip_is_idempotent
    )
  else
    cat "$probe_log" >&2
    printf '%s\n' 'ERROR: codec self-test failed; refusing to approve the package.' >&2
    return 1
  fi

  cargo test --locked --release --workspace --all-features --all-targets \
    -- "${filters[@]}"
)

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  run_package_checks "$@"
fi
