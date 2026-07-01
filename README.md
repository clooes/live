# 内网直播 · 单二进制（relay）

OBS 通过 **WHIP** 全程 WebRTC 推流到本地 `relay` 单二进制，用户在内网网页 **WHEP** 亚秒级观看；
直播自动全程录成 HLS，观看时点「开始/结束录制」标记一段区间即可**切片下载**，也能**回放整场**。

> 目标：一个 Rust 单二进制，mac/windows 直接运行，**不依赖 Docker / SRS / 数据库 / Node 运行时**。
> 前端 React 构建后用 `rust-embed` 打进二进制；录制/切片用的静态 ffmpeg 也已嵌入二进制（首次运行释放调用，见 §7），目标机无需预装。
> 完整设计与实测见 [docs/REFACTOR-SINGLE-BINARY.md](docs/REFACTOR-SINGLE-BINARY.md)。

---

## 1. 架构

```
OBS(WHIP) ─WebRTC─▶ ┌──────────────── relay 单二进制 ────────────────┐
                    │ xwebrtc(:8900)  WHIP入 ─▶ streamhub ─▶ WHEP出   │─WebRTC─▶ 浏览器播放(亚秒)
                    │                              │                  │
                    │                              └▶ 录制(Frame→ffmpeg)│─▶ data/recordings/<room>/<session>/*.ts + index.m3u8
                    │                                                  │
浏览器 ─HTTP(:8000)─▶│ axum：嵌入式 React（观看页 / 录制页 / 管理页）    │
                    │   /api/config (读写清晰度, SSE 实时下发)          │
                    │   /api/clip/* (标记/切片/状态)                    │─▶ data/clips/clip_*.mp4
                    │   /clips /recordings (下载 / HLS 回放, ServeDir)  │
                    └──────────────────────────────────────────────────┘
                                config.json（紧邻二进制，持久化）
```

监听端口：
- `:8900` WebRTC —— WHIP 推流入口 + WHEP 播放出口（同一 `WebRTCServer`，HTTP 信令 + UDP 媒体）
- `:8000` HTTP —— 内网页面 + 配置/录制/切片 API + 切片下载/回放静态托管
- `:1935` RTMP —— 可选保留，首版 WebRTC 链路不依赖

---

## 2. 环境依赖

| 依赖 | 用途 | 验证 |
| --- | --- | --- |
| Rust (cargo) | 编译 relay | `cargo --version` |
| Node.js（仅构建期） | 构建 React 前端，产物嵌入二进制，运行时不需要 | `node -v`（建议 18+） |
| FFmpeg | 录制写盘 + 切片；**已内置**（静态二进制嵌入，见 §7），未内置才回退 PATH | 构建日志「已嵌入内置 ffmpeg」 |
| OBS Studio 30+ | 推流（需支持 WHIP）；无 OBS 可用 `relay/push-*-whip.sh` | — |

> `xwebrtc` 传递依赖 `audiopus`/`fdk-aac`/`openssl-src` 等 vendored C 库，构建期需 C 工具链/cmake。

---

## 3. 目录结构

```
live/
├── README.md                  # 本文件
├── docs/
│   ├── REFACTOR-SINGLE-BINARY.md   # 现行架构文档（含录制/切片/回放设计与实测，§12）
│   └── legacy/                 # 旧 SRS+backend+cloud 架构文档（已废弃，仅留档）
├── relay/                      # ⭐ 单二进制主体
│   ├── Cargo.toml              # streamhub/rtmp/xwebrtc + axum + chrono …
│   │                           #   [patch.crates-io] 指向 vendor/xwebrtc（见下）
│   ├── build.rs               # 检测 vendor/ffmpeg/<平台>/ 有二进制则嵌入
│   ├── vendor/
│   │   ├── xwebrtc/            # 打过补丁的 xwebrtc 0.3.5（修硬编码 payload_type）
│   │   └── ffmpeg/<平台>/      # 静态 ffmpeg（嵌入用，大文件 gitignore 不入库）
│   ├── push-*-whip.sh          # WHIP 推流测试脚本（测试图样 / mac 摄像头）
│   ├── src/
│   │   ├── main.rs             # 装配 StreamsHub/WebRTC/RtmpServer/axum/record + 优雅退出
│   │   ├── config.rs           # RelayConfig + config.json + data_dir/data_root
│   │   ├── ffmpeg.rs           # 内置 ffmpeg：嵌入 + 首次运行释放到临时目录调用
│   │   ├── web.rs              # axum 路由：config(SSE) / clip / recordings + ServeDir
│   │   ├── record.rs           # 录制管理器：WebRTC 流 → 连续 HLS（自动全程录）
│   │   └── clip.rs             # 时间切片：按 PROGRAM-DATE-TIME 从 HLS 裁 mp4
│   └── web/                    # React + Vite + TS（观看 / 录制 / 管理 三页），构建到 web/dist 后嵌入
├── app/                        # Flutter 客户端（见 §6 说明）
└── data/                       # 运行时数据（默认在二进制同目录，见下「数据与配置存放」）
    ├── recordings/<room>/<session>/   # 连续 HLS（回放源 + 切片源）
    └── clips/                  # 切片输出 mp4
```

### 数据与配置存放

均以**二进制所在目录**为基准，**不随启动时的工作目录变化**（双击/任意目录启动都一致）：

