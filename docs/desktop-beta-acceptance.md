# Desktop beta: implementation and acceptance

This record separates implemented behavior, automated evidence, and hardware acceptance. It does not declare the full desktop beta accepted.

Completion states remain separate: **code and all required automatic checks ready — not yet** (rename/feedback contracts and cross-platform CI remain open); **full desktop beta accepted — no** (resource budget and hardware gates remain open). Passing local checks of the implemented subset does not change either full-plan status.

The incoming working tree was preserved in commit `9735e02` (parent `4824682`). Its provenance is the incoming uncommitted work, not newly authored changes from this run. The original patch, `instance.rs` patch and first test logs are kept locally in `.review/desktop-beta-start/`. Preservation and initial integration were performed locally before the user requested a pull request.

The measured implementation is local commit `b7037d2`. Its normal release binary has SHA-256 `c422dda9925c531bd7b739b25231c30d8c449880133897ca165b1f50d412cd97`.

The subsequent playlist/radio implementation is commit `97b3089`; its normal release binary has SHA-256 `105f0d79c1adc32533e5c3cb1794440fbd8c460451e58cdbd825cbc72cf35639`. Its final workspace run passed 183 tests; formatting, clippy, Rust 1.92 workspace/all-targets, seven Python tests and release build passed.

## Latest native run: blocked before launch completion

The final native fixture attempt timed out after 120 seconds without reaching its first stage; it produced no valid memory or CPU samples. A controlled comparison ran the older `b7037d2` binary and the new binary for 12 seconds each: neither reached the first marker. All owned fixture processes were stopped.

An independent minimal AppKit program reproduced missing `applicationDidFinishLaunching`, while its main queue and a prelaunch timer worked. Winit waits for that notification before dispatching the initial Slint events. In the same tool context `CGSessionCopyCurrentDictionary()` returned NULL, `scutil` reported an invalid destination port, Launch Services failed to open an existing temporary app, and Computer Use reported service startup failure. The user confirmed that the desktop was unlocked. These observations establish an unresolved native/session-service access problem in this execution context; they do not establish its root cause or prove a new Eagle regression. Production launch notifications were not fabricated and system services were not modified.

Repeat the native fixture from the user's normal graphical terminal after restoring access. A headless screenshot pass or a successful compile does not resolve this blocker. The [measurement record](measurements/desktop-beta-release.json) retains this failed attempt separately from earlier valid samples.

### User-session startup recovery and title bar

The real player had survived a WindowServer restart and retained both single-instance locks. Ordinary new launches therefore activated that orphan and exited. After a private SQLite backup and termination of only the verified old process, the user launched the new release from Terminal and confirmed that its window appeared. This restores ordinary startup; automated native fixtures in the tool context remain unverified. The code does not yet automatically recover an orphan after a graphical-session failure.

The native macOS title bar now takes the selected theme's opaque background, with a matching light/dark appearance. Auto mode releases the native appearance override and reconciles the actual system theme through a weak, deferred callback. It adds no polling timer. The user confirmed the visible title-bar correction on the new release (SHA-256 `cce4ea9a4355c6c1bd7fa65babb64f9843355401b0bb081c0cf947269f35bb66`). UI tests (8), formatting, UI/all-targets clippy with `-D warnings`, Rust 1.92 and release build passed. The complete native theme-switch/fullscreen/reopen matrix has not been performed.

### Pull-request review

Three `gpt-6.1-terra` reviewers performed six focused passes: shell/platform lifetime; storage/downloads/account isolation; Yandex/Session contracts; UI interaction; CI/platform configuration; and engine/covers/measurement code. Findings received follow-up review by a different reviewer. These were local source and fixture reviews, not hardware acceptance or completed remote CI.

- Recovered storage is locked before Session opens it. Relocation holds an opaque destination lease before copying and through Session shutdown; source activation files are excluded from the copy. Existing reserved lock files can be reclaimed after process exit, and lock files are never unlinked during acquisition failure.
- Save and Save All requeue cancelled jobs with a new revision. Running, AudioReady and Saved jobs retain their state; stale completion remains rejected.
- A newly created playlist uses the ordinary snapshot-loading route, including its read-only fallback on retrieval failure.
- Track-menu items accept keyboard focus and Enter/Space. A headless test exercises activation after opening; complete native Tab traversal remains unverified.
- Cancelling artwork releases the cover worker while an existing network worker may finish its socket operation. A stalled local HTTP fixture checks that dropping Covers returns within two seconds and does not write the cache. This does **not** bound full process shutdown: DNS, Client destruction and other outstanding network work may still delay it.

