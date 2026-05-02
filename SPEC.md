# Spec: cmafly

## Objective

A Rust toolkit for serving CMAF (fragmented MP4) HLS from MP4 archives **on demand**, without pre-generating segment files on disk. Two binaries + one library:

- **`cmafly-index`** — offline tool that walks an MP4 once and writes a compact `.idx` file (~13 MB / 2 h video). The `.idx` captures everything needed to dynamically build CMAF segments: per-track sample table, codec config blobs, segment boundaries, and the pre-rendered `init.mp4` payload.
- **`cmafly-serve`** — long-running HTTP origin (`tokio` + `axum`) that holds `.idx` files via `mmap`. On each `GET /v/{id}/seg_{NNNN}.m4s` it dynamically assembles the segment from `(.idx + original .mp4)` into an in-memory buffer and streams the response. No segment files exist on disk.
- **`cmafly`** — library that backs both binaries: demux, segmentation, fMP4 writers, playlist generation, and the `.idx` format.

**Scope (v1):** VOD only. Single bitrate. One video track (H.264 or H.265) + one audio track (AAC), muxed CMAF. No transcoding, no encryption, no LL-HLS, no master playlist, no subtitles, no alternate renditions.

**Target deployment:** A reference workload of ~5 000 MP4 archives, **expected to grow without bound over time** (the design must not impose a catalog ceiling — see _Scale Limits_), served to up to ~20 000 concurrent viewers behind a commercial CDN (Cloudflare / Bunny / equivalent). Origin runs as a single `axum` process on one server. Storage tiering (NVMe hot working set + indexes, HDD cold archives + backup) is an operational concern; the server makes no assumption about which tier holds a given file. CDN absorbs ~95 % of bandwidth — origin sees only cache-miss traffic.

## Tech Stack

- Rust **edition 2024**, tracking current **stable** channel.
- `rust-toolchain.toml` locks `channel = "stable"` for reproducibility; no `rust-version` pinned in `Cargo.toml`.
- **`clap`** v4 — CLI argument parsing
- **`thiserror`** — typed library errors; **`anyhow`** — binary-side error context
- **`byteorder`** — big-endian binary read/write for ISO/IEC 14496-12 atoms and fMP4 boxes
- **`memmap2`** — mmap input MP4 and `.idx` files (zero-copy, kernel page cache)
- **`blake3`** — fast hash binding `.idx` to source MP4 (offline audit only; not verified on the request path)
- **`tokio`** (`rt-multi-thread`, `net`) — async runtime for `cmafly-serve`
- **`axum`** v0.7+ — HTTP routing for `cmafly-serve`
- **`lru`** v0.12+ — bounded LRU eviction for the in-process index registry
- No `unsafe` outside (a) `IndexView` zero-copy slice cast (the only sanctioned site), (b) any future site explicitly justified with `// SAFETY:` comment.

The `crates/packager` library uses **only** the synchronous deps (`thiserror`, `byteorder`, `memmap2`, `blake3`). `tokio`, `axum`, and `lru` live exclusively in `crates/server`, keeping the library re-usable in non-async contexts.

**Prerequisite:** Rust toolchain not currently installed. Install via:
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Commands

```
Build:        cargo build --release
Test:         cargo test --workspace
Lint:         cargo clippy --workspace --all-targets -- -D warnings
Format:       cargo fmt --all
Format check: cargo fmt --all -- --check

Build index:  cargo run --release -p cmafly-index -- \
                --input /nvme/hls/originals/abc.mp4 \
                --output /nvme/hls/index/abc.idx \
                --segment-duration 6.0

Run server:   cargo run --release -p cmafly-serve -- \
                --media-dir /nvme/hls/originals \
                --index-dir /nvme/hls/index \
                --bind 127.0.0.1:8080
              # Capacity knobs default to auto (resolved at startup from
              # /proc/sys/vm/max_map_count and /proc/meminfo). Override only
              # when auto-derived values are wrong:
              #   --max-open-archives N         (LRU registry size)
              #   --max-inflight-segments N     (segment-assembly semaphore)
              #   --permit-wait-timeout SECS    (admission timeout, default 5)
```

## Project Structure

```
cmafly/
├── Cargo.toml              # workspace root
├── SPEC.md
├── README.md               # added later
├── rust-toolchain.toml
├── crates/
│   ├── packager/           # library: cmafly (no I/O / no async)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── error.rs            # PackagerError enum (thiserror)
│   │       ├── read_at.rs          # ReadAt trait abstracting MP4 access
│   │       ├── demux/              # ISO/IEC 14496-12 byte-range parser
│   │       │   ├── mod.rs
│   │       │   ├── reader.rs         # box header scan, big-endian primitives
│   │       │   ├── moov.rs           # parse moov/trak/mdia
│   │       │   └── sample_table.rs   # walk stts/ctts/stss/stsc/stsz/stco|co64
│   │       ├── segment/            # IDR-aligned segmentation
│   │       │   ├── mod.rs
│   │       │   └── splitter.rs
│   │       ├── fmp4/               # write fMP4 boxes (generic Write+Seek)
│   │       │   ├── mod.rs
│   │       │   ├── boxes.rs          # box headers, size patching
│   │       │   ├── init.rs           # ftyp + moov(mvex/trex)
│   │       │   └── media.rs          # styp + moof + mdat
│   │       ├── playlist/
│   │       │   ├── mod.rs
│   │       │   └── m3u8.rs           # write media playlist
│   │       └── index/              # .idx format
│   │           ├── mod.rs
│   │           ├── format.rs         # constants, layout, magic
│   │           ├── builder.rs        # build .idx from a Source
│   │           └── view.rs           # zero-copy IndexView<'a> over &[u8]
│   ├── indexer/            # binary: cmafly-index
│   │   ├── Cargo.toml
│   │   └── src/main.rs
│   └── server/             # binary: cmafly-serve
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           ├── config.rs
│           ├── registry.rs           # IndexRegistry: bounded LRU id → Arc<Entry>
│           ├── handlers.rs           # axum handlers
│           └── error.rs              # ApiError → HTTP status mapping
└── target/                 # gitignored
```

## Code Style

Top-level library API:

