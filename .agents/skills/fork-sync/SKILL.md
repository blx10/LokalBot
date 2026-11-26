---
name: fork-sync
description: Sync blx10/LokalBot fork with upstream stevyhacker/LokalBot, auto-drop merged or upstream-superseded features, and rebase active fork patches linearly. Trigger on "origine göre güncelle", "orjinal repo ile güncelle", "orijinal repo ile güncelle", "upstream'i sync et", or any request to update from the original repo.
---

# Fork & Upstream Sync (`blx10/LokalBot`)

Follow this workflow whenever the user asks to sync with `upstream` (`stevyhacker/LokalBot`), pull upstream releases, clean up merged PRs in the fork, or says `"origine göre güncelle"`, `"orjinal repo ile güncelle"`, `"orijinal repo ile güncelle"`, or `"upstream'i sync et"`. Note: in user requests, "origin" / "orjinal repo" refers to the upstream source repo (`stevyhacker/LokalBot`, git remote `upstream`), not the fork remote (`origin` = `blx10/LokalBot`).

## Non-negotiable rules

1. **Never push to `upstream`**: Confirm `git remote get-url --push upstream` is `DISABLED`. If not, set `git remote set-url --push upstream DISABLED`.
2. **Keep `master` linear**: Never merge `upstream/master` into `master`. Always rebuild `master` cleanly onto `upstream/master` with active feature commits followed by the single fork-meta commit (`AGENTS.md` policy, `FORK_PATCHES.md`, `Scripts/fork/sync-upstream.sh`, `.agents/skills/fork-sync/SKILL.md`).
3. **Never leak fork-only files into upstream PRs**: Any branch targeting `stevyhacker/LokalBot` MUST branch from `upstream/master` directly and MUST NOT contain fork-meta commits or files.

## 4-step sync workflow

1. **Inspect state**:
   ```bash
   Scripts/fork/sync-upstream.sh --check
   ```
   Review:
   - `git cherry -v upstream/master master` (`-` commits are already upstream by patch-id).
   - `gh pr list --repo stevyhacker/LokalBot --author blx10 --state merged`.
   - New `upstream/master` commits since the fork's base (`git merge-base upstream/master master..upstream/master`) against [FORK_PATCHES.md](../../FORK_PATCHES.md) to catch features upstream implemented independently.

2. **Drop merged or superseded features**:
   - Exclude any feature/commit from `master` whose PR merged upstream or whose functionality now exists in `upstream/master`.
   - Remove obsolete `origin/feat/*` branches and clean up finished worktrees (`git worktree remove ...`) after confirming nothing uncommitted remains.

3. **Rebase remaining active features & rebuild `master`**:
   - Rebase each remaining active `feat/*` branch onto `upstream/master`:
     ```bash
     git rebase upstream/master <feat-branch>
     ```
   - Rebuild `master` linearly from `upstream/master`:
     ```bash
     git checkout -B master upstream/master
     # cherry-pick remaining active feature commits in stack order
     # cherry-pick the fork-meta commit (or update FORK_PATCHES.md & commit with repo-local identity `blx10 <19878591+blx10@users.noreply.github.com>`)
     ```
   - Update `FORK_PATCHES.md` so **Active on `master`**, **Open Upstream PRs**, and **Dropped** reflect the new state. Commit fork-meta updates using the repo-local GitHub noreply identity (`blx10 <19878591+blx10@users.noreply.github.com>`).

4. **Verify & push**:
   - Run relevant test suites (`cargo test` in `LinuxWindows/` and/or macOS non-UI `xcodebuild test` if macOS code changed).
   - Verify `git log --oneline --graph upstream/master..master` is strictly linear with zero merge commits.
   - Push updated `master` (and any rebased active `feat/*` branches) with `git push --force-with-lease origin master ...`.
