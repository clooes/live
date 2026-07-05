# 改造文档：内网直播「单二进制 + 纯 Rust WebRTC」重构

> ⚠️ **历史文档（2026-07-01 前的重构记录）**，架构思路仍有效，但以下细节已被后续演进取代：
> 录制已从「HLS 连续切片」改为「RTMP 直录整场 full.mp4 + 按时间裁剪」；前端已并成单页面；
> 推流主路径改回 RTMP（服务端桥转 WebRTC）。**现状以根 README.md 与
> [RTMP-桥与录制.md](RTMP-桥与录制.md) 为准。**

> 状态：核心直播链路（WHIP→WHEP）+ 内网双页 + 配置 SSE 下发 + **录制/切片/回放**均已实测通过
> 范围：核心直播链路重构。旧架构（`backend/`/`frontend/`/`srs/`/`cloud/`）曾作参考与回退，**已于 2026-07-01 删除**（文档归档见 `docs/legacy/`）。下文中对 `backend/`、`frontend/` 的引用为改造当时的历史叙述。
> 重要修正（第 0 步实测）：推流入口由 RTMP 改为 **WHIP**，详见 §11。
> 录制/切片/回放：设计与实测见 §12（含一处必须的 xwebrtc 补丁）。

---

## 1. 背景与动机

当前系统要跑起来需要四个外部组件：**SRS(Docker) + PostgreSQL + Node + FFmpeg**。

带来的问题：
- 部署重：用户机器要装 Docker、起容器、建数据库、装 Node 构建前端。
- 运维坑多：README 的 FAQ 一半是 SRS 的 `candidate` 配错、Docker-on-Mac 的 UDP 限制、Opus↔AAC 转码受限等问题。
- 不可"双击即用"：无法交付给非技术用户。

**改造目标**：把核心直播链路收敛为**一个 Rust 单二进制**，mac/windows 双击即跑、零外部服务：
1. 接收 **OBS 的 WHIP 推流**（全程 WebRTC，替代 SRS 接收端）；
2. 同进程内生成两个**内网网页**：① 管理员配置页（配清晰度）② 用户观看页；
3. 用户端走 **WebRTC（WHEP）** 低延迟播放（亚秒级）。

---

## 2. 已确认的范围决策

| 维度 | 决策 | 含义 |
| --- | --- | --- |
| 推流来源 | **OBS（WHIP）** ⬅️ 第0步修正 | OBS 服务选 WHIP，全程 WebRTC 推到本进程 :8900。原定 RTMP 因 xiu 不做跨协议桥接而改用 WHIP（§11） |
| 播放协议 | **WebRTC（WHEP）** | 亚秒延迟，纯 Rust 实现（复用 xiu/webrtc-rs） |
| 依赖边界 | **可打包进二进制的都允许** | 但首版选型做到纯 Rust，连 ffmpeg 都不需要 |
| 清晰度 | **原画直通，仅码率档** | 管理员选的码率是给 OBS 的推荐/约束，**服务端不重编码** |
| 音频 | **首版只做视频** | H.264 直通；AAC→Opus 转码后续迭代 |
| 前端 | **沿用 React，构建后嵌入** | `npm run build` 产物用 `rust-embed` 打进二进制，运行时零 Node |

> 关键收益：清晰度直通 + 仅视频 ⇒ 媒体链路是纯 H.264 转发，**无需 ffmpeg、无需音频转码**，真·纯 Rust 单文件。

---

## 3. 架构对比

### 改造前

```
OBS ─RTMP→ SRS(Docker) ─WebRTC→ 浏览器
                │
                ├─ DVR/HLS → FFmpeg 裁剪
                └─ HTTP Hook → Rust后端(:8000) ─ React(Node构建) / PostgreSQL(云端)
```

### 改造后

```
OBS(WHIP) ─WebRTC─▶ ┌──────────── 单 Rust 二进制 ────────────┐
                     │  xwebrtc(:8900) WHIP入 ─▶ streamhub      │
                     │                  └─▶ WHEP出 ─────────────│─WebRTC─▶ 浏览器播放
                     │                                          │
浏览器 ──HTTP(:8000)─▶│  axum: 嵌入式 React(管理页 + 观看页)      │
                     │        /api/config (读写清晰度 → JSON)    │
                     └──────────────────────────────────────────┘
                                config.json（紧邻二进制，持久化）
```