```rust
// crates/packager/src/lib.rs

pub use crate::error::PackagerError;
pub use crate::index::{
    AudioTrackMeta, IndexBuilder, IndexView, SampleEntry, SegmentEntry, VideoTrackMeta,
};
pub use crate::read_at::ReadAt;

pub struct IndexBuilder;

impl IndexBuilder {
    /// Build a complete `.idx` byte image from a source MP4.
    /// `source` is a random-access reader over the source MP4 (typically `Mmap`);
    /// `source_len` is its byte length; `segment_duration_secs` is the nominal cut
    /// duration. Real cuts snap forward to the next IDR — see _Segment Strategy_.
    pub fn build<R: ReadAt>(
        source: &R,
        source_len: u64,
        segment_duration_secs: f64,
    ) -> Result<Vec<u8>, PackagerError>;
}

impl<'a> IndexView<'a> {
    /// Validate magic, header, and section directory; return a borrowed view over
    /// `bytes`. Returns `Err` on magic mismatch, malformed directory (non-ascending
    /// offsets, duplicate kinds, out-of-range offsets, `section_count > 64`), or
    /// misaligned section payloads.
    pub fn open(bytes: &'a [u8]) -> Result<Self, PackagerError>;

    pub fn max_segment_size(&self) -> u32;
    pub fn source_mp4_len(&self) -> u64;
    pub fn source_mp4_blake3(&self) -> &'a [u8; 32];

    pub fn video_track(&self) -> VideoTrackMeta<'a>;
    pub fn audio_track(&self) -> AudioTrackMeta<'a>;

    pub fn video_samples(&self) -> &'a [SampleEntry];
    pub fn audio_samples(&self) -> &'a [SampleEntry];

    pub fn segments(&self) -> &'a [SegmentEntry];
    pub fn segment_count(&self) -> u32;

    pub fn init_segment_bytes(&self) -> &'a [u8];
    pub fn playlist_bytes(&self) -> Option<&'a [u8]>;
}

pub struct VideoTrackMeta<'a> {
    pub timescale: u32,
    pub fourcc: [u8; 4],
    pub width: u32,
    pub height: u32,
    /// Verbatim bytes of the video sample-entry box (`avc1` / `hvc1` / `hev1`),
    /// including codec-config child and any siblings.
    pub sample_entry: &'a [u8],
    /// Verbatim `edts/elst` bytes; empty slice if the input had no edit list.
    pub elst: &'a [u8],
}

pub struct AudioTrackMeta<'a> {
    pub timescale: u32,
    pub fourcc: [u8; 4],
    pub sample_rate: u32,
    pub channel_count: u8,
    pub sample_entry: &'a [u8],
    pub elst: &'a [u8],
}

pub mod fmp4 {
    /// Write the init segment (`ftyp` + `moov` with `mvex`/`trex`).
    /// Used by `IndexBuilder` to embed `init.mp4` bytes inside the `.idx`;
    /// `cmafly-serve` simply forwards those bytes.
    pub fn write_init_segment<W: Write + Seek>(
        index: &IndexView<'_>,
        out: &mut W,
    ) -> Result<(), PackagerError>;

    /// Assemble one media segment (`styp` + `moof` + `mdat`) for `segment_idx`,
    /// reading sample bytes via `sample_data`.
    pub fn write_media_segment<W: Write + Seek, R: ReadAt>(
        index: &IndexView<'_>,
        segment_idx: u32,
        sample_data: &R,
        out: &mut W,
    ) -> Result<(), PackagerError>;
}

pub mod playlist {
    /// Write the media playlist text. Pure function over the segment table.
    pub fn write_media_playlist<W: Write>(
        index: &IndexView<'_>,
        out: &mut W,
    ) -> Result<(), PackagerError>;
}
```

The `ReadAt` abstraction:

```rust
// crates/packager/src/read_at.rs
pub trait ReadAt {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize>;
}

impl ReadAt for memmap2::Mmap {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let off = offset as usize;
        if off >= self.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.len() - off);
        buf[..n].copy_from_slice(&self[off..off + n]);
        Ok(n)
    }
}
```

`cmafly-serve` uses `Mmap`. Tests can use a thin `ReadAt` impl over `&[u8]` (or a `Cursor`-based wrapper). The library never opens a file.

**Conventions:**
- `snake_case` for fns/modules, `PascalCase` for types, `SCREAMING_SNAKE_CASE` for consts.
- Library returns typed errors (`thiserror`); binaries wrap with `anyhow::Context`.
- No `unwrap()` / `expect()` outside `#[cfg(test)]`. Propagate with `?`.
- Function signatures prefer `&[u8]` over `Vec<u8>`, `&Path` over `PathBuf`.
- One responsibility per module; if a file exceeds ~300 LOC, split.
- All multi-byte writes are explicit big-endian via `byteorder::WriteBytesExt`.
- Box writers return the number of bytes written; size fields and `trun.data_offset` are patched after the fact via `Seek`.

### Errors

Library entry points return a single `PackagerError` enum. Variants are grouped by pipeline phase; messages and field shapes are implementation detail, but the variants below are the contract other SPEC sections reference.

*Demux validation (raised by `IndexBuilder::build`):*
- `Io(std::io::Error)` — wrapped `ReadAt` failures.
- `UnsupportedBrand` — `ftyp` major or compatible brand not in `{isom, mp42, iso2, iso4, iso5, iso6, cmfc, mp41}`.
- `MissingAtom(&'static str)` — required atom (`tkhd` / `mdhd` / `hdlr` / `stsd` / `stts` / `stsc` / `stsz` / `stco|co64`) absent for some track.
- `MalformedAtom { atom: &'static str, reason: &'static str }` — atom present but unparseable (truncated, bad version flag, declared size exceeds container).
- `UnsupportedTrackLayout { video: u32, audio: u32 }` — track count is not exactly 1 video + 1 audio.
- `UnsupportedVideoCodec` / `UnsupportedAudioCodec` — sample-entry fourcc outside the allow-list.
- `FragmentedInput` — top-level `moof` present.
- `EncryptedInput` — `senc` / `tenc` / `sinf` present, or `sbgp` / `sgpd` carries the CENC `seig` grouping_type. Non-encryption sample-group metadata (`rap `, `roll`, `sync`, …) is accepted.
- `SampleTableInconsistent(&'static str)` — `stsc` / `stco` / `stsz` cross-references disagree.

*`.idx` parsing (raised by `IndexView::open`):*
- `IndexMagicMismatch` — first 4 bytes ≠ `b"HCMI"`.
- `MalformedIndexDirectory(&'static str)` — non-ascending offsets, duplicate kinds, `section_count > 64`, first offset before header end, last offset past file end, or misaligned section start.
- `MalformedIndexSection(&'static str)` — payload size doesn't match declared content (e.g., `VideoSampleTable` size not a multiple of 24 B), or a required section is missing.

*Output assembly (raised by `fmp4::write_*_segment`):*
- `SegmentIndexOutOfRange { idx: u32, count: u32 }` — caller passed `segment_idx >= IndexView::segment_count()`.

The always-on box-size assertion (see _Boundaries_) is a panic, not a `PackagerError` variant — it signals an internal invariant break that callers cannot recover from.

## Testing Strategy

User opted out of broad unit-test coverage for v1, but two minimum invariant tests are kept in scope because they guard the unsafe / binary-format surface:

**v1 minimum (must ship):**
- **`.idx` round-trip** — `IndexBuilder::build(...)` against an in-memory fixture → write to a `Vec<u8>` → `IndexView::open(...)` → assert every public accessor (track meta, sample tables, segment table, init bytes, `max_segment_size`) equals the input. Catches header / section-directory / on-disk struct-layout drift before it reaches `cmafly-serve`.
- **Box-size invariant exercised end-to-end** — the always-on assertion in every fMP4 box writer (declared size == bytes written; see _Boundaries_) is run by building one fixture's `init.mp4` plus the first/last/largest media segments. No dedicated unit test; relies on the assertion firing under realistic input.
- **Manual smoke** — build one `.idx`, run `cmafly-serve`, confirm playback start-to-finish in Safari (macOS 14+) and hls.js (latest), with a mid-playlist seek and a 2-hour playthrough watching for audio drift.

**Deferred to v2:**
- Unit tests inline (`#[cfg(test)] mod tests`) per module.
- Integration tests in `crates/packager/tests/` against fixture MP4s in `tests/fixtures/` (generated via `ffmpeg`, not committed if > 1 MB).
- Property-based: `proptest` for `IndexView` round-trip across randomized inputs and box-size patching under random padding.
- Validation: run Apple's `mediastreamvalidator` against `cmafly-serve` HTTP responses in CI.

This is an explicit tradeoff: v1 correctness rests on the round-trip test plus manual playback verification. Any regression that escapes both layers is the cost of the chosen scope.

## Boundaries

