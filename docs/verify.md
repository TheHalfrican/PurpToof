# VERIFY log

CLAUDE.md flags several things as "verify against reality rather than trusting
this sketch". This file records what was actually observed, so nobody has to
re-run a spike to recover the answer.

Rig: Windows 11 Pro 26200, Intel(R) Wireless Bluetooth(R), iPhone 15 Pro Max
paired as `Noah's iPhone`. Default render endpoint during these runs was
`A50 X Game` (Astro A50 X, USB base station, 2.4 GHz to the headset - not a
Bluetooth device).

---

## RESOLVED: packaging - unpackaged Win32 is sufficient

**MSIX is off the table.** No package identity, no UWP lifecycle, and
therefore no app-suspension risk - which removes the prime suspect for the
Store app's failure mode from our own design.

Verified by `src/bin/spike-a2dp.rs` run from an ordinary unpackaged
`cargo run` exe, clearing every gate the `bluetooth` DeviceCapability could
plausibly be enforced at:

| Gate | Call | Result |
|---|---|---|
| Class activation | `GetDeviceSelector()` | selector returned |
| Enumeration | `FindAllAsyncAqsFilter` | 1 device (`Noah's iPhone`, `...\SNK`) |
| Construction | `TryCreateFromId` | live object, no identity |
| Radio use | `Start()` | OK - PC advertised as a sink |
| Stream open | `Open()` | `Success`, `ExtendedError = 0x0` |

`DeniedBySystem` never appeared. The middle rung (`--start-only`) is worth
keeping: it exercises the radio without opening a stream, so the capability
question stays answerable on a machine somebody is using.

## RESOLVED: API shape, corrected against windows-rs 0.62.2 metadata

The CLAUDE.md lifecycle sketch was wrong in three ways:

- **`Start`/`Open`/`Close` exist in both sync and async forms.** The sync ones
  need no future at all, which suits `recover()`'s sub-second target. Prefer
  them.
- **There is an explicit `Close()`.** Recovery should call it rather than
  relying on `Drop` ordering to release the connection.
- **windows-rs 0.62 moved WinRT async into the `windows-future` crate**, where
  the blocking accessor is `join()`, not the old `get()`.

Enum values, which `Debug` will not show (windows-rs generates associated
consts on a tuple struct, so it prints a bare integer):

```
AudioPlaybackConnectionState        Closed = 0, Opened = 1
AudioPlaybackConnectionOpenResultStatus
                                    Success = 0, RequestTimedOut = 1,
                                    DeniedBySystem = 2, UnknownFailure = 3
```

---

## FINDING: `Open()` returning `Success` does NOT mean the link is open

Observed, in this exact order:

```
Open status: Success
  state: Closed          <- immediately after Open() returned
  [event] StateChanged -> Opened
  t+5s state: Opened
```

`Open()` resolved `Success` while `State()` still read `Closed`. The
transition to `Opened` arrived asynchronously afterwards.

**Consequence for `recover()`:** it must not report success on
`Open() == Success`. It has to wait for `StateChanged -> Opened` under a
timeout, and treat "Success but never transitioned" as a failed recovery that
advances the backoff ladder. Measuring recovery latency from `Open()`'s return
would measure the wrong thing.

## FINDING: the link closes on its own, and that is not a fault

Later in the same run, with no action from the app:

```
  t+10s state: Opened
  [event] StateChanged -> Closed
  t+15s state: Closed
```

The link went `Opened -> Closed` unprompted. **Cause not yet distinguished**
between (a) iOS tearing down the A2DP stream when nothing is playing and
(b) the phone being disconnected by hand mid-run. Worth pinning down, because
the two imply different steady-state behaviour.

Either way the design consequence already holds, and it is a real gap in the
CLAUDE.md health model:

- A `StateChanged -> Closed` must **not** by itself trigger `recover()` or
  advance the backoff ladder. It is an expected transition.
- But the app cannot just sit in `Closed` either, or audio will not resume
  when playback restarts. It needs to re-`Open()` after a benign close.