一个进程，监听口：
- WebRTC `:8900`（WHIP 推流入口 + WHEP 播放出口，同一个 `WebRTCServer` 处理，HTTP 信令 + UDP 媒体）
- HTTP `:8000`（页面 + 配置 API）
- RTMP `:1935`（可选保留，供后续录制/兼容；首版 WebRTC 链路不依赖它）

---

## 4. 技术选型

| 用途 | 选型（已核实版本） | 说明 |
| --- | --- | --- |
| HTTP 服务 | `tokio` + `axum 0.7` | 沿用现有 `backend/` 写法 |
| WHIP 接收 + WHEP 出 | **`xwebrtc 0.3.5`** | xiu 的 WebRTC 模块，`WebRTCServer` 同端口处理 `/whip` 和 `/whep` |
| 媒体路由 | **`streamhub 0.2.4`** | `StreamsHub` 撮合发布/订阅；按 `StreamIdentifier` 精确匹配（见 §11） |
| RTMP 接收（可选） | **`rtmp 0.6.5`** | 首版 WebRTC 链路不依赖；保留供后续录制/兼容 |
| WebRTC 协议栈 | `webrtc 0.8`（webrtc-rs，xwebrtc 底层依赖） | SDP/ICE/DTLS/SRTP 全包，不自己写 |
| 前端嵌入 | `rust-embed` | 把 `frontend/dist` 编进二进制 |
| 配置持久化 | `serde` + `serde_json` | 写本地 `config.json`，无数据库 |

> ✅ **第 0 步已完成**：上述 crate 已成功编译链接进 `relay` 单二进制，WHIP 推流 + WHEP 订阅匹配实测通过。详见 §11。
> ⚠️ **原生构建依赖**：`xwebrtc` 传递依赖 `audiopus`(libopus)、`fdk-aac`、`openssl-src` 等 C 库（vendored，静态进二进制），构建期需 C 编译器/cmake；Windows 交叉/本地编译需重点验证。

---

## 5. 目标仓库布局

新建独立 crate `relay/`。**注：下表为改造初期规划，实际落地结构以 README §3、§12 为准——`media.rs`/`assets.rs` 未采用，另加了 `ffmpeg.rs`（内置 ffmpeg）、`clip.rs`、`record.rs`、`build.rs`；旧 `backend/` 已删。**

```
relay/
├── Cargo.toml        # ✅ 已建：streamhub/rtmp/xwebrtc/commonlib + tokio
├── whep-test.html    # ✅ 已建：最小 WHEP 播放测试页（第0步人工验证用）
└── src/
    ├── main.rs       # ✅ 已建：StreamsHub + RtmpServer + WebRTCServer 装配跑通
    ├── media.rs      # 待拆：把 main.rs 的媒体装配抽出
    ├── config.rs     # 待建：NodeConfig（复用现有结构）+ load/save config.json
    ├── web.rs        # 待建：axum 路由 /api/config + 静态页面(rust-embed)
    └── assets.rs     # 待建：rust-embed 指向 ../frontend/dist
```

---

## 6. 实施步骤（按风险排序）

0. ~~**打通媒体内核**~~ **✅ 已完成**（见 §11）：`relay` 单二进制装配 `StreamsHub` + `WebRTCServer`，WHIP 推流 + WHEP 订阅匹配实测通过。

2. **axum 页面层 ✅ 已完成**
   `relay/src/web.rs` 用 `rust-embed` 托管 `relay/web/dist`，`GET/POST /api/config`，配置持久化到二进制同目录 `config.json`（`relay/src/config.rs`）。已实测：读/写/持久化/非法校验400/SPA回退200 全通过。
   > 端口编排：axum 页面/配置走 :8000，WebRTC 信令+媒体走 :8900（`WebRTCServer` 自管）。观看页 JS 直连 :8900 的 `/whep`。

3. **前端两页改造 ✅ 已完成**
   新建 `relay/web/`（React+Vite+TS，不引 tailwind/flvjs），hash 路由双页：
   - 观看页 `pages/Viewer.tsx` + `whep.ts`：复用 `frontend/src/hooks/usePlayer.ts` 的 WHEP 逻辑，去掉 FLV，信令指向 `http://<host>:8900/whep`。
   - 管理页 `pages/Admin.tsx`：读写 `/api/config` 的房间/清晰度/码率档，含 OBS WHIP 推流指引（直通不强制，仅引导）。
   - `npm run build` → `web/dist/` 被 rust-embed 嵌入二进制。
   - 未搬旧 React 的登录/裁剪/弹幕（产品定位不同，另起极简双页）。