**Always:**
- Run `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings` before any commit.
- Snap segment cuts to IDR boundaries — never split mid-GOP.
- After writing every fMP4 box, assert (always-on, not `debug_assert!`) declared size matches actual bytes written.
- Stream-process media payload (`mdat`); only sample-table metadata and codec config blobs live in memory.
- Validate input on `IndexBuilder::build`: exactly 1 video track (`avc1` / `hvc1` / `hev1`) + 1 audio track (`mp4a`); non-fragmented; fail fast otherwise.
- Byte-copy the entire video / audio sample-entry box (`avc1` / `hvc1` / `mp4a`) — including its codec-config child (`avcC` / `hvcC` / `esds`) and any optional siblings (`btrt`, `pasp`, `colr`) — and `edts/elst` from input into output `init.mp4` verbatim. Never reconstruct codec config or sample entry from parsed fields.
- `cmafly-serve` handlers are logically pure on `Arc<Registry>`: no per-request mutable state above the segment buffer, no disk write. This preserves the option to add `SO_REUSEPORT` multi-process later without code change.
- `IndexView` is the only legal way to inspect an `.idx`. Never `Vec<SampleEntry>`-deserialize a sample table.
- TLS terminates at the upstream reverse proxy / CDN. `cmafly-serve` runs plain HTTP.
- Bound origin process resource usage (mmap regions, FDs, registry size) by `max_open_archives` — a per-host hot-set knob, never by catalog size. The catalog grows without bound by design.
- Bound concurrent segment-assembly RAM via a `tokio::sync::Semaphore(max_inflight_segments)`; size each per-request buffer to `IndexView::max_segment_size()` (recorded in the `.idx` header). Segment-assembly RAM never tracks request rate.
- Resolve `max_open_archives` and `max_inflight_segments` once at startup (auto-derived from host limits unless overridden by flag) and **log the resolved values, the source (auto vs flag), and the host inputs that produced them**. Operators must be able to audit what the process actually picked without strace / debugger.

**Ask first:**
- Adding any dependency beyond those in Tech Stack.
- Expanding demux scope beyond what **Demux Scope** lists.
- Bumping MSRV.
- Changing the public API of `cmafly` crate.
- Any work that touches v1 scope (live, encryption, ABR, LL-HLS, master playlist, multi-process server model).

**Never:**
- Use `unsafe` without an explicit `// SAFETY:` comment justifying invariants.
- Decode codec internals (NALU walking, AAC bitstream, SEI). Demux scope is byte ranges + atom passthrough only.
- Accept fragmented input MP4 (top-level `moof`).
- Output MPEG-TS segments — pure fMP4 only.
- Skip IDR alignment to "just make it work."
- `cmafly-serve` writing to disk; `cmafly-serve` allocating per-request mutable state above the right-sized segment buffer (sized from `IndexView::max_segment_size()`, gated by the `max_inflight_segments` admission semaphore).
- Open the source MP4 with `BufReader<File>` per request; use mmap.
- Commit fixture binaries > 1 MB.
- Enumerate `index_dir` at startup, scan it on a timer, or expose a catalog / listing endpoint. Archive resolution is per-request and lazy; the running server holds no catalog.
- Verify `source_mp4_blake3` on the request path. The hash is offline metadata for `cmafly-index` and audit tooling; runtime mismatch detection uses `source_mp4_len` only.
- Tie any origin process resource bound to catalog size. Resource bounds tie exclusively to `max_open_archives`.

## Segment Strategy

- **Nominal duration:** 6.0 seconds (Apple recommendation for fMP4 HLS).
- **Cut policy:** advance the cut point forward to the next IDR if the nominal boundary lands inside a GOP. Real segments run from `nominal` up to roughly `nominal + GOP_duration`; for a typical streaming-friendly encode (GOP ≤ 1 s) that's 6.0–7.x s, but the spec does not bound input GOP size — pathological inputs (e.g., 4 s GOPs) will produce proportionally longer segments and a correspondingly larger `EXT-X-TARGETDURATION`. Never < nominal, never mid-GOP.
- **Playlist `EXT-X-TARGETDURATION`:** `ceil(max(actual segment durations))`.
- **`traf` box order:** `tfhd` (with `default-base-is-moof`) → `tfdt` (version 1, 64-bit `baseMediaDecodeTime` per track in track timescale) → `trun` (per-sample duration + size + composition offsets when CTTS is present). Both video and audio `traf` carry their own `tfdt`.
- **`trun.data_offset`:** patched after the `mdat` header is written, relative to `moof` start.
- **`trex` defaults:** video track — `default_sample_flags` = non-sync; audio track — `default_sample_flags` = sync. Each video segment's video `trun` sets `first-sample-flags-present` marking the first sample (the IDR) as sync. Audio `trun` omits both `first-sample-flags-present` and per-sample flags — every audio frame is independently decodable, the `trex` default already says so, and writing flags would only invite divergence.
- **Track timescale:** preserve from input per track (do not rescale).
- **`mvhd.duration` and per-track `tkhd.duration` in `init.mp4`:** written as `0`. CMAF init segments declare duration via fragments (`mvex`/`trex`), not the movie header.
- **`mvhd.timescale`:** fixed at `1000` (millisecond precision). CMAF players ignore the movie timescale for sample timing — per-track timescales in `mdhd` are authoritative — but it must be non-zero. `1000` is the de-facto industry default and avoids any temptation to derive it from one track and mismatch the other.
- **`mvhd.next_track_id`:** `3` (one past the highest assigned track id; tracks are `1` = video, `2` = audio per the muxed-CMAF contract).
- **`max_segment_size`:** during build, `IndexBuilder` computes the maximum across all segments of `(styp + moof + mdat) total bytes` and stores it in the `.idx` header (offset 4). `cmafly-serve` reads this once via `IndexView::max_segment_size()` to size each request's assembly buffer; no per-request scan over the segment table is needed.
- **Brand identifiers:**
  - `ftyp`: major = `cmfc`, minor = 0, compatible = [`iso6`, `cmfc`]
  - `styp`: major = `msdh`, minor = 0, compatible = [`msdh`, `cmfc`]
  - `msdh` (not `msix`) because v1 omits `sidx`; `msix` would assert `sidx` is present.

## Edit List Handling

Input track edit lists (`edts/elst`) are passed through verbatim to the corresponding `trak` in `init.mp4`. They are NOT folded into `tfdt`. CMAF §7.5.13 requires that track presentation-time offsets — including AAC priming silence (typically 2 112 samples ≈ 44 ms of leading metadata) and B-frame composition compensation — be expressed via `edts/elst` in the init segment, not via movie fragments. Folding into `tfdt` would corrupt audio priming.

If the input track has no `elst`, none is written. Inputs whose AAC track lacks an `elst` will exhibit the original priming as audible artifacts at the start; v1 documents this rather than synthesizes one.

## Demux Scope

Demux parses ISO/IEC 14496-12 atoms only enough to:

1. Verify exactly one video track (`avc1` / `hvc1` / `hev1`) and one audio track (`mp4a`); reject otherwise.
2. Surface per-track timescale, duration, dimensions, sample rate / channel count from `tkhd` / `mdhd` / `hdlr`.
3. Snapshot the entire sample-entry box from `stsd` — the complete `avc1` / `hvc1` / `mp4a` box including its `avcC` / `hvcC` / `esds` and any optional siblings (`btrt`, `pasp`, `colr`) — as raw bytes.
4. Snapshot `edts/elst` raw bytes per track when present.
5. Walk `stts` / `ctts` / `stss` / `stsc` / `stsz` / (`stco` | `co64`) to produce a per-sample table: `(offset, size, dts, cts_offset, is_sync)`.
6. Tolerate both fast-start (`moov` first) and trailing-`moov` layouts; box order is not assumed.

**Out of scope:**
- Parsing NALU structure, SPS/PPS, AAC `AudioSpecificConfig` bitstream, SEI.
- Writer features (creating or modifying input MP4s).
- Metadata atoms (`udta`, `ilst`, `meta`) — not copied to output.
- DASH-only boxes (`sidx`, `ssix`, `prft`) on input — silently ignored.
- Encryption boxes — input rejected when `senc` / `tenc` / `sinf` is present, or when an `sbgp` / `sgpd` carries the CENC `seig` grouping_type. `sbgp` / `sgpd` with non-encryption grouping types (`rap `, `roll`, `sync`, …) are tolerated and ignored.
- Fragmented input MP4 (top-level `moof`) — rejected.

