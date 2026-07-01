# 实施计划（分步骤，每步带验证）

主线：先跑通「直播结束后用 DVR 完整 MP4 裁剪」。HLS 实时裁剪只留接口。
每一步都要能独立验证，避免一次写完无从调试。

---

## 阶段 0：文档 ✅（已完成）

- [x] README.md（原理 / 架构 / 启动 / FAQ / 验收）
- [x] docs/DESIGN.md（数据结构 / API / 时间对齐 / 第二阶段）
- [x] docs/PLAN.md（本文件）
- [x] 目录骨架 + data/ 子目录

---

## 阶段 1：SRS 跑起来 + 能播直播 ✅ 已完成并验证

**产出**
- `srs/srs.conf`（http_remux / dvr / hls / http_hooks，路径用容器内 `/data`，hook 指向 `host.docker.internal:8000`）
- `srs/start-srs.sh`（docker run，挂载 `./data:/data` 与 `srs.conf`）
- `srs/push-camera.sh`（用 mac 自带摄像头/麦克风推流，替代 OBS）
- `.gitignore`（忽略 `data/`、`backend/target/`）

**验证（用 testsrc 测试图案 + 临时 mock 后端，已通过）**
1. `./srs/start-srs.sh` → SRS 正常监听 1935/8080/1985
2. ffmpeg 推 20s 测试流 → 推流成功建立（on_publish 放行）
3. 拉 `http://localhost:8080/live/room001.flv` → 探测到 h264 1280x720 + aac ✅
4. 停流后 `data/recordings/live/room001/*.mp4` 时长 20.038s ✅
5. hook 完整触发：on_publish → on_dvr → on_unpublish ✅
6. HLS 切片也已生成（`data/hls/live/room001/{live.m3u8, *.ts}`）✅

**踩坑记录（影响后续）**
- ⚠️ 宿主机 **3000 端口被 Docker Desktop 占用**，后端端口改为 **8000**（已同步所有文档/配置）。
- ⚠️ SRS hook 要求响应带 **Content-Length**，否则读响应超时拒绝推流。Rust/axum 自动满足；
  仅临时 python mock 需手动加。
- ⚠️ 后端未启动时 on_publish 失败 → **SRS 拒绝推流**。故摄像头/OBS 端到端推流需先起后端（阶段2）。

> 此阶段不依赖后端；hook 回调失败不影响推流（SRS 会重试/忽略）。

---

## 阶段 2：Rust 后端骨架 + Hook ✅ 已完成并验证

**产出**
- `backend/Cargo.toml`（axum/tokio/serde/tower-http/chrono/uuid/tracing/anyhow）
- `backend/src/state.rs`（AppState / StreamState / ClipJob / 工具函数 now_unix_ms / map_container_path / project_root）
- `backend/src/main.rs`（axum 启动、hook 路由、/clips 与 / 静态文件挂载，监听 8000）
- `backend/src/hooks.rs`（on_publish / on_unpublish / on_dvr，用 String body 手动解析避开 Content-Type 限制，含容器→宿主机路径映射）

**验证（已通过）**
1. `cargo build` 通过；`cargo run` 监听 8000
2. 手动 curl 三个 hook → 均返回 `{"code":0}`，on_dvr 路径 `/data/...` 正确映射为宿主机绝对路径
3. 真实推流：后端按序收到 SRS 回调 on_publish(记录 T0) → on_dvr(file_path) → on_unpublish
4. 真后端在线时推流被放行，直播可播、DVR/HLS 正常生成

**实现要点（供阶段3 衔接）**
- hook 用 `body: String` + `serde_json::from_str` 解析，不用 Json extractor（SRS 的 Content-Type 不保证 application/json）。
- `on_dvr` 已记录 `stream.file_path`（宿主机绝对路径），阶段3 在此处遍历 Pending 任务调用 ffmpeg。
- `ClipJob/JobStatus/ClipMark` 已定义（`#![allow(dead_code)]`），阶段3 启用。

