# Third-party notices

The STM32 peripheral models and parts of the timing framework in `crates/` are Rust ports of C# sources from [Renode infrastructure `066a7f13c052215632d469c995c89aea37c573b1`](https://github.com/renode/renode-infrastructure/tree/066a7f13c052215632d469c995c89aea37c573b1), the Renode 1.17.0 baseline. Each ported file keeps a `Ported from Renode 1.17.0 <path> (MIT License, Copyright (c) Antmicro)` header. The MIT license text is [Renode-infrastructure-MIT.txt](Renode-infrastructure-MIT.txt), byte-identical to `licenses/MIT.txt` at that commit (SHA-256 `8f71659370c5268d9a1dc962a46232540e8fca63462586d8efaa95aab492a208`).

The Cortex-M4F core and FPU are written from scratch. tlib (LGPL) was consulted only for behaviour; this repository contains no tlib code.

Files marked `Ported from emulation/models/NGC*.cs` derive from the project's own Renode models, which live in the separate Renode-based analysis workspace (not public).

These notices do not assign a license to the authored emulator code or to the original firmware. The authored code is covered by the repository's `LICENSE` (MIT). The firmware is never part of this repository.
