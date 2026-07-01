# 桌面主播推流端设计（streamer/）

> 三层架构里的「第2层 推流平台」面向主播的客户端。取代手动 OBS / `srs/push-camera.sh`：
> 枚举摄像头 → 采集 → 本地预览 + WHIP 推流 → 起内网观看页二维码。
> 任务清单见 [TODO.md](TODO.md) P1/P4；数据模型见 [MULTI-TENANT.md](MULTI-TENANT.md)。

---

## 1. 技术选型

### 1.1 推流方式：flutter_webrtc + WHIP（不用 OBS）

| 维度 | flutter_webrtc + WHIP（选用） | 内置 ffmpeg 推 RTMP（备选） |
|------|------------------------------|----------------------------|
| 摄像头枚举 | `navigator.mediaDevices.enumerateDevices()` | `ffmpeg -list_devices`（解析文本，平台各异） |
| 本地预览 | `localStream` 同时喂 `RTCVideoView` + 推流，**一次采集两用** | ffmpeg 独占设备，预览要回吐/抢设备 |
| 延迟 | 亚秒（WebRTC） | 1~3s |
| 分发 | 纯 Dart，无需打包二进制 | 三平台各打包 ffmpeg |
| 下游链路 | SRS `rtc_to_rtmp` 转回 RTMP，**复用现有 DVR/HLS/裁剪/FLV** | 直进 RTMP，链路一致 |

**结论：选 WHIP。** 唯一代价是 SRS 镜像 `--ffmpeg-opus=off` 导致 Opus 音频不进 DVR——P2 阶段换开启 opus 的 SRS 解决。ffmpeg/RTMP 作为高码率高级场景的降级备选，保留 `push-camera.sh`。

### 1.2 为何新建独立工程，而非给 `app/` 加桌面平台

- 角色与依赖相反：`app/` 是**观众端**（WHEP recvonly + video_player + url_launcher）；主播端是**采集/预览 + WHIP sendonly + 二维码 + 本地 IP 探测**。复用面小、冲突面大（`config.dart` host、`main.dart` 路由、权限声明都要分叉）。
- `app/` 当前只有 `android/ios`，无 desktop runner；加桌面要 `flutter create --platforms=macos .` 注入 runner，会污染观众端工程并增大构建矩阵。
- 复用方式：把 `app/lib/api.dart`、`config.dart` 的 HTTP/host 封装**模式照搬**进 `streamer/`（量小，不抽 shared package）。

---

## 2. 工程结构

```
streamer/
├── pubspec.yaml          # flutter_webrtc ^0.12.5, http, qr_flutter, network_info_plus, web_socket_channel
├── macos/  windows/      # flutter create 注入；macos 补 entitlements + Info.plist 权限
└── lib/
    ├── config.dart       # backend/SRS host（默认 localhost，可改内网 IP）
    ├── api.dart          # 复用 app/lib/api.dart 模式：登录、拉 streamKey、拉在线人数
    ├── models.dart       # StreamProfile{streamKey, app, whipUrl, viewUrl}, ClipJob
    ├── devices.dart      # 摄像头/麦克风枚举 + 选择
    ├── whip_pusher.dart  # 核心：getUserMedia + sendonly PC + WHIP 信令 + 状态机/重连
    ├── preview.dart      # RTCVideoView 本地预览
    ├── viewer_qr.dart    # 内网观看 URL + qr_flutter 二维码
    ├── danmaku_panel.dart# 复用 backend /ws/danmaku 显示观众弹幕
    └── main.dart         # 主播面板：设备下拉 + 预览 + 开播/停播 + 二维码 + 在线人数/弹幕
```

依赖：`flutter_webrtc ^0.12.5`、`http`、`qr_flutter`、`network_info_plus`、`web_socket_channel`。

---

## 3. WHIP 推流信令时序（核心）

SRS WHIP 端点：`http://<host>:1985/rtc/v1/whip/?app=live&stream=<streamKey>`
——与观众端 WHEP `http://<host>:1985/rtc/v1/whep/?app=live&stream=<key>`（`frontend/usePlayer.ts:30`、`app/lib/webrtc_player.dart:92`）**对称**，仅方向相反。

