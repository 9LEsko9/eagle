# Desktop beta: implementation and acceptance

This record separates implemented behavior, automated evidence, and hardware acceptance. It does not declare the full desktop beta accepted.

The incoming working tree was preserved in local commit `9735e02` (parent `4824682`). Its provenance is the incoming uncommitted work, not newly authored changes from this run. The original patch, `instance.rs` patch and first test logs are kept locally in `.review/desktop-beta-start/`. No changes were pushed.

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

Schema upgrades are sequential: v5 access, v6 outbox, v7 durable audio ownership. The v4 fixture includes existing history, downloads and settings. A complete download fixture does not demonstrate the provider's current live contract.

## Yandex contract gate

See [yandex-contracts.md](yandex-contracts.md). Experimental revision-aware playlist methods are explicitly named `*_unverified` and are not connected to production UI. A failed response after a playlist write is uncertain and must not be blindly retried.

Live radio sessions, feedback/history delivery, revision conflict behavior and explicit access revocation/restoration still require provider evidence. Their absence blocks the full beta; a local fixture or an unofficial client implementation does not confirm live-server behavior. Existing playlists, likes and real listening events were not changed by this work.

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

Package/interrupt wakeups in the JSON belong to the whole process. They cannot prove zero UI wakeups or separate audio activity. Other development workloads and foreground/occlusion state were not controlled, so the short CPU samples are diagnostic observations, not a stable performance benchmark. The fixture markers label commands/stages; they do not independently prove audible output. Slint 1.17's public graphics notifier does not expose native Metal identity; selecting Skia alone does not establish the actual API or successful fallback.

To repeat the current isolated macOS measurement after a normal release build:

```sh
python3 bench/desktop.py --binary target/release/eagle-desktop \
  --fixture-root /tmp/eagle-desktop-new-gpu --output /tmp/eagle-gpu.json
python3 bench/desktop.py --binary target/release/eagle-desktop \
  --fixture-root /tmp/eagle-desktop-new-software --output /tmp/eagle-software.json --software
```

Use new fixture directories and avoid concurrent profiling/builds. The script rejects failed, incomplete or stage-overlapping runs and records the binary hash. The evidence currently records 163 passing workspace tests, clippy with `-D warnings`, formatting, seven Python checks and a workspace/all-target check with Rust 1.92. These checks do not substitute for the outstanding platform, contract and resource gates.
