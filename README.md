# 内网直播 · 单二进制（relay）

OBS 推一路 **RTMP**（或 WHIP）到本地 `relay` 单二进制：内网网页 **WHEP 亚秒级观看**，
后台**自动整场录制**；观看页可随时「开始/停止录制」，从整场里按时间切出片段下载。

> 目标：一个 Rust 单二进制，mac/windows 双击即用，**不依赖 Docker / SRS / 数据库 / Node 运行时**。
> React 前端与静态 ffmpeg 都嵌入二进制内，目标机零安装。

---

## 1. 架构

```
                    ┌──────────────────── relay 单二进制 ────────────────────┐
OBS ─RTMP:1935─────▶│ rtmp 接收 ──┬─ 桥(bridge.rs)：ffmpeg 重编码 H264+Opus    │
                    │             │   → 裸 RTP → rtp_ingest 注入 hub           │
                    │             │   → WHEP(:8900) ─WebRTC─▶ 浏览器(亚秒)     │
                    │             └─ 直录(record.rs)：ffmpeg -c copy 无损      │
                    │                 → data/sessions/<id>/full.mp4           │
OBS ─WHIP:8900─────▶│ xwebrtc：WHIP 直推也可（同端口进 hub，WHEP 直接可播）    │
浏览器 ─HTTP:8000──▶│ axum：嵌入式 React 单页（观看+录制+分享二维码+全屏）     │
                    │   /api/config(SSE) /api/record/* /api/records /clips/*  │
                    └─────────────────────────────────────────────────────────┘
                      config.json（二进制同目录）   data/（sessions/clips/logs）
```

- **推流选 RTMP 即可**（设备/软件兼容面最广）：服务端自动分叉「直播」与「录制」两路,
  不需要 OBS 多输出，不翻倍编码。WHIP 直推同样支持。
- 监听端口（`config.json` 的 `ports` 可改，改后重启生效）：
  `:1935` RTMP 推流 ｜ `:8900` WHIP/WHEP（HTTP 信令 + UDP 媒体）｜ `:8000` 内网页面 + API

### OBS 填法（RTMP，推荐）

设置 → 直播：服务=自定义；服务器=`rtmp://<本机内网IP>:1935/live`；串流码=`room001`（房间名）。
**注意 `/live` 留在服务器栏，房间名单独放串流码栏，别拼一起。** 启动横幅会打印可直接复制的地址。

### OBS 填法（WHIP，可选）

服务=WHIP；服务器=`http://<IP>:8900/whip?app=live&stream=room001`；Bearer 留空。
输出：x264、关键帧 1s、profile baseline、tune zerolatency、附加参数 **`repeat-headers=1`**（必加，
否则中途进场的观众拿不到 SPS/PPS 一直 loading）。RTMP 推流无此要求（桥统一重编码）。

---

## 2. 构建与启动

| 构建期依赖 | 用途 |
| --- | --- |
| Rust (cargo) + C 工具链/cmake/NASM | 编译 relay（audiopus/fdk-aac/openssl-src 等 vendored C 库） |
| Node.js 18+（仅构建期） | 构建 React 前端，产物嵌入二进制 |

```bash
# 1) 前端（relay/web/dist 由 rust-embed 打进二进制）
cd relay/web && npm install && npm run build
# 2) 编译运行
cd .. && cargo run                  # 开发
cargo build --release               # 发布：target/release/relay 单文件即全部
```

开发期常用 `relay/dev.sh`（`kill` / `run` / `restart`）。
**Windows 包**：GitHub Actions `build-windows`（手动触发或推 `v*` tag），自动下载 BtbN ffmpeg
嵌入并产出 `relay.exe`。

浏览器打开 `http://<IP>:8000`（单页面）：WHEP 播放（自动播、断流自动重连、全屏按钮）+
录制条（选清晰度 → 开始/停止 → 下载片段）+ 分享二维码（手机扫码进直播）。

---

## 3. 数据与配置

均以**二进制所在目录**为基准，不随启动时的 CWD 变化：

- `config.json`：房间名、清晰度档、端口、`data_dir`。**改后重启生效**，前端无改配置入口（设计如此）。
- `data/sessions/<时间戳>/full.mp4`：每场直播的整场连续录制（fragmented mp4，h264 copy + aac），
  停播后保留一段时间供事后裁剪，过期自动清理；`ffmpeg.log` 为该场录制器日志。
- `data/clips/`：页面「录制」切出的成品 mp4（original 档秒级 `-c copy`，720p/480p 重编码）。
- `data/logs/`：`system`（运行日志）/ `user_ops`（推流、起停桥、录制、下载等操作）/
  `viewers`（进出直播间）按日滚动；`bridge-<app>-<stream>.log` 为桥 ffmpeg 的 stderr，
  **直播不通先看它**。
- 内置 ffmpeg 首次运行释放到 `$TMPDIR/relay-ffmpeg/`，被系统清理后自动重新释放。
- `data_dir` 可指到别处（绝对路径 / `~/…` / 相对二进制目录），例：`"data_dir": "/Volumes/ext/relay-data"`。

---

## 4. 目录结构