- So `recover()` splits in two: **re-arm** (benign, no backoff, expected, no
  log spam) and **recovery** (the watchdog fired because audio is genuinely
  dead, backoff applies, gets a reconnect-log entry).

Collapsing those two into one path would either produce a reconnect-log full
of noise every time a song ends, or a ladder that backs off to 60s during
normal use and then responds sluggishly to a real fault.

---

## FINDING: "connected" means three different things

Worth writing down because it causes real confusion: Windows can show the
phone as connected while the phone shows nothing, and neither is lying.

| Layer | What it reports | Where to read it |
|---|---|---|
| Bluetooth link | An ACL link with *some* profile attached - hands-free, AVRCP, phonebook | Windows Settings says "Connected" on this alone |
| A2DP stream | Whether audio can actually flow | `AudioPlaybackConnection::State()` |
| Remote's own view | What the phone believes it is doing | GSMTC / AVRCP |

Two traps this sets:

- `AudioPlaybackConnection::State()` reflects **only the connection object we
  hold**. If we have not opened it, it reads `Closed` regardless of what
  Settings displays. It is not a system-wide connectedness query.
- iOS is terse about a PC paired as a *sink* and frequently will not show it
  as connected even while a profile link exists. Absence of a connection on
  the phone's Bluetooth screen is not evidence the link is down.

`--debug-sessions` reports all three layers separately and never conflates
them, which is the whole reason it prints the layer numbers.

## PARTIAL: Signal A - the machinery works, A2DP attribution still unknown

> **SUPERSEDED 2026-09-14.** Resolved by the streaming run below; kept for
> the reasoning, not the conclusion.

Run on 2026-09-11 with Discord audio playing and the phone idle. Default
render endpoint was `Headphones (A50 X Game)`.

- **Endpoint meter works.** Peak tracked real audio, 1-second maxima ranging
  0.02 - 0.86 across the window.
- **Session-scoped metering and attribution work.** `DiscordPTB.exe` (pid
  6976) showed `Active` with peak 0.486 while the other eight sessions sat at
  exactly 0.0. So `IAudioSessionControl2::GetProcessId` -> QI for
  `IAudioMeterInformation` is a viable path *in general*.
- **Whether A2DP render audio gets its own session is STILL UNKNOWN.** The
  phone was not streaming during this run, so there was nothing to attribute.
  This needs a re-run while the phone actually plays. The `<audio engine /
  system>` entry at pid 0 is the outcome to watch for: if A2DP audio lands
  there, Signal A cannot be session-scoped.

### Bug found in our own interop, worth remembering

`IAudioSessionControl2::IsSystemSoundsSession()` returns a **raw HRESULT**,
not a `Result`: `S_OK` means yes, `S_FALSE` means no. Both are *success*
codes, so `.is_ok()` is true for every session and labelled all nine as
system sounds. Correct test is `== S_OK`.

This is exactly the failure mode `platform/` is supposed to be too dumb to
have, and an argument for keeping HRESULT-returning calls wrapped in one
named helper each rather than inlined at call sites.

## PARTIAL: Signal B - no session from the phone while idle

> **SUPERSEDED 2026-09-14.** Resolved by the streaming run below; kept for
> the reasoning, not the conclusion.

Same run. GSMTC returned exactly **one** session, and it was not the phone:

```
Helium.TL7FSSFXV44M357KD5SIY7AQBE
    PlaybackStatus: Paused
    metadata: Onimusha: Way of the Sword - Before You Buy - gameranx
```

That is a browser on the PC. The iPhone published **no GSMTC session at all**,
despite `Noah's iPhone Avrcp Transport` existing as a PnP node.

**Do not over-read this.** The phone was idle and our A2DP stream was closed,
and a GSMTC session may well only materialise once the remote is actually
playing over a live AVRCP link. The finding is that *the PnP node existing is
not sufficient* - which invalidates the earlier inference in `soak.md` that
the node's presence meant Signal B would be available. Still unresolved until
observed while streaming.

## CORRECTION: the decision table needs more rows than CLAUDE.md assumes

