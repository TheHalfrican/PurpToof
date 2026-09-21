# Progress — start here

Resume point for a new session. `CLAUDE.md` is the design; this file is the
state. If they disagree, this file is newer.

**Last updated:** 2026-09-21, v0.2.0
**CI:** green on `windows-latest` (fmt, clippy `-D warnings`, build, nextest, ratio gate)
**Tests:** 126 passing, ~0.2s, core still all on a fake clock
**Ratio gate:** passing on `src/core/` against a 0.9 floor

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
| 7 | Re-arm triggers | **done** — 3 event-driven + 1 polled, 3/3 registered on hardware |
| 8 | egui UI + tray | **done** — window, tray, close-to-tray |
| 9 | Config, autostart, logging | **done** — TOML, rolling daily log, Run key, settings panel |
| 10 | Overnight soak → `docs/soak.md` | **still outstanding** |
| + | Packaging: release build, NSIS + MSI | **done** — both verified install/uninstall |
| + | 2026-09-16 field fixes | **done** — see below; Signal A was returning wrong values |

### The one settled decision that gates everything

**No MSIX. Plain unpackaged Win32.** Every gate the `bluetooth`
DeviceCapability could be enforced at was cleared from an ordinary `cargo run`
exe: class activation, enumeration, `TryCreateFromId`, `Start()`, and `Open()`
returning `Success`. There is no packaging work in this project. Evidence and
the API corrections are in `docs/verify.md` — **read that file before touching
`platform/`**, it records three ways the CLAUDE.md lifecycle sketch was wrong.

---

## 2026-09-16 — what the first real field failure changed

Two faults in one evening, one hiding the other. Full evidence in
`docs/verify.md`; these are the parts that change what to do next.

1. **The meter was reporting silence during full-scale audio, for 31 minutes,
   and tore a working link down six times.** A re-pair mints a *new* `svchost`
   render session and the old one lingers, `Inactive` and permanently silent.
   Both match the identifier rule; `find_a2dp_session` took the first and never
   re-resolved, because `GetPeakValue` on a dead-but-present session succeeds
   and returns `0.0`. Signal A — the observation the whole app decides on — was
   wrong. Fixed: hold every match, report the max, re-resolve on the interval.

2. **A stale classic BR/EDR bond looks exactly like an absent phone.** The
   phone was in range with its LE bond up and its classic bond dead; the app
   said "Waiting for a device" for a day. `Open()` returns `ERROR_GEN_FAILURE`
   for both, and `platform/sink.rs` maps it to `Unreachable` → `NoRemote`. That
   mapping was only ever measured with the phone's radio **off**.

3. **Signal B is definitively absent on this hardware**, now measured with a
   freshly re-paired phone actively streaming. The watchdog is therefore inert
   for the fault it exists to catch — see `docs/verify.md`. This is the most
   important open problem in the project.

4. **The logs said nothing**, because the reconnect log was memory-only and
   `StateChanged` was at `debug` under an `info` filter. Six restarts during
   the outage erased the evidence six times. Now fixed, and the binary carries
   its commit so two builds can be told apart.

### A rule that was written and deliberately removed

A `HealthStatus::BondSuspect` — "LE up, classic down for 120s, so the pairing
is stale" — was built, tested and taken back out before it could reach a user.
`(Closed, LE up, classic down)` is most likely the *normal idle state*: this
repo already records that iOS drops the route during an idle gap
(`verify.md:220`) and that the link closes ~10s after audio stops
(`verify.md:453`). It would have accused the user's pairing daily.

`Presence` survives as an **observation only**, guarded by
`presence_never_changes_a_decision`. The lesson is in the commit message for
`56671c3`: the app has no concept of *demand*, and idle and underrun cannot be
told apart without one.

---

## 2026-09-21 — volume boost: asked, measured, closed

A "Volume Boost" feature was requested: a slider that goes past 100%. It was
spiked the same day and **closed as not constructible inside PurpToof.** Full
evidence in `docs/verify.md`; `src/bin/spike-boost.rs` reproduces all of it.

The short chain:

1. Windows states its own ceiling — the endpoint reports
   `volume range: -96.0 dB .. 0.0 dB`. Unity is the maximum, so no volume API
   can boost, and past unity means owning the PCM.
2. There is no slack to reclaim anyway: endpoint master and A2DP session
   volume were both already at 100%, unmuted, while the phone streamed.
3. Process loopback **activates** against the protected A2DP `svchost` — the
   risk that was expected to kill it did not — but delivers exactly 0.0000
   across 1200 packets while the session meter reads non-zero in the same
   second. A 440 Hz tone at 0.08 full scale from an ordinary process was
   captured as `0.0800` in that same run, so the capture path is correct and
   the A2DP samples are deliberately withheld.
4. Classic endpoint loopback **does** capture them (`0.2485` against a session
   meter of `0.2484`) — but only mixed with every other application, which
   cannot support a targeted boost.

What remains is an APO or a virtual audio device. Both are signed drivers and
both are already non-goals. **If this is ever wanted it belongs in Lockstep**,
exactly as CLAUDE.md always said — fed from a virtual endpoint. Noah's call on
2026-09-21 was to leave it there and not touch Lockstep.

