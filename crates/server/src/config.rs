//! Server config + capacity-knob auto-sizing.
//!
//! - parse the CLI surface,
//! - auto-derive `max_open_archives` from `/proc/sys/vm/max_map_count`
//!   and `max_inflight_segments` from `/proc/meminfo` (Linux), or fall
//!   back to fixed defaults with a stderr warning (non-Linux dev),
//! - allow explicit overrides via flags,
//! - verify the host tunable budget (`vm.max_map_count` ≥
//!   `2 × max_open_archives + 2 048`) and refuse to start otherwise,
//! - emit one INFO-level audit line so operators can audit the
//!   resolved values + source + host inputs without strace.
//!
//! All policy lives in [`resolve_with_host`], a pure function over
//! [`ResolveArgs`] + [`HostInputs`]. The platform-specific glue
//! ([`gather_host_inputs`]) is a thin wrapper that reads `/proc` on
//! Linux and returns the fallback enum elsewhere; this split lets the
//! test suite cover both branches on any host.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

/// VMAs reserved for the rest of the process (libs, allocator arenas,
/// tokio internals, stack). Subtracted from `vm.max_map_count` before
/// the registry capacity is derived so a fully-saturated LRU still
/// leaves headroom for non-mmap mappings.
const VMA_RESERVE: usize = 2_048;
/// Worst-case segment size used when budgeting in-flight assembly
/// buffers. 32 MiB covers UHD HDR / ~25 Mbps / 6 s segments.
const WORST_CASE_SEGMENT_BYTES: u64 = 32 * 1024 * 1024;
/// Fraction of physical RAM dedicated to segment-assembly buffers.
const SEGMENT_RAM_FRACTION_NUM: u64 = 5;
const SEGMENT_RAM_FRACTION_DEN: u64 = 100;

const MAX_OPEN_ARCHIVES_FLOOR: usize = 64;
const MAX_OPEN_ARCHIVES_CEIL: usize = 100_000;
const MAX_INFLIGHT_FLOOR: usize = 64;
const MAX_INFLIGHT_CEIL: usize = 4_096;

/// Non-Linux fallback values, used on macOS / dev hosts where `/proc`
/// is unavailable. Production deployment is Linux-only.
const FALLBACK_MAX_OPEN_ARCHIVES: usize = 1_024;
const FALLBACK_MAX_INFLIGHT: usize = 64;

const DEFAULT_PERMIT_WAIT_SECS: f64 = 5.0;

/// Where each capacity knob's value came from. Surfaces in the audit
/// log so operators can tell auto-derived values from explicit flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// Auto-derived from `/proc` (Linux).
    Auto,
    /// Explicit `--max-…` flag on the CLI.
    Flag,
    /// Hardcoded non-Linux fallback (no `/proc` available).
    Fallback,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Auto => "auto",
            Source::Flag => "flag",
            Source::Fallback => "fallback",
        }
    }
}

/// Raw inputs to [`Config::resolve`]; mirrors the CLI but is
/// constructible by tests without going through clap.
#[derive(Clone, Debug)]
pub struct ResolveArgs {
    pub media_dir: PathBuf,
    pub index_dir: PathBuf,
    pub bind: String,
    pub max_open_archives: Option<usize>,
    pub max_inflight_segments: Option<usize>,
    pub permit_wait_timeout_secs: Option<f64>,
}

/// Host-derived inputs that drive auto-sizing. Splitting this out from
/// [`gather_host_inputs`] keeps [`resolve_with_host`] pure, so the
/// Linux branch is testable on macOS and vice versa.
#[derive(Clone, Copy, Debug)]
pub enum HostInputs {
    // Constructed in production only on Linux; constructed by unit
    // tests on every platform. Tests live behind `#[cfg(test)]`, which
    // the bin target's dead-code analysis ignores, so silence the
    // false positive on non-Linux builds.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Linux {
        max_map_count: usize,
        mem_total: u64,
    },
    // Symmetric to `Linux`: constructed in production only on
    // non-Linux, in tests on every platform. Silence the bin
    // target's dead-code false positive on Linux builds.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Fallback,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub media_dir: PathBuf,
    pub index_dir: PathBuf,
    pub bind: String,
    pub max_open_archives: usize,
    pub max_open_archives_source: Source,
    pub max_inflight_segments: usize,
    pub max_inflight_segments_source: Source,
    pub permit_wait_timeout: Duration,
    /// Observed `vm.max_map_count` (None on non-Linux fallback).
    pub host_max_map_count: Option<usize>,
    /// Observed `MemTotal` in bytes (None on non-Linux fallback).
    pub host_mem_total_bytes: Option<u64>,
}