**Validation gates on `IndexBuilder::build`:**
- `ftyp` major or any compatible brand must be one of `isom`, `mp42`, `iso2`, `iso4`, `iso5`, `iso6`, `cmfc`, `mp41`.
- Track count: exactly 2.
- Video sample-entry fourcc ∈ {`avc1`, `hvc1`, `hev1`}; audio ∈ {`mp4a`}.
- Required atoms present per track: `tkhd`, `mdhd`, `hdlr`, `stbl/{stsd,stts,stsc,stsz,stco|co64}`. Missing any → `PackagerError::MissingAtom`.
- `stss` absent ⇒ all video samples treated as sync (legal but uncommon — proceed silently; `cmafly-index` reads the built index back and prints a `note: all video samples are sync (input likely lacked stss)` line to stderr in this case).
- `ctts` absent ⇒ `cts_offset = 0` for all samples.

## HLS Playlist Shape

The literal text shape `cmafly` emits. Numeric values below are illustrative for a typical 6.0-nominal / sub-1 s GOP encode; the actual `EXT-X-TARGETDURATION` and `EXTINF` values are computed at build time from the segment table.

```
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-TARGETDURATION:7
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-MAP:URI="init.mp4"
#EXTINF:6.006000,
seg_0001.m4s
#EXTINF:6.006000,
seg_0002.m4s
...
#EXT-X-ENDLIST
```

Version 7 is required for fMP4 segments. `#EXT-X-INDEPENDENT-SEGMENTS` is correct because each segment starts at an IDR (segment splitter guarantee) and AAC frames are independently decodable; any segment can be played without prior segments.

EXTINF durations are formatted with 6 decimal places (`format!("{:.6}", duration_secs)`), computed per segment as `(sum of that segment's video-track sample durations, in video timescale) / video_timescale`, to keep cumulative drift below 1 ms over a 2-hour playlist (3-decimal precision can drift up to ~0.6 s at 1 200 segments). The video track is the timeline master because IDR / segment boundaries are video-driven; audio frame boundaries do not align exactly and are not used. `EXT-X-TARGETDURATION` is computed from the same precise rational across all segments — `ceil(max(EXTINF))` — so the two values are coherent regardless of float formatting.

URLs in `playlist.m3u8` are relative paths (`init.mp4`, `seg_0001.m4s`, …). The `cmafly-serve` HTTP route shape preserves this: `GET /v/{id}/playlist.m3u8` returns a body whose `init.mp4` and `seg_NNNN.m4s` references resolve under the same `/v/{id}/` prefix.

## Index Format

The `.idx` file is a self-contained, mmap-friendly binary format. Reading is zero-copy: `IndexView::open(bytes: &[u8])` validates the header and returns a struct that exposes sample tables, segment table, init-segment bytes, and codec config blobs as borrowed `&` slices.

### File layout

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | magic = `b"HCMI"` (HLS-CMAF Index) |
| 4 | 4 | `max_segment_size: u32` — largest media-segment payload across all segments in this archive (`styp`+`moof`+`mdat` framing included, in bytes); used by `cmafly-serve` to right-size the per-request assembly buffer |
| 8 | 8 | `source_mp4_len` — byte size of source MP4 (sanity check against detached / replaced source) |
| 16 | 32 | `source_mp4_blake3` — BLAKE3-256 (default mode, no key, no derive-key context) over the source MP4 |
| 48 | 4 | `section_count: u32` |
| 52 | 4 | reserved (must be 0; pads to 8-byte alignment) |
| 56 | `section_count × 16` | section directory: `[(kind: u32, _pad: u32, offset: u64)]`, sizes implied by neighbouring offsets and EOF |
| … | … | section payloads (each section is 8-byte aligned) |

Where `header_end = 56 + section_count × 16` — the byte offset at which section payloads begin; referenced by the invariants below.

**Ordering invariants:**
- Section payloads SHALL appear on disk in offset-ascending order matching the directory's order. The directory is the single source of truth; payload order tracks it.
- Each intermediate section's length is `directory[i+1].offset − directory[i].offset`; the final section's length is `file_len − directory[last].offset`.
- `IndexView::open` rejects: directory offsets that are not strictly ascending, the first directory offset less than `header_end`, or the final offset greater than the file length.
- Section kinds are unique per directory (no kind appears twice). `IndexView::open` rejects duplicates.
- `section_count` ≤ 64. `IndexView::open` rejects larger values as malformed input — the format defines 7 kinds and is not expected to grow past a small constant. The bound also caps directory-walk cost during `open`.
- **No per-section size ceiling.** Each section's length is already bounded by the structural directory checks above (next-offset minus this-offset; final = `file_len − last_offset`) — i.e., by the underlying file size. For very long high-framerate content individual sections may legitimately exceed 100 MB (e.g., 24 h / 60 fps video → ~125 MB `VideoSampleTable`); imposing a fixed per-section ceiling would either be redundant with the structural check or risk falsely rejecting legitimate input.

The 4-byte magic exists as a defensive identity check (refuse to mmap non-`.idx` files); it is not a versioning or compatibility mechanism.

### Section kinds

| Kind | Section | Payload |
|------|---------|---------|
| `0x01` | VideoTrackMeta | `timescale: u32`, `fourcc: [u8;4]`, `width: u32`, `height: u32`, `sample_entry_size: u32`, `sample_entry_bytes`, `elst_size: u32`, `elst_bytes` |
| `0x02` | AudioTrackMeta | `timescale: u32`, `fourcc: [u8;4]`, `sample_rate: u32`, `channel_count: u8`, padding to 8B, `sample_entry_size: u32`, `sample_entry_bytes`, `elst_size: u32`, `elst_bytes` |
| `0x03` | VideoSampleTable | `n_samples: u32`, padding to 8B, then `[SampleEntry; n_samples]` |
| `0x04` | AudioSampleTable | same layout as VideoSampleTable; `cts_offset` is always 0 for audio |
| `0x05` | SegmentTable | `n_segments: u32`, padding to 8B, then `[SegmentEntry; n_segments]` |
| `0x06` | InitSegmentBytes | full pre-rendered `init.mp4` bytes |
| `0x07` | PlaylistBytes | full pre-rendered `playlist.m3u8` bytes (optional; absent → server computes via `playlist::write_media_playlist`) |

### On-disk entry types

```rust
#[repr(C)]
pub struct SampleEntry {
    pub offset:     u64,    // byte offset into the source MP4
    pub size:       u32,    // sample size in bytes
    pub dts_delta:  u32,    // delta from previous sample's DTS in track timescale
    pub cts_offset: i32,    // composition offset (audio: always 0)
    pub flags:      u32,    // bit 0 = is_sync, others reserved
}
// 24 bytes per sample (align_of == 8).

#[repr(C)]
pub struct SegmentEntry {
    pub video_sample_start: u32,
    pub video_sample_count: u32,
    pub audio_sample_start: u32,
    pub audio_sample_count: u32,
    pub video_base_dts:     u64,    // cumulative DTS in video timescale at segment start
    pub audio_base_dts:     u64,    // cumulative DTS in audio timescale at segment start
}
// 32 bytes per segment (align_of == 8).
```

`dts_delta` (rather than absolute DTS) is stored so `SampleEntry` fits in 24 bytes; the writer reconstructs absolute DTS by `prefix_sum(dts_delta) + segment.{v,a}_base_dts` during `trun` emission.

Both structs are plain `#[repr(C)]`, **not** `#[repr(C, packed)]`. Their fields are deliberately ordered so the natural C layout has zero compiler padding and equals the on-disk byte sequence:
- `SampleEntry`: leading `u64` puts the wide field at offset 0; the four 4-byte fields that follow are each at a 4-byte-aligned offset. `size_of == 24`, `align_of == 8`.
- `SegmentEntry`: four `u32` fields (16 bytes) followed by two `u64` fields starting at offset 16 (8-byte aligned). `size_of == 32`, `align_of == 8`.