- `config.json`：二进制同目录，持久化房间/清晰度/`data_dir` 配置。
- 录制/切片数据：默认 `<二进制目录>/data/`（`data/recordings/…`、`data/clips/…`）。
- **自定义位置**：在 `config.json` 设 `"data_dir"`——绝对路径按原样、`~/…` 相对家目录、其余相对二进制目录；改后**重启生效**。例：`"data_dir": "/Volumes/ext/relay-data"`。
- 内置 ffmpeg 释放到系统临时目录 `$TMPDIR/relay-ffmpeg/`，被系统清理后下次启动自动重新释放。

---

## 4. 启动

```bash
# 1) 构建前端（产物 relay/web/dist 会被 rust-embed 嵌入二进制）
cd relay/web && npm install && npm run build

# 2) 编译运行 relay
cd .. && cargo run          # 或 cargo build --release 后跑 target/release/relay
```

启动后日志显示 RTMP :1935 / WHIP·WHEP :8900 / HTTP :8000 就绪。浏览器打开 `http://localhost:8000`：
- **观看页**（默认）：WHEP 播放 + 录制条（开始/结束录制→切片下载）
- **录制页** `#/recordings`：整场回放 + 切片列表下载
- **管理页** `#/admin`：房间/清晰度配置（保存即经 SSE 实时下发到所有观看设备）

### OBS 推流设置（WHIP）

- 服务：**WHIP**
- 服务器：`http://<本机内网IP>:8900/whip?app=live&stream=room001`，Bearer 留空
- 输出：x264、关键帧间隔 1s、profile baseline、tune zerolatency，附加 x264 参数 **`repeat-headers=1`**

> `repeat-headers=1` 必加：否则中途加入的 WHEP 观看者拿不到 SPS/PPS，会「连上但一直 loading」。
> 无 OBS 可用 `relay/push-test-whip.sh`（测试图样）或 `relay/push-camera-whip.sh`（mac 摄像头）。

---

## 5. 已知限制（首版）

详见 [docs/REFACTOR-SINGLE-BINARY.md](docs/REFACTOR-SINGLE-BINARY.md) §8 / §12：

- **仅视频**：H.264 直通，音频（Opus→AAC）后续迭代。
- **清晰度不强制、观看页不切档**：直通只有一路原画流，「码率」是给 OBS 的建议值。
- **切片非帧级精确**：`-c copy` 秒级完成，起点对齐最近关键帧，时长 ±1s。
- **ffmpeg 已内置**：录制/切片用的静态 ffmpeg 已嵌入二进制、首次运行释放调用，目标机无需预装；未放二进制时回退 PATH（见 §7）。
- **单直播间**：`room` 来自 config；多房间/登录鉴权/云端上报不在首版。
- **实测范围**：录制/切片/回放已用 ffmpeg 模拟 WHIP 推流端到端验证；OBS 真机全流程待复验。

---

## 6. Flutter 客户端（app/）现状

`app/` 是既有 Flutter 客户端，原先依赖旧 `backend/`（已随旧架构删除）的 `:8000` 接口：
`/api/login`、`/ws/danmaku`、`/api/clip/*`、`/api/my/clips`。这些能力 **relay 首版尚未提供**，
故 app 的登录/弹幕/切片等功能当前不可用。待 relay 补齐鉴权/弹幕/多用户切片后，需将
`app/lib/config.dart` 的 `apiBase` 及相关调用指向 relay，并在 relay 实现对应接口。

---

## 7. 关键实现备注

- **xwebrtc 补丁（必看）**：上游 `xwebrtc 0.3.5` 的 `whip.rs` 硬编码 H.264 `payload_type=96` 才转发视频，
  而 ffmpeg 等会协商成 106，导致视频包被全部丢弃（录制/WHEP 都收不到）。已 vendor 到 `relay/vendor/xwebrtc`
  改用协商出的动态 PT，经 `[patch.crates-io]` 生效。**勿升级 xwebrtc 覆盖此补丁**，除非上游已修。
- **xiu 不做跨协议桥接**：RTMP 推的流 WHEP 看不到（streamhub 按 `StreamIdentifier` 精确匹配）。故推流必须走 WHIP。
- **录制自动触发**：`stream_hub.set_hls_enabled(true)` 后 streamhub 才广播 `Publish` 事件，录制管理器据此开录。
- **内置 ffmpeg**：把**静态**构建的 ffmpeg 放到 `relay/vendor/ffmpeg/<平台>/`，`build.rs` 检测到即 `include_bytes!` 嵌入二进制，`src/ffmpeg.rs` 首次运行释放到临时目录、赋可执行权限后调用（用字节长度做版本区分、原子写）。没放二进制则回退 PATH 的 `ffmpeg`，始终可编译。放置说明见 `relay/vendor/ffmpeg/README.md`。已实测：清空 PATH 启动 relay，录制+切片仍全程走内置 ffmpeg 成功。二进制体积因此 +40~80MB（单平台）。
- **优雅退出**：捕获 `SIGINT`/`SIGTERM`/`SIGHUP`（关终端窗口即发 SIGHUP），退出前广播 shutdown → 各录制任务关掉 ffmpeg stdin、等它写完 `#EXT-X-ENDLIST`、更新 `meta.json` 为 ended，再退（最多等 8s 兜底）。录制 ffmpeg 用 `process_group(0)` 脱离前台进程组，避免被同一个 SIGHUP 直接杀掉、跳过收尾。**关终端窗口不会残留占端口的进程，最后一场录制也完整**（已实测端口全释放、ENDLIST 写入）。⚠️ 别用 `nohup`/`&`/`disown` 启动，那会让进程脱离终端、关窗口不发信号 → 后台残留。
