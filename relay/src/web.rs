//! 内网页面 + 配置接口 + 录制/切片接口（axum，:8000）。
//! - GET/POST /api/config：读写清晰度配置（持久化 config.json）
//! - GET /api/config/stream：SSE，管理端一保存即广播新配置给所有在线观看端
//! - POST /api/clip/start | /api/clip/end：观看时标记一段区间，据起止时间切片
//! - GET /api/clip/status/:id | /api/clips | /api/recordings：切片/录制列表与进度
//! - /clips/*、/recordings/*：切片下载 与 整场 HLS 回放（ServeDir，支持 Range）
//! - 其余路径：托管 rust-embed 打进二进制的 React 构建产物（SPA，回退 index.html）

use std::convert::Infallible;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode, Uri},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures::Stream;
use rust_embed::RustEmbed;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::clip;
use crate::config::{self, RelayConfig, SharedConfig};
use crate::record::{now_ms, ClipJob, ClipMark, SharedRec};

/// web 层共享状态：当前配置 + 配置变更广播通道 + 录制/切片状态。
#[derive(Clone)]
pub struct WebState {
    pub cfg: SharedConfig,
    pub tx: broadcast::Sender<RelayConfig>,
    pub rec: SharedRec,
}

/// 前端构建产物（`relay/web/dist`）编进二进制。构建前该目录须存在。
#[derive(RustEmbed)]
#[folder = "web/dist"]
struct Assets;

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/api/config", get(get_config).post(post_config))
        .route("/api/config/stream", get(config_stream))
        // 录制/切片
        .route("/api/clip/start", post(clip_start))
        .route("/api/clip/end", post(clip_end))
        .route("/api/clip/status/:id", get(clip_status))
        .route("/api/clips", get(list_clips))
        .route("/api/recordings", get(list_recordings))
        // 切片下载 / 整场 HLS 回放（ServeDir 自带 Range 支持）
        .nest_service("/clips", ServeDir::new(clip::clips_dir()))
        .nest_service("/recordings", ServeDir::new(config::data_root().join("recordings")))
        .fallback(static_handler)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn get_config(State(st): State<WebState>) -> Json<RelayConfig> {
    Json(st.cfg.read().await.clone())
}

async fn post_config(
    State(st): State<WebState>,
    Json(new_cfg): Json<RelayConfig>,
) -> Result<Json<RelayConfig>, (StatusCode, String)> {
    if !new_cfg.qualities.iter().any(|q| q.name == new_cfg.default_quality) {
        return Err((
            StatusCode::BAD_REQUEST,
            "default_quality 不在 qualities 列表中".into(),
        ));
    }
    config::save(&new_cfg).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    *st.cfg.write().await = new_cfg.clone();
    // 广播给所有在线观看端（无订阅者时 send 会 Err，忽略即可）
    let _ = st.tx.send(new_cfg.clone());
    log::info!("配置已更新、持久化并广播（当前订阅端 {}）", st.tx.receiver_count());
    Ok(Json(new_cfg))
}

// ---------- 录制 / 切片 ----------

/// 标记「开始录制」：以当前直播 session + 当前墙钟时刻为起点。
async fn clip_start(State(st): State<WebState>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut s = st.rec.write().await;
    let Some(sess) = s.current_session().cloned() else {
        return Err((StatusCode::CONFLICT, "当前没有正在直播的录制".into()));
    };
    let mark = ClipMark { session_id: sess.id.clone(), start_ms: now_ms() };
    log::info!("标记切片起点 session={} start_ms={}", mark.session_id, mark.start_ms);
    let resp = json!({ "ok": true, "session_id": mark.session_id, "start_ms": mark.start_ms });
    s.mark = Some(mark);
    Ok(Json(resp))
}

/// 标记「结束录制」：据 [start, now] 建切片 job，异步跑 ffmpeg，返回 job。
async fn clip_end(State(st): State<WebState>) -> Result<Json<ClipJob>, (StatusCode, String)> {
    let job = {
        let mut s = st.rec.write().await;
        let Some(mark) = s.mark.take() else {
            return Err((StatusCode::CONFLICT, "尚未标记开始，请先点「开始录制」".into()));
        };
        let end_ms = now_ms();
        if end_ms <= mark.start_ms {
            return Err((StatusCode::BAD_REQUEST, "区间时长为 0".into()));
        }
        let id = format!("{}", now_ms());
        let job = ClipJob {
            id: id.clone(),
            session_id: mark.session_id,
            start_ms: mark.start_ms,
            end_ms,
            status: "processing".into(),
            file: None,
            size: None,
            error: None,
            created_at_ms: end_ms,
        };
        s.jobs.insert(0, job.clone()); // 最新在前
        job
    };
    // 异步执行切片
    tokio::spawn(clip::run_job(st.rec.clone(), job.id.clone()));
    Ok(Json(job))
}

/// 查询单个切片 job 进度。
async fn clip_status(
    State(st): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<ClipJob>, (StatusCode, String)> {
    let s = st.rec.read().await;
    s.jobs
        .iter()
        .find(|j| j.id == id)
        .cloned()
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "无此切片任务".into()))
}

/// 切片列表（最新在前）。
async fn list_clips(State(st): State<WebState>) -> Json<Vec<ClipJob>> {
    Json(st.rec.read().await.jobs.clone())
}

/// 可回放的录制场次列表（最新在前）。
async fn list_recordings(State(st): State<WebState>) -> Json<Vec<serde_json::Value>> {
    let s = st.rec.read().await;
    let list: Vec<serde_json::Value> = s
        .sessions
        .iter()
        .rev()
        .map(|sess| {
            json!({
                "id": sess.id,
                "room": sess.room,
                "started_at_ms": sess.started_at_ms,
                "ended_at_ms": sess.ended_at_ms,
                "live": sess.live,
                // 回放地址：ServeDir 挂在 /recordings（根目录 data/recordings）
                "playlist": format!("/recordings/{}/{}/index.m3u8", sess.room, sess.id),
            })
        })
        .collect();
    Json(list)
}

/// SSE 配置流：连接即先推一份当前快照，之后每次变更实时推送。
async fn config_stream(
    State(st): State<WebState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let initial = st.cfg.read().await.clone();
    let rx = st.tx.subscribe();

    let init_stream = tokio_stream::once(Ok::<Event, Infallible>(json_event(&initial)));
    let updates = BroadcastStream::new(rx)
        .filter_map(|res| res.ok())
        .map(|cfg| Ok::<Event, Infallible>(json_event(&cfg)));

    Sse::new(init_stream.chain(updates)).keep_alive(KeepAlive::default())
}

fn json_event(cfg: &RelayConfig) -> Event {
    Event::default()
        .json_data(cfg)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

/// 静态资源：命中则返回；未命中（SPA 前端路由）回退 index.html。
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
        }
        None => match Assets::get("index.html") {
            Some(content) => ([(header::CONTENT_TYPE, "text/html")], content.data).into_response(),
            None => (StatusCode::NOT_FOUND, "前端未构建：请先 npm run build").into_response(),
        },
    }
}
