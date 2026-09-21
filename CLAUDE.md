# CLAUDE.md — **PurpToof**

Windows Bluetooth audio receiver. Crate name `purptoof`, binary `purptoof.exe`.

## What this is

A Windows desktop app that makes the PC an A2DP **sink** — phone connects over Bluetooth, audio comes out of the PC's speakers. Functionally a replacement for the Microsoft Store "Bluetooth Audio Receiver" app.

The Store app works but drops the audio path while still reporting itself connected, requiring a manual restart. **The entire point of this project is that it recovers itself.** UI quality is second priority. Feature expansion is a non-goal.

## Hard constraint: the API is opaque

`Windows.Media.Audio.AudioPlaybackConnection` (WinRT, Windows 10 2004 / build 19041+) is the only sanctioned A2DP sink on modern Windows. It does not expose:

- the decoded PCM stream
- output endpoint selection (always the default render device)
- codec negotiation, buffer depth, or latency control
- any flow-level health signal

Do not design around getting these. Anything that needs them requires bypassing the Microsoft Bluetooth stack (WinUSB + L2CAP/AVDTP + own codec), which is explicitly out of scope.

**Consequence:** health must be inferred from *outside* the connection object, by observing whether audio is actually moving.

## Stack

- Rust 2021, `windows` crate (windows-rs) for WinRT + COM
- `egui` / `eframe` for UI, tray-resident
- Same conventions as Lockstep: single binary, TOML config next to the exe, no installer beyond the MSIX stub
- Logging via `tracing` + `tracing-appender`, rolling daily file

## Repository

Gitea on the TrueNAS box is **origin**. GitHub is a **mirror** — push-only backup, not where work happens.

```bash
git remote add origin  ssh://git@<gitea-host>:<port>/noah/purptoof.git
git remote add github  git@github.com:<user>/purptoof.git

# push both in one command
git remote set-url --add --push origin ssh://git@<gitea-host>:<port>/noah/purptoof.git
git remote set-url --add --push origin git@github.com:<user>/purptoof.git
```

After that, `git push origin main` writes to both. Same pattern as the other repos on this machine.

Gitea is reachable over Tailscale, so the host in the URL should be the Tailscale name, not a LAN IP — otherwise pushes fail the moment the MacBook is off the home network.

If Gitea is unreachable the push fails as a unit and nothing lands; that is the intended behavior. Do not paper over it with a `|| true` in a script. CI runs on both sides (GitHub Actions on `windows-latest`, Gitea Actions if a Windows runner is registered); GitHub's is authoritative since it always has a Windows runner available.

## Packaging — read this before writing code

Target: **plain Win32 desktop app, UNPACKAGED. No MSIX.**

**RESOLVED — this section's original premise was wrong.** It assumed
`AudioPlaybackConnection` needs the `bluetooth` DeviceCapability and therefore
package identity, which would have forced a sparse MSIX package. It does not.
The spike (`src/bin/spike-a2dp.rs`) cleared every gate the capability could be
enforced at — class activation, enumeration, `TryCreateFromId`, `Start()`, and
`Open()` — from an ordinary `cargo run` exe with no identity of any kind.
`Open()` returned `Success`; `DeniedBySystem` never appeared. Full evidence in
`docs/verify.md`.

This is the good outcome. It means the process is an ordinary desktop process
that Windows never suspends, so UWP app suspension under memory pressure —
the prime suspect for the Store app's failure mode — is designed out rather
than worked around. There is no packaging work in this project.

Re-run `cargo run --bin spike-a2dp` if a future Windows build is suspected of
tightening this. `--start-only` exercises the radio without opening a stream,
so the check is safe to run on a machine in use; bare (no flags) is read-only.

## Connection lifecycle

```
DeviceInformation::FindAllAsync(AudioPlaybackConnection::GetDeviceSelector())
  -> pick paired device by Id (persisted in config)
  -> AudioPlaybackConnection::TryCreateFromId(id)
  -> StartAsync()    // advertise PC as available sink
  -> OpenAsync()     // open the audio stream when remote connects
  -> StateChanged    // Closed | Opened
```

Confirm the exact `StartAsync` / `OpenAsync` split and `AudioPlaybackConnectionState` variants against current windows-rs metadata rather than trusting this sketch.

`StateChanged` is a **link**-state signal, not a flow signal. Log it, surface it in the UI, never use it alone to decide health.

## Health model — the core of the app