Choosing `#[repr(C)]` over `packed` avoids the `unaligned_references` UB that `packed` invites whenever a field is touched through a `&` reference, and lets `IndexView` hand out ordinary `&[SampleEntry]` / `&[SegmentEntry]` slices. The slices are created via `unsafe { std::slice::from_raw_parts }` — the only sanctioned `unsafe` block in the library, gated by:

```rust
// SAFETY:
// - section payloads are placed at 8-byte-aligned file offsets (header layout
//   guarantees this; IndexView::open re-checks each directory entry against
//   align_of::<SampleEntry>() == 8).
// - bytes.len() == n * size_of::<SampleEntry>(), validated against the section
//   directory in IndexView::open; otherwise open() returns Err.
// - SampleEntry is #[repr(C)] with deliberate field ordering that produces
//   zero compiler padding, so the on-disk byte sequence matches the in-memory
//   layout exactly. Fields are POD (u64/u32/i32) with no Drop, no references,
//   no interior mutability.
// - The returned slice's lifetime is tied to `&self.bytes`, so it cannot
//   outlive the underlying mmap.
```

### Size estimate (2 h video, 30 fps, 48 kHz AAC)

- Header + section directory: ~250 B
- VideoTrackMeta: ~80–500 B (depends on `avcC` / `hvcC` size)
- AudioTrackMeta: ~50 B
- VideoSampleTable: 216 000 × 24 B ≈ 5.2 MB
- AudioSampleTable: 337 500 × 24 B ≈ 8.1 MB
- SegmentTable: 1 200 × 32 B ≈ 38 KB
- InitSegmentBytes: ~1 KB
- PlaylistBytes: ~50 KB
- **Total: ~13 MB per 2 h video.**

For the reference 5 000-archive workload this totals ~65 GB. Catalog size is unbounded; `.idx` files live on disk like the originals and are mmapped into the process only when their archive enters the LRU hot set (see _Server Architecture_ and _Scale Limits_).

## Server Architecture

`cmafly-serve` is a single-process, multi-threaded `tokio` runtime hosting an `axum` HTTP server. It is logically stateless: every handler treats the in-process `IndexRegistry` as immutable shared state.

### HTTP routes

| Route | Behavior |
|-------|----------|
| `GET /v/:id/playlist.m3u8` | If `.idx` has a `PlaylistBytes` section, return it; else compute via `playlist::write_media_playlist`. `Cache-Control: public, max-age=300` (long TTL safe because VOD playlists are immutable, but kept short to allow operator regeneration). |
| `GET /v/:id/init.mp4` | Return `IndexView::init_segment_bytes`. `Cache-Control: public, max-age=31536000, immutable`. |
| `GET /v/:id/seg_:idx.m4s` | Acquire a permit from a `tokio::sync::Semaphore` of size `max_inflight_segments` (admission control — see below). Look up `SegmentEntry`. Allocate `Vec<u8>::with_capacity(index.max_segment_size())` (typically 1–20 MB depending on archive bitrate; read from the `.idx` header, not hardcoded). Call `fmp4::write_media_segment`, passing the mmap'd source MP4 as `ReadAt`. Return body; permit released when the response future drops. If a request waits longer than `permit_wait_timeout` (default 5 s) for a permit, respond `503 Service Unavailable` with `Retry-After: 1`. `Cache-Control: public, max-age=31536000, immutable`. |
| `GET /healthz` | Liveness probe; returns `200 OK` with body `ok`. |

`Range:` requests are answered with full content (`200 OK`); HLS players do not need byte-range for fMP4 segments, and supporting partial ranges complicates header / cache semantics with no benefit at our scale.

### Index registry

```rust
pub struct IndexRegistry {
    cache: std::sync::Mutex<lru::LruCache<String, Arc<Entry>>>,
    media_dir: PathBuf,
    index_dir: PathBuf,
    max_open_archives: usize,
}

pub struct Entry {
    idx_mmap: memmap2::Mmap,
    mp4_mmap: memmap2::Mmap,
}
```

The registry is a **bounded LRU**, sized by `max_open_archives` (a per-host hot-set knob — independent of catalog size). The default is auto-derived from `/proc/sys/vm/max_map_count` at startup; `--max-open-archives N` overrides for tests / staging / non-Linux dev — see _Capacity knob auto-sizing_ below for the formula. When the cache is full, inserting a new entry drops the least-recently-used `Arc<Entry>`. In-flight requests that already cloned the `Arc` finish safely because the underlying `Mmap` lives until the last reference drops; a re-request of an evicted archive re-mmaps in a few µs of syscalls plus the page-fault cost of any cold pages it touches.

**Catalog size is unbounded by design.** The server performs no startup scan, maintains no in-memory catalog, and exposes no listing endpoint. Each request resolves `:id` lazily: stat `{index_dir}/{:id}.idx` (`ENOENT` → `404`), then on cache miss mmap `{media_dir}/{:id}.mp4` to back the assembly path (`ENOENT` → `500 Internal Server Error`, because the `.idx` is internally consistent only with its source MP4 — a present `.idx` with a missing `.mp4` is server-side inconsistency, not a client-addressable 404). The `:id` → file-name mapping is the literal identifier (`.idx` and `.mp4` extensions appended); no rewriting, no nesting. New archives are served the moment both files appear on disk — no per-archive registration, no operator action against the running process. Deleted archives stop being served as soon as their LRU entry is evicted (or immediately, if not currently cached).

Per request, `handle_segment` takes the cache mutex briefly to look up `:id`, bump LRU recency, and clone `Arc<Entry>` out, then releases the mutex before doing any segment assembly. The critical section is sub-µs (HashMap probe + linked-list splice). At ~170 req/s steady-state utilisation is < 0.1 %; the choice of `Mutex` over `RwLock` is deliberate because `lru::LruCache::get` mutates recency state and so cannot use a shared lock without giving up LRU correctness.

Per request, `handle_segment` then builds a fresh `IndexView::open(&entry.idx_mmap)` (~100 ns: validates magic + section directory). This avoids any self-referential struct; `Entry` is plain owned data, `IndexView` is borrowed per call.

Cache miss / eviction triggers an `mmap` syscall pair (idx + mp4). At CDN cache-hit ~95 % the residual origin traffic is heavily skewed toward a small working-set tail, so misses are rare in steady state. If miss frequency or mutex contention ever measures non-trivial, the registry can be swapped for `DashMap` + atomic-timestamp eviction without API change to handlers — handlers only see `IndexRegistry` and `Arc<Entry>`.

### Admission control for segment assembly

Segment assembly holds a `Vec<u8>` buffer sized to `IndexView::max_segment_size()` — typically 1 MB (low-bitrate audio-light) to ~20 MB (UHD high-bitrate) per archive. Without bounded concurrency, a traffic spike or a wave of slow clients holding response buffers would scale RAM linearly with traffic and could OOM the host. The earlier draft pinned a 4 MB hardcode, which was both wrong-sized for high-bitrate archives (truncation risk) and unbounded under load (no admission ceiling).

A `tokio::sync::Semaphore` of size `max_inflight_segments` gates segment-assembly tasks. The default is auto-derived from `/proc/meminfo` `MemTotal` at startup; `--max-inflight-segments N` overrides — see _Capacity knob auto-sizing_ below for the formula. The handler acquires a permit **before** allocating the buffer and releases it when the response future drops (axum stream completion, client disconnect, or write error all release deterministically via RAII). Requests that wait longer than `permit_wait_timeout` (default 5 s) for a permit respond `503 Service Unavailable` with `Retry-After: 1`; the CDN's retry policy and edge buffering absorb the burst rather than melting the origin.

