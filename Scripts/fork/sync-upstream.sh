#!/usr/bin/env bash
set -euo pipefail

DO_FETCH=1
for arg in "$@"; do
  case "$arg" in
    --check) ;;
    --no-fetch) DO_FETCH=0 ;;
    -h|--help)
      cat <<'EOF'
Usage: Scripts/fork/sync-upstream.sh [--check] [--no-fetch]

Inspects fork alignment with upstream (stevyhacker/LokalBot):
  - Verifies upstream push URL is DISABLED
  - Fetches upstream and origin (unless --no-fetch)
  - Reports ahead/behind counts vs upstream/master
  - Lists commits on master not yet in upstream (`git cherry -v`)
  - Checks GitHub PRs by blx10 on stevyhacker/LokalBot (if `gh` is available)
EOF
      exit 0
      ;;
    *)
      echo "Unknown option: $arg" >&2
      exit 2
      ;;
  esac
done

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

echo "==> Remote configuration"
UPSTREAM_PUSH="$(git remote get-url --push upstream 2>/dev/null || true)"
if [[ "$UPSTREAM_PUSH" != "DISABLED" ]]; then
  echo "WARNING: upstream push URL is '$UPSTREAM_PUSH' (expected 'DISABLED')."
  echo "Fix with: git remote set-url --push upstream DISABLED"
else
  echo "OK: upstream push URL is DISABLED."
fi

if [[ "$DO_FETCH" -eq 1 ]]; then
  echo ""
  echo "==> Fetching upstream and origin..."
  git fetch upstream --tags --prune
  git fetch origin --prune
fi

echo ""
echo "==> Upstream vs local master"
echo "upstream/master: $(git log -1 --oneline upstream/master)"
echo "local master:    $(git log -1 --oneline master)"
COUNTS="$(git rev-list --left-right --count upstream/master...master)"
BEHIND="${COUNTS%%$'\t'*}"
AHEAD="${COUNTS##*$'\t'}"
echo "Behind upstream/master: $BEHIND | Ahead of upstream/master: $AHEAD"

echo ""
echo '==> Patch status on master (- = equivalent patch already in upstream, + = local)'
git cherry -v upstream/master master || true

if [[ "$BEHIND" -gt 0 ]]; then
  echo ""
  echo "==> New commits in upstream/master (review for superseded local features)"
  BASE="$(git merge-base upstream/master master)"
  git log --oneline "$BASE..upstream/master" | head -n 40
fi

if command -v gh >/dev/null 2>&1; then
  echo ""
  echo "==> Upstream PRs by blx10 on stevyhacker/LokalBot"
  gh pr list --repo stevyhacker/LokalBot --author blx10 --state merged --limit 20 \
    --json number,title,headRefName,mergedAt \
    --template '{{range .}}MERGED #{{.number}} ({{.headRefName}}) {{.mergedAt}} - {{.title}}{{"\n"}}{{end}}' 2>/dev/null || true
  gh pr list --repo stevyhacker/LokalBot --author blx10 --state open --limit 20 \
    --json number,title,headRefName \
    --template '{{range .}}OPEN   #{{.number}} ({{.headRefName}}) - {{.title}}{{"\n"}}{{end}}' 2>/dev/null || true
fi

echo ""
echo "==> Active worktrees"
git worktree list

cat <<'EOF'

==> Linear rebuild workflow (when syncing):
  1. Drop any feature whose PR is MERGED or whose functionality landed in upstream/master.
  2. Rebase remaining active feat/* branches onto upstream/master.
  3. Rebuild master linearly:
       git checkout -B master upstream/master
       git cherry-pick <remaining active feat commits>
       git cherry-pick <fork-meta commit>
     Update FORK_PATCHES.md if any patch was dropped/added.
  4. Run relevant tests, then push with:
       git push --force-with-lease origin master
EOF