impl Config {
    /// Resolve a [`Config`] using whatever host inputs the current
    /// platform exposes. On Linux that is `/proc`; elsewhere a stderr
    /// warning is emitted and fixed fallback values are used.
    pub fn resolve(args: ResolveArgs) -> Result<Self> {
        resolve_with_host(args, gather_host_inputs()?)
    }

    /// One-line startup audit record covering both knobs, their
    /// source, the permit-wait timeout, the resolved bind / dirs,
    /// and the host inputs used.
    pub fn audit_line(&self) -> String {
        let host = match (self.host_max_map_count, self.host_mem_total_bytes) {
            (Some(m), Some(t)) => format!("vm.max_map_count={m} MemTotal_bytes={t}"),
            _ => "vm.max_map_count=- MemTotal_bytes=-".to_string(),
        };
        format!(
            "audit: cmafly-serve config \
             bind={} media_dir={} index_dir={} \
             max_open_archives={} ({}) \
             max_inflight_segments={} ({}) \
             permit_wait_timeout={:.3}s \
             host: {}",
            self.bind,
            self.media_dir.display(),
            self.index_dir.display(),
            self.max_open_archives,
            self.max_open_archives_source.label(),
            self.max_inflight_segments,
            self.max_inflight_segments_source.label(),
            self.permit_wait_timeout.as_secs_f64(),
            host,
        )
    }
}

/// Pure resolver: combine CLI overrides with host-derived inputs,
/// then run the tunable-budget check. Public so unit tests can
/// exercise both the Linux and Fallback branches on any host.
pub fn resolve_with_host(args: ResolveArgs, host: HostInputs) -> Result<Config> {
    let (auto_open, auto_inflight, host_max_map_count, host_mem_total_bytes, auto_source) =
        match host {
            HostInputs::Linux {
                max_map_count,
                mem_total,
            } => (
                auto_max_open_archives(max_map_count),
                auto_max_inflight(mem_total),
                Some(max_map_count),
                Some(mem_total),
                Source::Auto,
            ),
            HostInputs::Fallback => (
                FALLBACK_MAX_OPEN_ARCHIVES,
                FALLBACK_MAX_INFLIGHT,
                None,
                None,
                Source::Fallback,
            ),
        };

    let (max_open_archives, max_open_archives_source) = match args.max_open_archives {
        Some(n) => (n, Source::Flag),
        None => (auto_open, auto_source),
    };
    let (max_inflight_segments, max_inflight_segments_source) = match args.max_inflight_segments {
        Some(n) => (n, Source::Flag),
        None => (auto_inflight, auto_source),
    };

    if let Some(host_max) = host_max_map_count {
        verify_tunable(host_max, max_open_archives)?;
    }
    let permit_wait_timeout = resolve_permit_wait_timeout(args.permit_wait_timeout_secs)?;

    Ok(Config {
        media_dir: args.media_dir,
        index_dir: args.index_dir,
        bind: args.bind,
        max_open_archives,
        max_open_archives_source,
        max_inflight_segments,
        max_inflight_segments_source,
        permit_wait_timeout,
        host_max_map_count,
        host_mem_total_bytes,
    })
}

// `Duration::from_secs_f64` panics on negative / NaN / infinite input;
// the CLI accepts an arbitrary `f64`, so route it through the fallible
// constructor and surface a clean error instead.
fn resolve_permit_wait_timeout(secs: Option<f64>) -> Result<Duration> {
    let secs = secs.unwrap_or(DEFAULT_PERMIT_WAIT_SECS);
    Duration::try_from_secs_f64(secs)
        .map_err(|err| anyhow!("invalid --permit-wait-timeout {secs:?}: {err}"))
}