### Two things worth carrying forward

- **The audio really is quiet, and the cause is the phone.** Peaks arrived at
  roughly −52 to −12 dBFS with the PC at unity end to end. The only lever that
  recovers headroom here is the iPhone's own volume on the Bluetooth route.
  A UI "headroom readout" naming where the loss is was offered and declined for
  now; it remains the in-scope half of this request if it ever comes back.
- **This does not rescue the watchdog.** Process loopback emits a continuous
  48 kHz timeline with `AUDCLNT_BUFFERFLAGS_SILENT` never set, whether or not
  A2DP is flowing, so packet arrival cannot discriminate "paused" from "dead
  path". Endpoint loopback sees samples but cannot attribute them, which is
  strictly worse than `platform/meter.rs` today. The most important open
  problem in the project is still open.

`--mute-probe` in the spike was built but **deliberately never run**: it
silences a working phone to ask whether muting kills Signal A, and with the
boost pipeline dead nothing needs to mute that session.

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

### Step 1 — the soak, and nothing else

Milestones 1-9 are done and the app is packaged. What remains is the one thing
that cannot be rushed: run it overnight and read the log.

Set up:

```bash
pwsh -File scripts/package.ps1      # or just: cargo run
```

Then route the phone via **Control Center** (not Settings > Bluetooth - that
path produces a link that drops on idle) and leave it. In the morning:

- `%APPDATA%\PurpToof\logs\purptoof.log.<date>` - the durable record
- the Reconnect log in the window - genuine recoveries only

Record the result in `docs/soak.md` with the date and build hash, against the
matrix in CLAUDE.md.

**A real fault has now been seen** (2026-09-15, `docs/verify.md`), and it
settled the last open question in the health model: a dead path presents as link
`Opened`, session `Active`, peak zero - **identical to a pause**. There is no
signal left that distinguishes them, so auto-recovery on silence stays off and
the Reconnect button is the only remedy.

**Close this observability gap before the soak.** Nothing in the log marked that
fault: `StateChanged` is logged at `debug` and the default filter is `info`, so
the link transitions that would say *when* and *why* are discarded. As it
stands, an overnight fault will leave a log that shows startup lines and nothing
else - which is exactly what happened when this one occurred. Promote the link
transition to `info`, and log each re-arm and its reason, before leaving it
running overnight.

Also still untested: **resume from sleep**. Soak matrix item 2 - sleep the PC
five minutes, confirm audio returns with no interaction.

### Step 2 — after the soak

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
cargo test                                  # 124 tests, ~0.2s
cargo clippy --all-targets -- -D warnings   # what CI gates on
cargo fmt --check
pwsh -File scripts/check-test-ratio.ps1     # the 0.9:1 gate on core/

cargo run --bin purptoof -- --debug-sessions   # read-only, safe any time
cargo run --bin purptoof -- --watch=120         # read-only, one line per second
cargo run --bin spike-boost                    # read-only: where is the audio attenuated?
cargo run --bin spike-boost -- --loopback=10   # process loopback; captures, disturbs nothing
cargo run --bin spike-boost -- --endpoint-loopback=10  # classic loopback, whole mix
cargo run --bin spike-boost -- --mute-probe=6  # INTRUSIVE: silences the phone. Never yet run.
cargo run --bin spike-a2dp                     # read-only: enumerate + construct
cargo run --bin spike-a2dp -- --start-only     # exercises the radio, moves no audio
cargo run --bin spike-a2dp -- --open --hold=N  # OPENS A STREAM — routes phone audio
cargo run --bin spike-a2dp -- --rearm --hold=N # always-armed sink; also opens a stream
cargo run --bin purptoof -- --run=300           # headless supervisor, no window. Routes phone audio.
cargo run                                      # THE APP (window + tray)
pwsh -File scripts/package.ps1                 # release build + both installers -> dist/
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

Both of the questions that stood here are now answered:

- ~~Does the A2DP session identifier survive a reconnect?~~ **No.** The
  grouping GUID changed across a re-pair and the old session lingered, which is
  what caused the meter fault above. `looks_like_a2dp_session` still does not
  depend on the GUID, which is why the rule survived — but it matches more than
  one session, which is what did not.
- ~~Radio selective suspend has never been read.~~ **Read, and it was on.** Now
  off, and `--debug-sessions` reports it as layer 0 with a one-click fix in the
  window.

What is open now:

1. **Is `(link Closed, LE up, classic down)` the normal idle state?** This
   decides whether any stale-bond rule is possible at all. Answered by building
   the presence reader, logging `(link, classic, le, Open() outcome)` and
   acting on none of it, then leaving it running for an ordinary day.
2. **Should Reconnect be diagnostic?** With Signal B absent, a button press is
   the only unambiguous "I want audio now" the PC ever gets. Making it report
   which layer failed would turn a day-long mystery into a five-second answer.
   Arguably the highest-value remaining work — and arguably scope creep, since
   CLAUDE.md calls feature expansion a non-goal. Noah's call.
3. **What actually broke the link key on 2026-09-15?** Still open, and may stay
   that way. Selective suspend is a correlation, not a demonstrated cause.
