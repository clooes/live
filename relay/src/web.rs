//! 内网页面 + 配置接口 + 录制/切片接口（axum，:8000）。
//! - GET/POST /api/config：读写清晰度配置（持久化 config.json）
//! - GET /api/config/stream：SSE，管理端一保存即广播新配置给所有在线观看端
//! - POST /api/clip/start | /api/clip/end：观看时标记一段区间，据起止时间切片
//! - GET /api/clip/status/:id | /api/clips | /api/recordings：切片/录制列表与进度
//! - /clips/*、/recordings/*：切片下载 与 整场 HLS 回放（ServeDir，支持 Range）
//! - 其余路径：托管 rust-embed 打进二进制的 React 构建产物（SPA，回退 index.html）

use std::convert::Infallible;

use axum::{
    extract::{Path, Request, State},
    http::{header, Method, StatusCode, Uri},
    middleware::{self, Next},
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
        // 配置纯看 config.json（D6：管理页已删，无写接口）；GET + SSE 供前端读取/下发 room/端口
        .route("/api/config", get(get_config))
        .route("/api/config/stream", get(config_stream))
        .route("/api/lan-ip", get(lan_ip))
        // 录制/切片
        .route("/api/clip/start", post(clip_start))
        .route("/api/clip/end", post(clip_end))
        .route("/api/clip/status/:id", get(clip_status))
        .route("/api/clips", get(list_clips))
        .route("/api/recordings", get(list_recordings))
        // 切片下载（ServeDir 自带 Range 支持）+ 下载埋点（user_ops 日志）
        .nest(
            "/clips",
            Router::new()
                .fallback_service(ServeDir::new(clip::clips_dir()))
                .layer(middleware::from_fn(log_clip_download)),
        )
        // 整场 HLS 回放
        .nest_service("/recordings", ServeDir::new(config::data_root().join("recordings")))
        .fallback(static_handler)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn get_config(State(st): State<WebState>) -> Json<RelayConfig> {
    Json(st.cfg.read().await.clone())
}

/// 内网分享（R6）：返回本机内网 IP + web 端口，前端据此生成二维码 http://<ip>:<port>。
/// 取不到内网 IP（无网卡/异常）时 ip 为 null，前端回退用当前主机名。
async fn lan_ip(State(st): State<WebState>) -> Json<serde_json::Value> {
    let ip = local_ip_address::local_ip().ok().map(|a| a.to_string());
    let web_port = st.cfg.read().await.ports.web;
    Json(json!({ "ip": ip, "web_port": web_port }))
}

// ---------- 录制 / 切片 ----------

/// 标记「开始录制」：以当前直播 session + 当前墙钟时刻为起点。
async fn clip_start(State(st): State<WebState>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut s = st.rec.write().await;
    let Some(sess) = s.current_session().cloned() else {
        return Err((StatusCode::CONFLICT, "当前没有正在直播的录制".into()));
    };
    let mark = ClipMark { session_id: sess.id.clone(), start_ms: now_ms() };
    log::info!(target: "user_ops", "开始录制 session={} start_ms={}", mark.session_id, mark.start_ms);
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
    log::info!(
        target: "user_ops",
        "结束录制 session={} 区间=[{}, {}] 建切片 job={}",
        job.session_id, job.start_ms, job.end_ms, job.id
    );
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

/// 下载埋点：记录对切片文件（.mp4）的 GET 到 user_ops 日志。
/// 挂在 /clips 子路由上，只影响切片下载，不影响其他静态资源/回放。
async fn log_clip_download(req: Request, next: Next) -> Response {
    if req.method() == Method::GET {
        let path = req.uri().path();
        if path.ends_with(".mp4") {
            log::info!(target: "user_ops", "下载切片 /clips{path}");
        }
    }
    next.run(req).await
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