Two independent observations, cross-checked. Audio is considered dead when **the remote says it is playing and the endpoint is silent.**

### Signal A — is sound actually coming out

`IMMDeviceEnumerator` -> default render endpoint (`eRender`, `eConsole`) -> `IAudioMeterInformation::GetPeakValue()`, polled ~10 Hz.

Prefer the *session*-scoped meter if the A2DP render session is attributable: `IAudioSessionManager2::GetSessionEnumerator` -> `IAudioSessionControl2::GetProcessId` -> QI for `IAudioMeterInformation`.

**RESOLVED 2026-09-14, amended 2026-09-16.** It *is* attributable, and not to our PID: A2DP render audio lands on a protected `svchost.exe`, so the session identifier — which embeds the host binary path — is the usable key, not the PID. Session-scoped Signal A is therefore viable.

**With one correction that cost 31 minutes of false silence:** there can be more than one matching `svchost` session. A re-pair mints a new render session and the old one lingers, `Inactive` and permanently silent, matching the same rule. Hold **every** match and report the max; binding to the first bound to the corpse, and `GetPeakValue` on it *succeeds* with `0.0`, so nothing ever re-resolved. See `docs/verify.md`.

### Signal B — does the remote think it is playing

`GlobalSystemMediaTransportControlsSessionManager::RequestAsync()` -> find the session corresponding to the connected device -> `GetPlaybackInfo().PlaybackStatus`.

When the PC is the A2DP sink, the phone should surface as a GSMTC session via AVRCP.

**RESOLVED 2026-09-16: it does not.** Measured with a freshly re-paired phone actively streaming — the cleanest conditions available — GSMTC reported only a local browser. This iPhone publishes no AVRCP metadata, so **Signal B is permanently unavailable on this hardware.**

The app degrades as designed: longer silence timeout, explicit user-triggered reconnect, and it says so in the UI. It does not reconnect on Signal A alone, which would thrash during quiet passages.

**But read the consequence, because it is severe.** With no Signal B and `recover_without_remote_signal = false`, `Recover(SilentWhilePlaying)` is *unreachable* — so the watchdog described below as "the core of the app" cannot fire for the fault it was written to catch. The remaining automatic path needs failed opens on a **closed** link, and the real fault presents as one that is **open**. Flipping the flag is not a fix: every pause past the degraded timeout would tear the link down, and a teardown makes iOS drop the route. This is the most important open problem in the project.

### Decision rule

```
if remote_playback == Playing
   && peak < SILENCE_EPS          // default 0.0005
   && elapsed_since_last_peak > SILENCE_TIMEOUT   // default 3s
then -> recover()
```

`recover()`: drop the `AudioPlaybackConnection`, release COM interfaces, re-run the lifecycle above. Target under 1s end to end. Call `Close()` explicitly rather than relying on `Drop`, and do not treat `Open() == Success` as "live" — see below.

**Amendment from observed behaviour (`docs/verify.md`), not yet folded into
the model above:**

1. `Open()` returns `Success` while `State()` still reads `Closed`; the
   `Opened` transition arrives asynchronously afterwards. So a recovery is
   only complete once `StateChanged -> Opened` is seen, under a timeout.
   `Success` followed by no transition is a *failed* recovery.
2. The link goes `Opened -> Closed` on its own during normal use. A `Closed`
   transition is therefore **not** a fault, but the app cannot idle in
   `Closed` either or audio will not resume. This splits the single
   `recover()` above into two paths:

   - **re-arm** — benign, expected, no backoff, no reconnect-log entry
   - **recovery** — the watchdog fired, audio is genuinely dead, backoff
     applies, gets a log entry

   Collapsing them gives either a reconnect log full of noise every time a
   song ends, or a ladder backed off to 60s during normal use that then
   responds sluggishly to a real fault.

Backoff on repeated failures: 1s, 2s, 5s, 10s, 30s, cap at 60s. Reset the ladder after 60s of healthy flow. Never busy-loop reconnect attempts — that is worse than the bug being fixed.

## Re-arm triggers

Re-run the lifecycle on each of these, independent of the meter watchdog:

| Trigger | Mechanism |
|---|---|
| Resume from sleep / modern standby | `RegisterSuspendResumeNotification`, handle `PBT_APMRESUMEAUTOMATIC` and `PBT_APMRESUMESUSPEND` |
| Bluetooth radio toggled | `Windows.Devices.Radios.Radio::StateChanged` |
| Default render device changed | `IMMNotificationClient::OnDefaultDeviceChanged` |
| Device appears / disappears | `DeviceWatcher` over the playback-connection selector |

