# Progress — start here

Resume point for a new session. `CLAUDE.md` is the design; this file is the
state. If they disagree, this file is newer.

**Last updated:** 2026-09-11, at commit `644c0bd`
**CI:** green on `windows-latest` (fmt, clippy `-D warnings`, build, nextest, ratio gate)
**Tests:** 57 passing, ~0.17s, all on a fake clock
**Ratio gate:** 2.51:1 on `src/core/` against a 0.9 floor

---

## Where the project is

| # | Milestone | State |
|---|---|---|
| 1 | Repo, mirror remote, green CI | **done** |
| 2 | Packaging spike — gates everything | **done — unpackaged Win32 wins, no MSIX** |
| 3 | Bare connect + render, confirm audio flows | **partial** — a real stream opens and reaches `Opened`; nobody has confirmed it is *audible* |
| 4 | `--debug-sessions`, resolve both VERIFY items | **tool done, both VERIFY items still open** |
| 5 | `core/` traits + `FakeConnection` + health state machine | **done** |
| 6 | `platform/` impls behind the traits, `recover()` wired | **next** |
| 7 | Re-arm triggers | not started |
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

### Step 1 — resolve the two VERIFY items (needs the phone, ~5 minutes)

Do this **before** writing `platform/`, because the answer changes what the
real `AudioMeter` implementation is allowed to do.

Two processes at once, with music playing on the iPhone:

```bash
# terminal A — holds the sink open for ~20s after the phone connects
cargo run --bin spike-a2dp -- --open

# terminal B — while A is streaming
cargo run --bin purptoof -- --debug-sessions
```

Then answer, and record in `docs/verify.md`:

1. **Signal A attribution.** Does a WASAPI session appear that corresponds to
   the phone's audio, and what PID owns it? If it lands on
   `<audio engine / system>` (pid 0), Signal A **cannot** be session-scoped and
   must use the endpoint meter — which means other apps' audio can mask A2DP
   silence, and Signal B has to carry more weight.
2. **Signal B availability.** Does the iPhone publish a GSMTC session with a
   usable `PlaybackStatus` while streaming? With the phone idle it published
   **nothing**, despite having an `Avrcp Transport` PnP node. If it stays
   absent while streaming, the app ships permanently degraded and the UI has
   to say so.

### Step 2 — milestone 6, `platform/`

Implement the four traits in `src/core/traits.rs` against the real OS. The
`--debug-sessions` COM code in `src/debug_sessions.rs` is where most of this
already lives, unlayered — **move it into `platform/` behind `AudioMeter` and
`RemotePlayback`** rather than writing it twice, and have `--debug-sessions`
render through the traits.

Carry these forward, each learned the hard way:

- **One named helper per HRESULT-returning call.** `IsSystemSoundsSession()`
  returns a raw `HRESULT` where `S_OK` means yes and `S_FALSE` means *no* —
  both success codes, so `.is_ok()` silently labelled all nine sessions as
  system sounds. Inlining HRESULT checks at call sites is how `platform/`
  stops being too dumb to be wrong.
- **`open()` returning `Ok` does not mean the link is open.** Observed on real
  hardware: `Open()` returned `Success` while `State()` still read `Closed`,
  and `StateChanged -> Opened` arrived asynchronously afterwards. `recover()`
  must wait for that transition under `Config::open_transition_timeout`
  (default 5s) and report `RecoveryOutcome::Failed` if it never comes.
  `FakeConnection`'s `OpenBehavior::SucceedWithoutTransition` models this.
- **Use the synchronous `Start`/`Open`/`Close`.** Both forms exist; the sync
  ones need no future and suit `recover()`'s sub-second target. Call `Close()`
  explicitly rather than relying on `Drop`.
- **Poll the meter at ~10 Hz** — that is the rate the fake tests assume and
  what `--debug-sessions` samples at.

### Step 3 — milestone 7 onward

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
cargo test                                  # 57 tests, ~0.2s
cargo clippy --all-targets -- -D warnings   # what CI gates on
cargo fmt --check
pwsh -File scripts/check-test-ratio.ps1     # the 0.9:1 gate on core/

cargo run --bin purptoof -- --debug-sessions   # read-only, safe any time
cargo run --bin spike-a2dp                     # read-only: enumerate + construct
cargo run --bin spike-a2dp -- --start-only     # exercises the radio, moves no audio
cargo run --bin spike-a2dp -- --open           # OPENS A STREAM — routes phone audio
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

1. **Was the audio ever audible?** The stream reached `Opened` but nobody has
   confirmed sound actually came out of the speakers. Milestone 3 is marked
   partial for this reason alone.
2. **Did he disconnect the phone ~10s into the `--open` run, or did it drop by
   itself?** His later report of tapping the PC in the iPhone's Bluetooth menu
   is the leading explanation, but iOS tearing down an idle A2DP stream has
   not been ruled out. The two imply different steady-state behaviour.
3. **Radio selective suspend** on the Intel adapter has never been read. Low
   priority, but it explains a class of failures the watchdog can only paper
   over.