3b. **配置实时下发（SSE）✅ 已完成**
   观看端通常在其他设备，需要管理端改完配置后**实时推送到所有观看设备**。实现：
   - 后端 `GET /api/config/stream`（SSE）：连接即推当前快照，`POST /api/config` 保存后经 `broadcast` 通道广播新配置。
   - 观看页用 `EventSource` 订阅：房间名一变自动切换到新流（`useWhep` 依赖 room 重连）。
   - 实测：初始快照 + 变更两条均实时到达，广播计数正确。
   - **观看页清晰度切换：已明确推迟**（直通只有一路流，真切换需转码或推流端多推，见 §8）。

4. **单二进制收尾 ✅ 已完成**
   构建顺序：先 `cd relay/web && npm run build`，再 `cd relay && cargo build --release`。已加录制/切片/回放（§12）、内置 ffmpeg（§12.9）、数据目录统一 + `data_dir`（§12.10）、优雅退出（§12.11）。mac release 实测跑通。

5. **跨平台（进行中）**
   mac(arm64) 已出包并实测。Windows/Intel/Linux 需放入对应平台静态 ffmpeg（`vendor/ffmpeg/<平台>/`）再 `cargo build --release`；纯 Rust 部分预期可直接编，`process_group` 等已按 `#[cfg(unix)]` 分平台。

---

## 7. 可复用的现有资产

| 资产 | 路径 | 复用方式 |
| --- | --- | --- |
| 清晰度配置结构 | `backend/src/state.rs:43` `NodeConfig` | 直接搬，去云端字段 |
| WebRTC 播放逻辑 | `frontend/src/hooks/usePlayer.ts` | 改信令地址即可复用 |
| 播放器组件 | `frontend/src/components/Player.tsx` | 复用 |
| axum 路由/静态托管 | `backend/src/main.rs` | 参考写法 |

---

## 8. 已知限制 / 后续迭代（首版不做）

- **无音频**：H.264 直通，AAC→Opus 转码留下一迭代（注意 WHIP 链路 OBS 原生推 Opus，加音频比 RTMP 顺）。
- **清晰度不强制 + 观看页不能切档**：直通模式只有一路原始流，"码率"仅是给 OBS 的建议值。观看页真正切换清晰度需二选一：①服务端转码（放弃直通、捆绑 ffmpeg、CPU 高）②推流端多推多路（OBS 多输出、每档一个流名、上行×N）。**已明确推迟**，首版观看页单档原画。
- **RTMP 不出 WebRTC**：xiu 按协议精确匹配，RTMP 推的流 WHEP 看不到（§11）。故首版 OBS 必须用 **WHIP**；要兼容老版 OBS(RTMP) 需自建 RTMP→WebRTC 桥接，列入后续。
- **WebRTC candidate / 内网 IP**：跨网段/真机访问需把 candidate 设为本机内网 IP（管理页暴露此项或启动参数）。与原 SRS 同类问题。
- ~~录制裁剪~~ **✅ 已实现**（§12）：直播中自动全程录成 HLS，观看时标起止即可切片下载 + 整场回放。录制/切片用 ffmpeg，**已按"可打包进二进制"原则内置静态 ffmpeg**（`build.rs` 嵌入 + 运行时释放，见 §12.9），目标机无需预装；未放二进制时回退 PATH。
- **切片非帧级精确**：首版 `-c copy` 不重编码（秒级），起点对齐到最近关键帧，时长有 ±1s 误差。需精确则加重编码分支（`quality_args` 结构已预留）。
- 单直播间 `live/room001` 写死；多房间、登录鉴权、云端上报均不在首版。

---

## 9. 风险与回退

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| ~~xiu WHEP 直通不可用~~ | ~~媒体链路打不通~~ | **✅ 第0步已验证可用**（WHIP→WHEP）；回退方案路线 B（捆绑 mediamtx）保留待命 |
| Windows 下 webrtc-rs/UDP + 原生库(openssl/opus/fdk-aac)编译 | 跨平台失败 | mac 先跑通，Windows 单独验收：先确认 C 工具链/cmake，再查 candidate/防火墙 |
| 直通模式无法满足"限码率"预期 | 产品语义偏差 | 文档与管理页明确"码率为建议值"；真限流需转码，列入后续 |
| OBS 版本过老不支持 WHIP | 用户推不上流 | 要求 OBS 30+；或后续补 RTMP→WebRTC 桥接 |

