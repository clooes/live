# 旧架构文档（已废弃 · 仅留档）

这些文档描述的是**旧的分层架构**：`OBS → SRS(Docker) → Rust backend(:8000) + 前端(frontend/) + 云端(cloud/ + PostgreSQL) + cloud-admin/`。

该架构的代码（`backend/`、`frontend/`、`srs/`、`cloud/`、`cloud-admin/`、`start-all.sh`、`stop-all.sh`）已于 2026-07-01 随「单二进制 relay」重构删除。保留本目录仅供追溯设计思路与历史决策。

**现行架构见** [`../REFACTOR-SINGLE-BINARY.md`](../REFACTOR-SINGLE-BINARY.md) **与项目根 README。**

| 文档 | 原主题 | 现状 |
| --- | --- | --- |
| DESIGN.md | 旧详细设计（数据结构/API/时间对齐/HLS 裁剪） | 裁剪时间对齐思路已在 relay 重新实现，见 REFACTOR §12 |
| PLAN.md | 旧分步实施计划（含迭代二） | 已废弃 |
| TODO.md | 旧迭代 TODO（桌面推流端/多租户/持久化） | 已废弃，部分条目可作 relay 后续 roadmap 参考 |
| STREAMER.md | 桌面主播推流端（streamer/，未落地） | 推流已改为 OBS WHIP，本设计未实现 |
| MULTI-TENANT.md | 多租户数据模型（云端 PG） | relay 单机首版不做多租户 |
| CLOUD.md | 云端后台管理系统 | 云端已删除 |
