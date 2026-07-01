# 详细设计

本文档描述数据结构、API 契约、时间对齐、裁剪逻辑与第二阶段接口预留。
与 [README.md](../README.md) 配合阅读。

---

## 1. 时间对齐（关键）

- T0 = 后端收到 `on_publish` 的时刻，记为 `start_unix_ms`（unix 毫秒）。
- 用户点「开始」：`start_offset = (now_unix_ms - start_unix_ms) / 1000.0`
- 用户点「停止」：`end_offset = (now_unix_ms - start_unix_ms) / 1000.0`
- 截取时长：`duration = end_offset - start_offset`

> 统一用后端时钟，不依赖 SRS 时间，避免多时钟漂移。Demo 单机，前后端同机，误差可忽略。

---

## 2. 内存状态结构

```rust
// 单直播间状态（Demo 单间，写死 stream_id = "live/room001"）
struct StreamState {
    start_time: Option<Instant>,    // 推流开始时刻（单调时钟，用于计时显示）
    start_unix_ms: Option<u64>,     // 推流开始 unix 毫秒（用于和用户点击对齐）
    file_path: Option<String>,      // DVR 录制完成后的完整 MP4 路径
    status: StreamStatus,           // Idle / Live / Ended
}

// 用户当前标记的开始点（Demo 单用户，单个即可）
struct ClipMark {
    start_offset_sec: f64,          // 距推流开始的秒数
}

// 裁剪任务
struct ClipJob {
    id: String,                     // job_id（uuid 或自增）
    stream_id: String,
    start_offset: f64,
    duration: f64,
    quality: String,                // "original" / "720p" / "480p"
    status: JobStatus,              // Pending / Processing / Done / Error
    output_file: Option<String>,    // 相对 /clips 的文件名或完整路径
    file_size: Option<String>,      // 人类可读，如 "15.2 MB"
    created_at: String,             // ISO8601
}

enum StreamStatus { Idle, Live, Ended }
enum JobStatus { Pending, Processing, Done, Error }
```

共享方式：`Arc<RwLock<AppState>>`，`AppState` 聚合 `stream`、`current_mark`、`jobs: Vec<ClipJob>`。

---

## 3. API 契约

### 3.1 SRS Hook（SRS 主动回调，返回 `{"code":0}` 放行）

SRS body 示例：
```json
{ "action": "on_publish", "app": "live", "stream": "room001",
  "cwd": "/usr/local/srs", "file": "/data/recordings/live/room001/xxx.mp4" }
```

| 路径 | 处理 |
|------|------|
| `POST /api/hooks/on_publish` | 记录 `start_time`、`start_unix_ms`，`status=Live`，清空上一轮 mark。返回 `{"code":0}` |
| `POST /api/hooks/on_unpublish` | `status=Ended`。返回 `{"code":0}` |
| `POST /api/hooks/on_dvr` | 记录 `file_path=body.file`，遍历所有 `Pending` 任务逐个执行 FFmpeg 裁剪。返回 `{"code":0}` |

> ⚠️ `on_dvr` 的 `file` 是**容器内路径** `/data/recordings/...`。后端在宿主机运行，需映射为宿主机路径 `./data/recordings/...`（前缀替换 `/data` → 项目 data 目录绝对路径）。此映射在 `hooks.rs` 处理。

### 3.2 用户裁剪接口

| 请求 | 响应 |
|------|------|
| `POST /api/clip/start`（body `{}`） | `{ "code":0, "start_offset": 12.3 }`，未在直播则 `code!=0` |
| `POST /api/clip/end?quality=original\|720p\|480p` | `{ "code":0, "job_id":"...", "status":"processing\|pending" }` |
| `GET /api/clip/status/:job_id` | `{ "status":"done", "download_url":"/clips/xxx.mp4", "file_size":"15.2 MB" }` |
| `GET /api/clips` | `{ "code":0, "clips":[ ClipJob, ... ] }`（最新在前） |

**`/api/clip/end` 逻辑：**
1. 解析 `quality`（缺省 `original`）
2. 无 mark / 未直播过 → 返回错误
3. 计算 `end_offset`、`duration`；`duration < 1` 秒 → 错误「片段太短」
4. 创建 `ClipJob`（含 quality）
5. 分支（见 §6 实时裁剪）：
   - **直播进行中**（status==Live） → `process_job_hls`（HLS 切片裁剪），返回 `processing`
   - **直播已结束且有 `file_path`** → `process_job`（DVR 裁剪），返回 `processing`
   - 既非直播也无录制 → `pending`（少见）
