# RPU template test data

`fel-template.bin` is the 384-byte synthetic RPU template used by the upstream
`Probes/DolbyVisionPulse` synthetic fixture generator. It contains an Annex-B-style `00 00 00 01` prefix
followed by the RPU payload beginning with `19`, without the HEVC type-62 header.
Tests replace the prefix with `7c 01` before CRC/preflight/semantic inspection.

SHA-256: `b2b27714b7279c4e24d1a795cb6f95d3ad06745db96d360698ee0932184117a0`.
This is synthetic test metadata, not a film frame and not proof of FEL residual
reconstruction, display management or native Dolby Vision output.
