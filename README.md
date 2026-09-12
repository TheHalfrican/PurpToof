# PurpToof

A Windows Bluetooth audio receiver (A2DP sink): your phone connects over
Bluetooth and the audio comes out of the PC speakers.

It exists because the Microsoft Store "Bluetooth Audio Receiver" app drops the
audio path while still reporting itself connected, and needs a manual restart.
**The entire point of PurpToof is that it recovers itself.**

## Where to start

- **`docs/progress.md`** — current state and what to do next. Start here.
- `CLAUDE.md` — the design, the health model, and the order of work.
- `docs/verify.md` — what was actually observed on hardware, including three
  ways the CLAUDE.md lifecycle sketch turned out to be wrong. Read before
  touching the platform layer.
- `docs/soak.md` — the pre-release manual soak matrix and the test rig.
