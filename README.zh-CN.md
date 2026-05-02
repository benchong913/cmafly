# cmafly

[![CI](https://github.com/benchong913/cmafly/actions/workflows/ci.yml/badge.svg)](https://github.com/benchong913/cmafly/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#授权)
[![Edition](https://img.shields.io/badge/edition-2024-orange.svg)](./Cargo.toml)
[![Toolchain](https://img.shields.io/badge/rust-stable-brightgreen.svg)](./rust-toolchain.toml)

面向 VOD MP4 媒体库的按需 HLS / CMAF 源站 — **磁盘上不落任何切片文件**。

[English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md)

一套 Rust 工具链,直接从现有 `.mp4` 媒体库对外提供 CMAF(分片 MP4)HLS 服务。
离线索引器对每个 MP4 扫描一次,产出一份精简的 `.idx` 文件(2 小时视频约 13 MB),
其中保存了在请求时动态组装切片所需的全部信息。HTTP 源站以 `mmap` 方式持有这些
索引,每收到一次 `GET /v/{id}/seg_{NNNN}.m4s`,即从 `(.idx + .mp4)` 在内存中
重新拼出一个 `styp + moof + mdat` 切片并写回网络。

切片不落盘、不转码、不重复占用存储空间。

```
离线(每份原片一次)                 请求时(每个切片)

.mp4 ─▶ cmafly-index ─▶ .idx ─┐
                              ├─▶ cmafly-serve ─▶ seg_NNNN.m4s
                        .mp4 ─┘   (mmap;内存中拼装)
```

## 为何如此设计

预先生成 HLS 切片有两项代价:存储量翻倍(甚至更多),存储的还是可能根本不会被
播放的内容;而且媒体库被「写死」 — 一旦替换或重编一支原片,意味着要删除并重写
数千个小文件。cmafly 把原始 MP4 视为唯一真实来源,把切片视为
`(.idx, .mp4, segment_index)` 的纯函数产物。一份 13 MB 的索引取代 2 小时视频
约 3.7 GB 的预生成 `.m4s`。新档案只要两个文件(原片 + `.idx`)同时就位即可被
服务;被删除的档案在其 LRU 条目被淘汰后立即停止对外服务。

## 仓库结构

| Crate | 类型 | 角色 |
| --- | --- | --- |
| `cmafly`(`crates/packager`)| 库 | 解封装、切片、fMP4 / 播放列表写入器、`.idx` 格式。纯同步,不涉 I/O 与 async。 |
| `cmafly-index`(`crates/indexer`)| 二进制 | 离线:扫描一个 MP4,输出一个 `.idx`(原子写入)。 |
| `cmafly-serve`(`crates/server`)| 二进制 | 长驻 `tokio` + `axum` HTTP 源站,按请求从 `.idx + .mp4` 组装切片。 |

刻意切分:只有 server crate 依赖 `tokio`、`axum`、`lru`;库可在非 async 环境
中复用,仅依赖 `thiserror`、`byteorder`、`memmap2`、`blake3`。

## v1 范围

- 仅 VOD。
- 单一码率、一条视频轨 + 一条音频轨,音视频合流 CMAF。
- 视频:H.264(`avc1`)或 HEVC(`hvc1` / `hev1`);音频:AAC(`mp4a`)。
- 不支持转码、加密、LL-HLS、master playlist、字幕、多版本(alternate renditions)。

## 环境要求

- Rust **stable**(edition 2024)。仓库通过 `rust-toolchain.toml` 锁定
  `channel = "stable"`,`rustup` 会自动采用。
- Linux 或 macOS。原子写入中的 `fsync(parent_dir)` 路径为 Unix-only;其余
  代码跨平台。

如尚未安装 Rust:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## 快速开始

整体构建:

```sh
cargo build --release
```

为单支原片产出 `.idx`:

```sh
cargo run --release -p cmafly-index -- \
    --input  /path/to/originals/abc.mp4 \
    --output /path/to/index/abc.idx \
    --segment-duration 6.0
```

启动源站:

```sh
cargo run --release -p cmafly-serve -- \
    --media-dir /path/to/originals \
    --index-dir /path/to/index \
    --bind 127.0.0.1:8080
```

容量参数(`--max-open-archives`、`--max-inflight-segments`、
`--permit-wait-timeout`)在启动时按主机限制自动解析;源站会将解析后的数值、
来源(auto 或 flag)以及作为输入的主机参数写入启动日志。仅在自动值不适用时
才覆盖。

请求接口 — `id` 即原片去掉扩展名后的文件名:

| 路由 | 用途 |
| --- | --- |
| `GET /v/{id}/playlist.m3u8` | media playlist |
| `GET /v/{id}/init.mp4` | CMAF init segment |
| `GET /v/{id}/seg_{NNNN}.m4s` | 单个 CMAF 媒体切片,按请求即时组装 |
| `GET /healthz` | 存活探针 |

把支持 CMAF 的播放器(Safari、hls.js、ExoPlayer 等)指向
`http://127.0.0.1:8080/v/abc/playlist.m3u8` 即可。

TLS 由上游卸载 — `cmafly-serve` 走纯 HTTP,设计上置于 CDN 或反向代理之后。

## 日常运维

源站从 `--media-dir` 与 `--index-dir` 按需懒加载内容 — 无需重启,无启动扫描,
无注册步骤。

- **新增。** 把 `<id>.mp4` 与 `<id>.idx` 分别投入两个目录;首次请求前两份
  文件必须都到位。
- **下线。** 删除其中任一或两份文件。新请求返回 `404`;LRU 中已缓存的条目
  可能会继续短暂提供服务,直到被淘汰或进程重启。
- **替换。** 重跑 `cmafly-index` 重建 `<id>.idx`(原子写入)。**严禁在源
  `.mp4` 仍处于 LRU 热点集合时就地覆盖或截断它** — 源站以 `mmap` 持有该
  文件,就地修改可能触发 `SIGBUS` 致使进程崩溃。等长替换若未配套重建索引,
  运行期不会被检测出来,会静默生成结构合法但内容错乱的切片。

## 开发

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI 在 stable 上跑同样三项检查,见
[`.github/workflows/ci.yml`](./.github/workflows/ci.yml)。

部分集成测试会在 `tests/fixtures/sample.mp4`(或 `HLS_TEST_FIXTURE_MP4`
环境变量所指路径)寻找 MP4 fixture;若找不到,测试会自行 skip 并向 stderr
输出说明 — 单元测试套件本身不依赖任何二进制 blob。大于 1 MB 的 fixture
不可提交(见 `.gitignore`)。

## 目录布局

```
cmafly/
├── Cargo.toml              workspace 根目录
├── rust-toolchain.toml     stable 通道锁定
├── README.md               英文 README(默认)
├── README.zh-CN.md         本文件
├── README.zh-TW.md         繁体中文翻译
├── LICENSE-MIT
├── LICENSE-APACHE
├── crates/
│   ├── packager/           库
│   ├── indexer/            cmafly-index 二进制
│   └── server/             cmafly-serve 二进制
└── tests/fixtures/         仅本地使用的 MP4 fixture(已 gitignore)
```

## 状态

v1 — 首个可用版本。`cmafly` 库公开 API 与 `.idx` 磁盘格式在版本之间尚未稳定
— 升级时请重建索引。

## 授权

采用双重授权,择一即可:

- Apache License, Version 2.0([LICENSE-APACHE](./LICENSE-APACHE))
- MIT license([LICENSE-MIT](./LICENSE-MIT))

### 贡献

除非另行明示,依 Apache-2.0 授权之定义,您有意提交以纳入本作品的任何贡献
将以上述双重授权方式授出,且不附加任何其他条款或条件。