Worst-case in-flight RAM is `max_inflight_segments × max(max_segment_size across hot archives)`. With auto-sized defaults on a 64 GB host (~102 permits × 32 MB worst-case) the ceiling is ~3.2 GB; on a 128 GB host (~204 × 32 MB) ~6.5 GB — both small relative to host RAM, and bounded **regardless of request rate or catalog size**. Auto-sizing scales the ceiling with the host; explicit overrides handle non-standard cases.

Buffer pooling (slab-recycling `Vec<u8>` instances) and streaming writes (writing directly to the response with a precomputed box layout, no buffer at all) are both deferred to v2 — see _Resolved Decisions_. At the design QPS the buffered model with right-sized allocation is sufficient and simple; both optimizations are reachable without API churn if profiling later justifies them.

### Capacity knob auto-sizing

Both `max_open_archives` and `max_inflight_segments` default to **auto-sized at startup** from host limits; explicit `--max-open-archives N` and `--max-inflight-segments N` flags override. The resolved values, the source (`auto` vs `flag`), and the host inputs that produced them are logged at startup so operators can audit what the process actually picked.

**`max_open_archives` (auto)**: bounded by mmap region budget. Each registry entry consumes 2 VMAs (`.idx` + `.mp4`). On Linux, `memmap2` releases the underlying FDs after `mmap()` returns — the mapping survives via the kernel's inode reference — so FD pressure is **not** the bound; only `vm.max_map_count` is.

```
reserve_vmas = 2048                          // stack, libs, allocator arenas, tokio internals
auto_max_open_archives = clamp(
    (read("/proc/sys/vm/max_map_count") - reserve_vmas) / 2,
    floor = 64,
    ceil  = 100_000,
)
```

On default Linux (`vm.max_map_count = 65 530`) → ~31 700. On suggested-tuned (`262 144`) → ~100 000 (ceiling). The 100 K ceiling is a sanity guardrail: hot sets larger than that are unusual and merit explicit configuration.

**`max_inflight_segments` (auto)**: bounded by RAM budget for segment buffers.

```
total_ram      = parse("/proc/meminfo")["MemTotal"]    // bytes
budget         = total_ram × 0.05                       // 5 % of RAM dedicated to segment assembly
worst_case_seg = 32 × 1024 × 1024                       // 32 MB covers UHD HDR / ~25 Mbps / 6 s segments
auto_max_inflight_segments = clamp(
    budget / worst_case_seg,
    floor = 64,                                         // minimum useful concurrency on small dev hosts
    ceil  = 4096,                                       // sanity ceiling
)
```

Examples: 8 GB dev host → 12 → clamped up to floor 64. 64 GB host → ~102. 128 GB host → ~204. The 5 % RAM budget is intentionally small — kernel page cache for hot MP4 / `.idx` pages is the dominant memory consumer and is governed by the kernel, not by us; segment-assembly buffers are a small additional ceiling on top.

**Non-Linux fallback (dev / test on macOS)**: `/proc` does not exist. Both knobs fall back to fixed conservative defaults (`max_open_archives = 1024`, `max_inflight_segments = 64`) and a startup warning is logged; production deployment is Linux-only by spec.

**No runtime resize.** Both knobs are resolved once at process start and held constant for the process lifetime. Dynamic adjustment based on observed traffic, hit rate, or memory pressure was rejected for v1: cache-resize eviction storms, oscillation around a memory-pressure threshold, and debug surprise outweigh the benefit at this scale. `lru::LruCache::resize()` keeps a future v2 implementation a localized change. Operators tune by reading registry LRU hit-rate and admission-503 metrics (deferred to v2 — see _Resolved Decisions_) and overriding the flag on next deploy.

### Concurrency model: single tokio process

Origin-side QPS at the design target is ~170 req/s (20 000 concurrent × 95 % CDN cache hit × 0.17 req/s). A single multi-threaded `tokio` runtime handles this with negligible scheduler contention; the work-stealing scheduler scales linearly to many cores.

Multi-process / `SO_REUSEPORT` is rejected for v1 because (a) it would duplicate per-process LRU / hot-set state, (b) it adds deployment complexity (per-worker config, log aggregation, metrics) for no measurable throughput gain at our scale, and (c) the failure-isolation argument is weak when `cmafly-serve` does no disk writes and Rust panic ≠ process death. The handler design is kept logically stateless precisely so this decision is reversible in a few lines of code if origin QPS ever grows two orders of magnitude.

### Path validation

`:id` is restricted to `^[a-zA-Z0-9_-]{1,64}$` (regex compiled once at startup). Any other character set returns `400 Bad Request`. This eliminates path-traversal attempts (`../`, encoded slashes, NUL bytes) without further per-request canonicalization.

`:idx` is parsed as `u32` and bounds-checked against `IndexView::segment_count()`; out-of-range returns `404 Not Found`.

### Failure modes

| Failure | Behavior |
|---------|----------|
| `.idx` file not found | `404 Not Found` |
| `.idx` magic mismatch | `500 Internal Server Error`; log; do not cache the failure (re-mmap on next request, in case operators replaced the file) |
| Source MP4 not found | `500 Internal Server Error` |
| Source MP4 truncated / replaced (mmap region invalidated → SIGBUS) | Process dies; supervisor (`systemd Restart=always`) brings it back. Operationally: never modify or delete an original while its archive is held in the LRU hot set. The blast radius is bounded by `max_open_archives`, not catalog size. |
| `source_mp4_len` mismatch on first load | `500 Internal Server Error`; require operator re-indexing |
| Source MP4 replaced with same-length but different content | **Not detected at runtime.** Server silently produces structurally-valid but content-garbage segments. `source_mp4_blake3` is recorded in the `.idx` for offline audit but never verified on the request path (multi-GB hashing per cache miss would defeat lazy-load latency — see _Resolved Decisions_). Mitigation is operational only: re-run `cmafly-index` whenever an original is replaced. |
| Buffer write error (OOM) | `500 Internal Server Error`; segment is idempotent so client retry is safe |

## Capacity Plan

Target: **20 000 concurrent viewers** (reference workload), single origin host, fronted by a commercial CDN. Origin process resource usage is bounded by `max_open_archives`, **not** by catalog size — see _Scale Limits_.

```
Player × 20 000
    │ HTTPS
    ▼
Commercial CDN (Cloudflare / Bunny / equivalent)
    │ ~95 % cache hit (segments are immutable + long max-age)
    │  ~5 % miss reaches origin
    ▼
cmafly-serve  (1 process, axum, tokio multi-thread, LRU-bounded mmap registry)
    │ mmap (lazy, on-demand; no startup scan)
    ▼
Storage (operational tiering — NVMe hot, HDD cold; transparent to server)
```

Origin-side numbers under design load (with auto-sized capacity knobs on a 64–128 GB Linux host: `max_open_archives` resolves to ~31 700 on default kernels and ~100 000 on the suggested-tuned host; `max_inflight_segments` resolves to ~100–200):

