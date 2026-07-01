# 第二阶段规划 · TODO（基于 需求.md）

> 来源：仓库根 `需求.md`（8 条，原编号有重复，下面重编为 R1–R8）
> 现状基线：relay 单二进制已实现 WHIP→WHEP 直播 + 三页前端 + 全程录制/切片/回放 + 内置 ffmpeg + 优雅退出。
> 本阶段目标：按需求打磨体验 + 加多用户隔离。**先定「待确认决策」，再按批次动手。**

---

## 需求逐条方案

### R1 进入播放页自动播放（去手动点击）
- 现状：`Viewer.tsx` 已 `<video autoPlay muted>`，但实测要手动点。根因大概率在 `whep.ts`——WHEP 流就绪后未显式 `video.play()`，或 autoplay 被浏览器策略拦。
- 方案：WHEP `ontrack`/流就绪即显式 `video.play()`（muted 下允许自动播）；overlay 自动消失。
- 文件：`web/src/whep.ts`、`web/src/pages/Viewer.tsx` ｜ 难度：低

### R2 砍掉录制页 + 管理页，只留单页面；配置改 json
- 现状：hash 路由三页（观看/录制/管理）。
- 方案：删管理页（配置直接编辑 `config.json`，重启生效）；单页 = 观看页内嵌「录制条 + 我的片段（下载/回放）」，去顶部导航与独立路由；回放做成单页内弹窗/抽屉。
- 文件：`App.tsx`（去路由）、`pages/Viewer.tsx`（合并）、删 `pages/Admin.tsx`/`Recordings.tsx`（功能并入）、`web.rs`（视情况精简 config API） ｜ 难度：中
- **决策 D1**：片段列表/下载/整场回放是否保留（合并进单页）。推荐：保留、合并。

### R3 回放放的是录制视频，不是直播（确认：回放误连了实时流 — bug）
- 现象（已确认）：点「回放」播的是直播 WHEP 实时流本身，不是录制的 HLS 文件。
- 方案：回放走**独立的 HLS 播放路径**——`<video>`(Safari 原生 / hls.js) 播 `/recordings/<room>/<session>/index.m3u8`(VOD)，与观看页 WHEP 实时播放**完全解耦**；先排查当前回放为何指向实时流（大概率单页/组件复用了观看页的 WHEP video）。回放列表只列「已结束(含 ENDLIST)」的场次；播放器 VOD 模式不追最新帧。
- 文件：回放组件（HLS 播放）、`web.rs`(`/api/recordings` 标状态) ｜ 难度：中
- 与 R2 强相关：单页合并时回放入口必须用 HLS 播放器，**勿复用直播 WHEP 的 `<video>`**。

### R4 清晰度：录制只存起止，下载时选清晰度
- 现状：直通单路原画流，切片 `-c copy`；`clip.rs` 已预留 `quality_args`（720p/480p 重编码分支，未接 UI）。
- 方案（采纳你的想法）：录制/标记只存起止时间（已如此）；**下载时选清晰度**——`original`=`-c copy` 秒级，`720p/480p`=`scale+libx264` 重编码。切片/下载接口带 `quality`。
- 文件：`clip.rs`（启用 quality_args）、`web.rs`（clip API 带 quality）、前端下载加清晰度选择 ｜ 难度：中
- **决策 D3**：确认「下载时选、用重编码」方向。

### R5 启动炫酷终端 + 分类文件日志
- 现状：`env_logger` → stderr，无 banner、无文件。
- 方案：① 启动 banner（内嵌 ASCII art + 彩色端口/地址表）；② 文件日志换 `tracing` + `tracing-appender`（滚动），分类存 `<data_root>/logs/`：系统日志、用户操作日志（录制起止/下载）、进入直播间日志（WHEP 接入）。
- 文件：`Cargo.toml`、`main.rs`（banner+日志初始化）、`web.rs`/`record.rs`（埋点） ｜ 难度：中
- **决策 D4**：日志「分文件」（system/user-ops/viewers）还是「单文件带标签」。推荐：分文件。

### R6 二维码弹窗，手机扫码进入
- 现状：无。
- 方案：后端 `/api/lan-ip` 返回本机内网 IP；前端「分享」按钮弹二维码（`qrcode` 库）指向 `http://<lan-ip>:<web_port>`。
- 文件：`Cargo.toml`（`local-ip-address`）、`web.rs`（/api/lan-ip）、前端二维码组件 + `qrcode` 依赖 ｜ 难度：低-中