CLAUDE.md's test matrix says
`{Playing, Paused, Stopped, Unknown, Unavailable}`. The real WinRT enum is:

```
GlobalSystemMediaTransportControlsSessionPlaybackStatus
    Closed = 0, Opened = 1, Changing = 2, Stopped = 3, Playing = 4, Paused = 5
```

Six variants, plus "no session exists at all" as a distinct seventh case
(which is what Signal-B-unavailable actually looks like, and is not the same
as `Closed`). `Changing` in particular is a transient the table has to handle
explicitly, or it will be lumped in with something it is not.

---

---

# Run 2026-09-14 - the phone actually streaming

Both PARTIAL sections above were blocked on the same missing condition: nobody
had observed `--debug-sessions` while the iPhone was genuinely playing. This
session supplied it, over three attempts, and resolves both. It also settles
the health model in a way CLAUDE.md's sketch does not anticipate.

Method: `spike-a2dp --open --hold=N` holding the sink up in one process, and a
new `purptoof --watch=N` sampling once a second in another, while the phone was
driven through play / pause / reroute-away / reroute-back.

## Two procedural traps, each of which cost a run

- **Start music on the phone BEFORE routing it to the PC.** Connect first and
  then go looking for a music app and iOS drops the route during the idle gap.
  Attempt 1 died this way.
- **iOS Settings > Bluetooth lies here.** It read "Not Connected" for entire
  sessions while audio was demonstrably streaming. When the PC is the A2DP
  *sink* the phone is the *source*, and iOS treats that as an audio route, not
  a connected accessory. Control Center's output picker is the screen that
  reflects reality. Never judge a test by the Bluetooth screen.

## RESOLVED: audio is audible - milestone 3 closes

Sound came out of the PC, confirmed by ear and by meter. The end-to-end path
works. Everything remaining is about keeping it alive.

## RESOLVED: Signal A - A2DP audio IS session-attributable

The favourable outcome, and not the one the PARTIAL above expected.

With music streaming, endpoint peak held ~0.43 and exactly one session tracked
it:

| session | state | peak max over window |
|---|---|---|
| `msedgewebview2.exe` (28288) | Inactive | 0.000000 |
| `wallpaper64.exe` (8644) | Inactive | 0.000000 |
| `steam.exe` (6848) | Inactive | 0.000000 |
| `<audio engine / system>` (0) | Inactive | 0.000000 |
| `iw3sp.exe` (28768) | Active | 0.000000 |
| **`svchost.exe` (4312)** | **Active** | **0.443092** |

Endpoint max over the same window was 0.443333. The A2DP session tracked it to
four decimals; every other session was flat zero.

- A2DP render audio does **not** land on the pid-0 audio engine. Signal A can
  be session-scoped. CLAUDE.md's fallback-to-endpoint-meter contingency is not
  the primary path.
- It is **not** attributable by PID. The owner is a protected `svchost.exe`
  that `OpenProcess` refuses even with `PROCESS_QUERY_LIMITED_INFORMATION`, and
  "svchost" would not be unique if it did.
- The usable key is `IAudioSessionControl2::GetSessionIdentifier()`:

  ```
  {0.0.0.00000000}.{a1b9084c-...}|\Device\HarddiskVolume2\Windows\System32\svchost.exe%b{C55CBD10-423D-4D4F-8D35-C4044AA8EBFC}
  ```

  The trailing GUID is the session grouping param, and it appears in the
  *session* identifier rather than only the *instance* identifier - suggesting
  a fixed GUID for the Bluetooth audio render service rather than a
  per-connection random. Byte-identical across every sample in this session.

  **NOT VERIFIED:** stability across reconnect, reboot, or another machine.
  `looks_like_a2dp_session` therefore matches on the `\system32\svchost.exe`
  path and treats the GUID as documentation only. A caller finding no match
  must fall back to the endpoint meter, never conclude the stream is dead.

These identifiers allocate with `CoTaskMemAlloc` and the caller owns them;
`pwstr_field` is the single place that ownership rule lives.

