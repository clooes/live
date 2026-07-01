# 迭代 TODO：桌面主播推流端 + 多租户 + 后端持久化

> 配套文档：详细阶段记录在 [PLAN.md](PLAN.md)（迭代二 P1-P4）；
> 桌面端设计见 [STREAMER.md](STREAMER.md)；数据模型见 [MULTI-TENANT.md](MULTI-TENANT.md)。
> 勾选方式：完成一项把 `- [ ]` 改成 `- [x]`，并在 PLAN.md 对应阶段补「踩坑记录」。

## 进度概览

| 阶段 | 目标 | 状态 | 独立验收 |
|------|------|------|---------|
| P1 | 桌面端打通单流 WHIP（沿用 room001） | ✅ 已完成并验证 | 枚举摄像头→预览→推流→frontend 能看→DVR 录到 MP4 |
| P2 | SRS opus + 多租户数据模型 | ⬜ 未开始 | 云端建流拿 streamKey→桌面推→独立录制目录 + DVR 有 aac |
| P3 | 后端持久化（SQLite） | ⬜ 未开始 | 推流+裁剪后重启 backend，jobs/会话仍在 |
| P4 | 观众端 + 二维码闭环 | ⬜ 未开始 | 扫码→手机浏览器→按 streamKey 看到桌面推的流 |

图例：⬜ 未开始 / 🟡 进行中 / ✅ 已完成并验证

---

## P1 — 桌面端打通单流 WHIP

> 先不动多租户，沿用现有 `app=live&stream=room001`，backend / SRS / cloud 全部不改。
> **风险标注**：本阶段最大不确定点是桌面端 WebRTC 默认可能 offer VP8，导致 SRS `rtc_to_rtmp` 失败（RTMP 只认 H264）。**优先用 `chrome://webrtc-internals` 等价手段或抓 SDP 实测，必要时 munge SDP 把 H264 排首位。**

### 工程脚手架
- [x] `flutter create --platforms=macos,windows streamer/`（新建独立工程，不动 `app/`）
- [x] `streamer/pubspec.yaml` 加依赖：`flutter_webrtc ^0.12.5`、`http`、`qr_flutter`、`network_info_plus`、`web_socket_channel`
- [x] macOS 权限：`macos/Runner/DebugProfile.entitlements` + `Release.entitlements` 加 `com.apple.security.device.camera`、`.device.audio-input`、`.network.client`、`.network.server`
- [x] macOS `Info.plist` 加 `NSCameraUsageDescription`、`NSMicrophoneUsageDescription`

### 核心模块
- [x] `streamer/lib/config.dart`：backend/SRS host（默认 `localhost`，可改内网 IP）
- [x] `streamer/lib/devices.dart`：先 `getUserMedia({video,audio})` 触发授权 → `enumerateDevices()` 过滤 `videoinput`/`audioinput` 建下拉
- [x] `streamer/lib/preview.dart`：`RTCVideoRenderer` + `RTCVideoView`，`srcObject = localStream`，`mirror: true`
- [x] `streamer/lib/whip_pusher.dart`：`addTransceiver(SendOnly)` + `addTrack` + `createOffer` → POST SDP(`application/sdp`) 到 `/rtc/v1/whip/?app=live&stream=room001` → `setRemoteDescription`，保存 `Location` 头
- [x] `whip_pusher.dart`：**强制 H264 在前**（SDP munge：`_preferH264` 把 video m-line 的 H264 payload 排首位）
- [x] `whip_pusher.dart`：推流状态机 `idle→connecting→live→reconnecting→ended/error` + 断流 3s 重连
- [x] 切设备用 `RTCRtpSender.replaceTrack()` 热切换不断流
- [x] `streamer/lib/main.dart`：设备下拉 + 预览 + 开播/停播按钮（最小可用面板）
- [x] `flutter analyze` 通过（零问题）