Relocation limitations remain explicit: a failed attempt retains its destination lease until the process exits, and retry after a partial copy requires a new empty destination. Recovery keeps the source authoritative before the durable switch; it does not automatically clear the partial destination. The lease change does not establish general power-loss or cross-volume acceptance.

The reviewed changes passed 189 workspace tests, workspace/all-target clippy with `-D warnings`, formatting, seven Python tests and Rust 1.92 workspace/all-target checking. Clippy initially rejected an incomplete HTTP-header read in the new cancellation fixture; after correcting that fixture, its focused test and the complete clippy/MSRV checks passed again. Logs and detailed reviewer reports remain local under `.review/`. These fixes have not received a new native release measurement or platform CI run.

PR #5 subsequently published `42b6d39`. Its first CI run passed the complete macOS desktop job, Android, iOS and Linux MSRV. Compiler logs identified Linux's new `chunks_exact_to_as_chunks` lint and Windows' cfg-only unused `sync_dir` argument. The subsequent changes use `as_chunks_mut` (checked with Rust 1.92) and name the conditional argument `_path`; neither change suppresses warnings. A new remote run is required to accept these platform fixes. The dependency job failed on the `ksni` Unlicense and a new Tokio-to-tracing edge. A package-specific license exception and a Tokio wrapper exception retain the direct-tracing ban. Full local cargo-deny 0.20.2 checking then passed, including advisories, licenses, bans and sources; unchanged iOS policy also passed. Cargo-audit 0.23.0 found no vulnerabilities against RustSec snapshot `c03ed03`, but that local run did not check yanked versions. These observations do not replace the next remote CI run.

The conflict with main was a squash-history divergence: `4824682` and main differed only in `instance.rs`, whose main version was already present in preserved commit `9735e02`. A reconciliation merge records main ancestry without changing the reviewed source tree.

The follow-up CI on `e404941` passed dependencies, Android, iOS and MSRV. Windows then reached the tray builder and exposed a macOS-only `with_icon_templated` call. Tray construction now uses that method only under macOS and the ordinary `with_icon` method under Windows; the compiler-suggested `with_icon_as_template` was avoided because it is deprecated and takes a boolean. Formatting and dependency-source verification passed; Windows compilation still requires the next CI run.

## Implemented paths

| Area | Behavior | Evidence required for acceptance |
| --- | --- | --- |
| Window lifetime | A persistent shell owns Session and recreates the Slint window and bridge. Snapshot gating rejects an old window generation. Detach clears decoded cover caches and invalidates in-flight results; reattach restores artwork from metadata. | Weak-window, cover suspension and snapshot tests pass. Repeated real-window measurements below fail the resource budget. |
| Media and tray | Media controls have their own owner thread. Windows uses an independent native HWND. macOS/Windows use tray-icon menus; Linux uses ksni without GTK for its tray. | Actual media keys, system player, tray and watcher behavior on each target OS. |
| Exit and preferences | Closing normally leaves background playback; an explicit preference exits. Autostart is disabled by default and changes OS registration when selected. Fixtures cannot register autostart. | Real OS enable/disable, notification delivery and application termination. |
| Saved audio | Durable account/track access decisions have no offline expiration. Auth/network/ambiguous provider answers never revoke access. Files are preserved after revocation. | Migration and account-isolation tests; Yandex's explicit revocation contract remains unconfirmed. |
| Ownership | Schema 7 stores audio ownership separately from transient download states. An unowned shared cache file requires current-account provider authorization before reuse. | Cross-account cache-hit and partial-range tests, including cancellation and restart. |
| Downloads | Two physical workers, revision-checked completion, resumable staging, AudioReady metadata phase, and cancellable reads. | HTTP fixtures and the 100-track crash test; actual-account run remains separate. |
| Storage move | Writers and readers are stopped before checksummed copy. Bootstrap journal and active path are outside the managed root. Before a durable switch recovery selects the source; after it selects the destination. Old files are retained. | Fault injection, v4 migration and runtime Session move tests. Real cross-volume/disk-full and Windows replacement acceptance remain required. |
| History | Local history and account-scoped pending outbox events are created in one SQLite transaction. Ambiguous events have an explicit uncertain state. | Transaction rollback and isolation tests. No real history or radio events are submitted automatically. |
| UI | Compact rows share the virtualization pitch; queue movement follows occurrence IDs. Download names and AudioReady actions are displayed. | Current UI tests and wide/narrow screenshots; hardware keyboard/drag checks remain required. |