fn gather_host_inputs() -> Result<HostInputs> {
    #[cfg(target_os = "linux")]
    {
        let max_map_count = read_proc_max_map_count()?;
        let mem_total = read_proc_mem_total()?;
        Ok(HostInputs::Linux {
            max_map_count,
            mem_total,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "warn: /proc not available; falling back to fixed defaults \
             (max_open_archives={FALLBACK_MAX_OPEN_ARCHIVES}, \
             max_inflight_segments={FALLBACK_MAX_INFLIGHT}). \
             Production deployment is Linux-only."
        );
        Ok(HostInputs::Fallback)
    }
}

#[cfg(target_os = "linux")]
fn read_proc_max_map_count() -> Result<usize> {
    let text = std::fs::read_to_string("/proc/sys/vm/max_map_count")
        .context("read /proc/sys/vm/max_map_count")?;
    parse_max_map_count(&text)
}

#[cfg(target_os = "linux")]
fn read_proc_mem_total() -> Result<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").context("read /proc/meminfo")?;
    parse_mem_total(&text)
}

// Used by [`read_proc_max_map_count`] (Linux only) and by unit tests
// on every platform; bin-target dead-code analysis on non-Linux does
// not see the test usage, so silence its false positive.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_max_map_count(text: &str) -> Result<usize> {
    text.trim()
        .parse::<usize>()
        .with_context(|| format!("parse vm.max_map_count: {text:?}"))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_mem_total(text: &str) -> Result<u64> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb_str = rest
                .trim()
                .strip_suffix("kB")
                .ok_or_else(|| anyhow!("MemTotal line missing 'kB' suffix: {line}"))?;
            let kb: u64 = kb_str
                .trim()
                .parse()
                .with_context(|| format!("parse MemTotal kB: {line}"))?;
            return kb
                .checked_mul(1024)
                .ok_or_else(|| anyhow!("MemTotal overflow: {kb} kB"));
        }
    }
    bail!("MemTotal not found in /proc/meminfo")
}

fn auto_max_open_archives(host_vma_max: usize) -> usize {
    let avail = host_vma_max.saturating_sub(VMA_RESERVE) / 2;
    avail.clamp(MAX_OPEN_ARCHIVES_FLOOR, MAX_OPEN_ARCHIVES_CEIL)
}

fn auto_max_inflight(mem_total: u64) -> usize {
    let budget = mem_total.saturating_mul(SEGMENT_RAM_FRACTION_NUM) / SEGMENT_RAM_FRACTION_DEN;
    let raw = usize::try_from(budget / WORST_CASE_SEGMENT_BYTES).unwrap_or(usize::MAX);
    raw.clamp(MAX_INFLIGHT_FLOOR, MAX_INFLIGHT_CEIL)
}

