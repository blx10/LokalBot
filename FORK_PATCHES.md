# Fork Patch Manifest (`blx10/LokalBot`)

Tracks fork-only features and upstream PRs layered on top of `upstream/master` (`stevyhacker/LokalBot`).
This file is **fork-only** and MUST NOT be included in pull requests sent to `stevyhacker/LokalBot`.

## Active on `master` (rebased onto `upstream/master`)

| Branch | Summary | Scope | Upstream PR | Status |
| --- | --- | --- | --- | --- |
| `feat/windows-outlook-lync` | Classic Outlook COM calendar provider, instant `lync.exe` + `SkypeMeetingsApp.exe` detection, PID-safe auto-stop, and on-prem ICS meeting URLs | Windows (`LinuxWindows/`) | — | `local-only` |
| `feat/speaker-voice-ux` | Speaker voice suggestion/enrollment thresholds, deleted-recording voice preservation, header speaker chip focus, voice similarity badges & quick-pick, Remembered People voice previews/merge, and Windows Active Directory person lookup & profile display name | macOS (`LokalBot/`) + Windows/Linux (`LinuxWindows/`) | — | `local-only` |
| `feat/windows-waveform-selection` | Windows (`LinuxWindows/`) port of meeting player waveform zoom/pan, 2-click range selection, trim & extract to new meeting | Windows (`LinuxWindows/`) | — | `local-only` |
| `feat/lokalbot-config-toml` | Unlocked bidirectional `lokalbot-config.toml` profile resolution/seeding, origin auto-approval, onboarding skip, API key masking/redaction, and `Settings → Models` configuration UI | Windows/Linux (`LinuxWindows/`) | — | `local-only` |
| `feat/asymmetric-multi-stt` | Bundled 3-model asymmetric STT (`whisper-turbo`, `whisper-turbo-tr-khan`, `zipformer-oayk`) + sequential `MultiSttRunner` without ROVER voting and model license notices | Windows/Linux (`LinuxWindows/`) + `THIRD_PARTY_NOTICES.md` | — | `local-only` |
| `feat/remote-post-correction-think` | Two-stage remote LLM post-correction (`gemma-4-31B-it` / `gpt-oss-120b` critics + `Qwen3.8-27B` judge + `asr-post-correction-audit.json`), remote OpenAI-compatible Think key precedence + `disable_local_llama_server`, and cross-crate pipeline wiring | Windows/Linux (`LinuxWindows/`) | — | `local-only` |
| `fork-meta` (`disable-auto-update`, `origin-badge`) | Fork policy, patch manifest, sync skill/script, opt-in `disable-auto-update` Cargo feature in `lokalbot-update` / `lokalbot-app`, and fork-only `origin` build badge in `lokalbot-app` (`build.rs`, `shell.rs`, `main.rs`) | Fork-only (`AGENTS.md`, `FORK_PATCHES.md`, `Scripts/fork/sync-upstream.sh`, `.agents/skills/fork-sync/SKILL.md`, `LinuxWindows/crates/lokalbot-{update,app}`) | — | `fork-only` |

## Open Upstream PRs (kept on separate branches, not yet stacked on `master`)

_None — all open upstream PRs are currently stacked on `master`._

## Dropped (merged or superseded in `upstream/master`)

| Branch | Summary | Upstream PR | Merged / Dropped Date | Notes |
| --- | --- | --- | --- | --- |
| `feat/apple-speech` | Built-in macOS `AppleSpeechEngine` (`SpeechAnalyzer`) transcription engine | `stevyhacker/LokalBot#529` | 2026-11-19 | Merged into `upstream/master`; dropped from fork stack. Local worktree `Local/worktrees/apple-speech` and local branch `feat/apple-speech` removed. |
| `feat/meeting-waveform-selection` | Meeting player waveform zoom/pan, 2-click range selection, trim & extract to new meeting | `stevyhacker/LokalBot#574` | 2026-11-22 | Merged into `upstream/master` at `e8cce541` (all 5 commits including `352707c3`/`12d19383` LinuxWindows i18n catalog sync); dropped from fork stack. Windows port tracked separately on `feat/windows-waveform-selection`. |