### Endpoint masking, caught in the wild

Mid-pause, with A2DP silent, something else on the PC made a noise:

```
   85  Opened   0.000012  Active/0.000000
   86  Opened   0.000013  Active/0.000000
   87  Opened   0.062576  Active/0.000000
   88  Opened   0.000013  Active/0.000000
```

The endpoint meter read 0.0626 - over 100x `SILENCE_EPS` - while the attributed
session correctly read exactly 0.0. Had Signal A been endpoint-scoped, that
sample would have reported healthy audio during a silent stretch. This is the
masking failure mode, observed rather than theorised, and it is the argument
for session-scoping being the primary path.

## RESOLVED: Signal B is UNAVAILABLE on this hardware

GSMTC returned **no sessions at all** while the iPhone was actively streaming
music. Not a session with poor metadata - nothing.

The earlier PARTIAL left open that a session might materialise once the remote
was really playing over a live AVRCP link. It does not. The `Noah's iPhone
Avrcp Transport` PnP node exists and publishes nothing GSMTC can see.

**The app ships permanently degraded on Signal B**, and the UI must say so.

## RESOLVED: the health model - what is and is not distinguishable

The decisive run. A 230s `--watch` while the phone was driven through four
states:

| phone / sink state | window | link | a2dp session | peak |
|---|---|---|---|---|
| playing | t=1-44, 107-132, 175-217 | `Opened` | `Active` | ~0.42 |
| **paused in the music app** | t=45-106 | `Opened` | `Active` | **0.000000** |
| **rerouted back to the iPhone** | t=133-172 | `Opened` | `Active` | **0.000000** |
| **our own sink closed** | t=218-230 | `Opened` | **`Inactive`** | 0.000000 |

### Pause is indistinguishable from a dead audio path

A user pause presents as link `Opened`, session present and `Active`, peak
exactly zero, indefinitely - held for 62 seconds here. That is precisely the
signature a dead audio path produces. With Signal B unavailable, **no
observation available to this app separates them.**

Consequences, and these are design-level:

- `recover_without_remote_signal` must stay **off by default**. Auto-recovering
  on silence alone fires on every pause - the reconnect storm CLAUDE.md
  correctly names as worse than the original bug.
- The manual **Reconnect** button is not a convenience. On this hardware it is
  the primary recovery path for the silent-failure case. Prominent, always
  enabled.
- The **live peak meter is the headline UI element**, because the user is the
  only reliable discriminator - they know whether they pressed pause.

### `Active` does not mean frames are arriving

Worth recording because it is the obvious hypothesis and it is wrong. The idea
was that iOS keeps pushing silence frames while paused, so a genuinely dead
path would stop them and WASAPI would flip the session to `Inactive`.

It does not hold: the session stayed `Active` for 40 seconds after the audio
was routed **away from the PC entirely** (t=133-172). `Active` tracks "our sink
is open", not frame flow.

### The one discriminator that survives, and why it is safe

`Inactive` appeared exactly once, at t=218, the moment the spike's hold expired
and called `Close()`. So:

- `Opened` + `Active` + zero peak -> **ambiguous.** Pause, reroute-away, and a
  dead path are identical. Never auto-recover.
- `Opened` + **`Inactive`** -> the render client stopped while we still believe
  we hold the sink open. **Safe to treat as a fault.**

The safety argument does not depend on knowing what a real fault looks like -
which is fortunate, because the fault is not producible on demand. It rests on
the benign cases: pause and reroute-away were both measured as `Active`, so
neither can generate this signature. Acting on it cannot fire on anything the
user does with the phone.

Treat "a real fault presents as `Inactive`" as **unverified**. Treat "acting on
`Inactive` is safe" as supported by this run.

### Caveat: a second connection object does not mirror link state

The `--watch` link column comes from its own `AudioPlaybackConnection`,
constructed but never started. It kept reporting `Opened` for t=218-230, after
the spike had closed the connection that was actually holding the sink.