---

## 插曲：播放与延迟优化（计划外，已完成）

阶段2 后用户要求降低播放延迟，提前做了播放相关工作（属阶段4 范畴）：

**产出**
- `frontend/index.html`：WebRTC(WHEP) 默认 + HTTP-FLV 一键切换 + 追帧按钮（阶段4 在此页加录制 UI）
- `srs/srs.conf`：新增 `rtc_server`(UDP 8000 + candidate) 与 vhost `rtc { rtmp_to_rtc on; rtc_to_rtmp on; }`
- `srs/start-srs.sh`：新增 `-p 8000:8000/udp`
- `srs/push-camera.sh`：GOP `-g 60→-g 15`(0.5s) + `-bf 0` 降延迟

**验证（已通过）**
- WebRTC 播放：WHEP 返回合法 answer SDP，SRS 实发 RTP（spkts rtp:794），媒体通路 OK
- WHIP 推流：ICE/SDP 交换成功、on_publish 触发、rtc_to_rtmp 录到 11MB 媒体到 DVR
- FLV 跨域：SRS 默认 `Access-Control-Allow-Origin: *`

**已知限制 / 待办**
- ⚠️ WHIP 端到端「正常停止→DVR finalize→on_dvr」需 **OBS 实测**（ffmpeg whip muxer 实验性，无法正常收尾）
- ⚠️ candidate 写死 `192.168.1.20`，换网段须改并重启
- ⚠️ 音频 opus 转码受限（镜像 `--ffmpeg-opus=off`），可能仅视频
- 延迟：RTMP 路线 ~1s；WHIP 全程 WebRTC <300ms

---

## 阶段 3：裁剪主线（DVR MP4）✅ 已完成并验证

**产出**
- `backend/src/clip.rs`（异步 `run_ffmpeg_clip`、`process_job` 跑任务+更新状态、`human_size`、`clip_from_hls` 占位）
- `backend/src/handlers.rs`（clip_start / clip_end / clip_status / clips）
- `main.rs` 挂 4 条用户 API 路由；`hooks.rs::on_dvr` 处理所有 Pending 任务

**验证（已通过，端到端脚本）**
1. 推流中 `POST /api/clip/start` → `start_offset=3.933` ✅
2. `POST /api/clip/end` → `pending`（直播中无 DVR）✅
3. 停流 → `on_dvr` 自动裁剪 → 任务 `done`，`data/clips/` 出现 MP4 ✅
4. `GET /api/clip/status/:id` → download_url + file_size(194.4 KB) ✅
5. `GET /api/clips` 列表正常；`GET /clips/xxx.mp4` 下载成功 ✅
6. ffprobe 时长 7.0s（标记 6.0s）——`-c copy` 关键帧对齐误差 ~1 GOP，符合预期

**踩坑**
- ⚠️ `StreamBusy`：上一推流会话(尤其 WHIP/RTC)未释放会拒绝新推流，重启 SRS 或换流名可解。
- 时长精度：要精确改 output seek（`-ss` 放 `-i` 后）或重编码，牺牲速度，Demo 不做。

---

## 阶段 4：前端页面

**产出 ✅**
- `frontend/index.html` 完整版：
  - ✅ 播放：WebRTC/FLV 切换 + 追帧
  - ✅ 录制：开始/停止按钮(绿/红切换) + 计时器 + 结束后轮询 status
  - ✅ 片段列表：表格(时间/时长/大小/状态/下载)，每 5s 自动刷新，done 显示下载链接
  - API 同源(8000)，相对路径调用

**验证（后端侧已通过，浏览器交互待用户实测）**
1. ✅ 页面含录制 UI 元素，后端正确托管
2. ✅ `/api/clips` 返回完整字段；`/clips/*.mp4` 下载 HTTP 200
3. ✅ start/end/status/clips API 全链路已在阶段3 验证
4. ⬜ 浏览器端：点开始→计时→停止→列表/下载（标准 DOM+fetch，待肉眼验收）