6. 清空 current_mark

### 3.2.1 登录鉴权（手机号 + 验证码）

- `POST /api/login {phone, code}`：验证码写死 = **手机号后 6 位**（`phone[len-6..]`）；通过则生成 uuid token 存 `AppState.sessions: HashSet<String>`，返回 `{code:0, token, phone}`。
- **录制接口需登录**：`clip_start` / `clip_end` 读 `Authorization: Bearer <token>`，token 不在 sessions → 返回 `code:401`。
- 其它接口（clips/status/播放/弹幕）无需登录。
- Demo 简化：内存会话、无过期、无密码、token 不绑手机号。**生产需换真实短信验证码 + JWT/过期 + HTTPS。**
- 前端：Web 存 localStorage、Flutter 存内存；请求带 Bearer，401 时自动登出并弹登录。

### 3.3 静态文件

- `GET /clips/*` → `./data/clips/`，响应头 `Content-Disposition: attachment`、`Accept-Ranges: bytes`（断点续传）。用 `tower-http::services::ServeDir`。
- `GET /hls/*` → `./data/hls/`（小程序 `<video>` 播放）。
- `GET /live.m3u8` → 动态生成的直播窗口 m3u8（只列最近 8 片，避免 video 从头播）。
- `GET /` → `frontend/index.html`。

### 3.4 弹幕 WebSocket（`danmaku.rs`）

- `GET /ws/danmaku` 升级为 WebSocket。
- `AppState.danmaku: broadcast::Sender<String>`（`tokio::broadcast`，容量 256）。
- 每个连接两条任务：`broadcast → 本连接`（订阅 rx）、`本连接 → broadcast`（发送给所有人）。
- 过滤：空白丢弃、长度 > 100 字符丢弃。
- 跨端互通：Web / Flutter 连同一 channel，一处发送全部可见。**小程序端冻结，不接弹幕。**

### 3.5 播放器控制（前端，无需后端）

- **暂停/播放**：暂停 = 断开拉流（关闭 PeerConnection / 销毁 flv.js）+ 阻止自动重连；播放 = 按当前协议重连。
- **弹幕速度**：档位 1~10 → 飘过时长 `14-speed` 秒；Web 用 CSS `animation-duration`，Flutter 用 `AnimationController` duration，实时生效。

---

## 4. FFmpeg 裁剪（含清晰度）

裁剪时按 `quality` 选择编码参数（`clip.rs::quality_args`）：

```
# 原画 original：不重编码，秒级
ffmpeg -y -ss <start> -i <input> -t <dur> -c copy -avoid_negative_ts make_zero <out>

# 720p / 480p：缩放重编码（较慢、CPU 高，但片段更小）
ffmpeg -y -ss <start> -i <input> -t <dur> \
       -vf scale=-2:720|480 -c:v libx264 -preset veryfast -crf 23 -c:a aac \
       -avoid_negative_ts make_zero <out>
```

- `-ss` 放 `-i` 前：快速 seek。原画 `-c copy` 起点对齐到关键帧（误差 ~1 GOP）。
- DVR 裁剪与 HLS concat 裁剪共用 `quality_args`，两条路径都支持三档清晰度。
- 用 `tokio::process::Command` 异步执行，不阻塞 axum。
- 执行前置 `Processing`；成功置 `Done` + 记录文件大小；失败置 `Error`（stderr 入日志）。
- 输出文件名：`clip_<stream>_<timestamp>_<id>.mp4`，放 `./data/clips/`。

---

## 5. 播放协议与延迟设计

SRS 用同一路推流同时对外提供多种协议，各端按需取用，互不影响。**裁剪基于 DVR，与播放/推流协议解耦。**

### 5.1 协议矩阵

| 协议 | 延迟 | 端 | 备注 |
|------|------|----|------|
| WebRTC (WHEP) | 0.2~1s | 网页默认 / 原生 App | 浏览器原生支持；**微信小程序不支持** |
| HTTP-FLV | 1~3s | 网页降级 / 小程序 | flv.js / `<live-player>` |
| HLS | 5~10s | 小程序 / 弱网 | `<live-player>` / hls.js |

