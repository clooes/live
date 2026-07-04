# RTMP 推流：桥接直播 + 直录录制

> 记录 relay 支持 OBS RTMP 推流的完整改造：为什么做、怎么做、已验证什么、还有什么坑没填。
> 涉及文件：`relay/src/{record.rs, bridge.rs, ffmpeg.rs, main.rs, banner.rs}`。

## 背景

relay 是纯 WebRTC 路线（WHIP 推 / WHEP 播），但 OBS 用 RTMP 更普遍。xiu 的 streamhub **按
`StreamIdentifier` 精确匹配、不做跨协议桥接** —— RTMP 推的流 WHEP 播不出、录制器也订不到。
为兼容「只会推 RTMP 的设备/软件」，加了一条 **RTMP→WebRTC 的服务端桥**，并把录制改成从 RTMP 直录。

## 架构总览

```
                       ┌─ 桥(bridge.rs)：ffmpeg 拉 RTMP → 重编码 H264 baseline + Opus
                       │                 → -f whip 回推本机 :8900 → WebRTC 流 → WHEP 直播 ✅
OBS ──单路 RTMP :1935──→│
                       └─ 录制(record.rs)：ffmpeg 直接拉 RTMP → -c copy 无损 → full.mp4 ✅
                                          （AAC 直拷，无 Opus/aresample/重编码 → 无哧哧底噪）
```

**OBS 只推一路 RTMP**，服务端分叉出「直播」和「录制」两路。不需要 OBS 多输出/插件，不翻倍编码。

OBS 填法（设置 → 直播）：服务=自定义；服务器=`rtmp://<IP>:1935/live`；串流码=`<房间名>`（默认 room001）。
**关键**：`/live` 留服务器栏，房间名单独放串流码栏，别拼一起。

## 一、录制录不出视频：SPS/PPS 修复（record.rs）

**现象**：ffmpeg.log 刷 `non-existing PPS 0 referenced` / `no frame!`，full.mp4 录不下来。

**根因**：喂 ffmpeg 的 H264 缺 PPS。旧逻辑只等 SPS(type 7) 就起步、且 SPS/PPS 只随起步一次性
RTP 转发，丢了就再等下一个关键帧（可能 5s）。

**修复**（仅影响 WHIP 直推那条 RTP 录制路）：
- `rtp_h264_has_sps` → `rtp_h264_params`：同时抽 SPS+PPS 原始 NAL 字节（单 NAL + STAP-A）。
- `build_sdp` 加 `sprop-parameter-sets=<b64 SPS>,<b64 PPS>`：解码头**带外**喂 ffmpeg，RTP 丢了也不怕。
- 预备阶段改成 **SPS+PPS 都收齐**才起 ffmpeg。
- 新增内联 `base64_encode`（不引三方 crate）。

✅ 已验证：`non-existing PPS` 归零，full.mp4 正常出视频+音频。

> 注：RTMP 直录路不经过这段（FLV 自带解码头），此修复只对 OBS 直接 WHIP 推流的场景有用。

## 二、RTMP→WHIP 桥（bridge.rs）

`spawn_rtmp_bridge` 监听 client-event，收到 `Publish{Rtmp}` 就起一路 ffmpeg：
`-i rtmp://127.0.0.1:1935/<app>/<stream>` → 重编码 `H264 baseline + zerolatency + repeat-headers`
+ `Opus 48k`（WebRTC 原生音频；RTMP 的 AAC 无法直进 WebRTC）→ `-f whip` 回推本机 :8900。
收到 `UnPublish{Rtmp}` 杀对应 ffmpeg。桥只认 Rtmp 身份，不成环。

### whip-capable ffmpeg 要求（重要坑）

`-f whip` 需要带 **whip muxer** 的 ffmpeg（依赖 DTLS/SSL，ffmpeg 8.1+）。现状：

| ffmpeg 来源 | whip |
|---|---|
| evermeet（macOS 静态，无 DTLS） | ❌ 无 |
| BtbN（Linux/Windows，Win 为 SChannel 后端） | ✅ 有（但**无 macOS 版**） |
| gyan.dev（Windows，GnuTLS 后端） | ⚠️ 有 muxer **但 DTLS 握手必失败**（见下） |
| homebrew 8.1+（arm64） | ✅ 有（动态链接，非单二进制） |

**⚠️ TLS 后端坑（2026-07-04 Windows 直播「等待推流」的根因）**：whip muxer 存在 ≠ 能用。
whip 的 DTLS 对端是 WebRTC **自签名证书**（按 SDP 指纹信任），whip.c 设了 `verify=0`，但
ffmpeg 各 TLS 后端行为不同：GnuTLS 的 DTLS 仍强走 CA 链校验 → 自签名直接
`Unable to verify peer certificate: The request is invalid.` 握手失败、桥 ffmpeg 退出（表象：
RTMP 侧刷 `pack error: bytes writer error: io error`，直播一直等待推流，录制正常）；
SChannel 在 verify=0 时不校验 → 能握手。所以 **Windows 只能用 BtbN，不能用 gyan.dev**
（gyan 是 GnuTLS）。CI（build-windows.yml）已改为拉 BtbN 并加了 `-buildconf` 含
`enable-gnutls` 即 fail 的防回归检查。

