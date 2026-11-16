# Small genuine OAMD metadata snippet

`amaze-first-oamd.bin` contains only the first 67-byte object metadata payload
from the user-provided local Amaze TrueHD Atmos test stream. It contains no
compressed audio, video frame or PCM samples.

- SHA-256: `6ce737c77532be59459fe582b8370f23124b45f061389ef52940ab15bcab16ec`.
- Source first-five-second elementary clip SHA-256:
  `c235096029ac827c598fe0c3bd0abbfd9e299ef36977d233b48809e0a15b49d9`.
- Parsed assignment: one LFE bed element, fifteen dynamic objects.
- First update: sample offset zero, ramp duration 523 samples.

The programme clip and its decoded PCM stay local and ignored. The metadata
snippet tests parsed assignment, scheduling and finite point-source output;
it is not a Dolby reference render, format certification or physical output test.
`examples/extract-local-oamd.rs` reproduces extraction given the local clip.