---

## 10. 验收标准（端到端）

1. `cd relay && cargo run`（或 release 二进制），日志显示 RTMP :1935 / WHEP :8900 / HTTP :8000 已启动。
2. 浏览器开 `http://localhost:8000` → 管理页改清晰度并保存；重启进程后 `config.json` 仍在、配置不丢。
3. OBS 服务选 **WHIP**，地址 `http://localhost:8900/whip?app=live&stream=room001`（或 `ffmpeg -f whip` 同地址）。
4. 观看页 WebRTC 出现实时画面，肉眼延迟 <1s。
5. **不起 Docker/SRS/PG/Node**，仅靠这一个二进制完成全流程 → 验证"零外部依赖单文件"达成。
6. （能力允许）Windows 上 `cargo build --release` 重复 1–4。

---

## 11. 第 0 步验证结论（媒体链路）

**目的**：在投入页面/配置层开发前，验证"纯 Rust 单二进制跑 OBS→WebRTC 低延迟"这条路是否成立。

**已完成 ✅**
1. **单二进制编译运行**：`relay` 依赖 `streamhub 0.2.4`/`rtmp 0.6.5`/`xwebrtc 0.3.5`（底层 `webrtc 0.8` = webrtc-rs）成功编译，启动后 RTMP :1935 与 WebRTC :8900 正常监听。
2. **RTMP 接收正常**：`ffmpeg` 推流，发布者握手成功、H.264 SPS 正确解析（640x360 baseline）、`streamhub` transceiver 运行。
3. **WHIP 推流 + WHEP 播放打通**：`ffmpeg -f whip` 推流完成完整 WebRTC 握手（offer/answer/ICE/DTLS/SRTP 全过）；有 WebRTC 发布者时 WHEP 订阅在 `streamhub` 匹配成功。

**关键发现（导致架构修正）⚠️**
- `streamhub::subscribe()` 用 `self.streams.get(identifier)` **按 `StreamIdentifier` 枚举变体精确匹配，无跨协议回退**。
- WHEP 订阅写死 `StreamIdentifier::WebRTC`（`xwebrtc/src/session/mod.rs:382`），而 RTMP 发布注册为 `StreamIdentifier::Rtmp` → **RTMP 推的流 WHEP 看不到**，报 `no app or stream name`。
- 结论：**xiu 开箱不做 RTMP→WebRTC 桥接**。低延迟链路必须 **WHIP 进 → WHEP 出**（全程 WebRTC）。已据此把推流入口从 RTMP 改为 WHIP。

**接口事实（实施直接用）**
- WHIP 推流：`POST http://<IP>:8900/whip?app=live&stream=<key>`，body 为 SDP offer，成功 `201` + `application/sdp` answer。
- WHEP 播放：`POST http://<IP>:8900/whep?app=live&stream=<key>`，同上。
- `WebRTCServer` 还会从 exe 同目录托管 `index.html`/`whip.js`/`whep.js`（自带 demo，可参考或忽略）。

**浏览器出画面：✅ 已人工验证**
- 用真实 **OBS（WHIP 推流）** → `relay` → 浏览器观看页端到端出画面成功（2026-07-01）。（当时用的最小测试页 `relay/whep-test.html` 已删除，功能由正式观看页 `web/src/pages/Viewer.tsx` 替代。）
- OBS 推流设置：服务=WHIP，服务器=`http://localhost:8900/whip?app=live&stream=room001`，Bearer 留空；输出 x264、关键帧间隔 1s、profile baseline、tune zerolatency、附加 `repeat-headers=1`。
- 关键排障点：H.264 over WHEP 若"连上但一直 loading"，是中途加入的订阅者拿不到 SPS/PPS——推流端加 **`repeat-headers=1`**（每关键帧内嵌参数集）即解。已固化到 `relay/push-*.sh`。
- 便捷脚本：`relay/push-test-whip.sh`（测试图样）、`relay/push-camera-whip.sh`（mac 摄像头）。

**遗留观察**
- `xwebrtc` 传递依赖 `audiopus`(libopus)、`fdk-aac`、`openssl-src` 等 vendored C 库 → 构建期需 C 工具链；Windows 需重点验证。

---

## 12. 录制 / 切片 / 回放（✅ 已实现，2026-07-01 实测通过）

