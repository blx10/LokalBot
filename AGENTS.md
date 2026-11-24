# LokalBot

LokalBot is a private work-memory app for macOS, with a Windows and Linux app (`LinuxWindows/`, Rust + GPUI) in alpha and iPhone, iPad and Android companion apps (`Companion/`). Meetings are one input. Preserve local processing defaults, capture exclusions, retention behavior, and separate consent boundaries for meeting-library access, screen memory, remote inference origins, Agent Mode actions, and sharing with phones and tablets. [PRIVACY.md](PRIVACY.md) defines the data and network contract.

## Working agreements

- Complete the requested scope through relevant verification and correction of failures caused by the change. Reuse authorization already given; ask when a material decision or action exceeds that scope. Keep reviews and audits read-only unless implementation is requested.
- Open pull requests as drafts while still pushing fixes, and mark them ready for review when complete. The required UI suite (about an hour of macOS runner time) runs only on pull requests that are ready for review.
- Preserve unrelated work. When committing is authorized, stage only task files. Publication, releases, and production changes require authorization covering those actions.
- **Sensitive organization/personnel data (GitHub prohibition):** Never commit, push, or upload any information related to any employer, bank/company, or personnel/employees (names, accounts, internal details, logs, screenshots, or other references) to GitHub (including commits, pull requests, issues, or comments). If any such information is detected in staged/changed files or requested to be uploaded to GitHub, stop immediately and warn the user.
- **Atomic feature isolation:** Every new feature must be self-contained and atomically isolated from other fork features—both at the git branch level (branching cleanly from `upstream/master` when targeting upstream PRs, without stacking unrelated features) and at the code/schema/UI/i18n level (no cross-feature config fields, dead UI controls, or shared wiring to unmerged features) so any feature can be opened as an independent upstream PR or dropped without affecting others.
- **Fork feature summary table ("özet tablo"):** Whenever the user asks for "özet tablo", inspect the current fork branches/stack, `FORK_PATCHES.md`, and code-level coupling against `upstream/master`, and present an up-to-date Markdown table with the columns `Grup`, `Dallar`, and `Upstream PR durumu`.
- Run checks relevant to the change and required by the repository. Repeat or broaden them when new changes, failures, or unresolved risks justify it. Documentation-only edits need link and diff checks, not an app build.
- Never run UI tests locally on this MacBook. Use hosted CI or a remote runner; `Scripts/test/ui-tests.sh --remote` is the explicit remote entry point. Local compilation and non-UI tests are allowed. Verify the tested revision includes the requested changes.
- When a change alters what an existing test expects, list it under the pull request template's **Changed test expectations** with the reason.
- When fixing a bug that reached users, add the check that would have caught it — a capture replay trace, a model scenario, a day-in-the-life assertion, or an upgrade check — and confirm it fails with the fix undone. See [Testing](Docs/development/testing.md).
- Code carries no comments. Put a non-obvious decision in its area's ADR under `Docs/decisions/`, file a known shortcut as an issue, and say the rest in code. The Lint workflow's comment ratchet (`Scripts/ci/comment-ratchet.py`) fails when a file gains comments; the `/delete-comment` skill (`.claude/skills/delete-comment/SKILL.md`) removes the ones left. Lint and formatter directives, shebangs and license notices are not comments.
- When asked to reinstall the installed app, use `Scripts/build/reinstall-preserve-permissions.sh`. Preserve its identity and permissions; do not use a delete-first installation.
- `project.yml` owns the generated Xcode project. Run `xcodegen generate` after changing project configuration or adding/removing source files. Preserve dependency pins unless the task calls for an update or a verified incompatibility blocks it.

## References by task

Read the relevant sections when the task touches them:

- Build, schemes, CLI packaging, runtime, or tests: [DEVELOPMENT.md](DEVELOPMENT.md), which links one page per topic under `Docs/development/`.
- Capture, retention, or data leaving the computer: [PRIVACY.md](PRIVACY.md).
- The Windows and Linux app: [LinuxWindows/README.md](LinuxWindows/README.md).
- The phone and tablet apps, their sync protocol, or what the Mac shares with them: [Companion/README.md](Companion/README.md).
- Signing, release artifacts, or publication: [RELEASING.md](RELEASING.md); setup and the non-Mac channels are under `Docs/releasing/`.
- Website changes: [Website/AGENTS.md](Website/AGENTS.md).
- Where a new file goes: [Project layout](Docs/development/project-layout.md). Scratch output (previews, exports, drafts, evidence you don't commit) goes in `Local/`, which git ignores, not in new top-level folders.
- Video work: [Video/AGENTS.md](Video/AGENTS.md) and the selected project's guide.
- Retrieving recorded work through the CLI or MCP: [.agents/skills/lokalbot-cli/SKILL.md](.agents/skills/lokalbot-cli/SKILL.md).

## Fast Windows Rust builds & tests

- Always run `cargo` from inside `LinuxWindows/` (`Push-Location LinuxWindows` / `cd LinuxWindows`), never from the repo root with `--manifest-path LinuxWindows/Cargo.toml`. Cargo resolves `.cargo/config.toml` from `cwd`, so running from the repo root skips `LinuxWindows/.cargo/config.toml` (`+crt-static`), invalidates the `target/` cache, and forces a full 400+ crate rebuild.
- Never override `RUSTFLAGS` without preserving `-C target-feature=+crt-static` (required by `sherpa-onnx`).
- Keep test commands scoped: run lightweight crates (e.g., `lokalbot-i18n`, `lokalbot-core`) separately from `lokalbot-engine`, and use `cargo check` for quick compile checks.
- Machine-local accelerators (`sccache`, `lld-link`) belong in `%USERPROFILE%\.cargo\config.toml`, not in tracked `LinuxWindows/.cargo/config.toml`. Ensure `rustup` default host is `x86_64-pc-windows-msvc` (`rustup set default-host x86_64-pc-windows-msvc`) so `LinuxWindows/rust-toolchain.toml` resolves to MSVC rather than `-gnu`.

Report whether the requested result was built, tested, installed, or released, with any concrete blocker and remaining work. A successful build or CI run alone does not establish installation or release completion.

## Fork & upstream sync policy (`blx10/LokalBot`)

This repository is the `blx10/LokalBot` (`origin`) personal fork of `stevyhacker/LokalBot` (`upstream`). Keep upstream updates and fork-only customizations cleanly separated at all times:

- **Read-only `upstream` remote**: `upstream` (`https://github.com/stevyhacker/LokalBot.git`) is read-only (`push` URL MUST remain `DISABLED`). Never push directly to `upstream`.
- **Branch from `upstream/master` for every feature / PR**: Create every new feature branch or PR targeting `stevyhacker/LokalBot` directly from `upstream/master` (`git checkout -b feat/<name> upstream/master`). Never branch an upstream PR off `master`, and never include fork-only files (`AGENTS.md` fork section, `FORK_PATCHES.md`, `Scripts/fork/sync-upstream.sh`, `.agents/skills/fork-sync/SKILL.md`) in any PR to `stevyhacker/LokalBot`.
- **Linear rebased `master`**: `master` MUST remain a linear patch stack on top of `upstream/master` (`upstream/master` → active `feat/*` commits → single fork-meta commit at tip). Never create merge commits from `upstream/master` into `master`; push updated `master` with `git push --force-with-lease origin master`.
- **Auto-update permanently disabled on test & feature builds**: Every test, feature, or portable Windows build produced on this fork MUST compile with `--features disable-auto-update` (which disables update checking, feed overrides including `LOKALBOT_UPDATE_FEED_URL`, download/preparation, and Uninstall registry version writes) so upstream GitHub releases never overwrite it. Keep `disable-auto-update` opt-in (off in `default` features) so upstream-targeted PR branches leave standard upstream release behavior unchanged. Never overwrite, replace, or update the user's installed binary (`%LOCALAPPDATA%\Programs\LokalBot\lokalbot.exe`).
- **Mandatory sync & auto-drop workflow**: Whenever syncing with `upstream`:
  1. Fetch latest refs: `git fetch upstream --tags --prune && git fetch origin --prune`.
  2. Check merged upstream PRs (`gh pr list --repo stevyhacker/LokalBot --author blx10 --state merged`) AND inspect `git log --oneline <previous-base>..upstream/master` for any feature now implemented upstream (even under a different PR or commit).
  3. Drop every feature/commit whose PR merged or whose capability landed in `upstream/master`, update `FORK_PATCHES.md` to record the drop, and delete obsolete local/remote feature branches and worktrees when safe.
  4. Rebase remaining active `feat/*` branches onto `upstream/master`, rebuild `master` linearly (`git checkout -B master upstream/master`, apply remaining active feature commits, apply the fork-meta commit), verify with relevant tests, and push with `--force-with-lease`.
- **Automation & skill**: Run `Scripts/fork/sync-upstream.sh --check` (or follow [.agents/skills/fork-sync/SKILL.md](.agents/skills/fork-sync/SKILL.md)) for every upstream sync.
- **Turkish sync triggers**: User phrases such as `"origine göre güncelle"`, `"orjinal repo ile güncelle"`, `"orijinal repo ile güncelle"`, or `"upstream'i sync et"` all mean syncing this fork against `upstream` (`stevyhacker/LokalBot`) using the workflow above.
- **Strict anonymization of company, service, and local/internal addresses**: Never commit, push, or leave in code, tests, docs, or configs any bank/company name, internal service or gateway name, real internal hostname/domain (including `.local` or cluster subdomains), or real internal IP address. Always use generic, non-identifiable placeholders (e.g., `ai-gateway.example.local`, `10.0.0.10`, `user@example.local`).
