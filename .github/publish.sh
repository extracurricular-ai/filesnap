#!/usr/bin/env bash
#
# Publish one crate, or say why it is not publishing it.
#
# A script rather than inline YAML because this is the step with no undo, and
# the logic — is it already there, is this a rehearsal — deserves to be
# readable and runnable on a laptop rather than assembled out of `run:` blocks
# nobody can execute outside CI.
#
#   .github/publish.sh filesnap            # publishes
#   DRY_RUN=true .github/publish.sh filesnap   # rehearses
set -euo pipefail

crate="${1:?usage: publish.sh <crate>}"
dry_run="${DRY_RUN:-false}"

version=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
echo "==> $crate $version"

# Already on the registry? Then this is a re-run of a release whose earlier
# half succeeded, and refusing here would leave the rest of it to be finished
# by hand — which, with no rollback, is exactly the situation to avoid.
#
# `|| true` on the curl: a registry that cannot be reached must not be read as
# "not published". An empty body falls through to the publish, which fails
# loudly and correctly if the version really is there.
existing=$(curl -sS --max-time 30 \
  -A "filesnap-release" \
  "https://crates.io/api/v1/crates/${crate}/${version}" 2>/dev/null || true)

if printf '%s' "$existing" | grep -q "\"num\":\"${version}\""; then
  echo "    already on crates.io — nothing to do"
  exit 0
fi

if [ "$dry_run" = "true" ]; then
  echo "    rehearsal: packaging and verifying, not uploading"
  cargo publish -p "$crate" --dry-run
  exit 0
fi

cargo publish -p "$crate"
echo "    published"