### R7 多用户观看隔离与识别 ⏸ 本阶段推迟（已决策 D5）
> 本阶段不做，维持录制标记/片段全局共享。下方方案保留备下一阶段。跨设备真实账号将来若要，再引入登录。
- 现状：`RecStore.mark` 全局单个、`jobs` 全局共享——所有观看端共用一套录制标记/片段。
- 需求：用户A 的录制片段用户B 看不到；用户A 离开再进来仍显示「录制中」，可手动结束。
- 方案（无登录、轻量身份）：浏览器首访生成匿名 `client_id`（`localStorage` UUID），每次 API 带上；服务端 `mark`/`jobs` 由「全局」改「按 client_id 分桶」（`HashMap<client_id, ClipMark>`、`jobs` 带 `owner`）；页面加载查 `/api/clip/state?client_id=` 恢复「录制中」；片段列表 `/api/clips?client_id=` 只返回自己的。
- 文件：`record.rs`（mark/jobs per-client）、`web.rs`（API 带 client_id）、前端（生成/携带 client_id + 状态恢复） ｜ 难度：高
- **决策 D5**：确认「浏览器匿名 client_id（localStorage）」无登录隔离；跨设备/真实账号的登录留待以后。

### R8 端口被占用可配置
- 现状：`RTMP_ADDR`/`WHEP_ADDR`/`WEB_ADDR` 写死常量；前端 `whep.ts` 硬编码 `:8900`。
- 方案：`config.json` 加 `ports { web, webrtc, rtmp }`（缺省用当前默认）；`main.rs` 读 config；前端 WHEP 端口从 `/api/config` 拿（不再硬编码）。
- 文件：`config.rs`、`main.rs`、`web.rs` + 前端 `whep.ts`/`api.ts` ｜ 难度：低-中

### R9 下载片段加图片水印（新需求 · 待排期）
- 目标：下载的视频片段叠加本地图片水印（防扩散/标识来源）。**仅作用于下载片段**，直播播放与整场回放不动（D10）。
- 方案：切片这一步用 ffmpeg `overlay` 滤镜叠图。示意滤镜链：
  `[1]scale=iw*<scale>:-1,format=rgba,colorchannelmixer=aa=<opacity>[wm];[0][wm]overlay=<pos>`。
  现有 `quality_args` 的 `-vf`（单输入）需改为 `-filter_complex`（片段 + 水印图两输入），把 `scale`(清晰度) 与 `overlay`(水印) 合进同一链；`overlay` 坐标用 `W-w`/`H-h` 表达式自适应各清晰度分辨率。
- 关键影响：**original 不再能 `-c copy`**——水印须逐帧绘制 → 全清晰度都重编码（D8=B）。牺牲原画秒级直拷，换全档带水印一致性。
- 配置（config.json 新增，本地路径 D9）：
  `watermark { enabled, image(本地路径，复用 data_dir 的 绝对/~//相对二进制目录 解析), position(br/bl/tr/tl/center), opacity(0~1), scale(相对视频宽) }`；只填 image+enabled 也能跑，其余给默认。
- 缓存失效：现按 `clip_<job>_<quality>.mp4` 缓存，需让 key 能区分「有/无水印」及「换图/换位置」——文件名带水印配置短 hash 或水印图 mtime，否则改配置仍下到旧文件。
- 容错：图片不存在/路径错 → 回退不加水印正常出片 + 记 user_ops 日志，勿让下载失败。
- 文件：`config.rs`(watermark 配置)、`clip.rs`(filter_complex + 缓存 key)、`web.rs`(prepare 传水印上下文，可选) ｜ 难度：中

---

## 关键决策（已定稿）
| # | 关于 | 结论 |
| --- | --- | --- |
| D1 | R2 单页是否保留片段列表/下载/整场回放 | ✅ 保留，合并进观看页 |
| D2 | R3「回放却是直播」的现象 | ✅ 回放播的是直播实时流本身 → 改为播录制 HLS VOD（bug 修复） |
| D3 | R4 下载时选清晰度、用重编码 | ✅ 是 |
| D4 | R5 日志分文件 vs 单文件带标签 | ✅ 分文件（system/user-ops/viewers） |
| D5 | R7 多用户隔离 | ⏸ 本阶段先不做，推迟 |
| D6 | R2 删管理页后改 room/清晰度入口 | ✅ 纯看 config.json（彻底删管理页，无改配置 UI；改后重启生效） |
| D7 | R3 回放展示形态 | ✅ 单页内弹窗（复用 R6 弹窗风格，独立 HLS VOD 播放器） |
| D8 | R9 水印作用范围（original 是否重编码） | ✅ B 全清晰度都加水印（original 也重编码，放弃秒级直拷） |
| D9 | R9 水印图片来源 | ✅ 本地图片路径（复用 data_dir 路径解析；不用 URL） |
| D10 | R9 水印作用对象 | ✅ 仅下载片段（直播播放/整场回放不加） |

