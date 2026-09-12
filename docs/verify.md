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

## STILL OPEN

- **Signal A attribution for A2DP specifically** - see PARTIAL above. Needs
  `--debug-sessions` while the phone streams.
- **Signal B availability while streaming** - see PARTIAL above.
- **Cause of the unprompted `Opened -> Closed`.** Best lead so far: the user
  reports tapping the PC in the iPhone's Bluetooth menu around that time,
  which would explain a brief drop. Still not distinguished from an idle
  teardown.
- **Radio selective suspend.** The Intel adapter's power-management state has
  not been read yet.
