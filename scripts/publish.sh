#!/usr/bin/env bash

set -Eeuo pipefail

readonly PACKAGES=(
  codescope-core
  codescope-telemetry
  codescope-git
  codescope-lsp
  codescope-testutil
  codescope-tui
  codescope-ai
  codescope-analysis
  codescope
)

usage() {
  cat <<'EOF'
Usage: ./scripts/publish.sh [--yes]

Publish the workspace to crates.io in dependency order. The script is resumable:
versions already present on crates.io are skipped, and crates.io HTTP 429 responses
are retried after the server-provided deadline.

Options:
  -y, --yes  Skip the interactive confirmation (for CI).
  -h, --help Show this help.
EOF
}

confirm=true
case "${1:-}" in
  "") ;;
  -y|--yes) confirm=false ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac
[[ $# -le 1 ]] || { usage >&2; exit 2; }

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

if [[ -n $(git status --porcelain) ]]; then
  echo "error: refusing to publish from a dirty working tree" >&2
  exit 1
fi

package_id=$(cargo pkgid -p codescope-core)
version=${package_id##*#}
if [[ -z "$version" || "$version" == "$package_id" ]]; then
  echo "error: could not determine the workspace version" >&2
  exit 1
fi

for package in "${PACKAGES[@]}"; do
  member_id=$(cargo pkgid -p "$package")
  member_version=${member_id##*#}
  if [[ "$member_version" != "$version" ]]; then
    echo "error: $package is $member_version, expected workspace version $version" >&2
    exit 1
  fi
done

if $confirm; then
  echo "About to publish ${#PACKAGES[@]} crates at version $version to crates.io."
  read -r -p "Type 'publish' to continue: " answer
  if [[ "$answer" != "publish" ]]; then
    echo "Aborted."
    exit 1
  fi
fi

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/codescope-publish.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT

retry_delay() {
  local log_file=$1
  local retry_at retry_epoch now

  retry_at=$(sed -n 's/.*Please try again after \(.* GMT\) and see .*/\1/p' "$log_file" | tail -n 1)
  if [[ -n "$retry_at" ]]; then
    if retry_epoch=$(LC_ALL=C date -j -u -f '%a, %d %b %Y %H:%M:%S GMT' "$retry_at" '+%s' 2>/dev/null); then
      :
    elif retry_epoch=$(LC_ALL=C date -u -d "$retry_at" '+%s' 2>/dev/null); then
      :
    else
      retry_epoch=""
    fi
  fi

  if [[ -n "${retry_epoch:-}" ]]; then
    now=$(date -u '+%s')
    if (( retry_epoch > now )); then
      echo $((retry_epoch - now + 5))
      return
    fi
  fi

  # crates.io currently applies a ten-minute publishing window. Add a small
  # buffer when its timestamp cannot be parsed on the host platform.
  echo 605
}

for package in "${PACKAGES[@]}"; do
  attempt=1
  while true; do
    log_file="$tmp_dir/$package.log"
    echo
    echo "Publishing $package@$version (attempt $attempt)..."

    set +e
    cargo publish --locked -p "$package" 2>&1 | tee "$log_file"
    cargo_status=${PIPESTATUS[0]}
    set -e

    if (( cargo_status == 0 )); then
      break
    fi

    if grep -Fq "crate $package@$version already exists on crates.io index" "$log_file"; then
      echo "$package@$version is already published; continuing."
      break
    fi

    if grep -Fq "429 Too Many Requests" "$log_file"; then
      delay=$(retry_delay "$log_file")
      echo "crates.io rate limit reached; retrying $package in $delay seconds."
      sleep "$delay"
      ((attempt += 1))
      continue
    fi

    echo "error: publishing $package@$version failed; fix the error and rerun this script" >&2
    exit "$cargo_status"
  done
done

echo
echo "Published all workspace crates at version $version."