| Resource | Demand | Capacity (single origin) | Utilization |
|----------|--------|--------------------------|-------------|
| Egress bandwidth | ~4 Gbps | 10 GbE NIC | ~40 % |
| Request rate | ~170 req/s (20 000 × 5 % miss × 0.17 req/s) | tokio + axum > 2 000 req/s sustained | < 10 % |
| CPU | ~2 cores busy | 16+ cores | < 15 % |
| Storage read | ~500 MB/s | NVMe ~6 GB/s; HDD adequate for cold-miss tail | < 10 % |
| RAM (resident) | working set of hot pages — kernel page cache | 64–128 GB host | low |
| Open mmap regions | up to `2 × max_open_archives` (idx + mp4) | `vm.max_map_count` ≥ that + reserve | OK |
| Process FDs | peak concurrent sockets + small reserve (mmap doesn't retain FDs after `mmap()` returns) | `RLIMIT_NOFILE = 65 536` | OK |
| In-flight segment buffers | up to `max_inflight_segments × max_segment_size` (auto-sized; e.g. ~100 × 20 MB ≈ 2 GB on a 64 GB host, ~200 × 20 MB ≈ 4 GB on a 128 GB host) | host RAM (64–128 GB) | < 5 % |

Demand scales with viewer count and CDN miss rate, **not** catalog size. `max_open_archives` should comfortably exceed the realistic working-set cardinality; CDN cache concentration drives this far below catalog size in practice. The auto-sized default on a tuned Linux host (~100 000) is generous for the reference workload — explicit override is rare and applies mainly to non-Linux dev, deliberately tighter limits, or operator-observed LRU pressure.

Required host tunables (verified at startup against runtime configuration; refuse to start if too low):

| Tunable | Required | Rationale |
|---------|----------|-----------|
| `vm.max_map_count` | ≥ `2 × max_open_archives` + 2 048 reserve | one VMA per `.idx` and per `.mp4` mmap |
| `RLIMIT_NOFILE` (per-process) | ≥ peak concurrent sockets + reserve (e.g. 65 536) | mmap'd files do not hold FDs after `mmap()` returns; budget is socket-driven |
| `fs.file-max` (system) | ≥ `RLIMIT_NOFILE × processes` + reserve | system-wide FD ceiling |
| `net.core.somaxconn` | ≥ 65 535 | TCP accept queue (capacity-driven) |
| `net.ipv4.tcp_max_syn_backlog` | ≥ 65 535 | SYN backlog (capacity-driven) |

Suggested host kernel tunables for a reference Linux deployment:

```
fs.file-max                   2 097 152
fs.nr_open                    1 048 576
net.core.somaxconn            65 535
net.ipv4.tcp_max_syn_backlog  65 535
vm.max_map_count              262 144
RLIMIT_NOFILE (ulimit -n)     65 536
```

`max_open_archives` and `max_inflight_segments` are **not** operator-set numbers in the normal case — they are auto-derived at process start from `/proc/sys/vm/max_map_count` and `/proc/meminfo` respectively (see _Server Architecture > Capacity knob auto-sizing_ for the formulas). With the tunables above on a 64–128 GB host, auto-sizing resolves to roughly `max_open_archives ≈ 100 000` and `max_inflight_segments ≈ 100–200`. Override via `--max-open-archives N` / `--max-inflight-segments N` only when auto-derived values are wrong (tests, staging, non-Linux dev, or deliberately tighter limits); the resolved values and their source are logged at startup.

## Scale Limits

The design separates two distinct scales: **catalog** (`.idx` and original MP4 files on disk) and **hot set** (resources held by the running `cmafly-serve` process). The former grows without bound; the latter is capped by configuration.

| Quantity | Bound | Driven by |
|----------|-------|-----------|
| Catalog size (`.idx` + originals on disk) | unbounded | storage provisioning (operational concern, outside this spec) |
| Mmap regions held by process | `2 × max_open_archives` | LRU eviction in `IndexRegistry` |
| Open file descriptors (per-process) | peak concurrent sockets + small reserve | mmap regions do not retain FDs; FD budget is socket-driven |
| `IndexRegistry` cache entries | `max_open_archives` | LRU eviction |
| RSS (resident) | working set of hot pages — governed by kernel page cache | kernel |
| Concurrent in-flight segment buffers | `max_inflight_segments × max_segment_size` (auto-sized; ~100 × 20 MB ≈ 2 GB on 64 GB host, ~200 × 20 MB ≈ 4 GB on 128 GB host) | admission semaphore — independent of request rate, viewer count, and catalog size |
| TCP accept queue / sockets | capacity-driven, independent of catalog | host network tunables |

`max_open_archives` is the single configuration knob bounding origin process resource usage. It is **auto-sized at startup** from `vm.max_map_count` (see _Server Architecture > Capacity knob auto-sizing_); explicit override is rare. Sizing rule of thumb: the value should comfortably exceed the realistic working-set cardinality. CDN cache hit rate (~95 % in the design) concentrates origin traffic onto a small hot set, which in steady state is far smaller than the full catalog. Auto-sized values (~31 700 on default Linux, ~100 000 on the suggested-tuned host) are generous for the reference workload; raise the kernel ceiling and re-derive (or override via `--max-open-archives`) only if the registry's own LRU hit rate falls below ~99 %.

Adding new archives requires no server action — drop the `.idx` and source MP4 into their respective directories. Deleting archives requires no server action either, though entries currently held in the LRU may continue to serve until evicted (or until the process restarts).

## Success Criteria

1. `cmafly-index` produces a valid `.idx` for any compliant H.264 / H.265 + AAC MP4 in under 10 s for a 2 h / 4 Mbps input on the NVMe origin.
2. `cmafly-serve` returns a valid `init.mp4` and `seg_NNNN.m4s` for any indexed video; output plays in Safari (macOS 14+) and hls.js (latest) without errors, artifacts, or audio drift over a 2-hour input.
3. Seeking to any segment boundary in the playlist works without freeze or visual corruption.
4. Origin segment-assembly latency: p50 < 10 ms, p99 < 50 ms (excluding network and disk-cache cold reads).
5. Origin sustains ≥ 2 000 segment requests/s on a single instance in a no-CDN stress test (synthetic load applied directly to `cmafly-serve`). This is a headroom benchmark, not the operating-point SLO: the design QPS at the 95 %-hit-rate operating point is ~170 req/s. The order-of-magnitude headroom is intentional, to absorb CDN edge cache flushes, regional cache misses, and short traffic spikes without saturating origin.
6. Process-owned RSS — excluding kernel page cache (governed by the kernel, not by us) and in-flight segment-assembly buffers (bounded by the admission semaphore, accounted in _Scale Limits_) — scales sub-linearly with index count and stays below ~1 GB regardless of indexed-archive count. Total observed RSS will be much higher in practice because the kernel keeps hot MP4 / `.idx` pages mapped; that page cache is a feature of the design, not unbounded growth in our process.
7. All written boxes pass internal size-vs-declared assertion.

## Resolved Decisions

- **Service model — on-demand, not pre-generated.** The earlier batch model (write all `seg_NNNN.m4s` to disk) was rejected after revealing the real workload (~5 000 archives growing to indefinite size, served behind a CDN to ~20 000 concurrent viewers). Pre-generation would consume ~3.7 GB × N archives of additional disk for content that may never be played, and double effective storage. Building a tiny `.idx` (~13 MB / 2 h) and assembling segments on demand from `(.idx + original .mp4)` costs ~10 ms CPU per cache-miss segment, easily absorbed by the available hardware (10 GbE / NVMe / 16 cores) and CDN cache hit ratio.

- **Index format — custom binary, mmap zero-copy.** Rejected `bincode` (no random access, full `Vec<SampleEntry>` deserialize cost), `rkyv` (large macro / proc-macro dependency footprint, schema-tooling complexity), `flatbuffers` / Cap'n Proto (extra tooling). The schema is small (1 header + 7 section kinds + 2 fixed-layout `#[repr(C)]` structs whose field order yields zero compiler padding) and self-writes in ~250 LOC.

- **No `.idx` versioning.** Single format. No version field. The 4-byte magic is a defensive identity check, not a compatibility token. Format evolution is not a v1 concern.

- **MP4 access — `mmap` (memmap2), not `pread`.** Originals are immutable in the operational model; `mmap` lets the kernel manage page cache across all concurrent requests with zero syscall overhead per read. SIGBUS risk (an in-process mmap region invalidated by source deletion / truncation) is accepted and mitigated operationally: never modify or delete an original while its archive is held in the LRU hot set; rely on `systemd Restart=always` if the process dies. With LRU eviction, the SIGBUS surface is bounded by `max_open_archives`, not by catalog size — archives that have been evicted (or never loaded) are safe to manipulate.

- **Server concurrency — single tokio process, no `SO_REUSEPORT`.** Origin QPS at design target (~170 req/s) is well within single-process tokio capacity. Multi-process would duplicate per-worker state and add deployment complexity without throughput gain. Handlers are designed logically stateless on `Arc<Registry>` so the decision is reversible without code change to handlers.

- **Track layout — muxed CMAF.** `init.mp4` carries one `moov` containing two `trak` boxes (video first, audio second). Each segment carries one `moof` with two `traf` boxes in the same order, followed by a single `mdat` with both tracks' samples. Demuxed audio (separate audio rendition) is out of scope for v1 because it would require a master playlist. The video-first `trak` ordering is contractual within v1: `track_id = 1` is always video, `track_id = 2` is always audio. Downstream `traf` order, `tfhd.track_id` references, and the `IndexView` accessor pair (`video_*` / `audio_*`) all rely on this; a future change would ripple through both the writer and the `.idx` schema.

- **`sidx` segment index box — not written in v1.** HLS players ignore it when each segment is a separate file; `sidx` is a DASH / byte-range concern.

- **H.265 (`hvc1`) in v1 — yes.** Both `avc1` and `hvc1` are first-class. Init-segment writer emits the corresponding sample-entry box verbatim from the source.

- **TLS termination — at upstream proxy / CDN.** `cmafly-serve` runs plain HTTP. Removes TLS handshake overhead from the Rust process and eliminates cert / key handling in the application layer.

- **Demux strategy — hand-rolled minimal ISO/IEC 14496-12 parser.** Estimated ~700 LOC. Off-the-shelf alternatives rejected:
  - `mp4` (Alfg) — `stsd` dispatcher handles `hev1` only, not `hvc1`; exposes parsed structs forcing reserialization with codec-config drift risk.
  - `mp4parse` (Mozilla) — omits sample-offset resolution from `stco` / `stsc` / `stsz`.
  - `mp4-atom` — same parser-shaped API, same byte-blob mismatch.

- **Index registry — bounded LRU, not "never evicted".** An earlier draft never evicted entries, which silently bound the system to whatever fixed `N` was assumed (e.g. 5 000) and would break under unbounded catalog growth: `vm.max_map_count`, FDs, and HashMap growth all scaled with catalog rather than with hot set. The registry is now `Mutex<lru::LruCache<String, Arc<Entry>>>` sized by `max_open_archives` — a per-host hot-set knob independent of catalog size. Evicted archives re-mmap on next request (~µs of syscalls + page-fault cost). Handler shape is unchanged: take the mutex briefly, clone `Arc<Entry>` out, release. CDN cache concentration (~95 % hit) means the hot set is a tiny fraction of catalog, so cache miss / eviction churn is rare. `RwLock` was rejected because `lru::LruCache::get` mutates recency state; `Mutex` is correct and at ~170 req/s critical-section utilisation is < 0.1 %.

- **Discovery — lazy on-demand, no catalog.** `cmafly-serve` performs no startup scan, maintains no in-memory catalog, and exposes no listing endpoint. Each request resolves `:id` by stat-ing `{index_dir}/{:id}.idx`; `ENOENT` → 404. This is what allows the catalog to grow without bound: new archives are served the moment they appear on disk, with no per-archive registration step or operator action against the running process. A listing endpoint was rejected because (a) it has no role in the playback path (CDN doesn't enumerate origin), (b) it would force startup scan or background indexing that doesn't scale with catalog size, and (c) ops tooling can list the directory directly.

- **`source_mp4_blake3` — recorded by indexer, not verified at runtime.** Hashing a multi-GB MP4 on every cache-miss load would defeat lazy-load latency (≈ seconds for a 4 GB file). The hash is recorded in the `.idx` for offline audit / forensic identification (e.g. detecting silent re-encodes between `cmafly-index` runs). Runtime mismatch detection on `cmafly-serve` relies on `source_mp4_len` only; a same-length but different-content replacement of the source MP4 will silently produce broken segments. Operational rule: re-run `cmafly-index` whenever an original is replaced.

- **Storage tiering — out of spec.** NVMe / HDD placement of originals and `.idx` files is an operational concern, not a server design concern. The server mmaps whatever path resolves; cold-archive page faults may hit slower storage on first read. This is acceptable: cold = CDN cache miss, which is rare; first-byte latency on a cold archive is dominated by CDN and TCP RTT, not by the NVMe-vs-HDD difference at the kernel page-fault level for a sequential read. Earlier text that pinned "the entire active content set" to NVMe was a hidden ceiling on catalog size and has been removed.

- **Segment assembly buffer — right-sized + bounded concurrency, not pooled or streamed (v1).** Each segment-assembly task allocates a fresh `Vec<u8>` sized from `IndexView::max_segment_size()` (recorded by `cmafly-index` in the `.idx` header — see _Segment Strategy_), typically 1–20 MB depending on archive bitrate. Concurrency is bounded by `max_inflight_segments` via `tokio::sync::Semaphore`, so worst-case in-flight RAM = `max_inflight_segments × max_segment_size` regardless of request rate, viewer count, or catalog size. The earlier `4 MB` hardcoded buffer was wrong: too small for high-bitrate UHD archives (truncation), and unbounded under traffic spikes (OOM risk grows linearly with concurrency). Two further optimisations were considered and **deferred to v2**:
  - **Buffer pooling** (recycling `Vec<u8>` allocations through a slab) — unnecessary at design QPS, where allocator churn is < 2 GB/s and well within `glibc malloc`. Adopt only if profiling later shows allocator hotspot under sustained higher-scale load.
  - **Streaming writes** (no in-memory buffer; write directly to the response writer using a precomputed box layout) — would eliminate the buffer entirely but requires a two-phase library API (`compute_segment_layout` → `write_media_segment_streaming`) because the current `Write + Seek`-based size patching doesn't fit a non-seek `AsyncWrite`. Worth the complexity only if the buffered model becomes a measured bottleneck.

- **Capacity knobs auto-sized at startup; runtime resize deferred.** `max_open_archives` and `max_inflight_segments` default to values derived once at process start from `/proc/sys/vm/max_map_count` and `/proc/meminfo` respectively (see _Server Architecture > Capacity knob auto-sizing_ for formulas). Operators can override with explicit flags but the normal case has nothing to set. Earlier drafts hardcoded `10 000` / `256` — workable but arbitrary and forced operators to learn the relationship between system tunables and these knobs by reading the spec. Auto-derivation makes the dependency machine-checked and audit-logged.

  Runtime adaptive resize was rejected for v1 despite `lru::LruCache::resize()` making it technically cheap. The downsides outweigh the benefit at this scale: (a) cache-resize eviction storms when shrinking under memory pressure compound the very pressure that triggered the resize; (b) oscillation around a threshold is hard to dampen without hysteresis and harder to debug from logs; (c) operators lose the predictability that "the process picked X at boot, it's still X" gives them. The chosen middle ground is **static knobs + observability (deferred to v2)**: expose registry LRU hit-rate and admission-503 counters; operators read those and override the flag on next deploy. If the static-tune cycle ever proves too slow, runtime resize can be added without API change.

  **Observability (LRU hit rate, admission 503 rate, segment-assembly latency histograms)** is itself a follow-on and explicitly deferred to v2 — adding `tracing`/`metrics` deps, a `/metrics` endpoint, and Prometheus integration is non-trivial scope and falls under "Ask first" until then. Until v2, the only operator visibility is the resolved-capacity startup log line.

---

**Status:** Draft. Awaiting human sign-off before Phase 2 (Plan).