---

## 阶段 6：直播中实时裁剪（HLS）✅ 已完成并验证

原计划第二阶段，应需求提前实现。

**产出**
- `clip.rs::clip_from_hls`：解析 m3u8 时间轴 → concat 覆盖区间 .ts → 精修
- `clip.rs::clear_hls` + `on_publish` 调用：清空上场切片，时间轴对齐本场 T0
- `clip.rs::process_job_hls`；`handlers.rs::clip_end` 按 `status==Live` 分流
- `srs.conf` HLS：`hls_window 7200` / `hls_cleanup off` / `hls_dispose 0` 全程保留切片

**验证（已通过）**
- 直播进行中 start→end → `processing`（不再 pending）
- 轮询 → `done`，**全程未停推流**（活跃流=1），下载成功
- 时长 6.3s（标记 8s）：末端 ~2s 在未完成 fragment 内，属实时裁剪固有限制

---

## 阶段 7：裁剪清晰度切换 ✅ 已完成并验证

在「生成片段时」选清晰度（不影响直播播放/延迟）。

**产出**
- `clip.rs::quality_args`：original=`-c copy`；720p/480p=`-vf scale=-2:H -c:v libx264 -crf 23`
- DVR 与 HLS 两条裁剪路径均按 quality 编码；`ClipJob` 增 `quality` 字段
- API：`clip_end?quality=original|720p|480p`
- 前端：录制区清晰度下拉 + 列表「清晰度」列

**验证（已通过，直播中 HLS 源）**
- original → 1280×720 / 114.9 KB（不重编码）
- 720p → 1280×720 / 87.8 KB（源即 720p，分辨率不变、码率降）
- 480p → **854×480** / 85.1 KB（缩放重编码生效）

---

## 阶段 8：前端 React 重构（响应式）✅ 已完成并验证

将单文件 index.html 重构为 Vite + React 工程，响应式适配 PC/手机。

**产出**
- `frontend/`：Vite + React 18；`package.json` / `vite.config.js`（dev 代理 /api、/clips → 8000）
- 组件：`Player`（usePlayer hook：WebRTC/FLV/追帧）、`RecordBar`（录制+计时+清晰度）、`ClipList`（卡片网格+轮询+自动刷新）
- `styles.css`：移动优先响应式（手机单列、PC 多列卡片、安全区适配）
- 后端 `frontend_dir` 改为 `frontend/dist`；`.gitignore` 加 node_modules/dist

**验证（已通过）**
- `npm run build` 成功（JS 305KB / gzip 88KB）
- 后端托管 dist：`/` 返回含 `#root` + assets，`/assets/*.js` HTTP 200
- API、静态下载不受影响

**启动**
- 开发：`npm run dev`（5173，热更新，代理到 8000）
- 生产：`npm run build` → 后端 8000 统一托管

---

## 阶段 9：前端 TypeScript 化 ✅ 已完成并验证

**产出**
- `tsconfig.json` + `tsconfig.node.json`（strict）；`vite.config.ts`
- `src/types.ts`：ClipJob / ClipsResp / StartResp / EndResp / StatusResp / Quality
- 全部 `.js/.jsx` → `.ts/.tsx`；`api.ts` 泛型化；`usePlayer` 返回 `PlayerApi`；
  组件 props/ref 类型化（`ClipListHandle`、`RecordBarProps`）
- flv.js 用自带 d.ts（`MediaDataSource`/`Config`/`Player` 类型）
- package.json：加 typescript、@types/react(-dom)；build = `tsc -b && vite build`

**验证（已通过）**
- `tsc -b` 类型检查零错误；`vite build` 成功
- 后端托管新产物正常，src 无残留 JS

---

## 阶段 10：Flutter App（Android/iOS）进行中

移动端原生 App，WebRTC(WHEP) 亚秒播放 + 录制裁剪。