### P1 验收点（连 SRS 实机运行验证）
- [x] 桌面下拉出现 `FaceTime高清相机` + 麦克风（替代 `push-camera.sh` 写死索引）
- [x] 选设备后 `RTCVideoView` 显示本地画面
- [x] 开播 → SRS 日志 `Publisher established` + `on_publish ok` + DTLS done
- [x] FLV `http://192.168.1.10:8080/live/room001.flv` 可拉（H264 High + AAC）
- [x] frontend（现有页面）能播放（浏览器 http://localhost:8000/ WHEP 拉流）
- [x] 停播 → `data/recordings/live/room001/*.mp4` 完整：**45MB / 211s / H264 1280x720 全程恒定 / AAC 48k 立体声**

### P1 踩坑记录（实测）
- ⚠️ **macOS App Sandbox 拒绝沙箱应用连 `127.0.0.1`**（Connection refused errno 61）→ `Config.host` 必须用内网 IP，SRS candidate 同步改内网 IP。
- ⚠️ **WHIP 推流 SDP 方向**：`addTransceiver(SendOnly, 不带 track)` + `addTrack` 混用会产生 inactive m-line，SRS 报 `publish API only support sendrecv/sendonly`。改为纯 `addTrack`（sendrecv）解决。
- ⚠️ **重连指数风暴**：`catch` 与 `onConnectionState` 各自 `Future.delayed` 重连 → 一裂二指数增长，撞 `RtcStreamBusy`。改单一 `Timer` + `_connectInFlight` 守卫。
- ⚠️ **SRS hook 依赖 backend**：`on_publish` 连不上 backend 会直接拒绝推流（code 1018），backend 必须先于推流存活。
- ⚠️ **WebRTC 自适应改分辨率 → DVR 中断**：约 30s 后编码器降分辨率，SRS DVR 报 `Mp4AvccChange`（MP4 不支持视频 AVCC 变化），录制残缺。修法：`whip_pusher.dart::_lockVideoResolution` 给视频 sender 设 `degradationPreference=MAINTAIN_RESOLUTION` + `maxBitrate=2Mbps` + `scaleResolutionDownBy=1.0`。实测 3.5 分钟全程 1280x720 恒定、无中断。
- ✅ **意外利好**：本 SRS 镜像 WHIP 音频直出 **AAC**（DVR 实测有 AAC 48k 音轨），P2 的 opus 改造可能非必需，待确认。

### P1 完成状态：✅ 端到端验证通过（2026-06-30）
摄像头枚举 → 预览 → WHIP 推流(H264) → 直播观看(FLV/WHEP) → DVR 录制(720p 恒定+AAC) 全链路打通，全程无 OBS。

---

## P2 — SRS opus + 多租户数据模型

> 数据模型详见 [MULTI-TENANT.md](MULTI-TENANT.md)；room001 参数化清单同文档。

### SRS opus
- [ ] `srs/srs.conf` / 镜像：换带 ffmpeg-opus 的 SRS 或自编译，去掉 `--ffmpeg-opus=off`
- [ ] 验证 WHIP 的 Opus → AAC 能转码进 DVR/FLV（无现成镜像 tag 需预留编译时间）

### cloud 多租户（PG）
- [ ] `cloud/src/db.rs`：新增 `merchants`、`devices`、`streams` 表 + `nodes.merchant_id` 外键
- [ ] `cloud/src/admin.rs`：商家/门店/设备/流 CRUD + 生成 streamKey（短 ID 如 `s_a1b2c3`）
- [ ] `cloud/src/nodes.rs::get_config`：扩展返回该 node 的 `streams` 数组
- [ ] `cloud-admin/`：管理后台加流管理界面（可选，本阶段可先用 API）

