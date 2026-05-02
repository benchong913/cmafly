//! Bounded LRU registry of `(.idx, .mp4)` mmap pairs.
//!
//! - keyed by `:id` (regex shape `^[a-zA-Z0-9_-]{1,64}$` enforced inline),
//! - capacity = `max_open_archives` resolved by [`crate::config::Config`],
//! - critical section is sub-µs: lock ⇒ peek-then-clone ⇒ release; mmap
//!   I/O happens with the mutex *not* held so a slow open never blocks
//!   the hot path,
//! - `IndexView::open` is run once on first load purely to verify
//!   `source_mp4_len` matches the `.mp4` mmap; per-request handlers
//!   re-open against the cached idx mmap themselves.
//!
//! Failure mapping:
//! - `.idx` not found ⇒ [`ApiError::NotFound`].
//! - any post-stat failure (mmap, IndexView::open, len mismatch) is
//!   server-side inconsistency and surfaces as [`ApiError::Internal`].

use std::fs::File;
use std::io::ErrorKind;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cmafly::{IndexView, PackagerError};
use lru::LruCache;
use memmap2::Mmap;

use crate::error::ApiError;

const MAX_ID_LEN: usize = 64;

pub struct IndexRegistry {
    cache: Mutex<LruCache<String, Arc<Entry>>>,
    media_dir: PathBuf,
    index_dir: PathBuf,
}

#[derive(Debug)]
pub struct Entry {
    idx_mmap: Mmap,
    mp4_mmap: Mmap,
}

impl Entry {
    /// Borrow the validated `.idx` byte image. Use
    /// [`cmafly::IndexView::open`] to access fields.
    pub fn idx_bytes(&self) -> &[u8] {
        &self.idx_mmap
    }

    /// Borrow the source `.mp4` mmap; passable to
    /// [`cmafly::fmp4::write_media_segment`] as the `ReadAt`
    /// source.
    pub fn mp4_mmap(&self) -> &Mmap {
        &self.mp4_mmap
    }

    /// Convenience: re-open the cached `.idx` bytes through `IndexView`.
    /// The registry already validated this once at load time, so the
    /// only reason this can fail is concurrent file replacement
    /// (SIGBUS / corruption); surfaced to the caller as `Internal`.
    pub fn open_index(&self) -> Result<IndexView<'_>, PackagerError> {
        IndexView::open(self.idx_bytes())
    }
}

impl IndexRegistry {
    pub fn new(media_dir: PathBuf, index_dir: PathBuf, max_open_archives: usize) -> Self {
        // `LruCache::new` requires NonZeroUsize. Config resolution clamps
        // `max_open_archives` to ≥ floor(64), so a zero is unreachable
        // through the supported path; defend against direct callers
        // (and tests that exercise that defence) by rounding 0 up.
        let capacity = NonZeroUsize::new(max_open_archives).unwrap_or(NonZeroUsize::MIN);
        Self {
            cache: Mutex::new(LruCache::new(capacity)),
            media_dir,
            index_dir,
        }
    }

    /// Resolve `id` → cached or freshly-mmap'd `Entry`.
    ///
    /// Critical section (lock held): hashmap lookup + LRU recency bump +
    /// `Arc::clone`. The miss path (mmap, IndexView::open, len check) is
    /// run with the mutex released so slow disk I/O does not block other
    /// requests. A second insert race is resolved via re-check on
    /// re-acquire: the first entry inserted wins; the loser drops its
    /// fresh `Arc<Entry>`.
    pub fn get(&self, id: &str) -> Result<Arc<Entry>, ApiError> {
        validate_id(id)?;

        if let Some(arc) = self.lookup(id) {
            return Ok(arc);
        }

        let entry = Arc::new(self.load(id)?);
        Ok(self.insert(id, entry))
    }

    fn lookup(&self, id: &str) -> Option<Arc<Entry>> {
        let mut cache = self.lock_cache();
        cache.get(id).map(Arc::clone)
    }

