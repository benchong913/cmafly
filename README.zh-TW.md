# cmafly

[![CI](https://github.com/benchong913/cmafly/actions/workflows/ci.yml/badge.svg)](https://github.com/benchong913/cmafly/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#授權)
[![Edition](https://img.shields.io/badge/edition-2024-orange.svg)](./Cargo.toml)
[![Toolchain](https://img.shields.io/badge/rust-stable-brightgreen.svg)](./rust-toolchain.toml)

面向 VOD MP4 媒體庫的隨選 HLS / CMAF 源站 — **磁碟上不落任何切片檔案**。

[English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md)

一套 Rust 工具鏈,直接從現有的 `.mp4` 媒體庫對外提供 CMAF(分片 MP4)HLS 服務。
離線索引器對每支 MP4 掃描一次,產出一份精簡的 `.idx` 檔案(2 小時影片約 13 MB),
其中保存了在請求時動態組裝切片所需的全部資訊。HTTP 源站以 `mmap` 方式持有這些
索引,每收到一次 `GET /v/{id}/seg_{NNNN}.m4s`,即從 `(.idx + .mp4)` 在記憶體中
重新拼出一個 `styp + moof + mdat` 切片並寫回網路。

切片不落盤、不轉碼、不重複佔用儲存空間。

```
離線(每份原片一次)                 請求時(每個切片)

.mp4 ─▶ cmafly-index ─▶ .idx ─┐
                              ├─▶ cmafly-serve ─▶ seg_NNNN.m4s
                        .mp4 ─┘   (mmap;記憶體中拼裝)
```

## 為何如此設計

預先產生 HLS 切片有兩項代價:儲存量翻倍(甚至更多),儲存的還是可能根本不會被
播放的內容;而且媒體庫被「寫死」 — 一旦替換或重編一支原片,意味著要刪除並重寫
數千個小檔案。cmafly 把原始 MP4 視為唯一真實來源,把切片視為
`(.idx, .mp4, segment_index)` 的純函數產物。一份 13 MB 的索引取代 2 小時影片
約 3.7 GB 的預生成 `.m4s`。新檔案只要兩個檔(原片 + `.idx`)同時就位即可被
服務;被刪除的檔案在其 LRU 條目被淘汰後立即停止對外服務。

## 倉庫結構

| Crate | 類型 | 角色 |
| --- | --- | --- |
| `cmafly`(`crates/packager`)| 函式庫 | 解封裝、切片、fMP4 / 播放清單寫入器、`.idx` 格式。純同步,不涉 I/O 與 async。 |
| `cmafly-index`(`crates/indexer`)| 執行檔 | 離線:掃描一個 MP4,輸出一個 `.idx`(原子寫入)。 |
| `cmafly-serve`(`crates/server`)| 執行檔 | 長駐 `tokio` + `axum` HTTP 源站,按請求從 `.idx + .mp4` 組裝切片。 |

刻意切分:只有 server crate 依賴 `tokio`、`axum`、`lru`;函式庫可在非 async
環境中複用,僅依賴 `thiserror`、`byteorder`、`memmap2`、`blake3`。

## v1 範圍

- 僅 VOD。
- 單一位元率、一條視訊軌 + 一條音訊軌,音視訊合流 CMAF。
- 視訊:H.264(`avc1`)或 HEVC(`hvc1` / `hev1`);音訊:AAC(`mp4a`)。
- 不支援轉碼、加密、LL-HLS、master playlist、字幕、多版本(alternate renditions)。

## 環境需求

- Rust **stable**(edition 2024)。專案透過 `rust-toolchain.toml` 鎖定
  `channel = "stable"`,`rustup` 會自動採用。
- Linux 或 macOS。原子寫入中的 `fsync(parent_dir)` 路徑為 Unix-only;其餘
  程式碼跨平台。

如尚未安裝 Rust:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## 快速開始

整體建置:

```sh
cargo build --release
```

為單支原片產出 `.idx`:

```sh
cargo run --release -p cmafly-index -- \
    --input  /path/to/originals/abc.mp4 \
    --output /path/to/index/abc.idx \
    --segment-duration 6.0
```

啟動源站:

```sh
cargo run --release -p cmafly-serve -- \
    --media-dir /path/to/originals \
    --index-dir /path/to/index \
    --bind 127.0.0.1:8080
```

容量參數(`--max-open-archives`、`--max-inflight-segments`、
`--permit-wait-timeout`)在啟動時依主機限制自動解析;源站會將解析後的數值、
來源(auto 或 flag)以及作為輸入的主機參數寫入啟動日誌。僅在自動值不適用時
才需覆寫。

請求介面 — `id` 即原片去掉副檔名後的檔名:

| 路由 | 用途 |
| --- | --- |
| `GET /v/{id}/playlist.m3u8` | media playlist |
| `GET /v/{id}/init.mp4` | CMAF init segment |
| `GET /v/{id}/seg_{NNNN}.m4s` | 單個 CMAF 媒體切片,按請求即時組裝 |
| `GET /healthz` | 存活探針 |

把支援 CMAF 的播放器(Safari、hls.js、ExoPlayer 等)指向
`http://127.0.0.1:8080/v/abc/playlist.m3u8` 即可。

TLS 由上游卸載 — `cmafly-serve` 走純 HTTP,設計上置於 CDN 或反向代理之後。

## 日常維運

源站從 `--media-dir` 與 `--index-dir` 隨需延遲載入內容 — 無需重啟、無啟動掃描、
無註冊步驟。

- **新增。** 把 `<id>.mp4` 與 `<id>.idx` 分別投入兩個目錄;首次請求前兩份
  檔案必須都到位。
- **下線。** 刪除其中任一或兩份檔案。新請求回傳 `404`;LRU 中已快取的條目
  可能會繼續短暫提供服務,直到被淘汰或行程重啟。
- **替換。** 重跑 `cmafly-index` 重建 `<id>.idx`(原子寫入)。**嚴禁在原
  `.mp4` 仍處於 LRU 熱點集合時就地覆寫或截斷它** — 源站以 `mmap` 持有該
  檔案,就地修改可能觸發 `SIGBUS` 導致行程崩潰。等長替換若未配套重建索引,
  執行期不會被偵測出來,會靜默產生結構合法但內容錯亂的切片。

## 開發

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI 在 stable 上跑同樣三項檢查,參見
[`.github/workflows/ci.yml`](./.github/workflows/ci.yml)。

部分整合測試會在 `tests/fixtures/sample.mp4`(或 `HLS_TEST_FIXTURE_MP4`
環境變數所指路徑)尋找 MP4 fixture;若找不到,測試會自行 skip 並向 stderr
輸出說明 — 單元測試套件本身不依賴任何二進位 blob。大於 1 MB 的 fixture
不可提交(見 `.gitignore`)。

## 目錄佈局

```
cmafly/
├── Cargo.toml              workspace 根目錄
├── rust-toolchain.toml     stable 通道鎖定
├── README.md               英文 README(預設)
├── README.zh-CN.md         簡體中文翻譯
├── README.zh-TW.md         本檔案
├── LICENSE-MIT
├── LICENSE-APACHE
├── crates/
│   ├── packager/           函式庫
│   ├── indexer/            cmafly-index 執行檔
│   └── server/             cmafly-serve 執行檔
└── tests/fixtures/         僅本機使用的 MP4 fixture(已 gitignore)
```

## 狀態

v1 — 首個可用版本。`cmafly` 函式庫公開 API 與 `.idx` 磁碟格式在版本之間尚未
穩定 — 升級時請重建索引。

## 授權

採用雙重授權,擇一即可:

- Apache License, Version 2.0([LICENSE-APACHE](./LICENSE-APACHE))
- MIT license([LICENSE-MIT](./LICENSE-MIT))

### 貢獻

除非另行明示,依 Apache-2.0 授權之定義,您有意提交以納入本作品的任何貢獻
將以上述雙重授權方式授出,且不附加任何其他條款或條件。
