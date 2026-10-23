#!/usr/bin/env bash
set -euo pipefail

# Run the repository's authoritative Conversation Engine v2 checks locally against
# the checked-out SHA. GitHub Actions remains the source of truth for disposable
# PostgreSQL and Chromium execution.
root=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$root/final-review-artifacts"
printf '%s\n' "sha=$(git -C "$root" rev-parse HEAD)" > "$root/final-review-artifacts/local-gates.txt"
(
  cd "$root/backend"
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test --workspace --locked
) 2>&1 | tee -a "$root/final-review-artifacts/local-gates.txt"
(
  cd "$root/frontend"
  npm run lint
  npm test
  npm run build
) 2>&1 | tee -a "$root/final-review-artifacts/local-gates.txt"
git -C "$root" diff --check
printf '%s\n' 'git diff --check: passed' | tee -a "$root/final-review-artifacts/local-gates.txt"
