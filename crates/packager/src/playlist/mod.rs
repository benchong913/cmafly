//! HLS media-playlist writer.
//!
//! [`write_media_playlist`] consumes a borrowed [`crate::IndexView`] and
//! emits the playlist text. The `.idx` builder embeds the rendered bytes as
//! the optional `KIND_PLAYLIST_BYTES` section so `cmafly-serve` can forward
//! them without re-running the writer.

pub(crate) mod m3u8;

pub use m3u8::write_media_playlist;