    fn insert(&self, id: &str, fresh: Arc<Entry>) -> Arc<Entry> {
        // `LruCache::push` returns the evicted `(K, V)` (vs `put` which
        // drops it in place). We hold onto the evicted entry until the
        // lock guard drops so that — if our `Arc<Entry>` is the final
        // reference — the `Mmap` unmap (a syscall) does not happen
        // inside the cache critical section.
        let evicted = {
            let mut cache = self.lock_cache();
            if let Some(existing) = cache.get(id) {
                return Arc::clone(existing);
            }
            cache.push(id.to_string(), Arc::clone(&fresh))
        };
        drop(evicted);
        fresh
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, LruCache<String, Arc<Entry>>> {
        // Mutex poisoning only happens if a previous holder panicked
        // mid-critical-section. Our critical sections only call
        // `LruCache::{get,put}` and clone an `Arc`; none can panic on
        // well-typed input. Recover the inner guard so a single rogue
        // thread cannot wedge the whole server.
        self.cache
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn load(&self, id: &str) -> Result<Entry, ApiError> {
        let idx_path = self.index_dir.join(format!("{id}.idx"));
        let mp4_path = self.media_dir.join(format!("{id}.mp4"));

        // Stat first so `.idx` ENOENT → 404 (client-addressable). Any
        // later failure (mmap, IndexView, len mismatch) → 500 (server
        // inconsistency the client cannot fix by retrying).
        match std::fs::metadata(&idx_path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Err(ApiError::NotFound),
            Err(err) if err.kind() == ErrorKind::NotFound => return Err(ApiError::NotFound),
            Err(err) => return Err(ApiError::internal_io("stat idx", &idx_path, err)),
        }

        let idx_file =
            File::open(&idx_path).map_err(|e| ApiError::internal_io("open idx", &idx_path, e))?;
        // SAFETY: read-only mmap of a regular file we just opened.
        // Operationally, originals must not be mutated while the LRU
        // holds them; a concurrent unlink does not invalidate the
        // mapping on Unix.
        let idx_mmap = unsafe { Mmap::map(&idx_file) }
            .map_err(|e| ApiError::internal_io("mmap idx", &idx_path, e))?;

        let mp4_file = match File::open(&mp4_path) {
            Ok(f) => f,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Err(ApiError::Internal(format!(
                    "source mp4 missing for `{id}`: {}",
                    mp4_path.display(),
                )));
            }
            Err(err) => return Err(ApiError::internal_io("open mp4", &mp4_path, err)),
        };
        // SAFETY: same as above for the source MP4. In-place
        // replacement of the underlying file would risk SIGBUS on
        // subsequent access; this is treated as an operational hazard
        // rather than something the code mitigates.
        let mp4_mmap = unsafe { Mmap::map(&mp4_file) }
            .map_err(|e| ApiError::internal_io("mmap mp4", &mp4_path, e))?;

        let view = IndexView::open(&idx_mmap)
            .map_err(|err| ApiError::internal_packager("open idx", id, err))?;

        if view.source_mp4_len() != mp4_mmap.len() as u64 {
            return Err(ApiError::Internal(format!(
                "source_mp4_len mismatch for `{id}`: idx_records={} mp4_actual={}",
                view.source_mp4_len(),
                mp4_mmap.len(),
            )));
        }

        Ok(Entry { idx_mmap, mp4_mmap })
    }

    #[cfg(test)]
    fn cache_capacity(&self) -> usize {
        self.lock_cache().cap().get()
    }

    #[cfg(test)]
    fn cache_peek(&self, id: &str) -> bool {
        self.lock_cache().peek(id).is_some()
    }

    #[cfg(test)]
    fn force_insert(&self, id: &str, entry: Arc<Entry>) {
        self.lock_cache().put(id.to_string(), entry);
    }
}

fn validate_id(id: &str) -> Result<(), ApiError> {
    let len = id.len();
    if len == 0 {
        return Err(ApiError::BadRequest("empty id"));
    }
    if len > MAX_ID_LEN {
        return Err(ApiError::BadRequest("id too long"));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ApiError::BadRequest("invalid id characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "cmafly-serve-test-{label}-{}-{nanos}",
            std::process::id(),
        ));
        std::fs::create_dir(&dir).expect("create tempdir");
        dir
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        let mut f = File::create(&path).expect("create test file");
        f.write_all(bytes).expect("write test file");
        f.sync_all().expect("fsync test file");
        path
    }

    fn dummy_entry(scratch: &Path, label: &str) -> Arc<Entry> {
        // Mmap of a 0-byte file fails with EINVAL on Linux; use 1 byte
        // so the test mmaps succeed regardless of host.
        let idx_path = write_file(scratch, &format!("{label}.idx"), &[0u8]);
        let mp4_path = write_file(scratch, &format!("{label}.mp4"), &[0u8]);
        let idx_file = File::open(&idx_path).unwrap();
        let mp4_file = File::open(&mp4_path).unwrap();
        // SAFETY: test-only mmap of 1-byte regular files we own.
        let idx_mmap = unsafe { Mmap::map(&idx_file) }.unwrap();
        let mp4_mmap = unsafe { Mmap::map(&mp4_file) }.unwrap();
        Arc::new(Entry { idx_mmap, mp4_mmap })
    }

    #[test]
    fn validate_id_accepts_legal_shapes() {
        for id in ["a", "abc_DEF-123", "x", "0", &"z".repeat(MAX_ID_LEN)] {
            assert!(validate_id(id).is_ok(), "should accept `{id}`");
        }
    }

    #[test]
    fn validate_id_rejects_empty() {
        assert!(matches!(
            validate_id(""),
            Err(ApiError::BadRequest("empty id"))
        ));
    }

