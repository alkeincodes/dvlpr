# `blocked.wav` — provenance

- **Purpose:** Embedded default for the "agent blocked" notification sound played by the dvlpr sidebar.
- **Format:** PCM WAV, 22050 Hz, 16-bit mono, ≤500 ms.
- **Source:** Synthesized procedurally with Python's stdlib `wave` + `math` (two-tone 880 Hz → 660 Hz chime with attack/decay envelope) on 2026-05-29.
- **Author / License:** Created by the dvlpr project; dedicated to the public domain under [CC0-1.0](https://creativecommons.org/publicdomain/zero/1.0/). No attribution required.
- **Integrity:** sha256 `ef73b0de4eceb5288e74fc99636ea494d6348bf2c0e350293157e4d1c0aa6c79`.

If the asset is regenerated, update the sha256 in this file in the same commit so the integrity record stays accurate.