> 实现补充：
> - R8 端口属**启动期配置**，改 config.json 的 ports 后**需重启**（不像 room/清晰度 SSE 热更新）。
> - R5-a banner 传统 Windows cmd 兼容：零依赖 kernel32 FFI 启用 VT(ANSI) + UTF-8 代码页，修彩色转义乱码/中文花屏，失败静默降级。
> - R5-b 日志分类用 **log target** 路由（`viewers`/`user_ops`/其余），vendor 里只加 `log::info!(target:…)` 一行、不引 tracing 依赖。

---

## 分批 TODO（按优先级 / 依赖排序）

### 批次 1 · 快速见效（低风险、互相独立）✅ 已完成（commit 40690dd）
- [x] R1 观看页自动播放（WHEP ontrack 就绪即 `video.play()`）
- [x] R8 端口可配（config.ports + main 读取 + 前端从 /api/config 拿 webrtc 端口，去掉硬编码 :8900）
- [x] R5-a 启动 banner（ASCII art + 彩色端口/地址表；含 Windows 零依赖 FFI 启用 ANSI+UTF-8）

### 批次 2 · 日志与分享 ✅ 已完成（commit 0928b5d）
- [x] R5-b 文件日志（tracing + 按天滚动，按 log target 分类 system/user-ops/viewers → data_root/logs，控制台保留全量）
- [x] R6 二维码弹窗（/api/lan-ip 返回内网 IP + web 端口；前端 qrcode.react 分享按钮弹窗）

### 批次 3 · 单页面重构 + 回放 ✅ 已完成
- [x] R2 砍管理/录制页 → 单页（录制条+片段/回放并入观看页，去 hash 路由；删 Admin/Recordings.tsx；后端删 POST /api/config 写接口，配置纯看 config.json — D6）
- [x] R3 回放修正（Library 只列已结束场次 `!r.live`，回放走单页内弹窗独立 HLS VOD 播放器，与直播 WHEP video 完全解耦；根因=旧代码对 live playlist 追尾、观感等同直播 — D2/D7）

### 批次 4 · 录制体验 ✅ 已完成
- [x] R4 下载时选清晰度：结束录制只登记区间（不预切）；下载时按 quality 切片——
  `original`=`-c copy` 秒级、`720p/480p`=`scale=-2:N + libx264 crf23` 重编码；
  同 (job,quality) 产物磁盘缓存复用。接口 `POST /api/clip/prepare/:id?quality=`，前端片段行清晰度按钮组。

### 批次 5 · 多用户隔离 ⏸ 本阶段推迟
- [ ] ~~R7 client_id 身份 + mark/jobs per-client + 状态恢复 + 片段隔离~~（推迟到下一阶段）

### 批次 6 · 下载水印（R9 · 待排期，决策 D8~D10 已定）
- [ ] R9 下载片段加图片水印（config.watermark + clip.rs filter_complex overlay + 缓存 key 含水印 + 容错回退）

> 第二阶段除 R7（推迟）外全部完成：批次1 `40690dd`、批次2 `0928b5d`、批次3 `6481113`、批次4（本次）。

---

## 风险 / 注意
- **R7** 改动录制状态模型（全局→per-client），API 兼容要处理；放最后、单独充分测试。
- **R8** 端口可配后，前端所有硬编码端口（`whep.ts` 的 `:8900`）必须一并改为从配置读，否则改端口后前端连不上。
- **R4** 重编码引入 CPU 开销（内置 ffmpeg 已含 libx264）。
- **R2** 删页面要「合并功能」而非「丢功能」——回放/下载并入观看页，别丢。
- **R3** 先确认现象再改，避免误判。
- **R9** 开水印后 original 失去 `-c copy` 秒级优势（全档重编码，CPU 上升）；缓存 key 必须含水印状态，否则改配置仍下到旧文件；水印图缺失要回退不加而非报错。
