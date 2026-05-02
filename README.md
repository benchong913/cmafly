# cmafly

[![CI](https://github.com/benchong913/cmafly/actions/workflows/ci.yml/badge.svg)](https://github.com/benchong913/cmafly/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Edition](https://img.shields.io/badge/edition-2024-orange.svg)](./Cargo.toml)
[![Toolchain](https://img.shields.io/badge/rust-stable-brightgreen.svg)](./rust-toolchain.toml)

On-demand HLS / CMAF origin for VOD MP4 archives — **no segment files on disk**.

[English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md)

A Rust toolkit that serves CMAF (fragmented MP4) HLS straight from your existing
`.mp4` archives. An offline indexer walks each MP4 once and writes a compact
`.idx` (~13 MB / 2 h video) capturing everything needed to assemble segments at
request time. The HTTP origin holds those indexes via `mmap`, and on each
`GET /v/{id}/seg_{NNNN}.m4s` it builds a fresh `styp + moof + mdat` segment from
`(.idx + .mp4)` into memory and streams it to the wire.

No segments on disk, no transcoding, no duplicated storage.

```
offline (once per archive)          request time (per segment)

.mp4 ─▶ cmafly-index ─▶ .idx ─┐
                              ├─▶ cmafly-serve ─▶ seg_NNNN.m4s
                        .mp4 ─┘   (mmap; in-memory assembly)
```

## Why

Pre-generating HLS segments has two costs: storage doubles (or worse) for
content that may never be played, and the catalog becomes write-once — replacing
or re-encoding an original means deleting and rewriting thousands of small
files. cmafly keeps the original MP4 as the only source of truth and treats
segments as a pure function of `(.idx, .mp4, segment_index)`. A 13 MB index per
2-hour archive replaces ~3.7 GB of pre-generated `.m4s` files. New archives are
served the moment both files appear on disk; deleted archives stop being served
as soon as their LRU entry is evicted.

## What's in this repo

| Crate | Kind | Role |
| --- | --- | --- |
| `cmafly` (`crates/packager`) | library | Demux, segmentation, fMP4 / playlist writers, `.idx` format. Pure sync, no I/O, no async. |
| `cmafly-index` (`crates/indexer`) | binary | Offline: walk one MP4, emit one `.idx` (atomic write). |
| `cmafly-serve` (`crates/server`) | binary | Long-running `tokio` + `axum` HTTP origin: assemble segments per request from `.idx + .mp4`. |

The split is deliberate: only the server crate depends on `tokio`, `axum`, and
`lru`. The library is reusable in non-async contexts and carries only
`thiserror`, `byteorder`, `memmap2`, and `blake3`.

## Scope (v1)

- VOD only.
- Single bitrate, one video track + one audio track, muxed CMAF.
- Video: H.264 (`avc1`) or HEVC (`hvc1` / `hev1`). Audio: AAC (`mp4a`).
- No transcoding, no encryption, no LL-HLS, no master playlist, no subtitles,
  no alternate renditions.

## Requirements

- Rust **stable** (edition 2024). The repo pins `channel = "stable"` via
  `rust-toolchain.toml`; `rustup` will pick it up automatically.
- Linux or macOS. The atomic-write `fsync(parent_dir)` path is Unix-only;
  the rest of the code is portable.

Install Rust if needed:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Quick start

Build everything:

```sh
cargo build --release
```

Build a `.idx` for one archive:

```sh
cargo run --release -p cmafly-index -- \
    --input  /path/to/originals/abc.mp4 \
    --output /path/to/index/abc.idx \
    --segment-duration 6.0
```

Run the origin:

```sh
cargo run --release -p cmafly-serve -- \
    --media-dir /path/to/originals \
    --index-dir /path/to/index \
    --bind 127.0.0.1:8080
```

Capacity knobs (`--max-open-archives`, `--max-inflight-segments`,
`--permit-wait-timeout`) auto-resolve from host limits at startup; the server
logs the resolved values, their source (auto vs flag), and the host inputs that
produced them. Override only when the auto values are wrong.

Request a stream — the `id` is the source filename without extension:

| Route | Meaning |
| --- | --- |
| `GET /v/{id}/playlist.m3u8` | Media playlist |
| `GET /v/{id}/init.mp4` | CMAF init segment |
| `GET /v/{id}/seg_{NNNN}.m4s` | One CMAF media segment, assembled per request |
| `GET /healthz` | Liveness probe |

Point a CMAF-capable player (Safari, hls.js, ExoPlayer, …) at
`http://127.0.0.1:8080/v/abc/playlist.m3u8`.

TLS terminates upstream — `cmafly-serve` runs plain HTTP and is meant to sit
behind a CDN or reverse proxy.

## Operating

The server picks up content live from `--media-dir` and `--index-dir` — no
restart, no startup scan, no registration step.

- **Add.** Drop `<id>.mp4` and `<id>.idx` into their respective directories.
  Both must exist before the first request.
- **Remove.** Delete one or both files. New requests return `404`; entries
  already in the LRU may keep serving briefly until they're evicted or the
  process restarts.
- **Replace.** Re-run `cmafly-index` to rebuild `<id>.idx` (atomic write).
  **Never overwrite or truncate the source `.mp4` while its archive is in the
  LRU hot set** — the server holds it via `mmap`, and an in-place modification
  can deliver `SIGBUS` and crash the process. Same-length replacements that
  bypass re-indexing are not detected at runtime and silently emit
  content-garbage segments.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same three checks on stable; see
[`.github/workflows/ci.yml`](./.github/workflows/ci.yml).

Some integration tests look for an MP4 fixture under
`tests/fixtures/sample.mp4` (or the path in the `HLS_TEST_FIXTURE_MP4`
environment variable). Tests that cannot find the fixture skip themselves with
a stderr note — none of the unit suite depends on a binary blob. Fixtures
larger than 1 MB must not be committed (see `.gitignore`).

## Project layout

```
cmafly/
├── Cargo.toml              workspace root
├── rust-toolchain.toml     stable channel pin
├── README.md               this file
├── README.zh-CN.md         Simplified Chinese translation
├── README.zh-TW.md         Traditional Chinese translation
├── LICENSE-MIT
├── LICENSE-APACHE
├── crates/
│   ├── packager/           library
│   ├── indexer/            cmafly-index binary
│   └── server/             cmafly-serve binary
└── tests/fixtures/         local-only MP4 fixtures (gitignored)
```

## Status

v1 — first usable cut. The `cmafly` library API and the `.idx` on-disk format
are unstable across versions; rebuild your indexes when upgrading.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE))
- MIT license ([LICENSE-MIT](./LICENSE-MIT))

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall
be dual-licensed as above, without any additional terms or conditions.