### backend 按 streamKey 参数化（去 room001 写死）
- [ ] `backend/src/state.rs:14-15`：删 `const APP/STREAM`，运行时按 streamKey 传递
- [ ] `backend/src/state.rs`：`AppState.stream` 单例 → `streams: HashMap<key, StreamState>`
- [ ] `backend/src/hooks.rs`：on_publish/on_unpublish/on_dvr 解析 body `stream` 字段路由
- [ ] `backend/src/handlers.rs:107/112/222`：clip 流程 / HLS 路径按请求所属 streamKey
- [ ] `backend/src/clip.rs:20`：`hls_dir()` → `hls_dir(app, key)` 参数化
- [ ] `backend/src/report.rs:107`：遍历本节点所有流上报
- [ ] backend 新增 `GET /api/streamer/profile` 返回 `{streamKey, app, whipUrl, viewUrl}`
- [ ] 桌面端 `whip_pusher.dart`：改为登录后拉 streamKey，不再写死 room001

### P2 验收点
- [ ] 云端为某门店建流拿到 streamKey → 桌面端拉到并推流
- [ ] 该 streamKey 独立录制目录 `data/recordings/live/<key>/`
- [ ] `ffprobe` 确认 DVR MP4 **含 aac 音轨**（opus 转码生效 = 本次核心改进）

---

## P3 — 后端持久化（SQLite）

- [ ] `backend/Cargo.toml`：引入 `sqlx`（sqlite feature）
- [ ] 建表：`stream_sessions`、`clip_jobs`、`streams`（缓存）、`sessions`
- [ ] `AppState.current_mark`（单例）→ `marks: HashMap<(key,phone), Mark>`（多流多用户）
- [ ] `AppState.danmaku`（单 Sender）→ `HashMap<key, Sender>`（弹幕分房间）
- [ ] `AppState.jobs: Vec` → SQLite 持久 + 内存索引
- [ ] 内存态写穿持久层；启动时从 SQLite 恢复
- [ ] 未配 `CLOUD_URL` 时从本地 SQLite 读（首启 seed 默认流兼容旧行为）

### P3 验收点
- [ ] 推流 + 裁剪后重启 backend，`/api/clips` 列表与 jobs 状态仍在
- [ ] 多用户在不同流标记录制互不串

---

## P4 — 观众端 + 二维码闭环

- [ ] `frontend/src/hooks/usePlayer.ts:30-31`：从 `location.search` 读 `?stream=<key>`（缺省回退默认流），WHEP/FLV URL 用该 key
- [ ] `frontend` `App.tsx`/`Player.tsx`：透传 streamKey，标题显示流标题
- [ ] `app/lib/config.dart:15`：whepUrl 加 streamKey 参数
- [ ] `app/lib/main.dart:213`：标题去写死 room001，显示流标题
- [ ] `streamer/lib/viewer_qr.dart`：`network_info_plus` 取内网 IP → 拼 `http://<lan-ip>:8000/?stream=<key>` → `qr_flutter` 出码
- [ ] `streamer/lib/main.dart`：面板展示二维码 + （可选）`danmaku_panel.dart` 显示观众弹幕
- [ ] `miniprogram/` 冻结，不改

### P4 验收点
- [ ] 桌面二维码 → 手机浏览器打开 → frontend 按 streamKey 播放到桌面推的流
- [ ] 观众登录 → 录制 start/end → `/api/clips` 出现该流片段 → `/clips/xxx.mp4` 下载，`ffprobe` 时长正确
- [ ] 多租户隔离：云端建第二 streamKey，第二桌面端推流，两流目录/jobs/弹幕互不串

---

## 端到端联调（全部阶段完成后）

```bash
cd cloud && docker compose up -d && cargo run                       # PG + 云端:9000
cd srs && CANDIDATE=$(ipconfig getifaddr en0) ./start-srs.sh        # opus 镜像
cd backend && CLOUD_URL=http://localhost:9000 NODE_NAME=门店A cargo run
cd frontend && npm run build                                        # backend 托管
cd streamer && flutter run -d macos                                 # 桌面主播端
```

- [ ] 走通 P1→P4 全部验收点
- [ ] 重启 backend 后 jobs 与历史会话仍在