**`platform/` must read link state from the connection it owns**, and must not
use a probe object as a health signal. Layer 2 of `--debug-sessions` carries
the same caveat and its NOTE understates it.

### Partially corrects the earlier teardown theory

The old "cause of the unprompted `Opened -> Closed`" entry led with "user tapped
the PC in the iPhone's Bluetooth menu". That is now **ruled out** - nothing in
Bluetooth settings was touched in any attempt, and the link still closed in
attempt 1.

What replaces it is **not** settled:

- Attempt 1: audio stopped, the user moved to a music app, link closed ~10s
  later.
- Attempt 3: music paused, the user left the music app to type, link stayed
  `Opened` for a further ~55s; and a full reroute back to the iPhone also left
  it `Opened` for 40s.

Leaving the foreground app is therefore **not** the trigger. The surviving
difference is whether an app on the phone still held an audio session -
paused-but-loaded versus fully finished. Plausible, one observation each way,
**not verified**.

The app's handling is unchanged either way: `Closed` is benign, re-arm handles
it, and re-arm skips the backoff ladder.

## RESOLVED: an always-armed sink survives the whole app lifecycle

The question: the app should keep audio playing no matter which app on the
phone is the source and no matter how the user switches between them. Is that
achievable from the PC side alone, or does iOS drop the route and require a tap
in Control Center?

Answer: **achievable, and it needs no recovery at all in this scenario** - the
link simply does not drop.

Method: `spike-a2dp --rearm --hold=300` holds `Start()` for the whole run and
keeps an `Open()` in flight, reopening the instant the link closes. Alongside
it, `purptoof --watch=300`. The operator played music, stopped it, **swiped the
music app away entirely**, sat on the home screen for ~97s, then opened a
different app (Twitter) and played a video - **without touching Control Center
at any point.**

Sink log, in full:

```
[t+  0.0s] arm #1: Open() ...
[t+  0.8s] arm #1: Success after 0.8s - waiting for Opened
  [event] StateChanged -> Opened
[t+  0.9s] arm #1: LINK UP after 0.1s
[t+300.0s] run complete, 1 arm(s)
```

Audio, from the watch log:

| window | operator action | link | session | audio |
|---|---|---|---|---|
| t=1-64 | music playing | `Opened` | `Active` | yes, ~0.45 |
| t=65-161 | stopped, app swiped away, home screen | `Opened` | `Active` | silent |
| t=162-296 | Twitter video, no Control Center tap | `Opened` | `Active` | yes, ~0.26 |
| t=297-300 | our sink closed at the deadline | - | `Inactive` | silent |

**One arm. Zero link drops across 300 seconds**, spanning an app switch, an app
*termination*, 97 seconds of continuous silence, and a change of source app.
The audio came out of the PC, confirmed by ear, with no user action.

### What this settles

- **The route survives the app lifecycle.** iOS keeps the A2DP route bound to
  the PC as long as something on this side holds the sink open. A new app's
  audio follows the existing route automatically.
- **"Any source app" needs no work.** A2DP is a device-level route, not
  per-app.
- **The always-armed design is the whole feature.** Hold `Start()` for process
  lifetime, keep an `Open()` outstanding, reopen immediately on close. The
  Store app's intermittency is most likely explained by treating `Closed` as a
  resting state and waiting for the user.

### Corrects the attempt-1 teardown, again

Attempt 1 this session had the link close ~10s after audio stopped. Here, 97s
of silence held it. The difference is how the route was established:

- Attempt 1: **Settings > Bluetooth**, tapping the PC in the device list.
- This run: **Control Center's output picker.**

Same phone, same PC, opposite behaviour. The Bluetooth-menu entry is most
likely a connection toggle that establishes a weaker binding, or disconnected
it outright. Not worth chasing further, but it means **test protocols must
route via Control Center**, and the UI should tell users to do the same.

### Design consequence: "armed and waiting" is not a failure

`Open()` completed in 0.8s here because the phone was already routed. With the
phone absent or out of range it will return `RequestTimedOut` repeatedly, and
the re-arm loop will keep reissuing - correctly, forever.