The default-device change case matters: the render target is bound when the connection opens and does not follow the system default. Switching outputs therefore requires a full reopen, not just a UI refresh.

Debounce all four — resume in particular fires a burst of overlapping events. Single-flight the recovery path with a mutex/flag so watchdog and trigger paths cannot reconnect concurrently.

## Radio power management

Bluetooth adapters (especially USB dongles) default to "allow the computer to turn off this device to save power," which produces exactly this symptom class.

The app should not silently rewrite this. **Detect and warn.**

**DONE 2026-09-16.** Read from the registry rather than SetupAPI: unticking the box writes `IdleInWorkingState = 0` under the devnode's `Device Parameters\WDF`, and a `REG_DWORD` read is something `platform/` can do in a dozen lines and get right. Reported as layer 0 by `--debug-sessions`, and surfaced in the window as a banner that names the exact change before a one-click fix applies it through an elevated helper — so the app never holds an elevated token and nothing happens without the user asking. An absent value reads as `Unknown` and warns about nothing.

It was enabled on the target machine, and has been turned off. Whether it caused anything remains **unproven**.

## UI

Tray-resident, window optional and closable to tray. Single view:

- Device row: name, link state, and a **live peak meter** — the meter is the feature, it tells the user at a glance whether the thing is actually working
- Big honest status line: `Streaming` / `Connected, silent` / `Reconnecting (attempt 3)` / `Disconnected`
- Manual **Reconnect** button, always enabled
- **Reconnect log**: scrolling list of timestamped recovery events with reason. This is what separates "it healed itself twice last night" from "it has been broken for an hour"
- Settings: autostart, start minimized, silence timeout, preferred device
- No modal dialogs. No toasts on successful auto-recovery — silent recovery should be silent

Follow Lockstep's egui idioms: dark by default, compact spacing, no decorative chrome.

## Config

TOML beside the exe:

```toml
device_id = "BluetoothLE#..."
silence_timeout_ms = 3000
silence_eps = 0.0005
autostart = true
start_minimized = true
```

## Testing

Target is roughly **1:1 test code to production code**. That ratio is only achievable — and only meaningful — if the OS surface is isolated first. Write the architecture below before writing the tests, or the ratio will be met with worthless tests over interop shims.

### Layering

```
src/
  core/       pure logic, zero windows-rs imports, fully testable
  platform/   WinRT + COM + Win32 shims, thin, near-zero logic
  ui/         egui, thin
  main.rs     wiring
```

`core/` depends on traits, not on Windows:

```rust
trait AudioMeter        { fn peak(&self) -> f32; }
trait RemotePlayback    { fn status(&self) -> Option<PlaybackStatus>; }
trait SinkConnection    { fn open(&mut self) -> Result<()>; fn close(&mut self); fn link_state(&self) -> LinkState; }
trait Clock             { fn now(&self) -> Instant; }
```

`platform/` implements them for real. Tests implement them as fakes. **Rule: if a function contains a branch, it belongs in `core/`.** The interop layer should be so dumb it cannot be wrong in an interesting way.

### Ratio enforcement

Enforce 1:1 on `core/` only. Exempt `platform/` and `ui/` explicitly — padding tests onto unmockable FFI is how coverage targets produce fake confidence.

CI step: count with `tokei`, compare `src/core/` production lines against `#[cfg(test)]` blocks plus `tests/`, fail under 0.9:1. Put the exemption list in the CI script with a comment saying why, so nobody "fixes" it later.

### What gets tested

The health state machine is the whole point and should be tested exhaustively:

- **Decision table.** Every combination of `{Playing, Paused, Stopped, Unknown, Unavailable} × {peak above/below eps} × {elapsed above/below timeout}` → expected `Action`. Table-driven, one assertion per row.
- **The false-positive cases specifically.** Quiet passage in a song, user paused, remote disconnected cleanly, no AVRCP metadata available. Each must produce `Action::None`. These matter more than the positive cases — a reconnect storm during a quiet intro is worse than the original bug.
- **Backoff ladder.** 1/2/5/10/30/60 progression, cap holds, resets only after 60s of continuous healthy flow — not after a single good sample.
- **Single-flight.** Watchdog fires while a trigger-initiated recovery is in progress → exactly one reconnect.
- **Debounce.** Burst of resume events collapses to one recovery.
- **Config.** Serde round-trip, every default applied on a missing key, unknown keys ignored rather than fatal.

