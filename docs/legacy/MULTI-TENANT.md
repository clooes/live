# 多租户数据模型

> 把写死的单直播间 `live/room001` 升级为多商家 / 多门店 / 多流。
> 主数据在云端 PG（`cloud/`），运行态（直播会话/裁剪任务）在 backend 本地持久化。
> 任务清单见 [TODO.md](TODO.md) P2/P3；桌面端设计见 [STREAMER.md](STREAMER.md)。

---

## 1. 层级

```
商家 merchant           （连锁品牌 / 账号主体）
  └─ 门店 node           （= 现有 nodes 表，一个本地节点 = 一家门店）
       └─ 设备 device    （摄像头，可选层）
            └─ 流 stream  （streamKey，SRS 的 stream 名，对应一路直播）
```

- **门店 = 现有 `nodes`**：一个本地节点（backend + SRS）部署在一家门店，复用不另起。
- **流 streamKey** 全局唯一短 ID（如 `s_a1b2c3`），取代写死的 `room001`。
- SRS 约定 `app=live` 固定，`stream=streamKey`，`srs.conf` 的 `[app]/[stream]` 模板自动把录制隔离到 `data/recordings/live/<streamKey>/`、`data/hls/live/<streamKey>/`。**SRS 命名规则无需改。**

---

## 2. 云端 PG 表结构（扩展 `cloud/src/db.rs` 的 SCHEMA）

现有表：`nodes` / `node_metrics` / `node_configs` / `admins` / `admin_sessions`（见 `cloud/src/db.rs`）。新增/改动：

```sql
-- 新增：商家
CREATE TABLE IF NOT EXISTS merchants (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 改动：门店(nodes) 挂到商家
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS merchant_id UUID REFERENCES merchants(id);

-- 新增：设备（可选层，摄像头）
CREATE TABLE IF NOT EXISTS devices (
    id          UUID PRIMARY KEY,
    node_id     UUID NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL DEFAULT 'camera',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 新增：流 / 直播间（streamKey 主体，取代 node_configs.room_name 的单值）
CREATE TABLE IF NOT EXISTS streams (
    id              UUID PRIMARY KEY,
    node_id         UUID NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    device_id       UUID REFERENCES devices(id),
    stream_key      TEXT UNIQUE NOT NULL,            -- SRS stream 名，如 s_a1b2c3
    app             TEXT NOT NULL DEFAULT 'live',
    title           TEXT,
    qualities       JSONB NOT NULL DEFAULT '["original","720p","480p"]',
    default_quality TEXT NOT NULL DEFAULT 'original',
    enabled         BOOLEAN NOT NULL DEFAULT true,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_streams_node ON streams(node_id);
```

- `node_configs.room_name` / `qualities`（`db.rs:30-36`）**保留兼容**，但多流以 `streams` 为准（清晰度下沉到流级）。
- 清晰度字段沿用现有 `node_configs` 的 `qualities` / `default_quality` 语义，backend `clip_end` 按流的允许清晰度回退（逻辑同现有 `AppState.config`）。

---

## 3. API 改动（`cloud/`）

- `cloud/src/admin.rs`：新增 商家 / 门店 / 设备 / 流 的 CRUD（管理端鉴权同现有 admin_session），含**生成 streamKey**（短 ID）。
- `cloud/src/nodes.rs::get_config`：从返回单个 `room_name` 扩展为返回该 node 的 **`streams` 数组**（含每流 streamKey / app / qualities）。backend 据此知道本节点要服务哪些 streamKey。
- 下发链路 `report.rs::fetch_config` ←→ `nodes.rs::get_config` 已通，仅改 payload 结构。

---

## 4. streamKey 路由与 SRS

- SRS `srs.conf` 的 `http_hooks` body **已自动带 `app`/`stream` 字段**，URL 无需改。
- 改造点全在 backend：`hooks.rs` 当前**忽略 body 的 stream**，把所有流当单例处理 → 改为读 `stream` 字段路由到对应流。
- `backend/src/state.rs:132` 的 `map_container_path` 已天然兼容含 streamKey 的路径（`/data/recordings/live/<key>/...` → 宿主机绝对路径），**无需改**。

---

## 5. room001 写死点参数化清单（backend 改造 checklist）

| 文件:行 | 现状 | 改法 |
|---------|------|------|
| `backend/src/state.rs:14-15` | `const APP="live"` / `STREAM="room001"` | 删 const，运行时按 streamKey 传递 |
| `backend/src/state.rs`（AppState） | `stream: StreamState` 单例 | → `streams: HashMap<key, StreamState>` |
| `backend/src/hooks.rs`（on_publish/on_unpublish/on_dvr） | 忽略 body stream，操作单例 `s.stream` | 解析 body `stream` 字段路由到 `streams[key]` |
| `backend/src/handlers.rs:107` | `clip_{STREAM}` 命名 | 用请求所属 streamKey |
| `backend/src/handlers.rs:112` | `{APP}/{STREAM}` 路径 | 按 streamKey |
| `backend/src/handlers.rs:222` | `/hls/{APP}/{STREAM}/` | 按 streamKey |
| `backend/src/clip.rs:20`（`hls_dir()`） | 写死 `data/hls/live/room001` | 改 `hls_dir(app, key)` 参数化 |
| `backend/src/report.rs:107` | `{APP}/{STREAM}` 单流上报 | 遍历本节点所有流上报 |
| `frontend/src/hooks/usePlayer.ts:30-31` | `stream=room001` / `room001.flv` | 读 URL `?stream=<key>` |
| `app/lib/config.dart:15` | `stream=room001` | streamKey 参数化 |
| `srs/push-camera.sh:17` | `live/room001` | 保留为降级备选，加 `STREAM` 环境变量 |

---

## 6. 运行态归属（backend 本地，不回云端存）

直播会话 / 裁剪进度是高频本地实时数据，且 README 强调节点对云端是 **outbound-only**（云端无法反查内网）。故运行态放 backend 本地 SQLite（详见 [TODO.md](TODO.md) P3）：

- `streams`（缓存 cloud 下发的 streamKey 列表，离线可用）
- `stream_sessions`（替代内存 `StreamState` 单例，按 streamKey）
- `clip_jobs`（替代内存 `jobs: Vec`，关联 streamKey / owner，重启不丢）
- `sessions`（登录态，可选持久化）

`AppState` 内存做热缓存、写穿 SQLite；`marks` 按 `(streamKey, phone)` 多流多用户；`danmaku` 按 streamKey 分房间。