**需求**：直播中**自动全程录制**；观看时点「开始/结束录制」标记一段区间，据起止时间**切片**成 mp4 下载；能**回放**整场录制。思路 = DVR 完整录制 + 时间戳裁剪。

### 12.1 关键前置 bug —— xwebrtc 硬编码 payload type（必须修，否则录制/播放都收不到视频）

`xwebrtc-0.3.5/src/whip.rs` 把 WHIP 进来的 RTP 转发给 streamhub 时，**写死 `match payload_type { 96 => 视频 }`**（`nal_payload_type::H264 = 96`）。但 H.264 的 PT 是 SDP 动态协商的，因编码器而异：**实测 ffmpeg WHIP 协商成 106**，于是所有视频包落进 `_ => {}` 被丢弃——录制订阅收 0 帧，WHEP 也只有 OBS「碰巧协商成 96」时才有画面。

**修复**：vendor 该 crate 到 `relay/vendor/xwebrtc`，把 match 改成用 offer 里解析出的**真实动态 PT**（`video_codec.payload_type` / `audio_codec.payload_type`，PT=0 回退常量），经 `Cargo.toml` 的 `[patch.crates-io] xwebrtc = { path = "vendor/xwebrtc" }` 生效。改后 ffmpeg(106)/OBS 均正常收帧。

### 12.2 录制原理（直播 → 连续 HLS，天然按时间分片）

```
WHIP 推流 ─▶ streamhub ─(BroadcastEvent::Publish{WebRTC})─▶ record::spawn 监听
                 │                                             │ 订阅 WebRTC 流(Frame)
                 └── FrameData::Video (Annex-B 裸 H.264) ──────┘
                                   │ 管道喂 ffmpeg stdin
                                   ▼   ffmpeg -f h264 -i pipe:0 -c:v copy -f hls
                    data/recordings/<room>/<session>/{n}.ts + index.m3u8(带 PROGRAM-DATE-TIME + ENDLIST)
```
- 触发自动开录：`stream_hub.set_hls_enabled(true)` 后 streamhub 才广播 `BroadcastEvent::Publish`；`record::spawn` 收到匹配 room 的 WebRTC 发布即建 session、订阅 `SubDataType::Frame`。
- 数据源用 **Frame**（whip.rs 已把 RTP 深包化成带起始码的 Annex-B，含在带 SPS/PPS，因推流侧 `repeat-headers=1`），直接管道给 ffmpeg，`-c:v copy` 不重编码。
- `-hls_list_size 0` 保留全部片（录制），停流关 stdin → ffmpeg 写 `ENDLIST` 成 VOD；`program_date_time` 写每片墙钟起点。

### 12.3 时间切片（clip.rs，移植 backend/src/clip.rs 并改进对齐）

- 解析 session 的 `index.m3u8`，用 **`#EXT-X-PROGRAM-DATE-TIME`（每片绝对墙钟时间）** 建「分片→[起,止]ms」时间轴 → 选覆盖标记区间的连续 `.ts` → `ffmpeg -f concat -ss <seek> -t <dur> -c copy` 精修两端 → `data/clips/clip_<id>.mp4`。
- **为何用绝对时间而非相对偏移**：Publish 事件到首个关键帧之间有 ~2s 空档，若用「相对 session 起点秒数」会错位；PROGRAM-DATE-TIME 是墙钟绝对时间，与前端标记的 `now_ms` 同一基准，对齐更准。
- 两个实测踩坑（已修）：① ffmpeg 写的偏移是 `+0800`（无冒号），`parse_from_rfc3339` 解析失败会退化成相对时间轴 → 改用 `%z` 格式兜底；② concat 列表里相对路径被 ffmpeg 按「list 文件所在目录」（临时目录）解析 → 必须写**绝对路径**（canonicalize）。

### 12.4 接口 + 静态托管（web.rs）

- `POST /api/clip/start`：以当前直播 session + 当前 `now_ms` 存标记（无直播返回 409）。
- `POST /api/clip/end`：据 `[start, now]` 建 job，`tokio::spawn(clip::run_job)` 异步切，返回 job。
- `GET /api/clip/status/:id`、`GET /api/clips`（列表）、`GET /api/recordings`（可回放场次，含 `playlist` 地址）。
- `nest_service("/clips", ServeDir)` 下载（自带 Range）、`nest_service("/recordings", ServeDir)` 回放整场 HLS。

### 12.5 前端