```
live/
├── README.md                   # 本文件（入口，保持与实现同步）
├── docs/
│   ├── RTMP-桥与录制.md         # ⭐ 现行专题：RTMP 桥（裸 RTP 注入）+ 直录，含踩坑记录
│   ├── REFACTOR-SINGLE-BINARY.md  # 历史：单二进制重构设计（部分细节已被后续演进取代）
│   ├── STAGE2-PLAN.md          # 历史：阶段二计划
│   └── legacy/                 # 历史：旧 SRS+backend 架构（已废弃）
├── relay/                      # ⭐ 单二进制主体
│   ├── build.rs                # 检测 vendor/ffmpeg/<平台>/ 有二进制则嵌入
│   ├── vendor/
│   │   ├── xwebrtc/            # 补丁版（动态 PT / 禁 mDNS / 仅 IPv4 / Connection:close / 404 …）
│   │   ├── streamhub/          # 补丁版（死 sender 清理 / recv None 防空转）
│   │   └── ffmpeg/<平台>/      # 静态 ffmpeg（gitignore 不入库；Windows 由 CI 放入）
│   ├── src/
│   │   ├── main.rs             # 装配 hub/RTMP/WebRTC/axum/桥/录制 + 优雅退出
│   │   ├── banner.rs           # 启动横幅（推流地址置顶，内网 IP）
│   │   ├── bridge.rs           # RTMP→WebRTC 桥：起停 ffmpeg + 每桥守护(child.wait)
│   │   ├── rtp_ingest.rs       # 收 ffmpeg 裸 RTP → 以 WebRTC 身份发布进 hub
│   │   ├── record.rs           # 整场直录 full.mp4 + 页面录制裁剪
│   │   ├── config.rs / web.rs / ffmpeg.rs / logging.rs
│   │   └── …
│   ├── web/                    # React+Vite 单页（构建到 web/dist 后嵌入）
│   └── push-*-whip.sh          # WHIP 推流测试脚本
├── app/                        # Flutter 客户端（暂不可用，见 §6）
└── .github/workflows/build-windows.yml   # Windows 原生构建（含 ffmpeg 嵌入）
```

---

## 5. 关键实现备注（改代码前必读）

- **RTMP→WebRTC 桥走「裸 RTP 注入」，不用 ffmpeg `-f whip`**：whip 的 DTLS-SRTP 在 Windows
  各家静态 ffmpeg（gyan=GnuTLS、BtbN=SChannel）上都无法与 webrtc-rs 握手。桥 ffmpeg 输出
  RTP 到本机 UDP，`rtp_ingest.rs` 收包发布进 hub——ffmpeg 只需 libx264+libopus。
  完整根因与验证见 [docs/RTMP-桥与录制.md](docs/RTMP-桥与录制.md)。
- **streamhub 只广播 `Publish`、从不广播 `UnPublish`**：任何「停发收尾」逻辑不能等事件，
  桥靠守护 task `child.wait()` 收尾（否则幽灵发布，二次推流撞 Exists）。
- **vendored 补丁勿被升级覆盖**（`[patch.crates-io]` 指向 `relay/vendor/`）：
  - `xwebrtc`：动态 payload type（上游硬编码 96 会丢弃 ffmpeg 协商的 106）；禁用 mDNS
    （Windows WSAEMSGSIZE 崩收包循环）；ICE 仅 IPv4 UDP（公网 IPv6 候选会 connected 后秒断）；
    HTTP 响应统一 `Connection: close`（一条连接只服务一个请求，keep-alive 复用会挂死播放页）；
    WHEP 订阅不存在的流回 404；上游断流即关订阅端 PC（触发播放端自动重连）。
  - `streamhub`：fanout 失败即清死 sender；三个接收循环 recv 到 `None` 必须 break
    （否则空转吃满一整核）。
- **任何 `loop { select! { x = rx.recv() } }` 都必须处理 `None`**——本项目已两次踩「通道关闭
  后空转 100% CPU」的坑（streamhub 接收循环、whep 转发循环）。
- **录制自动触发**：`stream_hub.set_hls_enabled(true)` 后 streamhub 才广播 `Publish` 事件
  （录制与桥都靠它）；副作用是 rtmp crate 的 relay 会 phantom 自转推，见专题文档「待修」。
- **优雅退出**：SIGINT/SIGTERM/SIGHUP（Windows: Ctrl+C/关窗口/注销/关机）→ 广播 shutdown →
  录制 ffmpeg 写完 moov 再退（最多 8s）。⚠️ 别用 `nohup`/`&`/`disown` 启动，关窗口不发信号会残留进程。
- **性能基线**（release，720p30 推流 + 1 观看，Apple Silicon 实测）：relay 本体 ~3% 单核、
  ~40MB 内存；桥 ffmpeg 重编码 ~30% 单核（与观看人数无关）；录制 ffmpeg（copy）~0.4%。

---

## 6. Flutter 客户端（app/）现状

`app/` 依赖旧 `backend/`（已删）的 `/api/login`、`/ws/danmaku` 等接口，relay 未提供，
登录/弹幕/切片功能当前不可用。待 relay 补齐后需把 `app/lib/config.dart` 的 `apiBase` 指向 relay。
（小程序端已冻结：HLS 延迟过高，不再维护。）

---

## 7. 排障速查

| 症状 | 先看 |
| --- | --- |
| 直播「等待推流」但录制正常 | `data/logs/bridge-*.log`（桥 ffmpeg 挂了）；system.log 找「RTP 注入 * 首包已到」 |
| 推流地址不确定 | 启动横幅（内网 IP + 房间名可直接复制） |
| 播放页黑屏/转圈 | system.log 的 WHEP/ICE 行；页面会自动重连，持续失败通常是流侧问题 |
| 端口被占 | 改 `config.json` 的 `ports`，重启 |
| 录制文件在哪 | 整场 `data/sessions/<id>/full.mp4`；页面切的片段 `data/clips/` |