`HealthMonitor` currently escalates after `rearm_escalation_threshold`
consecutive re-arms that do not produce flow
(`RecoveryReason::ReArmExhausted`). For an always-armed sink that is wrong: a
phone in another room is not a fault, but it would ratchet the ladder to the
60s cap and then respond sluggishly when the phone comes back.

**`platform/` and `core/` need a distinct `Armed`/`Listening` state** - waiting
for a remote that has not arrived - separate from re-arming that keeps failing
with a remote present. Only the latter may touch the backoff ladder. This is
the one change milestone 6 must make to the state machine rather than just
implementing behind it.

### Still untested

The link held across the app lifecycle. It has **not** been tested across the
four re-arm triggers - sleep/resume, radio toggle, default render device
change, device removal. Those remain the reason the re-arm path exists.

## Tooling changes made during this run

- **`--watch[=SECS]`** added: one line per second of link state, endpoint peak,
  and the attributed A2DP session's state and peak. It exists because every
  interesting question here is about a *transition*, and a one-shot dump
  requires the operator to hold the phone and the keyboard at the same instant.
  Read-only - it never advertises a sink or opens a stream.
- **`--debug-sessions` sampling order fixed.** It sampled the endpoint for 6s
  and *then* read each session peak once, so any audio that stopped before the
  second pass reported six zeroes next to a moving endpoint. It now meters the
  endpoint and every session on the same ~10 Hz tick and reports per-session
  maxima. Attribution was only readable after this change.
- **`spike-a2dp --hold=SECS`** added, so a run can be held open long enough to
  drive the phone through a sequence.
- **`spike-a2dp --rearm`** added: the always-armed sink. `Start()` held for the
  whole run, an `Open()` always in flight, reopened the instant the link
  closes, every transition timestamped. It answered the app-lifecycle question
  above and is a miniature of the milestone-6 re-arm path.

---

## STILL OPEN

- **Session identifier stability.** The A2DP session's grouping GUID
  `{C55CBD10-...}` was byte-identical across every sample in one session, but
  has never been checked across a reconnect, a reboot, or another machine.
  `looks_like_a2dp_session` deliberately does not depend on it. If it turns out
  stable, matching can tighten; if it turns out per-connection, nothing breaks.
- **Whether a real fault presents as `Inactive`.** The one discriminator left
  rests on an untestable premise - the fault is not producible on demand. Acting
  on it is safe regardless (see above), but the app cannot rely on it *firing*.
  The overnight soak is the first realistic chance to observe a genuine fault;
  log the session state alongside the peak so the answer is in the log when it
  happens.
- **Cause of the unprompted `Opened -> Closed`.** Narrowed, not solved. Ruled
  out: user action in the iPhone's Bluetooth menu, and leaving the foreground
  app. Leading hypothesis: whether any app on the phone still holds an audio
  session.
- **Radio selective suspend.** The Intel adapter's power-management state has
  still never been read.

---

## RESOLVED: what `Open()` does when the phone is not reachable

Measured 2026-09-14 with the iPhone's Bluetooth switched off, via
`spike-a2dp --rearm`. Two answers, and the second one was a live bug.

**1. `Open()` does not block for long.** 25 consecutive attempts returned in
**0.8s to 4.9s**, typically ~1s. The concern that a synchronous `Open()` would
stall the tick loop or freeze a UI for a long timeout does not materialise. The
synchronous call is fine; `OpenAsync` is not needed, and the supervisor's
existing awaiting-phase already covers the asynchronous part that does exist
(the `Opened` transition).

**2. "No remote" does NOT arrive as `RequestTimedOut`.**

```
[t+  1.4s] arm #1: UnknownFailure after 1.4s (extended Ok(HRESULT(0x8007001F)))
[t+  2.7s] arm #2: UnknownFailure after 1.1s (extended Ok(HRESULT(0x8007001F)))
[t+  3.9s] arm #3: UnknownFailure after 1.0s (extended Ok(HRESULT(0x8007001F)))
```

