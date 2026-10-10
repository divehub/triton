# Third-party notices

The STM32 peripheral models and parts of the timing framework in `crates/` are Rust ports of C# sources from [Renode infrastructure `066a7f13c052215632d469c995c89aea37c573b1`](https://github.com/renode/renode-infrastructure/tree/066a7f13c052215632d469c995c89aea37c573b1), the Renode 1.17.0 baseline. Each ported file keeps a `Ported from Renode 1.17.0 <path> (MIT License, Copyright (c) Antmicro)` header. The MIT license text is [Renode-infrastructure-MIT.txt](Renode-infrastructure-MIT.txt), byte-identical to `licenses/MIT.txt` at that commit (SHA-256 `8f71659370c5268d9a1dc962a46232540e8fca63462586d8efaa95aab492a208`).

The Cortex-M4F core and FPU are written from scratch. tlib (LGPL) was consulted only for behavior; this repository contains no tlib code.

Files marked `Ported from emulation/models/NGC*.cs` derive from the project's own Renode models, which live in the separate Renode-based analysis workspace (not public).

## Recorded audio (the dive game's MAV sound)

The dive game's MAV sound uses four short recorded clips, `web/mav-oxygen-onset.wav`, `web/mav-oxygen-loop.wav`, `web/mav-diluent-onset.wav` and `web/mav-diluent-loop.wav`: edited excerpts (an opening of 0.18 s and a loop of 0.70 s for each of the two gases, 44.1 kHz mono 16-bit) of a regulator breathing sound effect that the project owner downloaded.

- Source: the file `spinopel-to-breathe-with-a-scuba-gear-429804` on Pixabay, by the user "spinopel" (item 429804).
- License: the Pixabay Content License, which the project owner states makes it free to use in a product. This repository did not check that independently.
- What is shipped: only those four excerpts, never the whole recording; they are not covered by the repository's MIT license. All other sounds of the game are synthesized in the browser (`web/game-sound.js`); no other audio is published (`deploy/build_site.py` allows exactly these four files).

These notices do not assign a license to the authored emulator code or to the original firmware. The authored code is covered by the repository's `LICENSE` (MIT). The firmware is never part of this repository.
