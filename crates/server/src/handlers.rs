//! HTTP handlers for `cmafly-serve`.
//!
//! Wires the four routes against [`crate::registry::IndexRegistry`] and
//! a `tokio::sync::Semaphore` admission gate that bounds concurrent
//! segment assembly (and therefore worst-case in-flight RAM).
//!
//! Routes:
//! - `GET /healthz` → `200 OK` body `ok`.
//! - `GET /v/:id/playlist.m3u8` → embedded `KIND_PLAYLIST_BYTES` if present,
//!   else compute via [`cmafly::playlist::write_media_playlist`].
//! - `GET /v/:id/init.mp4` → [`cmafly::IndexView::init_segment_bytes`].
//! - `GET /v/:id/:filename` → segment handler when `:filename` matches
//!   `seg_NNNN.m4s` (4-or-more decimal digits, ≥ 0001); otherwise `404`.
//!
//! Why the segment route is `:filename` rather than `seg_:idx.m4s`: axum
//! 0.7's matchit router only supports full-segment captures, so the
//! `:filename` capture is split apart in [`parse_segment_idx`] for direct
//! unit-test coverage.

use std::convert::Infallible;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use cmafly::{PackagerError, fmp4, playlist};
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::ApiError;
use crate::registry::IndexRegistry;

const SEG_PREFIX: &str = "seg_";
const SEG_SUFFIX: &str = ".m4s";
/// Minimum digit width for `seg_NNNN.m4s`. Matches the playlist
/// writer's `{:04}` format so a `seg_1.m4s` URL cannot alias the same
/// immutable resource as `seg_0001.m4s`; ≥ 5 digits widen naturally for
/// >9 999 segments.
const SEG_MIN_DIGITS: usize = 4;

/// Cache headers shared by every response variant below.
const PLAYLIST_CACHE_CONTROL: &str = "public, max-age=300";
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";
const SEGMENT_CONTENT_TYPE: &str = "video/mp4";

/// Shared application state passed to every handler. `Clone`-able so axum
/// can clone cheaply per request (each field is already `Arc`-wrapped or
/// `Copy`).
#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<IndexRegistry>,
    pub inflight: Arc<Semaphore>,
    pub permit_wait_timeout: Duration,
}

/// Build the axum router. Static paths (`playlist.m3u8`, `init.mp4`) are
/// declared before the `:filename` catch-all so matchit's static-over-param
/// preference resolves overlapping shapes deterministically.
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handle_healthz))
        .route("/v/:id/playlist.m3u8", get(handle_playlist))
        .route("/v/:id/init.mp4", get(handle_init))
        .route("/v/:id/:filename", get(handle_segment_path))
        .with_state(state)
}

async fn handle_healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn handle_playlist(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let entry = state.registry.get(&id)?;
    let view = entry
        .open_index()
        .map_err(|e| ApiError::internal_packager("open idx for playlist", &id, e))?;

    let body: Vec<u8> = match view.playlist_bytes() {
        Some(b) => b.to_vec(),
        None => {
            let mut buf = Vec::new();
            playlist::write_media_playlist(&view, &mut buf)
                .map_err(|e| ApiError::internal_packager("write playlist", &id, e))?;
            buf
        }
    };

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, PLAYLIST_CONTENT_TYPE),
            (header::CACHE_CONTROL, PLAYLIST_CACHE_CONTROL),
        ],
        body,
    )
        .into_response())
}

async fn handle_init(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let entry = state.registry.get(&id)?;
    let view = entry
        .open_index()
        .map_err(|e| ApiError::internal_packager("open idx for init", &id, e))?;
    let body = view.init_segment_bytes().to_vec();

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, SEGMENT_CONTENT_TYPE),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL),
        ],
        body,
    )
        .into_response())
}