### 5.2 两种推流路径（SRS 同时支持）

- **RTMP 推流**（OBS/摄像头）→ vhost `rtc { rtmp_to_rtc on }` 转 WebRTC 播放，延迟 ~1s；DVR 原生录制。
- **WHIP 推流**（全程 WebRTC，OBS 30+）→ vhost `rtc { rtc_to_rtmp on }` 转回 RTMP 落盘，延迟 <300ms。
- 两者 `on` 不冲突：按推流源类型自动选择。`rtc_server { candidate <内网IP> }` 是 WebRTC 媒体可达的关键。

### 5.3 前端 WHEP 播放流程（frontend/index.html）

```
new RTCPeerConnection → addTransceiver(video/audio, recvonly)
→ createOffer → POST offer SDP 到 /rtc/v1/whep/?app=live&stream=room001
→ 拿 answer SDP → setRemoteDescription → ontrack 绑定 <video>.srcObject
```
页面默认 WebRTC，失败/手动可切 flv.js；`liveBufferLatencyChasing` + 追帧按钮控制累积延迟。

### 5.4 延迟来源与已知限制

- **GOP（关键帧间隔）是延迟大头**：摄像头脚本 `-g 15`(0.5s)，OBS 建议 1s + `tune=zerolatency`。
- **candidate 绑定内网 IP**：换网段必须改 `srs.conf` 并重启，否则 WebRTC 连不上。
- **音频转码受限**：`ossrs/srs:5` 镜像 `--ffmpeg-opus=off`，Opus↔AAC 转码能力有限，WebRTC/DVR 可能仅视频。Demo 看画面不受影响。
- ffmpeg 8.1.1 的 `whip` muxer 仍实验性（无法正常收尾），WHIP 端到端验证以 OBS 为准。

---

## 6. 直播中实时裁剪（HLS）✅ 已实现

直播进行中标记的片段**无需等停止推流**，直接从 HLS 切片裁剪。

**实现（`clip.rs::clip_from_hls`）**
1. 读 `data/hls/live/room001/live.m3u8`，按 `#EXTINF` 累加每片时长，建立「切片 → [起,止]秒」映射。
2. 选出覆盖 `[start, start+duration]` 的连续 `.ts`。
3. `ffmpeg -f concat` 合并这些切片，`-ss/-t -c copy` 精修两端。

**时间对齐（关键）**
- HLS 切片 seq 跨会话累积，会导致 m3u8 时间轴起点 ≠ 本场 T0。
- 解决：`on_publish` 时 `clip::clear_hls()` 清空 HLS 目录，使本场切片从头开始，
  m3u8 第一个切片 cum=0 对齐推流起点，从而与 `start_offset`（基于 T0）一致。

**分支逻辑（`clip_end`）**
- 直播进行中（`status==Live`）→ `process_job_hls`（HLS 实时裁剪）。
- 直播已结束且有 DVR → `process_job`（完整 MP4 裁剪，更精确）。

**已知限制**
- 末端 1~2 秒可能还在未完成的 fragment 里（2s/片），实时裁剪会略短；DVR 裁剪无此问题。
- 精度受切片边界影响，误差 ±1 片（~2s）。需更准用 DVR。

---

## 7. Rust 依赖（Cargo.toml 计划）

| crate | 用途 |
|-------|------|
| `axum` | Web 框架 / 路由 |
| `tokio` (full) | 异步运行时 + `tokio::process` |
| `serde` / `serde_json` | JSON 序列化 |
| `tower-http` (fs, cors) | 静态文件 ServeDir + CORS |
| `chrono` | 时间戳 `created_at` |
| `uuid` | job_id |
| `tracing` / `tracing-subscriber` | 日志（调试推流回调） |
| `anyhow` | 错误处理 |
| `reqwest` | 本地→云端上报（report 模块） |

---

## 8. 云端后台管理系统

本设计文档聚焦**本地节点**。云端中心（节点管理 + 配置下发 + 业务数据看板，Rust+PostgreSQL）独立成文：见 **[CLOUD.md](CLOUD.md)**。本地侧对接点：`backend/src/report.rs`（注册/上报/拉配置）+ `GET /api/config`（暴露下发配置给前端）。
