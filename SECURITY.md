# Security policy

## Supported versions

Fixes land on the latest release only. Older versions are not patched.

## Reporting a vulnerability

Please report security issues **privately**, not as a public issue or pull request. Use GitHub's private reporting
(the repository's **Security** tab, then **Report a vulnerability**). Only you and the maintainers can see the report.

Please include:

- the version or commit, the browser and its version, and the operating system,
- what the issue is and what an attacker could do with it,
- steps or a proof of concept. For a malformed-media issue, a sample file or `ffprobe` output is ideal.

You can expect a first answer within a few days. When a fix is ready it ships in a new release and the advisory is
published with credit, unless you prefer to stay anonymous.

## Scope

FraxEngine parses untrusted media in the browser. The areas that matter most:

- **Media parsing.** Demuxing and decoding of untrusted containers and bitstreams in WebAssembly (Rust) and
  JavaScript: Matroska, MP4, MPEG-TS, Dolby Vision RPU, audio and subtitle decoders.
- **Network handling.** Range requests to the media URL the page passes in. The engine sends only the headers the host
  gives it and does not log URLs that carry tokens.
- **The reference pages.** `web/player` and the Jellyfin client in `web/app` (which stores the server's access token in
  the browser's `localStorage`, never the password).

Out of scope: bugs in the browser itself (report those to the browser vendor) and in a host site that embeds the
engine.
