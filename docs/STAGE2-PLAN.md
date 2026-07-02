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
- **启动期全局配置（D12）**：随 `ports` 一样在**启动时读一次** config.watermark，运行期不变、改后重启生效；**全局作用于所有下载片段**，前端不逐次选水印（水印不进下载选项维度）。启动时可校验图片存在，缺失则视作 disabled + 记日志。
- 缓存失效：水印全局固定，同一次运行内无「有/无水印」之分；但**重启换了水印图/位置/透明度后，磁盘旧片段缓存仍在** → 需在缓存 key 里带水印配置短 hash（或启动时清 clips 缓存），否则改配置后仍下到旧水印片段。
- 容错：图片不存在/路径错 → 回退不加水印正常出片 + 记 user_ops 日志，勿让下载失败。
- 文件：`config.rs`(watermark 配置)、`clip.rs`(filter_complex + 缓存 key)、`web.rs`(prepare 传水印上下文，可选) ｜ 难度：中

### R10 录制补音频 + 下载有声/无声（新需求 · 高优先 · 待排期）
- 现状：`record.rs` 只收 `FrameData::Video`，第 247 行 `Some(_) => {}` 把音频帧丢了 → **录制/回放/切片全程无声**（直播 WHEP 本身有声）。这对教学/存证/集锦几个核心卖点是硬伤。
- 关键前提（好消息）：`whip.rs`(320~376) 接收 WHIP 时**已把 Opus 实时转码成 AAC**，并作为 `FrameData::Audio`（含 AAC 头 ASC）发进同一 frame 通道——音频「已送到门口」，record 只是没接。
- 补音频三步：① record 接住 `FrameData::Audio` + 给裸 AAC 加 ADTS 头；② ffmpeg 由单路 `-f h264 -i pipe:0` 改**双路输入**（视频 pipe + 音频命名管道 mkfifo），混成带音轨 HLS；③ **音画同步**（视频 90k / 音频 48k 时钟不一致，用 `-use_wallclock_as_timestamps` 近似，需实测防漂移）。
- **下载 6 选项**（本需求新增，见 D11）：original/720p/480p 各出「有声」「无声」两版 = 6 个下载入口。
  - 有声：从带音轨 HLS 正常切（original `-c copy` 带音轨；720p/480p 重编码已有 `-c:a aac`）。
  - 无声：切片时加 `-an` 去音频。
  - 接口 `prepare` 在 quality 之外加 `audio=on|off`；缓存文件名加音频维度（如 `clip_<job>_<quality>_<mute|snd>.mp4`）。
- 容错/边界：推流端无音频（OBS 不推/摄像头无麦）→ 有声版自动等同无声、不报错。
- 跨平台：mkfifo 在 Windows 无 → 先做 mac/Linux 最小可用（约 3.5~4.5 天），Windows 命名管道兼容作为后续单列（约 +1~1.5 天）。
- 工作量：约 **5.5~7.5 人天**（熟悉本库），风险集中在**音画同步**与 **Windows 管道**。切片 `clip.rs` 基本不用改（HLS 带音轨后自动受益），只加「无声 `-an` + 音频维度缓存」。
- 文件：`record.rs`(收音频+ADTS+双管道)、`clip.rs`(无声 `-an` + 缓存 key 加音频)、`web.rs`(prepare 加 audio 参数)、前端(下载改 6 选项) ｜ 难度：中-高

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
| D11 | R10 下载选项形态 | ✅ 6 选项 = original/720p/480p × 有声/无声（有声依赖补音频，无声切片加 `-an`） |
| D12 | R9 水印配置性质 | ✅ 启动期全局配置（启动读 config.watermark，运行不变、改后重启生效；全局应用于所有下载片段，不逐次选） |
| D13 | R10 音画同步策略 | ✅ 先墙钟近似（视频+音频两路都 `-use_wallclock_as_timestamps`），实测漂移不可接受再上 RTP 精确对齐 |
| D14 | R10 无音频推流降级 | ✅ 静默出片（探测+缓冲：录制开头最多等 1.5s 探测有无音频，无则退回纯视频单路，避免 ffmpeg 空等音频卡死） |
| D15 | R10 平台范围 | ✅ 先 mac/Linux（命名管道 mkfifo）；Windows 命名管道兼容后续单列 |
| D16 | R10 回放是否有声 | ✅ 回放也有声（补音频作用于录制 HLS，回放/下载均自然受益） |

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

### 批次 6 · 录制补音频 + 下载有声/无声（R10 · 决策 D13~D16 已定）🚧 代码完成，待 OBS 带麦实测
- [x] R10 录制补音频（record 探测+缓冲有无音频 → 有则 mkfifo 音频管道 + ffmpeg 双路(视频 stdin + AAC/ADTS fifo)混音轨 HLS；无则退回纯视频。ADTS 头硬编码 48k/2ch/LC，跳过首个 ASC 帧。优雅退出关两端）
- [x] R10 下载 6 选项（clip.rs 无声版 `-an` + 缓存名加 snd/mute 维度；web.rs prepare 加 `audio=on|off`；前端 ClipDownload 有声/无声两行×3 清晰度）
- 已离线验证：`-an` 无声/有声产物音轨差异正确、720p 重编码带音轨正确、prepare 参数分支 400/500 正确；**编译全绿**
- ⚠️ 待真机验证（我无法用 OBS 推流测）：① 音画同步是否漂移（D13 墙钟近似）② 有音频时 fifo 双路混流实际出片 ③ 无麦推流降级不卡死（D14）④ ADTS 封装能被正确解码
- 备注：仅 mac/Linux（D15），Windows 命名管道后续单列

### 批次 7 · 下载水印（R9 · 待排期，决策 D8~D10 已定）
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
- **R10** 音画同步是主要不确定项（音视频 RTP 时钟不同，长录制易漂移），做完须真机带麦长录听感验证，不能只看「有声音」；依赖 vendor 现成 Opus→AAC 链路，补音频会第一次真正压测到它，可能暴露 vendor 问题。
- **R9 × R10 组合**：水印为启动期全局配置（D12），**不进 UI 选项**——下载 UI 维度只有 清晰度 × 有声/无声 = 6，不爆炸。缓存文件磁盘维度 = 清晰度 × 有声/无声 + 水印配置 hash（防重启换图串档），启动期水印恒定故不需逐次区分。
