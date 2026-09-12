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

`AudioPlaybackConnection` requires the `bluetooth` **DeviceCapability**, which requires **package identity**. But a packaged UWP app gets the UWP lifecycle, and app suspension under memory pressure is a prime suspect for the Store app's failure mode.

Target: **Win32 desktop app + sparse MSIX package** — identity for the capability check, but the process is an ordinary desktop process that Windows never suspends.

**VERIFY FIRST, before building anything else:** write a ~50-line spike that calls `AudioPlaybackConnection::GetDeviceSelector()` and `TryCreateFromId()` from an unpackaged Win32 exe. If it works unpackaged, skip MSIX entirely. If it fails, confirm sparse MSIX satisfies the capability check. This decision gates the whole project — do not proceed on assumption.

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

**VERIFY:** the internally-rendered A2DP audio may not appear as a session owned by our PID — Windows may render it from the audio engine or a system process. Enumerate sessions while streaming and find out where it actually lands. If it is not attributable, fall back to the endpoint-level meter and lean harder on Signal B to avoid false positives from other apps' audio.

### Signal B — does the remote think it is playing

`GlobalSystemMediaTransportControlsSessionManager::RequestAsync()` -> find the session corresponding to the connected device -> `GetPlaybackInfo().PlaybackStatus`.

When the PC is the A2DP sink, the phone should surface as a GSMTC session via AVRCP. **VERIFY** this holds for the target phone; some devices publish no AVRCP metadata. If Signal B is unavailable, degrade gracefully: fall back to a longer silence timeout plus an explicit user-triggered reconnect, and say so in the UI. Do not silently reconnect on Signal A alone — that will thrash during genuine quiet passages.

### Decision rule

```
if remote_playback == Playing
   && peak < SILENCE_EPS          // default 0.0005
   && elapsed_since_last_peak > SILENCE_TIMEOUT   // default 3s
then -> recover()
```

`recover()`: drop the `AudioPlaybackConnection`, release COM interfaces, re-run the lifecycle above. Target under 1s end to end.

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

The app should not silently rewrite this. **Detect and warn**: read the adapter's power-management state via SetupAPI device properties and show a one-line banner with a "how to fix" link when selective suspend is enabled. Optional, low priority, but it will explain a class of failures the watchdog can only paper over.

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

`cargo nextest run`, `cargo clippy -- -D warnings`, `cargo fmt --check` in CI on `windows-latest`. Ship a `--debug-sessions` flag that dumps the WASAPI session list and GSMTC sessions, for the two VERIFY items above.

## Non-goals

- Output device selection, DSP, multi-output routing (that is Lockstep's territory — if it is ever wanted, feed a virtual endpoint and let Lockstep take it from there)
- LDAC/aptX or any codec control
- Latency tuning
- Being an AVRCP controller beyond reading playback state
- Supporting anything below Windows 10 2004

## Order of work

1. Repo on Gitea, GitHub mirror remote, CI skeleton green on an empty crate
2. Packaging spike (unpackaged vs sparse MSIX) — gates everything
3. Bare connect + render, no UI, confirm audio flows
4. `--debug-sessions` and resolve both VERIFY items
5. `core/` traits + `FakeConnection` + health state machine, test-first
6. `platform/` impls behind those traits, `recover()` wired up
7. Re-arm triggers
8. egui UI and tray
9. Config, autostart, logging
10. Overnight soak, record in `docs/soak.md`