fn verify_tunable(host_max_map_count: usize, max_open_archives: usize) -> Result<()> {
    let required = max_open_archives
        .checked_mul(2)
        .and_then(|n| n.checked_add(VMA_RESERVE))
        .ok_or_else(|| anyhow!("max_open_archives overflow when computing tunable budget"))?;
    if host_max_map_count < required {
        bail!(
            "vm.max_map_count={host_max_map_count} too low; need >= {required} \
             (2 × max_open_archives [{max_open_archives}] + reserve [{VMA_RESERVE}]). \
             raise via `sudo sysctl -w vm.max_map_count={required}`",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> ResolveArgs {
        ResolveArgs {
            media_dir: PathBuf::from("/m"),
            index_dir: PathBuf::from("/i"),
            bind: "127.0.0.1:8080".into(),
            max_open_archives: None,
            max_inflight_segments: None,
            permit_wait_timeout_secs: None,
        }
    }

    #[test]
    fn parse_max_map_count_strips_trailing_newline() {
        assert_eq!(parse_max_map_count("65530\n").unwrap(), 65_530);
    }

    #[test]
    fn parse_max_map_count_rejects_garbage() {
        assert!(parse_max_map_count("not-a-number\n").is_err());
    }

    #[test]
    fn parse_mem_total_finds_line_and_converts_kb_to_bytes() {
        let meminfo = "MemTotal:       16331548 kB\nMemFree:        1234 kB\n";
        assert_eq!(parse_mem_total(meminfo).unwrap(), 16_331_548u64 * 1024);
    }

    #[test]
    fn parse_mem_total_rejects_missing_line() {
        assert!(parse_mem_total("MemFree: 100 kB\n").is_err());
    }

    #[test]
    fn parse_mem_total_rejects_missing_kb_suffix() {
        assert!(parse_mem_total("MemTotal:       16331548\n").is_err());
    }

    #[test]
    fn auto_max_open_archives_clamps_to_floor_for_tiny_hosts() {
        // (2_100 - 2_048) / 2 = 26 → floor 64.
        assert_eq!(auto_max_open_archives(2_100), MAX_OPEN_ARCHIVES_FLOOR);
    }

    #[test]
    fn auto_max_open_archives_default_kernel() {
        // (65_530 - 2_048) / 2 = 31_741.
        assert_eq!(auto_max_open_archives(65_530), 31_741);
    }

    #[test]
    fn auto_max_open_archives_tuned_kernel_clamps_to_ceiling() {
        // (262_144 - 2_048) / 2 = 130_048; clamp ceiling 100_000.
        assert_eq!(auto_max_open_archives(262_144), MAX_OPEN_ARCHIVES_CEIL);
    }

    #[test]
    fn auto_max_inflight_clamps_to_floor_for_dev_host() {
        // 8 GiB × 0.05 / 32 MiB ≈ 12 → floor 64.
        let mem = 8u64 * 1024 * 1024 * 1024;
        assert_eq!(auto_max_inflight(mem), MAX_INFLIGHT_FLOOR);
    }

    #[test]
    fn auto_max_inflight_for_64gib_host_yields_102() {
        // 64 GiB × 0.05 / 32 MiB = 102.4 → 102.
        let mem = 64u64 * 1024 * 1024 * 1024;
        assert_eq!(auto_max_inflight(mem), 102);
    }

    #[test]
    fn auto_max_inflight_for_128gib_host_yields_204() {
        // 128 GiB × 0.05 / 32 MiB = 204.8 → 204.
        let mem = 128u64 * 1024 * 1024 * 1024;
        assert_eq!(auto_max_inflight(mem), 204);
    }

    #[test]
    fn verify_tunable_passes_when_within_budget() {
        assert!(verify_tunable(65_530, 31_741).is_ok());
    }

    #[test]
    fn verify_tunable_fails_with_actionable_message() {
        let err = verify_tunable(1_000, 100_000).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("vm.max_map_count=1000"), "got: {msg}");
        assert!(msg.contains("max_open_archives"), "got: {msg}");
        assert!(msg.contains("sysctl"), "got: {msg}");
    }

    #[test]
    fn resolve_with_host_uses_overrides_when_present() {
        let cfg = resolve_with_host(
            ResolveArgs {
                max_open_archives: Some(50),
                max_inflight_segments: Some(7),
                permit_wait_timeout_secs: Some(2.5),
                ..base_args()
            },
            HostInputs::Linux {
                max_map_count: 100_000,
                mem_total: 64u64 << 30,
            },
        )
        .expect("resolve");
        assert_eq!(cfg.max_open_archives, 50);
        assert_eq!(cfg.max_open_archives_source, Source::Flag);
        assert_eq!(cfg.max_inflight_segments, 7);
        assert_eq!(cfg.max_inflight_segments_source, Source::Flag);
        assert_eq!(cfg.permit_wait_timeout, Duration::from_secs_f64(2.5));
        assert_eq!(cfg.host_max_map_count, Some(100_000));
        assert_eq!(cfg.host_mem_total_bytes, Some(64u64 << 30));
    }

    #[test]
    fn resolve_with_host_auto_sizes_on_linux_without_overrides() {
        let cfg = resolve_with_host(
            base_args(),
            HostInputs::Linux {
                max_map_count: 65_530,
                mem_total: 64u64 << 30,
            },
        )
        .expect("resolve");
        assert_eq!(cfg.max_open_archives, 31_741);
        assert_eq!(cfg.max_open_archives_source, Source::Auto);
        assert_eq!(cfg.max_inflight_segments, 102);
        assert_eq!(cfg.max_inflight_segments_source, Source::Auto);
    }

    #[test]
    fn resolve_with_host_falls_back_on_non_linux() {
        let cfg = resolve_with_host(base_args(), HostInputs::Fallback).expect("resolve");
        assert_eq!(cfg.max_open_archives, FALLBACK_MAX_OPEN_ARCHIVES);
        assert_eq!(cfg.max_open_archives_source, Source::Fallback);
        assert_eq!(cfg.max_inflight_segments, FALLBACK_MAX_INFLIGHT);
        assert_eq!(cfg.max_inflight_segments_source, Source::Fallback);
        assert_eq!(cfg.host_max_map_count, None);
        assert_eq!(cfg.host_mem_total_bytes, None);
    }

    #[test]
    fn resolve_with_host_runs_tunable_check_on_linux() {
        let err = resolve_with_host(
            ResolveArgs {
                max_open_archives: Some(100_000),
                ..base_args()
            },
            HostInputs::Linux {
                max_map_count: 1_000,
                mem_total: 64u64 << 30,
            },
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("vm.max_map_count"), "got: {msg}");
    }

    #[test]
    fn resolve_with_host_skips_tunable_check_on_fallback() {
        // On macOS/dev fallback we do not have a real vm.max_map_count
        // to compare against, so a generous override must still resolve.
        resolve_with_host(
            ResolveArgs {
                max_open_archives: Some(100_000),
                ..base_args()
            },
            HostInputs::Fallback,
        )
        .expect("resolve must succeed on fallback regardless of override");
    }

    #[test]
    fn resolve_with_host_default_permit_wait_timeout() {
        let cfg = resolve_with_host(base_args(), HostInputs::Fallback).expect("resolve");
        assert_eq!(
            cfg.permit_wait_timeout,
            Duration::from_secs_f64(DEFAULT_PERMIT_WAIT_SECS),
        );
    }

    #[test]
    fn resolve_with_host_rejects_invalid_permit_wait_timeout() {
        for secs in [-1.0, f64::NAN, f64::INFINITY] {
            let err = resolve_with_host(
                ResolveArgs {
                    permit_wait_timeout_secs: Some(secs),
                    ..base_args()
                },
                HostInputs::Fallback,
            )
            .unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("--permit-wait-timeout"), "got: {msg}");
        }
    }

    #[test]
    fn audit_line_includes_resolved_values_sources_and_host_inputs() {
        let cfg = resolve_with_host(
            base_args(),
            HostInputs::Linux {
                max_map_count: 65_530,
                mem_total: 64u64 << 30,
            },
        )
        .expect("resolve");
        let line = cfg.audit_line();
        assert!(line.contains("max_open_archives=31741"), "got: {line}");
        assert!(line.contains("(auto)"), "got: {line}");
        assert!(line.contains("max_inflight_segments=102"), "got: {line}");
        assert!(line.contains("vm.max_map_count=65530"), "got: {line}");
        assert!(
            line.contains(&format!("MemTotal_bytes={}", 64u64 << 30)),
            "got: {line}",
        );
        assert!(line.contains("permit_wait_timeout=5.000s"), "got: {line}");
    }

    #[test]
    fn audit_line_marks_fallback_source_on_non_linux() {
        let cfg = resolve_with_host(base_args(), HostInputs::Fallback).expect("resolve");
        let line = cfg.audit_line();
        assert!(line.contains("(fallback)"), "got: {line}");
        assert!(line.contains("vm.max_map_count=-"), "got: {line}");
        assert!(line.contains("MemTotal_bytes=-"), "got: {line}");
    }

    #[test]
    fn audit_line_marks_flag_source_when_overridden() {
        let cfg = resolve_with_host(
            ResolveArgs {
                max_open_archives: Some(50),
                max_inflight_segments: Some(7),
                ..base_args()
            },
            HostInputs::Linux {
                max_map_count: 100_000,
                mem_total: 64u64 << 30,
            },
        )
        .expect("resolve");
        let line = cfg.audit_line();
        assert!(line.contains("max_open_archives=50 (flag)"), "got: {line}");
        assert!(
            line.contains("max_inflight_segments=7 (flag)"),
            "got: {line}",
        );
    }
}
