# 云端后台管理系统（架构与部署）

本地推流裁剪是**内网本地部署**；云端是**公网部署的中心**，用于：对本地节点下发配置、聚合查看各节点业务数据。

## 架构

```
        ☁️ 云端（公网）                cloud/  +  cloud-admin/
        ┌────────────────────────────────────────────┐
        │ 云端后端 Rust axum (:9000) + PostgreSQL      │
        │  节点注册/心跳/接收上报、配置下发、数据聚合   │
        │  管理员登录鉴权                              │
        │ 云端前端 React+TS+Tailwind（后端托管 dist）  │
        │  登录 → 数据看板 + 节点列表 + 配置编辑        │
        └────────────────────────────────────────────┘
              ▲ 注册/心跳/上报      │ 拉配置
              │ (HTTP outbound)     ▼
        ┌────────────────────────────────────────────┐
        │ 🏠 本地节点（内网）backend/ 的 report 模块    │
        │  启动注册 → 定时上报统计 + 心跳 + 拉配置      │
        └────────────────────────────────────────────┘
```

**关键约束**：本地在内网、云端在公网，云端无法主动连内网 → 只能**本地 outbound**（注册/上报/拉配置）。

## 数据流

1. **注册**：本地启动 `POST /api/node/register {name}`（按 name 幂等）→ 拿 `node_id + token`。
2. **上报**：本地每 10s `POST /api/node/report`（Bearer token）上报片段数/用户数/推流状态 → 存 `node_metrics`。
3. **心跳/在线**：上报即刷新 `last_seen`；云端按 `last_seen < 30s` 判在线。
4. **配置下发**：管理员在云端改配置 → 写 `node_configs`；本地每 10s `GET /api/node/config` 拉取 → 缓存并应用（`clip_end` 按允许清晰度回退）。

## 数据库（PostgreSQL）

| 表 | 用途 |
|----|------|
| `nodes` | 节点：id/name/token/last_seen |
| `node_metrics` | 上报快照（时间序列）：片段/用户/推流 |
| `node_configs` | 下发配置：直播间名/可选清晰度/默认清晰度 |
| `admins` / `admin_sessions` | 管理员账号 / 登录会话 |

启动时 `CREATE TABLE IF NOT EXISTS`（`cloud/src/db.rs`），默认管理员 `admin/admin123`。

## API

**节点侧（本地 outbound，Bearer node token）**
- `POST /api/node/register` `{name}` → `{node_id, token}`
- `POST /api/node/heartbeat`
- `POST /api/node/report` `{clips_total,clips_done,clips_processing,users_count,streaming,stream_info}`
- `GET  /api/node/config` → `{room_name, qualities, default_quality}`

**管理侧（需管理员登录，Bearer admin token）**
- `POST /api/admin/login` `{username,password}` → `{token}`（不鉴权）
- `GET  /api/admin/overview` → 节点数/在线/片段总数/用户总数
- `GET  /api/admin/nodes` → 节点列表 + 最新指标 + 配置 + 在线
- `PUT  /api/admin/nodes/:id/config` → 改配置（下发源头）
- `GET  /api/admin/nodes/:id/metrics` → 指标趋势

## 启动顺序

```bash
# 1. 云端 PostgreSQL
cd cloud && docker compose up -d

# 2. 云端管理前端构建（后端托管 dist）
cd cloud-admin && npm install && npm run build   # 或 npm run dev (5174)

# 3. 云端后端
cd cloud && cargo run            # :9000，访问 http://localhost:9000/ 管理后台

# 4. 本地后端（开启上报：配置 CLOUD_URL + NODE_NAME）
cd backend && CLOUD_URL=http://<云端IP>:9000 NODE_NAME="门店A" cargo run
#   未配置 CLOUD_URL 则纯本地运行，不上报
```

管理后台默认 `admin / admin123`。

## 端到端验证（已通过）

- 本地注册 → 云端 `nodes` 出现节点；定时上报 → `node_metrics` 入库
- 云端 `/api/admin/overview` 看到 节点数/在线数/片段/用户
- 管理端 `PUT config`（默认 720p）→ 本地 10s 内 `GET /api/config` 拉到 `default_quality=720p`，`clip_end` 据此回退清晰度

## 边界 / 生产建议

- Demo：admin 密码明文、node/admin token 无过期、上报间隔写死 10s。
- 生产：密码哈希、token 过期/刷新、HTTPS、本地↔云端用 mTLS 或签名、指标做聚合/降采样、节点离线告警。