信令流程（镜像 `app/lib/webrtc_player.dart:79-100`，把 RecvOnly 改 SendOnly、whep 改 whip）：

```
1. getUserMedia({video:{deviceId}, audio:{deviceId}})  → localStream
2. pc = RTCPeerConnection({iceServers: []})            // 内网直连，无需 STUN/TURN
3. pc.addTransceiver(video, direction: SendOnly)
   pc.addTransceiver(audio, direction: SendOnly)
   pc.addTrack(videoTrack, localStream)                // 同一 localStream 既预览又推流
   pc.addTrack(audioTrack, localStream)
4. 【强制 H264 在前】 setCodecPreferences 或 SDP munge
5. offer = pc.createOffer(); pc.setLocalDescription(offer)
6. POST offer.sdp → WHIP URL  (Content-Type: application/sdp)
7. 200/201 → answer = resp.body; pc.setRemoteDescription(answer)
   保存 resp 的 Location 头（WHIP 资源 URL，停播时 DELETE）
```

停播：
```
sender.replaceTrack(null) → HTTP DELETE <Location> → pc.close() → localStream.getTracks().forEach(stop)
```
正常 `pc.close()` + WHIP DELETE 会触发 SRS `on_unpublish` 回调 backend（实现时实测 SRS 是否要求显式 DELETE）。

对照现有 WHEP POST（`app/lib/webrtc_player.dart:91-100`）：
```dart
final resp = await http.post(Uri.parse(Config.whepUrl),
    headers: {'Content-Type': 'application/sdp'}, body: offer.sdp);
if (resp.statusCode == 200 || resp.statusCode == 201) {
  await pc.setRemoteDescription(RTCSessionDescription(resp.body, 'answer'));
}
```
WHIP 版只需把 URL 换成 whip、transceiver 换 SendOnly、并加 H264 强制与 Location 保存。

---

## 4. 设备枚举与采集

```
1. 启动先 getUserMedia({video:true, audio:true}) 触发系统授权
   （未授权前 enumerateDevices 返回的 label 为空，拿不到设备名）
2. enumerateDevices() → 过滤 kind == 'videoinput' / 'audioinput' → 建下拉
3. 选定设备：getUserMedia({video:{deviceId}, audio:{deviceId}})
   deviceId 写法桌面端有差异，需实测 {deviceId: id} 与 {'optional':[{'sourceId':id}]} 两种
4. 切换设备（推流中不断流）：sender.replaceTrack(newTrack)
```
这替代了 `srs/push-camera.sh:13` 写死的 `VIDEO=0 AUDIO=1` 硬编码索引。

**支持范围**：凡是操作系统识别为视频输入设备（UVC/video input）的摄像头都能枚举到——内置摄像头、**USB 外接摄像头**、**采集卡**（HDMI/SDI 接专业相机）、虚拟摄像头均可。底层 macOS 走 AVFoundation、Windows 走 Media Foundation/DirectShow。
**不支持**：IP/监控摄像头（RTSP/ONVIF，如海康/大华）——它们是网络流而非本机设备，`getUserMedia` 看不到。若将来需接入，应在服务端用 ffmpeg/SRS 拉 RTSP 转推（`ffmpeg -i rtsp://... -f flv rtmp://srs/live/<key>`），不走桌面端 WebRTC 路径。**当前场景以 USB/内置摄像头为主，暂不实现 RTSP 接入。**

---

## 5. 本地预览（一次采集两用）

```
RTCVideoRenderer renderer; await renderer.initialize();
renderer.srcObject = localStream;          // 同一 localStream，既此处预览，又 addTrack 推流
RTCVideoView(renderer, objectFit: ...Contain, mirror: true)
```
渲染参考观众端 `app/lib/webrtc_player.dart:163`，区别：source 是**本地** localStream（非 ontrack 远端流），且前置摄像头 `mirror: true`。

---

## 6. 内网观看页 + 二维码