- 观看页 `Viewer.tsx` 加**录制条**：开始/结束录制 → 轮询 `/api/clip/status` 显示进度 → 完成给下载链接。
- 新增**录制页** `pages/Recordings.tsx`（hash `#/recordings`）：列整场录制（回放）+ 切片（下载）。HLS 回放 Safari 用原生 `<video>`，其它浏览器**动态 import `hls.js`**（code-split，不进主包）。

### 12.6 存储布局

```
data/
├── recordings/<room>/<sessionId>/   # 每场直播一个 session（回放源 + 切片源）
│   ├── index.m3u8 + <n>.ts          # 连续 HLS，全程保留，带 PROGRAM-DATE-TIME/ENDLIST
│   └── meta.json                    # { room, started_at_ms, ended_at_ms, status }
└── clips/clip_<id>.mp4              # 切片输出（下载）
```

### 12.7 实测结论（2026-07-01，ffmpeg 模拟 WHIP 推流）

- 录制：4 个 `.ts` + 合法 `index.m3u8`，`ffprobe` 得 h264 640×360，时长 ≈ 推流时长。
- 切片：标记 ~6s 区间，产出 mp4 `status=done`，`ffprobe` 时长 5.38s（`-c copy` 关键帧对齐的正常误差）。
- 接口：`/api/clips`、`/api/recordings` 数据正确；切片下载 HTTP 200、回放 m3u8 HTTP 200；无直播时 `/api/clip/start` 返回 409。
- 待真机复验：用 OBS 实推一场，走「观看页标起止 → 下载片段 → 录制页回放整场」全流程。

### 12.8 新增依赖

- Rust：`chrono`（解析 PROGRAM-DATE-TIME）、`tower-http` 加 `fs` feature（ServeDir）。
- 前端：`hls.js`（非 Safari 回放，动态加载）。
- 运行时：**ffmpeg**（录制写盘 + 切片）——**已内置**（静态二进制嵌入，见 §12.9），无需目标机预装；未内置时回退 PATH。

### 12.9 内置 ffmpeg（✅ 已实现并实测，方案 B）

为满足「双击即用、目标机不装外部依赖」，把**静态构建**的 ffmpeg 嵌入 relay 二进制，首次运行释放到临时目录再调用。

- `build.rs`：检测 `vendor/ffmpeg/<平台>/ffmpeg[.exe]` 是否存在，存在则设 `cfg(embed_ffmpeg)` + 用 `rustc-env` 传绝对路径；不存在则什么都不做（始终可编译）。
- `src/ffmpeg.rs`：`#[cfg(embed_ffmpeg)] include_bytes!(env!("EMBED_FFMPEG_PATH"))` 把二进制编进来；`path()` 首次调用释放到 `TMPDIR/relay-ffmpeg/<len>-ffmpeg`、赋 `0o755`、`OnceLock` 缓存；用**字节长度**做版本区分（升级自动换新文件）、**原子写**（临时文件 + rename）防并发。未内置或释放失败则回退 `PathBuf::from("ffmpeg")`（PATH）。
- `record.rs`/`clip.rs`：`Command::new("ffmpeg")` → `Command::new(crate::ffmpeg::path())`。启动日志打印 `ffmpeg：内置(嵌入二进制)` 或 `外部 PATH`。
- 二进制放置见 `relay/vendor/ffmpeg/README.md`；必须静态（`otool -L` 只应有 `/System`、`/usr/lib/libSystem|libc++|libobjc`），Homebrew 的动态版不能用。大二进制经 `.gitignore` 不入库，按平台放入后编译时嵌入。
- **实测（2026-07-01）**：放入 osxexperts arm64 静态 ffmpeg 8.1（77MB），`cargo build` 打印「已嵌入内置 ffmpeg」；**清空 PATH（`env PATH=`）启动 relay**，录制产出 `.ts`+`index.m3u8`、切片产出 mp4 `done`，全程走释放到 `TMPDIR/relay-ffmpeg/…` 的内置 ffmpeg——证明删掉系统 ffmpeg 也能录制+切片。代价：单平台二进制 +40~80MB。
- 顺带修复一个竞态：切片 job 在分片尚未落盘时会立即失败 → `clip::run_job` 加有限重试（每 500ms、最多 8 次），4s 内救回。

### 12.10 数据目录：统一到二进制目录 + 可配 `data_dir`（✅ 已实现并实测）