Use a fake `Clock` throughout — no `sleep()` in tests, no flake, suite stays under a second.

Add `proptest` for two invariants: never more than N reconnects in any rolling window regardless of input sequence, and given sustained `Playing` + silence, a recovery is always eventually emitted.

### The fake that earns its keep

`FakeConnection` must be able to report `Opened` while its meter reads zero. That is the actual production bug, reproduced deterministically in a unit test — something the real hardware will not do on demand. This is the single most valuable test in the repo; write it first and let the state machine grow around it.

### Manual soak matrix

Not automatable, run before any release:

1. Connect phone, play audio, confirm the meter moves
2. Sleep the PC 5 min, resume — audio returns with no interaction
3. Toggle Bluetooth off/on from Action Center
4. Switch default output device mid-stream
5. Force memory pressure, confirm the process is not suspended
6. Stream 8h overnight, then read the reconnect log

Every one ends in `Streaming` with zero manual steps. Record results in `docs/soak.md` with date and build hash.

### Tooling

`cargo nextest run`, `cargo clippy -- -D warnings`, `cargo fmt --check` in CI on `windows-latest`. Ship a `--debug-sessions` flag that dumps the WASAPI session list and GSMTC sessions, for the two VERIFY items above. (Both resolved 2026-09-16; the tool stays, because it is how any future "Windows says connected and the phone disagrees" gets answered.)

## Non-goals

- Output device selection, DSP, multi-output routing (that is Lockstep's territory — if it is ever wanted, feed a virtual endpoint and let Lockstep take it from there)
- **Volume boost past 100%. Asked for 2026-09-21, measured, and closed as
  impossible here — do not re-open it without reading `docs/verify.md`.**
  Every Windows volume API stops at unity (the endpoint reports its own
  range as `-96.0 dB .. 0.0 dB`), so boosting means owning the PCM.
  Process loopback *activates* against the protected A2DP `svchost` but
  delivers an all-zero timeline — proven against a known-amplitude tone
  that captured correctly in the same run. Classic endpoint loopback does
  capture the samples, but only mixed with every other app, which cannot
  support a targeted boost. What remains is an APO or a virtual audio
  device; both are signed drivers, and both are the bullet above.
- LDAC/aptX or any codec control
- Latency tuning
- Being an AVRCP controller beyond reading playback state
- Supporting anything below Windows 10 2004

## Order of work

1. ~~Repo on Gitea, GitHub mirror remote, CI skeleton green on an empty crate~~ **DONE**
2. ~~Packaging spike (unpackaged vs sparse MSIX) — gates everything~~ **DONE — unpackaged wins, no MSIX**
3. ~~Bare connect + render, no UI, confirm audio flows~~ **DONE** — audible
   out of the PC's speakers, 2026-09-14.
4. ~~`--debug-sessions` and resolve both VERIFY items~~ **DONE** — the tool
   reports five layers now, and both VERIFY items are resolved as of
   2026-09-16. Signal A is attributable; Signal B is absent on this hardware,
   permanently. See `docs/verify.md`.
5. ~~`core/` traits + `FakeConnection` + health state machine, test-first~~
   **DONE** — 57 tests, ratio 2.5:1, `FakeConnection` reproduces the
   `Opened`-while-silent bug deterministically
6. ~~`platform/` impls behind those traits, `recover()` wired up~~ **DONE**
7. ~~Re-arm triggers~~ **DONE** — 3 event-driven + 1 polled, 3/3 registered
8. ~~egui UI and tray~~ **DONE**
9. ~~Config, autostart, logging~~ **DONE** — and logging made *durable*
   2026-09-16; before that the file recorded nothing but process starts, and
   six restarts during an outage erased the evidence six times over.
10. Overnight soak, record in `docs/soak.md` — **still outstanding**

Added after the first real field failure, 2026-09-16:

11. **Presence reader, wired to logging and to nothing else.** Log
    `(link, classic, le, Open() outcome)` and act on none of it, then leave it
    running for an ordinary day. That measurement decides whether a stale-bond
    rule is possible at all — a first attempt at one was written and removed
    because it would most likely have fired on the normal idle state. See
    commit `56671c3`.
12. **Consider making Reconnect diagnostic.** With Signal B absent, a button
    press is the only unambiguous "I want audio now" this PC ever receives.
    Having it report which layer failed would turn a day-long mystery into a
    five-second answer. Weigh against the non-goal on feature expansion.