async fn handle_segment_path(
    State(state): State<AppState>,
    Path((id, filename)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    // Filename mismatch (e.g. a stray request to /v/:id/random.txt) is
    // mapped to 404 — there is no resource at this name, and a 400 here
    // would conflate "id is malformed" with "client took a wrong turn".
    let segment_idx = parse_segment_idx(&filename).ok_or(ApiError::NotFound)?;
    handle_segment(&state, &id, segment_idx).await
}

async fn handle_segment(
    state: &AppState,
    id: &str,
    segment_idx: u32,
) -> Result<Response, ApiError> {
    let entry = state.registry.get(id)?;
    // IndexView is a borrowed view over `entry.idx_bytes()`; `entry` is
    // an `Arc<Entry>` held in this fn's stack so the borrow's lifetime
    // is bounded by the fn body. `&[u8]` is `Send`, so holding the view
    // across the permit-wait `.await` is sound.
    let view = entry
        .open_index()
        .map_err(|e| ApiError::internal_packager("open idx for segment", id, e))?;

    if segment_idx >= view.segment_count() {
        return Err(ApiError::NotFound);
    }
    let max_segment_size = view.max_segment_size() as usize;

    let permit = acquire_permit(&state.inflight, state.permit_wait_timeout).await?;

    // Buffer allocation must happen AFTER permit acquisition so the
    // worst-case in-flight RAM is bounded by `permits × max_segment_size`.
    let mut buf: Vec<u8> = Vec::with_capacity(max_segment_size);
    let mut cursor = Cursor::new(&mut buf);
    fmp4::write_media_segment(&view, segment_idx, entry.mp4_mmap(), &mut cursor).map_err(|e| {
        match e {
            // Defensive: bounds were checked above, but keep the 404
            // mapping in case a future code path skips the early return.
            PackagerError::SegmentIndexOutOfRange { .. } => ApiError::NotFound,
            other => ApiError::internal_packager("assemble segment", id, other),
        }
    })?;

    // The permit must release on response stream completion (or client
    // disconnect / write error), not at handler return. Wrap the
    // assembled buffer in a body that owns the permit so hyper's
    // body-complete drop is what frees the admission slot.
    let body = Body::new(PermitBody::new(buf, permit));

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, SEGMENT_CONTENT_TYPE),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL),
        ],
        body,
    )
        .into_response())
}

/// Parse the path component `seg_NNNN.m4s` into the 0-based segment
/// index the index uses internally. Returns `None` for any deviation:
/// missing prefix / suffix, fewer than the canonical 4 digits, non-digit
/// body, leading `seg_0000` (segment numbering starts at `0001`), or
/// numeric overflow past `u32`. The 4-digit minimum matches the playlist
/// writer's `{:04}` format so a `seg_1.m4s` URL cannot collide with
/// `seg_0001.m4s` for the same immutable resource.
fn parse_segment_idx(filename: &str) -> Option<u32> {
    let body = filename
        .strip_prefix(SEG_PREFIX)?
        .strip_suffix(SEG_SUFFIX)?;
    if body.len() < SEG_MIN_DIGITS {
        return None;
    }
    if !body.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let one_based: u32 = body.parse().ok()?;
    one_based.checked_sub(1)
}

/// Chunked response body that owns its admission permit. Yields the
/// assembled segment in [`PERMIT_BODY_CHUNK_SIZE`] slices so hyper polls
/// the body multiple times across the wire flush; a one-frame body
/// would let hyper observe end-of-stream after a single poll and drop
/// the permit before the kernel send buffer drained, defeating the
/// in-flight-RAM ceiling under network backpressure. Dropping the body
/// (end-of-stream, client disconnect, or write error) drops the permit.
///
/// Each yielded chunk is a fresh `Bytes` allocation rather than a
/// zero-copy slice into the segment buffer. Slicing would let a single
/// tail chunk in hyper's send queue hold the entire segment allocation
/// alive, so the worst-case in-flight RAM ceiling
/// `max_inflight_segments × max_segment_size` would not be strict. The
/// per-chunk copy adds at most `max_segment_size` of incremental memcpy
/// across the request and caps the post-permit residual to a single
/// chunk.
struct PermitBody {
    /// Source allocation drained sequentially by `poll_frame`. Each
    /// chunk is `copy_from_slice`'d out so the `Vec` is freed as soon
    /// as the body is dropped — independent of any chunks still queued
    /// in hyper.
    source: Vec<u8>,
    pos: usize,
    _permit: OwnedSemaphorePermit,
}