`0x8007001F` is `HRESULT_FROM_WIN32(ERROR_GEN_FAILURE)` - "a device attached to
the system is not functioning." Identical on every single attempt.
`RequestTimedOut` was never observed at all.

This mattered immediately. `RecoveryOutcome::NoRemote` - the whole mechanism
for *not* escalating when the phone is simply elsewhere - keyed on
`SinkError::TimedOut`. An `UnknownFailure` fell through to `SinkError::Other`
and was treated as a genuine failure, which would escalate past the re-arm
threshold, ratchet the backoff ladder to its 60s cap, and leave the app
sluggish when the phone came back. Exactly the bug `NoRemote` was written to
prevent, reintroduced through the error mapping.

Fixed by `platform::sink::classify_unknown_failure`, which reads the extended
error and returns `SinkError::Unreachable` for `0x8007001F` and
`SinkError::Other` for anything else. `Unreachable` and `TimedOut` are kept as
separate variants so the reconnect log can say which actually happened, but the
supervisor treats both as `NoRemote`.

**Do not collapse `UnknownFailure` to a single meaning.** One value of it means
"come back later" and every other value may not; a mapping that cannot tell
them apart either escalates against a switched-off phone or retries a real
fault forever in silence.

### Also observed

- **The device still enumerates with the phone's radio off.** The selector's
  `System.Devices.InterfaceEnabled` stayed true, and `TryCreateFromId`
  succeeded. So the `DeviceChanged` re-arm trigger will not fire merely because
  the phone was switched off, and an absent phone is distinguishable from an
  unpaired one.
- **Re-arm cadence while unreachable** settles at roughly one attempt per
  1-2s: the 250ms floor plus the ~1s call. Acceptable - the PC keeps
  advertising throughout via the held `Start()`, so the worst case for noticing
  a returning phone is about a second - but it is the number to revisit if
  radio churn ever becomes a concern.

---

## RESOLVED: `AudioPlaybackConnection` lifecycle - closing is DISCARDING

Found by an access violation on the first real `--run`, then pinned down with a
throwaway spike. None of this is in the WinRT docs.

| sequence | result |
|---|---|
| repeated `Open()`, no close between | **works** |
| `Close()`, drop, reconstruct, `Open()` | **works** |
| `Close()` then `Open()` on the same object | `DeniedBySystem`, permanently |
| `Close()` on a **never-started** object, then `Start()` | **ACCESS VIOLATION** |

So a connection object is single-use with respect to closing. Once closed it
cannot be revived, and calling `Close()` on one that was never `Start()`ed
corrupts it badly enough that the next `Start()` crashes the process.

`platform::sink::Sink` now holds `Option<Live>`: `close()` drops the object
outright and is a no-op when there is nothing live, and `open()` reconstructs
via `TryCreateFromId` + `Start()` when needed. That is also exactly what
CLAUDE.md's `recover()` always described - "drop the connection, release COM
interfaces, re-run the lifecycle" - which the first implementation had quietly
weakened into "close and reuse".

### Consequence: only a recovery should tear down

Because closing is expensive and irreversible, the supervisor no longer closes
unconditionally before every open:

- **Benign re-arm** - just `Open()` again on the live connection. Measurably
  fine: the `--rearm` spike ran many arms this way without a single close, and
  the link is reopened dozens of times in ordinary use.
- **Genuine recovery** - close, discard, reconstruct. The heavy hammer, for
  when the link is actually wedged.
- **`DefaultDeviceChanged`** - also tears down, because the render target is
  bound when the connection opens and does not follow the system default.

This is the re-arm/recovery split earning its keep a second time, on a
dimension it was not designed for.

## First successful `--run` on real hardware

```
[   0.0s] Disconnected
[   0.9s] meter scope: A2DP session (trustworthy)
[   1.0s] Connected, silent (degraded - no AVRCP signal)
[   1.5s] Streaming
[  22.7s] Connected, silent (degraded - no AVRCP signal)
[  24.7s] Streaming
run complete, 0 recovery event(s) logged
```