    #[test]
    fn validate_id_rejects_overlong() {
        let id = "a".repeat(MAX_ID_LEN + 1);
        assert!(matches!(
            validate_id(&id),
            Err(ApiError::BadRequest("id too long"))
        ));
    }

    #[test]
    fn validate_id_rejects_path_traversal() {
        for bad in ["../etc/passwd", "..", "./x", "a/b", "a\\b", "a b", "a.b"] {
            let err = validate_id(bad).unwrap_err();
            assert!(
                matches!(err, ApiError::BadRequest("invalid id characters")),
                "expected BadRequest for {bad:?}, got {err:?}",
            );
        }
    }

    #[test]
    fn validate_id_rejects_nul_and_control_bytes() {
        for bad in ["a\0b", "a\nb", "a\tb"] {
            assert!(matches!(
                validate_id(bad),
                Err(ApiError::BadRequest("invalid id characters"))
            ));
        }
    }

    #[test]
    fn get_returns_bad_request_for_traversal_attempt() {
        let dir = unique_dir("traversal");
        let registry = IndexRegistry::new(dir.clone(), dir.clone(), 4);
        let err = registry.get("../etc/passwd").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
    }

    #[test]
    fn get_returns_not_found_when_idx_missing() {
        let media = unique_dir("missing-idx-media");
        let index = unique_dir("missing-idx-index");
        let registry = IndexRegistry::new(media, index, 4);
        let err = registry.get("ghost").unwrap_err();
        assert!(matches!(err, ApiError::NotFound), "got {err:?}");
    }

    #[test]
    fn get_returns_internal_when_mp4_missing() {
        let media = unique_dir("missing-mp4-media");
        let index = unique_dir("missing-mp4-index");
        // .idx file present (any bytes — we want mmap to succeed before
        // the .mp4 lookup runs).
        write_file(&index, "abc.idx", &[0u8]);
        let registry = IndexRegistry::new(media, index, 4);
        let err = registry.get("abc").unwrap_err();
        match err {
            ApiError::Internal(msg) => {
                assert!(msg.contains("source mp4 missing"), "got: {msg}");
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn get_returns_not_found_when_idx_is_a_directory() {
        // A missing `.idx` is 404; a directory standing in for the
        // `.idx` is the same operational condition (the file is not
        // there) and is mapped to NotFound rather than 500 so a stray
        // dir does not page operators with a fake server-side error.
        let media = unique_dir("idx-dir-media");
        let index = unique_dir("idx-dir-index");
        std::fs::create_dir(index.join("z.idx")).unwrap();
        let registry = IndexRegistry::new(media, index, 4);
        let err = registry.get("z").unwrap_err();
        assert!(matches!(err, ApiError::NotFound), "got {err:?}");
    }

    #[test]
    fn lru_evicts_oldest_when_capacity_exceeded() {
        let scratch = unique_dir("lru-evict");
        let registry = IndexRegistry::new(scratch.clone(), scratch.clone(), 2);
        assert_eq!(registry.cache_capacity(), 2);

        registry.force_insert("alpha", dummy_entry(&scratch, "alpha"));
        registry.force_insert("beta", dummy_entry(&scratch, "beta"));
        assert!(registry.cache_peek("alpha"));
        assert!(registry.cache_peek("beta"));

        // Inserting a third key while capacity is 2 must evict the LRU
        // entry — `alpha` was inserted first and never touched since.
        registry.force_insert("gamma", dummy_entry(&scratch, "gamma"));
        assert!(!registry.cache_peek("alpha"), "alpha should be evicted");
        assert!(registry.cache_peek("beta"));
        assert!(registry.cache_peek("gamma"));
    }

    #[test]
    fn lookup_bumps_recency_so_idle_entry_is_evicted() {
        let scratch = unique_dir("lru-bump");
        let registry = IndexRegistry::new(scratch.clone(), scratch.clone(), 2);

        registry.force_insert("alpha", dummy_entry(&scratch, "alpha"));
        registry.force_insert("beta", dummy_entry(&scratch, "beta"));
        // Touch alpha → beta becomes the LRU candidate.
        let _ = registry.lookup("alpha").expect("alpha resolves");

        registry.force_insert("gamma", dummy_entry(&scratch, "gamma"));
        assert!(registry.cache_peek("alpha"), "alpha kept (touched)");
        assert!(!registry.cache_peek("beta"), "beta evicted (idle)");
        assert!(registry.cache_peek("gamma"));
    }

    #[test]
    fn cache_new_clamps_zero_capacity_to_one() {
        // Defence-in-depth: config clamps `max_open_archives` to ≥ 64,
        // but `IndexRegistry::new` is part of the public surface and
        // callers might pass 0. Instead of panicking in `LruCache::new`
        // we round up to 1.
        let dir = unique_dir("zero-cap");
        let registry = IndexRegistry::new(dir.clone(), dir, 0);
        assert_eq!(registry.cache_capacity(), 1);
    }
}