`ffmpeg::whip_path()` 探测：内置有 whip 就用内置，否则回退 PATH 的 ffmpeg，都没有则**禁用桥并告警**。
**macOS 上没有现成的「静态 + whip」build**，故当前 mac 决策：`vendor/ffmpeg/macos-arm64/` 留空，
桥回退 homebrew ffmpeg。Linux/Windows 打包时把 BtbN build 放进对应槽即可纯内置。

各平台 ffmpeg 槽位（build.rs 按编译目标平台选，目前只认 x64）：
- `macos-arm64/`：evermeet arm64（录制用，whip 不需要，桥走 homebrew 回退）
- `linux-x64/`：BtbN linux64（含 whip）
- `windows-x64/`：BtbN win64（含 whip）

✅ 已验证：OBS/ffmpeg 推 RTMP → 起桥 → WHIP 回推 → WHEP 直播正常。

## 三、录制音频哧哧声：改 RTMP 直录（record.rs）

**现象**：只有**录制文件**有连续哧哧底噪，直播（WHEP）干净。

**根因**：录制器对音频做了一串有损加工 —— Opus RTP 裸 UDP 转发（关键帧突发丢包）→ 解 Opus →
`aresample=async=1` 拉伸补样本 → 重编码 AAC。直播干净是因为 WebRTC 把 Opus 可靠送到浏览器解码。

**修复**：RTMP 源改成**直接从 RTMP 拉、`-c copy` 无损直录**（`session_recorder_rtmp`）。RTMP 走 TCP
可靠、FLV 自带 AAC+SPS/PPS，绕开整条加工链 → 哧哧声根除、音质无损、代码更简。

### 分流录制逻辑（spawn_monitor）

监听里按源分流，用 `rtmp_active` 标记本 room 是否有 RTMP 源：
- `Publish{Rtmp}` → RTMP 直录（`on_publish_rtmp` / `session_recorder_rtmp`）。
- `Publish{WebRTC}` → 若 `rtmp_active` 则**跳过**（那是桥的产物，仅供直播，避免录两遍）；
  否则走原 RTP 录制路（WHIP 直推源）。
- `UnPublish{Rtmp/WebRTC}` → 相应收尾。
- 抽出公共 `finalize_session`（清 session + 切挂起标记），RTP/RTMP 两路共用。

✅ 已验证：走 RTMP 直录、跳过桥 WebRTC、ffmpeg.log 无 aresample/PPS、音频 AAC 直拷。

## ⚠️ 已知问题（待修）：phantom RTMP 自转推 → 多录一个会话

**现象**：单次 RTMP 推流，收尾时会**多冒出一个会话**（full.mp4 目录 >1）。

**根因**：`rtmp` crate 的 `relay::push_client`（flash_ver `FMSc/1.0`）在收到
`BroadcastEvent::Publish` 时，会连到 `rtmp://localhost:1935/live` **把本地流回推到自己**（RtmpRelay
特性）。时序实测：
- 推流期间：phantom 每 ~2.5s 自推一次，都撞真流的「exists」被拒 → **不产生 Publish 事件**，无害。
- 真流一停的空档：某次 phantom 自推**成功** → 触发一个 `Publish{Rtmp}` → 录制器起了个**幽灵会话**。

> 注：这个自推早于 WebRTC 发布出现，**不是桥/WebRTC 触发的，是 RTMP 发布本身 + `hls_enabled`**。
> `PushClient` 由谁用 `address=localhost:1935` 实例化尚未最终定位（我们 main.rs 没显式建它，疑似
> streamhub/rtmp 在 hls 路径下内部起的；`set_hls_enabled(true)` 又是录制/桥拿 Publish 事件所必需，
> 不能简单关掉）。

**待实施的修复（二选一）**：
1. **监听层去抖（推荐，改动小、不碰 vendor）**：`spawn_monitor` 里
   - `Publish{Rtmp}` 若 `rtmp_active` 已 true → 忽略（已在录，去重）；
   - 记 `UnPublish{Rtmp}` 时刻，其后 ~4-5s 内到来的 `Publish{Rtmp}` 视为 phantom/抖动 → 忽略。
   代价：OBS 断线 5s 内快速重连会跳过一次重录（边缘情况，可接受）。
2. **根治**：vendor `rtmp` 或 streamhub，禁掉自推到 localhost 的 relay，或让它不发 Publish 事件。
   改动大、碰 vendor，暂不做。

## 四、启动地址横幅（banner.rs）

启动横幅把两个推流地址置顶，用**内网 IP**（`local_ip_address::local_ip()`，推流多来自另一台设备）+
真实房间名，可直接复制。取不到 IP 退回 localhost。房间名跟随 `config.json` 的 `room`。

## 当前验证状态小结

| 项 | 状态 |
|---|---|
| RTMP→WHIP 桥直播 | ✅ 通 |
| RTMP 直录、音频 AAC 无损、无哧哧 | ✅ 通 |
| SPS/PPS 修复（WHIP 直推录制路） | ✅ 通 |
| 桥挑 whip-capable ffmpeg + 回退 | ✅ 通 |
| phantom 自转推 → 多录一个会话 | ⚠️ 待修（见上，方案 1 去抖） |

## 未提交

以上改动均在工作区，未 commit。
