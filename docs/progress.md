# Progress — start here

Resume point for a new session. `CLAUDE.md` is the design; this file is the
state. If they disagree, this file is newer.

**Last updated:** 2026-09-14
**CI:** green on `windows-latest` (fmt, clippy `-D warnings`, build, nextest, ratio gate)
**Tests:** 96 passing, ~0.2s, all on a fake clock
**Ratio gate:** 2.55:1 on `src/core/` against a 0.9 floor

---

## Where the project is

| # | Milestone | State |
|---|---|---|
| 1 | Repo, mirror remote, green CI | **done** |
| 2 | Packaging spike — gates everything | **done — unpackaged Win32 wins, no MSIX** |
| 3 | Bare connect + render, confirm audio flows | **done** — audio confirmed audible out of the PC on 2026-09-14 |
| 4 | `--debug-sessions`, resolve both VERIFY items | **done — both VERIFY items resolved 2026-09-14** |
| 5 | `core/` traits + `FakeConnection` + health state machine | **done** |
| 6 | `platform/` impls behind the traits, `recover()` wired | **done** — supervisor runs headless via `--run` |
| 7 | Re-arm triggers | **next** |
| 8 | egui UI + tray | not started |
| 9 | Config loading, autostart, logging | `Config` type exists and is tested; no file I/O, no autostart, no `tracing` yet |
| 10 | Overnight soak → `docs/soak.md` | not started |

### The one settled decision that gates everything

**No MSIX. Plain unpackaged Win32.** Every gate the `bluetooth`
DeviceCapability could be enforced at was cleared from an ordinary `cargo run`
exe: class activation, enumeration, `TryCreateFromId`, `Start()`, and `Open()`
returning `Success`. There is no packaging work in this project. Evidence and
the API corrections are in `docs/verify.md` — **read that file before touching
`platform/`**, it records three ways the CLAUDE.md lifecycle sketch was wrong.

---

## Do this next

### The hardware questions are answered — read this before `platform/`

Resolved on 2026-09-14 with the phone actually streaming. Full evidence and the
raw tables are in `docs/verify.md`; these are the conclusions that change code.

1. **Signal A is session-attributable.** A2DP audio renders from a protected
   `svchost.exe`, not the pid-0 audio engine, and its session meter tracked the
   endpoint to four decimals while every other session read exactly zero. So
   the real `AudioMeter` should be **session-scoped, with the endpoint meter as
   fallback** — not the other way round.

   Match on the session *identifier*, never the PID (`OpenProcess` is refused
   on that process, and "svchost" would not be unique anyway).
   `looks_like_a2dp_session` in `debug_sessions.rs` is the existing heuristic.

   This is not academic: mid-run, another app on the PC pushed the endpoint
   meter to 0.0626 — 100x `SILENCE_EPS` — while A2DP was genuinely silent. The
   endpoint meter would have reported healthy audio during a dead stretch.

2. **Signal B is unavailable, permanently, on this hardware.** GSMTC returns
   *no sessions at all* while the iPhone streams. The app ships degraded and
   the UI has to say so.

3. **Pause is indistinguishable from a dead audio path.** A user pause holds
   link `Opened`, session `Active`, peak exactly `0.000000`, indefinitely — the
   same signature a dead path produces. So:

   - `recover_without_remote_signal` stays **off by default**. This is now
     measured, not merely cautious.
   - The manual **Reconnect** button is the primary recovery path for the
     silent-failure case, not a convenience.
   - The **live peak meter is the headline UI element** — the user is the only
     reliable discriminator, because they know whether they pressed pause.

4. **One safe auto-recover signature survives:** link `Opened` **+** session
   `Inactive`. Pause and a full reroute-away were both measured as `Active`, so
   nothing the user does on the phone can produce it. Whether a *real* fault
   produces it is unverified — so treat it as a trigger worth acting on, never
   as the only thing the watchdog watches.

5. **An always-armed sink survives the entire phone app lifecycle.** A 300s
   run held the link `Opened` through an app switch, the source app being
   *swiped away*, 97s of silence, and a different app starting playback - one
   arm, zero drops, audio auto-routed to the PC with no user action. So the
   headline feature is just: hold `Start()` for process lifetime, keep an
   `Open()` outstanding, reopen immediately on close. Never idle in `Closed`.

   Corollary: route via **Control Center**, not Settings > Bluetooth. The
   Bluetooth-menu path produced a link that dropped after ~10s of silence; the
   Control Center path held for 97s+. Say so in the UI.

6. **"Armed and waiting" must not escalate.** `HealthMonitor` currently
   escalates after `rearm_escalation_threshold` consecutive re-arms without
   flow (`RecoveryReason::ReArmExhausted`). With a permanently armed sink,
   `Open()` returns `RequestTimedOut` forever while the phone is out of
   range - which is normal, not a fault, and would otherwise ratchet the ladder
   to its 60s cap and respond sluggishly when the phone returns. Add a distinct
   `Armed`/`Listening` state; only re-arms that fail *with a remote present*
   may touch the ladder. **This is a `core/` change, not just a `platform/`
   one.**

7. **Do not read link state from a second connection object.** A probe
   `AudioPlaybackConnection` kept reporting `Opened` after the process actually
   holding the sink had closed it. `platform/` must read state from the
   connection it owns.

### Step 1 — milestone 7, the four re-arm triggers

The groundwork is done. `platform/worker.rs` runs the supervisor on its own
**MTA** thread behind a channel boundary, so all four triggers can be
callback-based with **no message pump** - an earlier note here claimed
otherwise and was wrong. Each callback should do nothing but post
`Command::Trigger`; `IMMNotificationClient` in particular must not block or
re-enter the enumerator.

