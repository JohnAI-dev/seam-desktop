# Seam Desktop

Your phone and your computer, as one. One codebase for Linux, Mac and Windows, built with
[Tauri](https://tauri.app): the phone logic is Rust (`crates/seam-core`), the window is
`src-tauri`, and the UI is plain HTML/CSS/JS in `ui/`.

## What it does today

- Finds your Android phone over USB or Wi-Fi (via `adb`) and shows its status.
- **Mirror**: opens your phone's screen in a window you can control (via `scrcpy`).
- **Phone app link (no USB debugging):** pair the Seam Android app by scanning a QR code;
  phone notifications and battery then show up on the computer over Wi-Fi, end-to-end
  encrypted (TLS with the certificate pinned via the QR code). Protocol: `protocol/PROTOCOL.md`.
- `adb` and `scrcpy` are **built in** (downloaded at build time by `scripts/fetch_tools.py`,
  pinned version, checksum-verified), so there is nothing else to install.

On Linux (incl. Arch) run the `.AppImage` from the latest release; on Mac open the `.dmg`.

## How it gets built (autonomous)

1. **Open an issue** (as the repo owner) describing a bug or feature.
2. **Grok engineer** writes the change, `scripts/format.sh` formats it, `scripts/test.sh` checks it:
   rustfmt, clippy (warnings are errors), unit tests, a full build, and **launching the real app**
   headless with `--self-test`, which must open the window, render the UI and call into Rust.
3. **Grok reviewer** (separate call, sees only issue + diff + test output) approves or rejects.
   Up to 3 attempts with feedback.
4. A PR is opened, **CI re-runs everything** on a clean machine, and it is **auto-merged**.
5. **Release**: a `.deb` and `.AppImage` are published to GitHub Releases, and the issue is closed
   with a link.

Mac and Windows installers: Actions → Release → Run workflow → tick "Also build Mac and Windows".
They are unsigned for now (Mac: right-click → Open the first time).

## Controls

- Issues from other people are ignored until the owner adds the `agent` label.
- `no-agent` label keeps the agent off an issue. Remove and re-add `agent` to re-run.
- The agent can never modify `.github/` or `scripts/`, and tests run without access to secrets.

## Configuration

- Secret `XAI_API_KEY`: your xAI API key. Variable `XAI_MODEL` (optional): defaults to `grok-4.7`.
- Settings → Actions → General → Workflow permissions: *Read and write* and
  *Allow GitHub Actions to create and approve pull requests*.

## Agent cost controls

Every LLM call logs its tokens and cost (from the API's own usage report) in the job log and
the job summary, and the spend is recorded on the `agent-ledger` branch. Repository variables
(Settings → Secrets and variables → Actions → Variables) tune it; defaults in brackets:

- `AGENT_BUDGET_ISSUE_USD` [2]: no new call once an issue has used this. Re-adding the `agent`
  label gives it a fresh budget.
- `AGENT_BUDGET_DAY_USD` [10]: per repository and UTC day; waiting issues continue the next day.
- `AGENT_MAX_ATTEMPTS` [3] per run, `AGENT_MAX_FAILED_RUNS` [1] automatic re-runs of a failed issue.
  CI/merge failures, crashes, timeouts and budget stops wait for a human.
- `AGENT_MAX_CONTEXT_BYTES` [80000]: the engineer sees the file tree plus the files named in the
  issue and up to 12 picked by a cheap selection call, not the whole repository.
- `AGENT_REASONING_EFFORT` [medium], `XAI_REVIEW_MODEL` / `XAI_SELECT_MODEL` [grok-build-0.1;
  falls back to `XAI_MODEL` if unavailable].