**问题**：原先 `config.json` 以二进制所在目录为基准（`current_exe`），但录制/切片的 `data/` 用 `PathBuf::from("data/...")` **相对启动时的工作目录 CWD**。从家目录直接跑二进制时，数据会写到 `~/data/`，与 config 分裂、对「双击即用」不友好。

**方案**：所有落盘统一以**二进制所在目录**为基准，不随 CWD 变化；并支持 `config.json` 里 `data_dir` 自定义。

- `config.rs`：
  - `base_dir()`：二进制所在目录（`current_exe().parent()`，取不到退回 `.`）。`config_path()` 改用它。
  - `RelayConfig` 加 `data_dir: Option<String>` + `#[serde(default)]`（旧 config 无此字段也能加载，补 None，**向后兼容**）。
  - `resolve_data_dir()`：绝对路径原样；`~/…` 相对 `$HOME`；其余相对二进制目录；留空 = `<二进制目录>/data`。
  - `init_data_root()`（启动定一次、`create_dir_all`、`OnceLock` 缓存）+ `data_root()`（读缓存）。`data_dir` 改动需重启生效（运行中不迁移数据）。
- `record.rs`/`clip.rs`：`recordings_dir`/`clips_dir` 改用 `config::data_root().join(...)`。
- `web.rs`：`/recordings` 的 `ServeDir` 改用 `data_root().join("recordings")`（原来写死 `"data/recordings"`）。
- `main.rs`：启动时 `init_data_root(&cfg)` 并打印 `数据目录：…`。

**实测**：从无关目录（`/private/tmp`）启动 → 数据落在二进制目录 `target/debug/data`、CWD 下不误建 `data`；`config.json` 设 `"data_dir": "/private/tmp/my-relay-data"` → 数据目录指向它并自动创建；旧 config（无 `data_dir`）照常加载。

> 落盘位置总览：`config.json`（二进制目录）、录制/切片（`data_root()`，默认二进制目录 `data/`）、内置 ffmpeg 释放（`$TMPDIR/relay-ffmpeg/`）、切片临时 concat 表（`$TMPDIR`，用完即删）。

### 12.11 优雅退出（✅ 已实现并实测）

**动机**：主要使用方式是双击/终端前台跑，然后关终端窗口。关窗口时系统对**前台进程组**发 `SIGHUP`。要保证：端口不泄漏、子 ffmpeg 不残留、**最后一场录制写完 `ENDLIST` 成完整 VOD**。

**实测基线（未加优雅退出前）**：SIGHUP 会直接终止 relay，端口随进程死释放、录制 ffmpeg 靠 stdin EOF 自然退出——端口/进程不泄漏，但最后一场录制可能没写 `ENDLIST`（VOD 播放列表不完整）。

**实现**：
- `main.rs`：`tokio::select!` 同时等 `stream_hub.run()` 与 `wait_for_signal()`（注册 `SIGINT`/`SIGTERM`/`SIGHUP`，注册后覆盖默认「直接终止」）。收到信号 → `watch::Sender<bool>` 广播 shutdown → `drain` 收集的录制任务句柄、`join_all` 等其收尾（`timeout` 8s 兜底）。
- `record.rs`：录制循环改 `tokio::select!`（`frame_rx.recv()` vs `shutdown.changed()`）；收到 shutdown 即 break → `drop(stdin)` → ffmpeg 读到 EOF 写 `ENDLIST` → `child.wait()` → 更新 `meta.json` status=ended。任务句柄收集到 `RecTasks`（`Arc<Mutex<Vec<JoinHandle>>>`）供 main 等待。
- **关键**：录制 ffmpeg spawn 时 `#[cfg(unix)] cmd.process_group(0)`，让它**脱离 relay 的前台进程组**——否则关窗口的 SIGHUP 会同时直接杀掉 ffmpeg，跳过 `ENDLIST` 收尾。脱离后由 relay 主动关 stdin 触发它优雅收尾。

**实测**：录制中 `kill -HUP <relay>` → 日志「收到退出信号 → 等待 1 场录制写完收尾 → 录制结束 → 已优雅退出」；`index.m3u8` 末尾出现 `#EXT-X-ENDLIST`、`meta.json` status=ended、三端口全部释放、无残留 ffmpeg。

> ⚠️ 用 `nohup`/`&`/`disown` 启动会让进程脱离终端，关窗口不发信号 → 后台残留占端口；双击/前台直接跑无此问题。