| Trigger | Mechanism |
|---|---|
| Resume from sleep | `RegisterSuspendResumeNotification` with `DEVICE_NOTIFY_CALLBACK` |
| Bluetooth radio toggled | `Windows.Devices.Radios.Radio::StateChanged` |
| Default render device changed | `IMMNotificationClient::OnDefaultDeviceChanged` |
| Device appears / disappears | `DeviceWatcher` over the playback-connection selector |

`Trigger::DefaultDeviceChanged` already rebinds the meter via
`AudioMeter::rebind`; the trigger plumbing only has to deliver it.

Note the device still enumerates with the phone's radio **off**, so
`DeviceChanged` will not fire merely because the phone was switched off.

### Step 2 — milestone 8 onward

Triggers, then UI, then config file I/O and logging, then soak. The trigger
plumbing feeds `HealthMonitor::note_trigger`, which already debounces a burst
into one re-arm; the platform side only has to call it.

---

## How the code is laid out

```
src/
  lib.rs            library half — pure logic, so core/ items are public API
                    rather than dead code (keeps clippy -D warnings honest)
  main.rs           thin bin: COM init + --debug-sessions dispatch
  debug_sessions.rs the four-layer diagnostic. MOVES INTO platform/ at m6.
  bin/spike-a2dp.rs the packaging spike. Keep it: --start-only re-checks the
                    capability gate safely if a future Windows build tightens.
  core/             ZERO windows-rs imports, by rule. Every branch lives here.
    types.rs        PlaybackStatus (6 variants), LinkState, Action, Trigger...
    traits.rs       AudioMeter, RemotePlayback, SinkConnection, Clock
    config.rs       Config + serde. Every field defaulted, unknown keys ignored.
    backoff.rs      the 1/2/5/10/30/60 ladder
    health.rs       HealthMonitor — the state machine. The whole point.
    fakes.rs        #[cfg(test)] test doubles, incl. THE fake
```

`platform/` and `ui/` do not exist yet. Both are **exempt from the ratio
gate** on purpose — see the comment block at the top of
`scripts/check-test-ratio.ps1` before "fixing" that.

### The rule that keeps the layering honest

**If a function contains a branch, it belongs in `core/`.**

---

## The health model as actually implemented

Beyond CLAUDE.md's sketch, in ways that matter:

- **Silence is a condition start, not "time since last peak."** A last-peak
  timestamp makes a long pause followed by pressing play look instantly
  overdue and fires a spurious recovery on the first sample of a new track.
- **Re-arm and recovery are different paths.** The link goes
  `Opened -> Closed` unprompted in ordinary use, so a close is not a fault —
  but idling in `Closed` means audio never resumes. Re-arms skip the ladder
  and the reconnect log; recoveries get both. Because re-arms skip the ladder
  they need their own floor (1s) plus an escalation threshold (5), or a link
  that closes instantly on open reopens in a tight loop.
- **Only real recoveries show as `Reconnecting (attempt N)`.** A benign re-arm
  showing that label invents a fault and displays a ladder attempt number for
  a path that never touches the ladder.
- **Signal B unavailable does not enable auto-recovery by default.** CLAUDE.md
  is in tension here — it offers a longer timeout *and* forbids reconnecting
  on Signal A alone. The conservative reading is the default; the permissive
  one is opt-in via `recover_without_remote_signal`, paired with a 15s
  degraded timeout. Documented at the config field.
- **The decision table has seven remote cases, not five.** Six real WinRT
  variants plus "no session at all", which is distinct from `Closed`.
  `Changing` is a transient and is asserted not to read as `Playing`.

---

## Commands

```bash
cargo test                                  # 96 tests, ~0.2s
cargo clippy --all-targets -- -D warnings   # what CI gates on
cargo fmt --check
pwsh -File scripts/check-test-ratio.ps1     # the 0.9:1 gate on core/

cargo run --bin purptoof -- --debug-sessions   # read-only, safe any time
cargo run --bin purptoof -- --watch=120         # read-only, one line per second
cargo run --bin spike-a2dp                     # read-only: enumerate + construct
cargo run --bin spike-a2dp -- --start-only     # exercises the radio, moves no audio
cargo run --bin spike-a2dp -- --open --hold=N  # OPENS A STREAM — routes phone audio
cargo run --bin spike-a2dp -- --rearm --hold=N # always-armed sink; also opens a stream
cargo run --bin purptoof -- --run=300           # THE REAL APP, headless. Routes phone audio.
```

`cargo nextest run --no-tests=pass` is what CI runs; nextest is not installed
locally. Neither is `tokei` — the ratio script deliberately does not use it,
because tokei counts per file and our test code lives in `#[cfg(test)]` blocks
*inside* production files.

### Git

`git push origin main` writes to **both** GitHub and Gitea — `origin` carries
two push URLs. This is Lockstep's layout and is deliberately the inverse of
what CLAUDE.md's Repository section describes; do not "fix" it.

- GitHub (authoritative CI): https://github.com/TheHalfrican/PurpToof
- Gitea over Tailscale: `thehalfrican-truenas.tail1cdca8.ts.net:30009`

---

## Open questions for Noah

1. **Does the A2DP session identifier survive a reconnect?** One `--watch` run
   spanning a disconnect and reconnect would answer it. Not blocking —
   `looks_like_a2dp_session` deliberately does not depend on the GUID.
2. **Radio selective suspend** on the Intel adapter has still never been read.
   Low priority, but it explains a class of failures the watchdog can only
   paper over.
