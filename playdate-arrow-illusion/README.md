# Arrow Illusion (Playdate)

A Playdate rendering of the classic optical illusion: diagonal stripes drift
at a **constant** rate, but arrow-shaped apertures laid over them hijack your
sense of motion. Turn the arrows with the crank and the very same stripes
appear to speed up, slow down, and reverse — even though the stripe motion
never changes.

## Controls

- **Crank** — rotate the arrows (this is what the illusion is all about).
- **D-pad ◀ ▶** — rotate the arrows when the crank is docked.

## Build

```bash
make        # build ArrowIllusion.pdx for the Simulator
make run    # build and open in the Playdate Simulator
make device # build for hardware (needs the ARM GCC toolchain)
```

Needs the [Playdate SDK](https://play.date/dev/) installed.