Session attribution resolved in 0.9s, link up at 1.0s, audio at 1.5s. The
22.7s entry is a real gap in the source audio, correctly reported as
`Connected, silent` with **zero** recovery events - the conservative default
declining to reconnect on silence alone, which is the behaviour the pause
measurements demanded.

---

## Milestone 7 triggers, measured on hardware

One 100s `--run` with the operator switching the PC's default output twice and
toggling Bluetooth off/on.

### CONFIRMED: `DefaultDeviceChanged` fires, and matters

```
[  35.9s] re-arm x1 - ReArm(Trigger(DefaultDeviceChanged))
[  38.0s] Streaming
[  42.3s] re-arm x1 - ReArm(Trigger(DefaultDeviceChanged))
[  43.8s] Streaming
```

Two switches, two triggers, audio back within ~2s each time. The polled
`DefaultDeviceWatch` is a complete substitute for `IMMNotificationClient` here.

**But it costs a manual play press.** The operator reports that on every
switch the audio returned to the *correct* new output, and that the iPhone
paused itself and had to be restarted by hand. That follows from the mechanism:
moving the render target requires closing and reopening the connection, iOS
sees the A2DP stream disappear, and it pauses. There is no way to move the
target without the reopen, so this is a property of the API rather than
something to fix.

The UI should say so rather than let it surprise people - a switch that
silently pauses their music looks like a bug even though the recovery worked
perfectly.

An earlier attempt at this test was **inconclusive** rather than negative, and
for an instructive reason: a `DefaultDeviceChanged` is a *re-arm*, which is
deliberately excluded from the reconnect log, so it left no trace at all. The
snapshot now carries a `rearms` counter and `last_rearm`, printed by `--run`
but still kept out of the user-facing log. Without that, a trigger that fired
and a trigger that never registered look identical.

### CONFIRMED: `DeviceChanged` fires at startup

```
[   2.5s] re-arm x1 - ReArm(Trigger(DeviceChanged))
```

`DeviceWatcher` reports every already-present device when `Start()` is called,
so this fires once on launch. Harmless - it is debounced and we were opening
anyway - but worth knowing it is not a spurious device event.

### NOT CONFIRMED: `RadioToggled` never fired

Bluetooth was toggled off and back on. Registration had reported success, and
no `Trigger(RadioToggled)` arrived. Every re-arm in the window was
`LinkClosed`.

Likely cause: the `Radio` object is invalidated when the adapter is disabled,
so the handler is attached to an object that no longer exists by the time the
radio returns. Surviving that would need re-enumeration.

`Radio::RequestAccessAsync` has since been added as the documented
prerequisite, but that is a guess and is **unverified**. The registration is
kept because it is cheap and may behave differently elsewhere, and `--run` now
labels it unconfirmed rather than counting it as working.

**It cost nothing**, which is the point below.

### CONFIRMED: the `NoRemote` mapping works in the field

The radio-off window is the best evidence in the log that the earlier
`0x8007001F` fix was necessary:

```
[  54.9s] Listening - advertising, waiting for a device
[  54.9s] re-arm x1 - ReArm(LinkClosed)
   ... 11 re-arms over ~17 seconds ...
[  71.4s] re-arm x1 - ReArm(LinkClosed)
[  71.5s] Connected, silent (degraded - no AVRCP signal)
[  72.0s] Streaming
```

Eleven consecutive re-arms with no remote reachable: **zero escalations, zero
backoff-ladder movement, zero reconnect-log entries**, and recovery 0.6s after
the radio returned. Before `UnknownFailure`/`0x8007001F` was mapped to
`Unreachable` -> `NoRemote`, those would have escalated past the threshold of 5
and ratcheted the ladder toward its 60s cap, turning a half-second recovery
into a minute-long one.

It also means the radio trigger is not load-bearing: a toggled adapter is
handled completely by the `LinkClosed` re-arm path.

### STILL UNTESTED

- **Resume from sleep.** Needs the PC actually slept; soak matrix item 2.
- **`RadioToggled` with `RequestAccessAsync`** in place.
