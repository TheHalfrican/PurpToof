# Soak results

Not automatable. Run the full matrix before any release and record the outcome
here with the date and the build hash. Every row must end in `Streaming` with
zero manual intervention.

| # | Scenario |
|---|---|
| 1 | Connect phone, play audio, confirm the meter moves |
| 2 | Sleep the PC 5 min, resume — audio returns with no interaction |
| 3 | Toggle Bluetooth off/on from Action Center |
| 4 | Switch default output device mid-stream |
| 5 | Force memory pressure, confirm the process is not suspended |
| 6 | Stream 8 h overnight, then read the reconnect log |

## Runs

_No release runs yet._

## Hardware notes

Reference test rig, recorded because several results are device-specific:

- **Host:** Windows 11 Pro, build 26200 (well above the 19041 floor)
- **Radio:** Intel(R) Wireless Bluetooth(R)
- **Phone:** iPhone 15 Pro Max — pairs as `Noah's iPhone`, exposes
  `A2DP SNK` plus an `Avrcp Transport` node, so Signal B (GSMTC over AVRCP)
  is expected to be available on this rig. Do not generalise that to other
  phones; some publish no AVRCP metadata at all and the app must degrade.
- **Default render endpoint during development:** Astro A50 X (`A50 X Game`),
  a USB base station with a 2.4 GHz link to the headset — *not* a Bluetooth
  device, so it is not disturbed by radio-toggle tests.