/// Chunk size for [`PermitBody`] frames. 64 KiB matches the fmp4 writer's
/// per-segment copy buffer and is small enough that a slow client running
/// at any bitrate sees backpressure-induced re-polls of the body before
/// completion (so the permit lifetime tracks the wire drain rather than
/// the hyper-side accept).
const PERMIT_BODY_CHUNK_SIZE: usize = 64 * 1024;

impl PermitBody {
    fn new(buf: Vec<u8>, permit: OwnedSemaphorePermit) -> Self {
        Self {
            source: buf,
            pos: 0,
            _permit: permit,
        }
    }

    fn remaining(&self) -> usize {
        self.source.len().saturating_sub(self.pos)
    }
}

// `Vec<u8>`, `usize`, and `OwnedSemaphorePermit` are all `Unpin`; manual
// impl rather than `pin_project_lite` to keep the surface free of
// macro-expanded code. The body is exclusively owned (via `Body::new`),
// so a `Pin<&mut Self>` projection is sound to dereference.
impl Unpin for PermitBody {}

impl HttpBody for PermitBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.pos >= self.source.len() {
            return Poll::Ready(None);
        }
        let take = (self.source.len() - self.pos).min(PERMIT_BODY_CHUNK_SIZE);
        let chunk = Bytes::copy_from_slice(&self.source[self.pos..self.pos + take]);
        self.pos += take;
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }

    fn is_end_stream(&self) -> bool {
        self.pos >= self.source.len()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining() as u64)
    }
}

