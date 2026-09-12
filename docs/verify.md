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

## STILL OPEN

- **Signal A attribution.** Does the internally-rendered A2DP audio appear as
  a WASAPI session attributable to our PID, or does it land on the audio
  engine / a system process? Needs `--debug-sessions` while streaming. Falls
  back to the endpoint-level meter if unattributable.
- **Signal B availability.** The iPhone exposes an `Avrcp Transport` PnP node,
  so GSMTC is expected to work on this rig - but "the node exists" is not
  "a `GlobalSystemMediaTransportControlsSession` shows up with a usable
  `PlaybackStatus`". Unconfirmed until observed while streaming.
- **Cause of the unprompted close** (above).
- **Radio selective suspend.** The Intel adapter's power-management state has
  not been read yet.
