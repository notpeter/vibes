# AGENTS.md

## Project

`playdate-arrow-illusion` is a Playdate game in C that renders the
"moving stripes / changing arrows" optical illusion.

- Diagonal stripes drift across the screen at a **constant** rate and
  direction — this never changes.
- Arrow-shaped apertures are laid over the stripes. The aperture
  (barber-pole) effect makes the perceived motion follow the arrows.
- Cranking the crank rotates the arrows, so the same stripes appear to
  speed up, slow down, and reverse.

## Stack

- C (Playdate C API), single file: `src/main.c`
- Playdate SDK (newest); simulator + device builds

## How it works

- `buildStripePattern()` builds an 8x8 `LCDPattern` of diagonal stripes and
  shifts it each frame by `stripePhase` (the constant drift).
- `buildArrow()` generates a rotated block-arrow polygon per grid cell;
  `fillPolygon()` fills it with the stripe pattern so stripes show only
  inside the arrows.
- Crank angle (`getCrankAngle`) sets the arrow direction; d-pad turns the
  arrows when the crank is docked.

## Important Files

- [src/main.c](src/main.c): all game logic and rendering
- [Source/pdxinfo](Source/pdxinfo): bundle metadata
- [Makefile](Makefile) / [CMakeLists.txt](CMakeLists.txt): SDK build

## Build

```bash
make          # simulator .pdx
make device   # hardware build (needs ARM GCC toolchain)
```

Requires the Playdate SDK installed (`~/.Playdate/config` set by the SDK
installer), or set `PLAYDATE_SDK_PATH` for the CMake build.