**产出（`app/` Flutter 工程）**
- 依赖：`flutter_webrtc`（WHEP 播放）、`http`（后端 API）
- `lib/config.dart`：服务器内网 IP 配置
- `lib/api.dart` / `lib/models.dart`：后端 API 封装 + ClipJob
- `lib/webrtc_player.dart`：RTCPeerConnection + WHEP 信令 + RTCVideoView
- `lib/main.dart`：播放 + 录制（计时/清晰度）+ 片段列表（轮询/自动刷新）
- 权限：Android `usesCleartextTraffic` + INTERNET；iOS ATS 明文 + 摄像头/麦克风描述

**关键约束**
- ⚠️ 真机访问宿主机内网 IP，且 **SRS candidate 必须设为该内网 IP**（非 127.0.0.1）并重启 SRS。
- `config.dart` 的 `host` 要与后端机器内网 IP 一致。

**验证**
- `flutter pub get` ✅、`flutter analyze` 零问题 ✅
- `flutter build apk --debug` 进行中
- 真机运行（看直播+录制+裁剪）待用户在设备上验证

---

## 阶段 11：微信小程序 进行中

`miniprogram/` 原生小程序，FLV 播放 + 录制裁剪。

**产出**
- `app.json`/`project.config.json`（urlCheck:false 允许内网 http）/`sitemap.json`
- `utils/config.js`（内网 IP）/`utils/api.js`（wx.request 封装）
- `pages/index`：`<live-player>`(HTTP-FLV) + 录制(计时/清晰度 picker) + 片段卡片列表 + 保存到相册

**关键决策：放弃 live-player，改用 `<video>` + HLS**
- `<live-player>` 需企业主体 + 直播类目权限（`jsapi has no permission`），个人小程序用不了，**开发者工具也不绕过**。
- 改用 `<video>` 播 HLS：任何小程序都能用，无需资质；模拟器也能渲染。
- 后端新增：`/hls/*`（ServeDir 切片）+ `/live.m3u8`（动态直播窗口，只列最近 8 片，避免 video 从头播）。
- 代价：HLS 延迟 5~10s（小程序无 WebRTC）。

**其它约束**
- ⚠️ 内网 http：开发者工具勾「不校验合法域名」；真机/发布需 HTTPS + 备案。
- ⚠️ `config.js` 的 HOST 要与后端内网 IP 一致。

**验证**
- `/live.m3u8` → 200、ts → 200、JS 语法 + JSON 合法 ✅
- 后端动态窗口 m3u8 输出正确（最近 8 片 + 绝对 ts 路径 + 直播模式）✅
- 开发者工具/真机内播放待用户验证

---

## 阶段 12：暂停/播放 + 弹幕（Web & Flutter）✅ 已完成

> 小程序端已冻结，不在此次范围。

**暂停/播放（WebRTC）**
- Web `usePlayer`：`pause()` 断开拉流 + `pausedRef` 阻止自动重连，`resume()` 按协议重连；Player 加按钮
- Flutter `webrtc_player`：`_pause()/_resume()`，播放器左下角图标按钮