Schema upgrades are sequential: v5 access, v6 outbox, v7 durable audio ownership, v8 local radio context for play intents. History, the outbox item and its radio context commit together. The v4 fixture includes existing history, downloads and settings; a v7 fixture verifies the subsequent upgrade. A complete download fixture does not demonstrate the provider's current live contract.

Legacy pinned audio without a Saved/AudioReady download job has no trustworthy account ownership in the old schema. Migration retains the file and its pin but does not assign it to the currently stored UID. Offline playback of that file is denied until online authorization establishes an owner. This confirmed compatibility gap remains open; assigning every old pin to the current account would transfer rights between accounts.

The 100-track test kills a separate child process and restarts the same test executable. It resumes synthetic HTTP objects, then decodes the saved audio and pre-seeded artwork. It does not establish application-version upgrade recovery, cancellation during the crash scenario, 100 complete audible plays, real Yandex downloads, or network artwork retrieval. Relocation fault injections exercise journal boundaries, not hardware power-loss durability.

## Yandex contract gate

See [yandex-contracts.md](yandex-contracts.md). Live tests on disposable private playlists confirmed insert/delete, duplicate ordering, one direction of move and rejection of a stale revision. Provider and UI now connect revision-aware positional edits, independently read the result, and reject incomplete snapshots. Mutation busy state and uncertain notices survive window recreation. Rename was exercised live but remains disconnected because its revision semantics were not recorded. A failed response after a playlist write is uncertain and must not be blindly retried.

With explicit user permission, a radio session and its next batch were fetched without playback or feedback. Both returned five tracks; overlap between batches is allowed. The radio screen fetches only on explicit action, preserves per-occurrence account/session/batch context in the playback checkpoint, and manually extends a matching active radio queue. After application restart the saved queue retains context, but fetching more requires a new radio session. Feedback/history delivery and explicit access revocation/restoration still lack provider evidence; server conflict error classification and rename revision semantics remain incomplete. These gaps block the full beta. Existing playlists, likes and real listening events were not changed by this work; disposable test playlists were removed and their absence checked.

The user reported successful sign-in and ordinary playback. This is a useful macOS smoke result, not acceptance of media keys, background playback, restart recovery or other platforms. No credential, signed URL or personal response was added to the repository.

An independent review reproduced an audible-position race: a position for the next engine track could previously record the earlier track's radio batch. History now follows the audible engine index and occurrence; late/missing `TrackChanged` and repeated tracks have regression coverage.

UI fixtures include wide/narrow playlist editing, conflict, and empty/populated/offline radio states. The native screenshot runner stalled during this run, so layout checks use `cargo run -p eagle-ui --example screenshots -- /tmp/eagle-preview --headless`, backed by Slint's actual software renderer. This does not establish native focus, popup interaction, GPU behavior or hardware rendering. The preview now returns an error for repeated capture failure instead of silently looping on it.

## Reproduction