**不自建 HTTP 服务**——观看页就是 backend 已托管的 `frontend/dist`（`backend/src/main.rs:62` 的 ServeDir fallback）。桌面端只做：
```
1. network_info_plus 取本机内网 IP（或读 config 里 backend host）
2. 拼 viewUrl = http://<lan-ip>:8000/?stream=<streamKey>
3. qr_flutter 渲染 viewUrl 二维码
观众扫码 → 手机浏览器打开 frontend → 按 ?stream=<key> 拉流（见 P4）
```
若主播机与 backend 不同机，viewUrl 指向 backend 机 IP。

---

## 7. 推流状态机 / 重连

```
idle → connecting → live → reconnecting → ended / error
```
复用 `app/lib/webrtc_player.dart:67-76` 的 `onConnectionState` 重连模式：`failed`/`disconnected` 时 3s 后重连（推流版重新 getUserMedia + 重建 PC）。

在线人数：新增 backend `GET /api/stream/:key/stats`（从 SRS HTTP API `/api/v1/clients` 拉该流播放数，或 backend 自计）。弹幕：复用 `/ws/danmaku`（`backend/src/danmaku.rs`），P3 起按 streamKey 分房间后桌面面板展示。

---

## 8. 已知坑清单（实现前务必处理）

| 坑 | 说明 | 应对 |
|----|------|------|
| **H264/VP8 协商** | 桌面 flutter_webrtc 默认可能 offer VP8，SRS rtc_to_rtmp（RTMP 只认 H264）失败 | setCodecPreferences 把 H264 排首位，或 munge SDP；P1 优先实测 |
| **macOS 权限** | 缺权限 getUserMedia 直接抛异常 | `macos/Runner/*.entitlements`(Debug+Release) 加 `com.apple.security.device.camera`/`.device.audio-input`/`.network.client`/`.network.server`；`Info.plist` 加 `NSCameraUsageDescription`/`NSMicrophoneUsageDescription` |
| **明文 HTTP** | 桌面端访问内网 `http://` SRS/backend | macOS 加 `com.apple.security.network.client` entitlement |
| **Opus 音频** | sendonly 音频是 Opus，SRS `--ffmpeg-opus=off` 时不进 DVR | P2 换开启 opus 的 SRS 镜像/自编译 |
| **WHIP 收尾** | 不显式 DELETE 可能 on_unpublish 不触发 | 停播走 `pc.close()` + DELETE Location，实测 SRS 行为 |
| **设备 deviceId 写法** | 桌面端与 Web/移动端实现有差异 | 实测 `{deviceId:id}` 与 `{'optional':[{'sourceId':id}]}` |

---

## 9. 保留 OBS 推流（可选高级方式，并存不冲突）

桌面端 WHIP 是**默认/主力**推流方式（延迟最低、零配置、USB 摄像头即插即用）；**OBS 推流作为可选高级方式保留**，二者共用同一套 SRS，无需改代码即可并存。

**何时用 OBS**：需要多机位切换、台标/字幕/贴图叠加、画中画、稳定超高码率等**直播制作**能力时——这些 WebRTC 单源做不到。日常单摄像头直播用桌面端 WHIP 即可。

**WHIP vs OBS 权衡**：

| | 桌面端 WHIP（默认） | OBS → RTMP（可选） |
|---|---|---|
| 端到端延迟 | ~200~500ms（全程 WebRTC，无 GOP 缓冲） | ~1~3s（RTMP/TCP + GOP 缓冲，采集端那段省不掉） |
| 制作能力 | 单摄像头 | 多机位/叠加/场景切换/高码率 |
| 音频进 DVR | Opus，需 SRS 开 opus（P2） | AAC 直推，无障碍 |
| 主播操作 | 打开即枚举、一键开播 | 装 OBS + 手填推流地址 |

**OBS 在多租户下的用法**：OBS 推到 `rtmp://<host>:1935/live/<streamKey>`，流名用云端分配的 streamKey（不再是写死的 room001）。backend `hooks.rs` 按 body 的 `stream` 字段路由，OBS 与 WHIP 走完全相同的下游链路（DVR/HLS/裁剪/分发），录制、裁剪、观看页扫码全部通用。`srs/push-camera.sh` 同理保留为命令行降级推流（加 `STREAM` 环境变量传 streamKey）。