**弹幕（后端 WebSocket 广播，可开关）**
- 后端：`danmaku.rs` `/ws/danmaku`，`AppState.danmaku` 用 `tokio::broadcast` 广播；空白/超长(>100)过滤
- `Cargo.toml`：axum `ws` feature + `futures-util`
- Web：`useDanmaku`（连 WS、飘过、发送）+ Player 弹幕层/开关/输入框/**速度滑块**；CSS 动画时长跟随
- Flutter：`web_socket_channel` + `danmaku_overlay.dart`（AnimationController 飘过）+ main 开关/输入框/**速度 Slider**
- 速度可调：档位 1~10 → 飘过时长 `14-speed` 秒，实时生效

**验证（已通过）**
- 后端 WS：A 订阅 + B 发送 → A 收到广播；空白过滤 ✅
- Web `npm run build` ✅；Flutter `analyze` 零问题 ✅

---

## 阶段 13：播放器 UI 改造 + Tailwind v4（Web & Flutter）✅ 已完成

**Web → Tailwind CSS v4**
- `@tailwindcss/vite` 插件；`styles.css` 用 `@import "tailwindcss"` + `@theme`(语义色) + `@layer components`(`.btn`/`.ctrl`) + 保留 `@keyframes`(弹幕/红点)
- 4 组件 className 全改 utility；`tsc -b` + build 通过

**YouTube 风格播放器（两端一致）**
- 控制按钮从播放器下方移入**播放器底部悬浮控制栏**（渐变背景）
- Web：桌面 hover 显示 / 移动常显；`Fullscreen API` 真全屏；控制项 暂停·追帧·协议切换·弹幕·全屏
- Flutter：tap 显隐 + 3s 自动隐藏（`AnimatedOpacity`+`Timer`）；控制项 暂停·弹幕·全屏；`WebRTCPlayer` 收 `danmakuOn`/`onToggleDanmaku`
- 弹幕输入框 + 速度滑块留在播放器下方

---

## 阶段 14：用户登录（手机号 + 验证码）✅ 已完成

> 覆盖原 spec「不做鉴权」——按新需求实现。**登录后才能录制。**

**后端**
- `AppState.sessions: HashSet<String>`（内存会话）
- `POST /api/login {phone, code}`：验证码写死 = **手机号后 6 位**；通过则生成 token 存 sessions
- `is_authed`：校验 `Authorization: Bearer <token>`；`clip_start`/`clip_end` 加鉴权，未登录返回 `code:401`

**Web**
- `useAuth`（token 存 localStorage 持久）+ `apiLogin` + `LoginModal`
- header 显示登录态/退出；`RecordBar` 收 `token`/`onNeedLogin`，未登录拦截并弹登录；请求带 `Authorization`

**Flutter**
- `Api.login` + `_post` 带 token；`main.dart` 登录态（内存）+ `_showLogin` 弹窗 + AppBar 登录状态
- `_startRec`/`_stopRec` 检查 token，401 自动登出并弹登录

**验证（已通过）**
- 未登录录制 → 401；错码登录 → 拒；正确码（后 6 位）→ token；带 token 全链路裁剪 done ✅
- Web build / Flutter analyze 通过 ✅

---

## 阶段 15：用户管理 / 我的视频 ✅ 已完成

片段与用户关联，登录后可查看/播放/下载自己录的视频。

**后端**
- `sessions: HashMap<token, phone>`（token 绑手机号）；`auth_phone` 取登录手机号
- `ClipJob.owner`（手机号）；`clip_end` 写入 owner
- `GET /api/my/clips`（需登录）：只返回 owner==当前手机号 的片段

**Web**
- header 加「直播 / 我的视频」导航；`UserCenter` 组件：我的视频卡片 + **播放弹窗(video)** + 下载
- `apiGet` 支持 token

**Flutter**
- AppBar 加「我的视频」入口 → `UserCenterPage`
- 依赖 `video_player`(播放页) + `url_launcher`(下载)；`Api.myClips`

**验证（已通过）**
- A 录制 → A 的 my/clips=1(owner=A)、B 的=0；未登录 401 ✅
- Web build / Flutter analyze 通过 ✅

---

## 阶段 16：云端后台管理系统 ✅ 已完成

本地内网部署 + 云端公网中心（配置下发 + 业务数据查看）。详见 [CLOUD.md](CLOUD.md)。

**云端后端（`cloud/` Rust axum + PostgreSQL）**
- PG via docker-compose；启动建表（nodes/node_metrics/node_configs/admins/admin_sessions）
- 节点：注册(幂等)/心跳/接收上报/配置读取（Bearer node token）
- 管理：登录鉴权(中间件) + 总览/节点列表/改配置/指标趋势（Bearer admin token）

**本地上报（`backend/src/report.rs`）**
- 配置 `CLOUD_URL`+`NODE_NAME` 才启用；注册→定时上报统计+心跳+拉配置；云端不可达静默重试
- 拉到的配置缓存到 `AppState.config`，`clip_end` 按允许清晰度回退；`GET /api/config` 暴露给前端

**云端前端（`cloud-admin/` React+TS+Tailwind v4）**
- 登录页 + 数据看板（总览卡片）+ 节点列表（在线/指标）+ 配置编辑（下发）；云端后端托管 dist

**验证（已通过端到端）**
- 本地注册→云端 nodes；上报→node_metrics；overview 有数据
- 管理端改配置(默认 720p)→ 本地 10s 内拉到生效 ✅
- 管理鉴权：未登录 401、admin/admin123 登录、错码拒绝 ✅

**踩坑**：PG `SELECT 1` 返回 int4，用 `Option<i64>` 解码失败 → 改 `count(*)`。

---

## 阶段 5：收尾

- README FAQ 按实测补充
- 代码 `// TODO: 多房间 / 鉴权 / HLS实时裁剪 留待后续` 标注到位
- （可选）`docker-compose.yml` 一键起 SRS + 后端

---

## 风险点 / 注意

| 风险 | 应对 |
|------|------|
| mac Docker 回调宿主机 | 用 `host.docker.internal:8000`，已写入 srs.conf 计划 |
| on_dvr 路径是容器内 `/data` | hooks.rs 做前缀替换为宿主机 `./data` 绝对路径 |
| flv.js 自动播放被拦截 | `<video muted autoplay>` |
| `-c copy` 起点对齐到关键帧 | 文档说明属正常；精确需重编码，Demo 不做 |
| 内存状态重启丢失 | Demo 可接受，README 已注明 |

---

## 建议实现顺序总结

阶段1（SRS+播放） → 阶段2（后端+Hook） → 阶段3（裁剪主线） → 阶段4（前端） → 阶段5（收尾）

每个阶段为一个可验证里程碑，逐步推进，不一次性写完。

---
---

# 迭代二：桌面主播推流端 + 多租户 + 后端持久化

> 把 Demo 推向产品方向：明确三层（云端平台 / 推流平台 / 用户端），新增**桌面主播端**取代手动 OBS，
> 直播间从写死 `room001` 升级为多商家/多流，backend 去内存态。
> 可勾选清单见 [TODO.md](TODO.md)；桌面端设计见 [STREAMER.md](STREAMER.md)；数据模型见 [MULTI-TENANT.md](MULTI-TENANT.md)。
> 沿用「产出 / 验证 / 踩坑记录」结构，逐阶段独立验证。

## 阶段 P1：桌面主播端打通单流 WHIP ⬜ 未开始

先不动多租户，沿用现有 `app=live&stream=room001`，backend / SRS / cloud 不改。

**产出**
- `streamer/`（新建独立 Flutter 工程，target macOS + Windows，不在 `app/` 上加桌面平台）
- `streamer/lib/devices.dart`（摄像头/麦克风枚举：先 getUserMedia 触发授权再 enumerateDevices）
- `streamer/lib/preview.dart`（RTCVideoView 本地预览，mirror）
- `streamer/lib/whip_pusher.dart`（**核心**：getUserMedia + sendonly PC + WHIP 信令 + 状态机/重连 + 强制 H264）
- `streamer/lib/config.dart` / `main.dart`（host 配置 + 最小主播面板）
- `macos/Runner/*.entitlements` + `Info.plist`（摄像头/麦克风/网络权限）

**验证**
1. 桌面下拉出现 `FaceTime高清相机` + 麦克风（替代 push-camera.sh 写死索引）
2. 选设备 → RTCVideoView 显本地画面
3. 开播 → SRS 日志见 publish + rtc_to_rtmp
4. `ffplay http://<ip>:8080/live/room001.flv` 旁证；frontend 能播
5. 停播 → `data/recordings/live/room001/*.mp4` 生成（音频本阶段不纠结）

**踩坑记录**（实现时补）
- ⚠️ **预留**：桌面 WebRTC 默认可能 offer VP8 → SRS rtc_to_rtmp（RTMP 只认 H264）失败。需抓 SDP 实测，必要时 setCodecPreferences / munge SDP 强制 H264 在前。这是本阶段最大不确定点。
- ⚠️ **预留**：macOS 缺 entitlements/Info.plist 权限时 getUserMedia 直接抛异常。

---

## 阶段 P2：SRS opus + 多租户数据模型 ⬜ 未开始

**产出**
- SRS：换带 ffmpeg-opus 的镜像或自编译，去 `--ffmpeg-opus=off`（让 WHIP Opus→AAC 进 DVR）
- `cloud/src/db.rs`：新增 `merchants` / `devices` / `streams` 表 + `nodes.merchant_id`
- `cloud/src/admin.rs`：商家/门店/设备/流 CRUD + 生成 streamKey（短 ID `s_xxxxxx`）
- `cloud/src/nodes.rs::get_config`：返回该 node 的 `streams` 数组
- backend 去 room001 写死（按 [MULTI-TENANT.md](MULTI-TENANT.md) 参数化清单）：`state.rs`(去 const、`streams: HashMap`)、`hooks.rs`(读 body stream 路由)、`handlers.rs`、`clip.rs`(hls_dir 参数化)、`report.rs`(遍历多流)
- backend 新增 `GET /api/streamer/profile`；桌面端改为拉真实 streamKey

**验证**
1. 云端建流拿 streamKey → 桌面端拉到并推
2. 该 streamKey 独立录制目录 `data/recordings/live/<key>/`
3. `ffprobe` 确认 DVR MP4 **含 aac 音轨**（opus 转码生效 = 本迭代核心改进）

**踩坑记录**（实现时补）
- ⚠️ **预留**：无现成 ffmpeg-opus 镜像 tag 时需自编译 SRS，预留时间。

---

## 阶段 P3：后端持久化（SQLite）⬜ 未开始

**产出**
- `backend/Cargo.toml`：引入 sqlx（sqlite）
- 建表 `stream_sessions` / `clip_jobs` / `streams`(缓存) / `sessions`
- `AppState`：`current_mark`→`marks: HashMap<(key,phone),Mark>`；`danmaku`→`HashMap<key,Sender>`（弹幕分房间）；`jobs: Vec`→SQLite 持久 + 内存索引
- 内存态写穿 + 启动恢复；未配 CLOUD_URL 时从本地 SQLite 读（seed 默认流兼容旧行为）

**验证**
1. 推流 + 裁剪后重启 backend，`/api/clips` 与 jobs 状态仍在
2. 多用户在不同流标记录制互不串

**踩坑记录**（实现时补）

---

## 阶段 P4：观众端 + 二维码闭环 ⬜ 未开始

**产出**
- `frontend/src/hooks/usePlayer.ts`：读 `?stream=<key>`（缺省回退默认流）；`App.tsx`/`Player.tsx` 透传
- `app/lib/config.dart` whepUrl 加 streamKey；`main.dart:213` 标题去写死显流标题
- `streamer/lib/viewer_qr.dart`：network_info_plus 取内网 IP → `http://<lan-ip>:8000/?stream=<key>` → qr_flutter 出码
- `streamer/lib/danmaku_panel.dart`（可选，复用 /ws/danmaku 显示观众弹幕）

**验证**
1. 桌面二维码 → 手机浏览器 → frontend 按 streamKey 播放
2. 观众登录 → 录制 start/end → `/api/clips` 出现该流片段 → 下载 ffprobe 时长正确
3. 多租户隔离：第二 streamKey + 第二桌面端，两流目录/jobs/弹幕互不串

**踩坑记录**（实现时补）

---

## 迭代二建议实现顺序

P1（桌面端单流 WHIP） → P2（opus + 多租户） → P3（持久化） → P4（观众端 + 二维码）

每阶段独立可验证；P1 先用 room001 跑通推流链路，把 WebRTC 编码协商风险前置消化。
