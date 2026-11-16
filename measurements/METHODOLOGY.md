# Measurement methodology

Every measurement in this folder is one record, written with the template below. A record states what was measured,
how, on what, and how far the numbers can be trusted. A claim in the docs or the changelog points at a record.

## Rules

- **Unknown is a valid answer.** If a field was not recorded (the repeat count, the exact build, the error margin),
  write `Unknown`. Never fill it in from memory or by guessing.
- **One environment per record.** If the same test ran on two machines or two browser versions, write two result
  rows and say which is which.
- **Numbers keep their unit.** CPU is "% of one core" unless the record says otherwise (100 % = one core fully busy;
  a 12-core machine has 1200 %).
- **Dates are absolute** (YYYY-MM-DD).
- **A record is not rewritten when the code changes.** It gets a new `Status` and, if it was measured again, a link
  to the newer record.

## Template

```markdown
# <Title>

| Field | Value |
| --- | --- |
| Date | YYYY-MM-DD (or a range) |
| Group | the big area, for example `Dolby Vision`, `Playback performance`, `Audio`, `Network` |
| Type | `Performance`, `Quality`, `Compatibility`, `Behaviour` |
| Section | the narrow topic, for example `Profile 8.1`, `Profile 7 FEL`, `Seek` |
| Environment | machine, CPU, OS, browser and version, display, engine commit or version |
| Status | `Current`, `Superseded by <record>`, `Historical (code path removed)` |

## Purpose

The question this measurement answers, in one or two sentences.

## Implementation

| Field | Value |
| --- | --- |
| Test asset | file name, container, codecs, resolution, frame rate, bitrate, duration used |
| Baseline | what the result is compared against (a plain `<video>`, an exact reference decode, an earlier build) |
| Methodology | the procedure, the metrics and how they are collected, the number of repeats |
| Observations | what was seen while running it that the numbers alone do not show |
| Margin of error | spread between repeats, tool resolution, known noise sources; `Unknown` if not recorded |

## Results

Tables first. Every number with its unit.

## Limitations

What the measurement does not cover, and conditions under which it would not hold.

## Fixes

Changes made because of this measurement, with the commit or changelog entry. `None` if it changed nothing.
```

## Common definitions

| Term | Meaning |
| --- | --- |
| CPU % | process CPU from `ps` (`%cpu`), as % of one core, averaged over the measure window. For Chrome, the sum over the whole process tree (browser, GPU process, renderers, utility processes) unless the record names one process. |
| RSS | resident set size from `ps`, summed the same way. It double counts shared pages, so treat it as an upper bound. |
| Frames delivered | frames actually presented in the measure window, from `requestVideoFrameCallback` or the engine's own presented-frame counter. |
| Dropped frames | `getVideoPlaybackQuality().droppedVideoFrames` for a `<video>`, the engine's own counter for the canvas route. |
| PSNR | peak signal to noise ratio against the stated reference, in dB. `inf` means bit-identical. |
| Route | how the session plays the video: `mse` (a `<video>` fed through Media Source) or `canvas` (WebCodecs decode drawn with WebGPU). |

## Known noise sources

These apply to most records and are not repeated in each one:

- **A busy machine.** Development machines run other apps. CPU numbers swing by several points between identical
  runs; core placement alone moved one Chrome measurement by up to 12 points.
- **Hidden windows.** Chrome pauses video-only media and drops the video track of media with sound when the page is
  hidden or the window is fully covered. A run during which the window was hidden is invalid. Benchmarks either keep
  the window visible or start Chrome with `--disable-backgrounding-occluded-windows --disable-renderer-backgrounding
  --disable-background-timer-throttling`.
- **Tracing overhead.** A Chrome trace raises CPU use. Compare traced runs only with each other.
- **Display refresh.** The test machines have 120 Hz ProMotion displays; frame pacing results may differ at 60 Hz.

## Index

The list of records is [README.md](README.md).