Run from the integration checkout, not simultaneously from worktrees sharing the same target directory:

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
python3 -m unittest discover -s bench
cargo build --release -p eagle-desktop
./target/release/eagle-desktop --fixture-root /tmp/eagle-isolated-new-root --fixture-smoke
./target/release/eagle-desktop --fixture-root /tmp/eagle-isolated-software-root --fixture-smoke --software
```

Fixture roots must be new or contain the `.eagle-fixture` marker. They contain generated audio and 100 deterministic tracks, use an injected provider, and do not consult Keychain. `--fixture-smoke` exercises playback, pause, window destruction, repeated reopen and exit; `smoke-stage` records the current phase. The fixture's sine-wave seed is not the HTTP crash test: the latter uses valid MP3 data and a child process in `crates/app/tests/desktop_offline.rs`.

Local HTTP and instance tests require permission to bind loopback sockets. A sandbox `PermissionDenied` before the fixture starts is not an application assertion failure; rerun with loopback permission and preserve both results.

## Acceptance gates still requiring hardware

- Windows, macOS and Linux media keys and system player without a window; tray, repeated activation and autostart. Linux additionally needs Wayland/X11 and watcher disappearance/reappearance.
- The real-account 100-track interrupted download, upgrade and offline playback scenario with artwork and metadata.
- GPU and software release runs: open idle, playback, minimized, and repeated window destruction/reopen. Record binary hash, machine, actual API where exposed, physical footprint, CPU, wakeups and GPU resources.
- After warmup, background footprint must be at most 30 MiB, with no periodic UI wakeups and released renderer/GPU resources. Audio wakeups are counted separately. Other metrics are compared with a same-machine desktop baseline without invented thresholds.
- Animation timers must stop within two seconds after the animation ends; visible-player progress is measured separately.

The previous `desktop-beta-core.json` contains CLI measurements only. It is not desktop baseline evidence. Initial release baseline compilation overlapped later source edits and therefore was not accepted as a baseline; any reconstruction from the preserved source must be identified as such and instrumented identically for comparison.

## Measured release result: blocked

The [release measurement record](measurements/desktop-beta-release.json) contains actual desktop measurements on the same MacBook Air M2 with 8 GiB, macOS 27.2. The baseline was reconstructed from preserved commit `9735e02` with isolated fixture instrumentation. Both binaries ran real windows and audio. It was not a baseline captured before editing began.

| Physical footprint, MiB | Baseline Skia | Current Skia | Baseline software | Current software |
| --- | ---: | ---: | ---: | ---: |
| Open idle | 88.8 | 67.5 | 36.2 | 44.5 |
| Open playback | 98.6 | 75.0 | 46.0 | 49.5 |
| Window destroyed, playback | — | 38.8 | — | 36.2 |
| Window destroyed, paused | — | 39.3 | — | 31.0 |
| Destroyed again after reopen | — | 39.0 | — | 34.8 |

The **30 MiB requirement failed in both backends**, including after repeated reopen. The software open-window footprint also increased relative to baseline. These final runs include the decoded-cover detach change; the fixture has no artwork. Memory varied between runs, so these are individual observations rather than a statistical estimate. The baseline minimized its window instead of destroying it; those phases are not equivalent comparisons.

Heap stacks identify approximately 7.17 MiB of retained ColorSync transfer curves, a roughly 3.4 MiB live audio ring buffer, and approximately 1.69 MiB of Skia allocations. This does not prove a safe way to reclaim them. The audio buffer was not shortened to improve a UI metric. `vmmap` still reported IOAccelerator memory; full GPU release remains open.

Two additional isolated diagnostics did not resolve the limit. macOS `malloc_zone_pressure_relief` reported 0 reclaimed bytes, with footprint unchanged at 40,584,200 bytes. A same-binary Skia global-cache control/treatment pair measured closed-playing 38.89/37.42 MiB and closed-again 37.66/37.95 MiB. The global font cache was only 80,357 bytes and the resource cache was empty; neither run met 30 MiB. These temporary probes and their direct Skia dependency were removed. The measured revision above predates the subsequent playlist/radio integration; its numbers remain evidence of an unmet gate, not measurements of the later feature build.

Package/interrupt wakeups in the JSON belong to the whole process. They cannot prove zero UI wakeups or separate audio activity. Other development workloads and foreground/occlusion state were not controlled, so the short CPU samples are diagnostic observations, not a stable performance benchmark. The fixture markers label commands/stages; they do not independently prove audible output. Slint 1.17's public graphics notifier does not expose native Metal identity; selecting Skia alone does not establish the actual API or successful fallback.

To repeat the current isolated macOS measurement after a normal release build:

```sh
python3 bench/desktop.py --binary target/release/eagle-desktop \
  --fixture-root /tmp/eagle-desktop-new-gpu --output /tmp/eagle-gpu.json
python3 bench/desktop.py --binary target/release/eagle-desktop \
  --fixture-root /tmp/eagle-desktop-new-software --output /tmp/eagle-software.json --software
```

Use new fixture directories and avoid concurrent profiling/builds. The script rejects failed, incomplete or stage-overlapping runs and records the binary hash. The integrated playlist/radio subset has 183 passing workspace tests, clippy with `-D warnings`, formatting, seven Python checks and a workspace/all-target check with Rust 1.92. The final album-title adjustment also has a focused provider regression test. These checks do not substitute for the outstanding platform, contract and resource gates.