/// Acquire a permit from the in-flight semaphore, returning
/// [`ApiError::ServiceUnavailable`] if the wait exceeds `timeout`. The
/// permit is acquired before the segment assembly buffer is allocated;
/// the segment handler attaches the permit to the response body so the
/// RAII drop fires on stream completion rather than handler return.
async fn acquire_permit(
    sem: &Arc<Semaphore>,
    timeout: Duration,
) -> Result<OwnedSemaphorePermit, ApiError> {
    let acquire = Arc::clone(sem).acquire_owned();
    match tokio::time::timeout(timeout, acquire).await {
        Ok(Ok(permit)) => Ok(permit),
        // Semaphore::close is never called in production (no graceful
        // shutdown wired in v1), but a future shutdown pathway must not
        // surface a closed semaphore as 503; map to 500 so it stands out
        // in logs.
        Ok(Err(_)) => Err(ApiError::Internal("admission semaphore closed".into())),
        Err(_) => Err(ApiError::ServiceUnavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::body::to_bytes;

    fn unique_dir(label: &str) -> PathBuf {
        // Process-wide counter rather than wall-clock nanos: parallel
        // tests inside a single test binary can land on the same
        // nanosecond on macOS (microsecond-resolution clock), causing
        // intermittent `create_dir` collisions.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cmafly-serve-handler-test-{label}-{}-{n}",
            std::process::id(),
        ));
        std::fs::create_dir(&dir).expect("create tempdir");
        dir
    }

    fn fresh_state(timeout: Duration, permits: usize) -> AppState {
        let media = unique_dir("media");
        let index = unique_dir("index");
        AppState {
            registry: Arc::new(IndexRegistry::new(media, index, 4)),
            inflight: Arc::new(Semaphore::new(permits)),
            permit_wait_timeout: timeout,
        }
    }

    #[test]
    fn parse_segment_idx_accepts_canonical_filenames() {
        assert_eq!(parse_segment_idx("seg_0001.m4s"), Some(0));
        assert_eq!(parse_segment_idx("seg_0042.m4s"), Some(41));
        // 4-digit minimum; 5+ digits widen naturally without erroring.
        assert_eq!(parse_segment_idx("seg_99999.m4s"), Some(99_998));
    }

    #[test]
    fn parse_segment_idx_rejects_zero_index() {
        // Segment numbering starts at 0001; seg_0000 is not a valid
        // resource.
        assert!(parse_segment_idx("seg_0000.m4s").is_none());
    }

    #[test]
    fn parse_segment_idx_rejects_non_canonical_names() {
        for bad in [
            "seg_abcd.m4s",
            "seg_1a.m4s",
            "seg_0001.mp4",
            "init_0001.m4s",
            "seg_.m4s",
            "playlist.m3u8",
            "seg_-1.m4s",
            "seg_0001.M4S",
            // The playlist writer formats segment names as `{n:04}`;
            // sub-canonical widths would alias the same immutable
            // resource under two URLs and are rejected.
            "seg_1.m4s",
            "seg_42.m4s",
            "seg_999.m4s",
        ] {
            assert!(
                parse_segment_idx(bad).is_none(),
                "expected None for {bad:?}",
            );
        }
    }

    #[test]
    fn parse_segment_idx_rejects_overflow() {
        // 11 digits > u32::MAX (4_294_967_295); `parse::<u32>` errors.
        assert!(parse_segment_idx("seg_99999999999.m4s").is_none());
    }

    #[tokio::test]
    async fn handle_healthz_returns_200_ok_body() {
        let resp = handle_healthz().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 64).await.expect("body");
        assert_eq!(&bytes[..], b"ok");
    }

    #[tokio::test]
    async fn acquire_permit_returns_503_on_timeout() {
        let sem = Arc::new(Semaphore::new(0));
        let err = acquire_permit(&sem, Duration::from_millis(0))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::ServiceUnavailable));
    }

    #[tokio::test]
    async fn acquire_permit_returns_internal_when_closed() {
        let sem = Arc::new(Semaphore::new(1));
        sem.close();
        let err = acquire_permit(&sem, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[tokio::test]
    async fn acquire_permit_succeeds_when_available() {
        let sem = Arc::new(Semaphore::new(1));
        let permit = acquire_permit(&sem, Duration::from_secs(1))
            .await
            .expect("permit");
        // Releasing the permit lets the semaphore recover its capacity.
        drop(permit);
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn handle_segment_returns_400_for_traversal_id() {
        let state = fresh_state(Duration::from_millis(0), 4);
        let err = handle_segment(&state, "../etc/passwd", 0)
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn handle_segment_returns_404_for_unknown_id() {
        let state = fresh_state(Duration::from_millis(0), 4);
        let err = handle_segment(&state, "ghost", 0).await.unwrap_err();
        assert!(matches!(err, ApiError::NotFound));
    }

    #[tokio::test]
    async fn handle_segment_path_returns_404_for_unrecognised_filename() {
        let state = fresh_state(Duration::from_millis(0), 4);
        let err = handle_segment_path(
            State(state),
            Path(("anyid".to_string(), "garbage.txt".to_string())),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::NotFound));
    }

    #[tokio::test]
    async fn handle_playlist_returns_404_for_unknown_id() {
        let state = fresh_state(Duration::from_millis(0), 4);
        let err = handle_playlist(State(state), Path("ghost".to_string()))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::NotFound));
    }

    #[tokio::test]
    async fn handle_init_returns_400_for_bad_id() {
        let state = fresh_state(Duration::from_millis(0), 4);
        let err = handle_init(State(state), Path("a/b".to_string()))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn build_app_constructs_router_without_panic() {
        let state = fresh_state(Duration::from_secs(1), 1);
        // axum's Router doesn't expose its route table; constructing it
        // without panicking is the cheapest smoke we can run at unit
        // scope. End-to-end route coverage lives in integration tests.
        let _app = build_app(state);
    }

    #[tokio::test]
    async fn permit_body_holds_permit_until_dropped() {
        // Permit must release on body completion, not handler return.
        // Capacity = 1, so failing to release after dropping the body
        // would deadlock the next acquire.
        let sem = Arc::new(Semaphore::new(1));
        let permit = acquire_permit(&sem, Duration::from_millis(0))
            .await
            .expect("first permit");
        assert_eq!(sem.available_permits(), 0);

        let body = Body::new(PermitBody::new(b"segment-bytes".to_vec(), permit));
        // The permit is moved into the body — semaphore is still drained.
        assert_eq!(sem.available_permits(), 0);
        // Sanity: body still has the segment bytes.
        let bytes = to_bytes(body, 1024).await.expect("body");
        assert_eq!(&bytes[..], b"segment-bytes");
        // `to_bytes` consumes the body, dropping it; the permit drops with
        // it, returning the admission slot.
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn permit_body_yields_chunks_and_holds_permit_across_polls() {
        // Single-frame would let hyper observe end-of-stream after one
        // poll and drop the body before the kernel TCP queue drained.
        // The body must yield multiple frames for a payload larger than
        // `PERMIT_BODY_CHUNK_SIZE`. Hyper may stop polling once
        // `is_end_stream` returns true, so we do not assert a trailing
        // `Ready(None)` — only that the body reports end-of-stream after
        // the tail frame and that dropping the body releases the permit.
        let payload_len = PERMIT_BODY_CHUNK_SIZE * 2 + 1;
        let mut payload = vec![0u8; payload_len];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = i as u8;
        }

        let sem = Arc::new(Semaphore::new(1));
        let permit = acquire_permit(&sem, Duration::from_millis(0))
            .await
            .expect("first permit");
        let mut body = PermitBody::new(payload.clone(), permit);

        let mut cx_noop = Context::from_waker(std::task::Waker::noop());
        let mut reassembled = Vec::with_capacity(payload_len);
        let mut frames_yielded = 0usize;
        loop {
            match Pin::new(&mut body).poll_frame(&mut cx_noop) {
                Poll::Ready(Some(Ok(f))) => {
                    let data = f.into_data().expect("data frame");
                    if !body.is_end_stream() {
                        // Non-tail frames are full-size — backpressure
                        // re-polls observe a uniform stride.
                        assert_eq!(data.len(), PERMIT_BODY_CHUNK_SIZE);
                    }
                    reassembled.extend_from_slice(&data);
                    frames_yielded += 1;
                    // Permit is held while the body is alive, regardless
                    // of how many frames have already been yielded.
                    assert_eq!(sem.available_permits(), 0);
                    if body.is_end_stream() {
                        break;
                    }
                }
                other => panic!("unexpected poll outcome: {other:?}"),
            }
        }
        assert_eq!(frames_yielded, 3);
        assert_eq!(reassembled, payload);

        drop(body);
        // Body drop releases the admission permit.
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn permit_body_chunks_do_not_alias_source_allocation() {
        // RAM ceiling guard: a chunk Bytes still in hyper's send queue
        // after the body drops must not pin the entire segment buffer.
        // We verify by ensuring the yielded Bytes is byte-equal to the
        // source slice but lives independently — overwriting the chunk
        // would not affect a fresh PermitBody, and vice versa.
        let payload: Vec<u8> = (0..PERMIT_BODY_CHUNK_SIZE * 2).map(|i| i as u8).collect();
        let sem = Arc::new(Semaphore::new(1));
        let permit = acquire_permit(&sem, Duration::from_millis(0))
            .await
            .expect("permit");
        let mut body = PermitBody::new(payload.clone(), permit);

        let mut cx_noop = Context::from_waker(std::task::Waker::noop());
        let chunk = match Pin::new(&mut body).poll_frame(&mut cx_noop) {
            Poll::Ready(Some(Ok(f))) => f.into_data().expect("data"),
            other => panic!("expected first frame, got {other:?}"),
        };
        // Chunk content matches the head of the source.
        assert_eq!(&chunk[..], &payload[..PERMIT_BODY_CHUNK_SIZE]);
        // Drop the body. If the chunk had been a `Bytes::slice` of the
        // body's `Vec`, the underlying allocation would still be held;
        // here the chunk is its own copy, and the body's Vec is freed
        // along with the body. The chunk remains valid.
        drop(body);
        assert_eq!(
            &chunk[..PERMIT_BODY_CHUNK_SIZE],
            &payload[..PERMIT_BODY_CHUNK_SIZE]
        );
        assert_eq!(sem.available_permits(), 1);
    }
}
