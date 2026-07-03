//! 内网页面 + 配置接口 + 录制/切片接口（axum，:8000）。
//! - GET/POST /api/config：读写清晰度配置（持久化 config.json）
//! - GET /api/config/stream：SSE，管理端一保存即广播新配置给所有在线观看端
//! - POST /api/clip/start | /api/clip/end：观看时标记一段区间，据起止时间切片
//! - GET /api/clip/status/:id | /api/clips | /api/recordings：切片/录制列表与进度
//! - /clips/*、/recordings/*：切片下载 与 整场 HLS 回放（ServeDir，支持 Range）
//! - 其余路径：托管 rust-embed 打进二进制的 React 构建产物（SPA，回退 index.html）

use std::convert::Infallible;

use axum::{
    extract::{Query, Request, State},
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

use streamhub::define::StreamHubEventSender;
use tokio::sync::watch;

use crate::config::{RelayConfig, SharedConfig};
use crate::record::{self, RecTasks, Recording, SharedRec};

/// 「开始录制」请求体：选清晰度（缺省用 default_quality）+ 归属浏览器 uid。
#[derive(serde::Deserialize)]
struct StartBody {
    quality: Option<String>,
    owner: Option<String>,
}

/// 录制列表/状态的查询参数：按归属浏览器 uid 过滤（缺省不过滤，返回全部）。
#[derive(serde::Deserialize)]
struct OwnerQuery {
    owner: Option<String>,
}

/// 「停止录制」请求体：录制 id。
#[derive(serde::Deserialize)]
struct StopBody {
    id: String,
}

/// web 层共享状态：配置 + 配置广播 + 录制状态 + 录制所需的媒体 hub/退出信号/任务收集。
#[derive(Clone)]
pub struct WebState {
    pub cfg: SharedConfig,
    pub tx: broadcast::Sender<RelayConfig>,
    pub rec: SharedRec,
    pub hub: StreamHubEventSender,
    pub shutdown: watch::Receiver<bool>,
    pub tasks: RecTasks,
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
        // 分段录制（点击录制即录成品 mp4）
        .route("/api/record/state", get(record_state))
        .route("/api/record/start", post(record_start))
        .route("/api/record/stop", post(record_stop))
        .route("/api/records", get(list_records))
        // 录制片段下载（ServeDir 自带 Range 支持）+ 下载埋点（user_ops 日志）
        .nest(
            "/clips",
            Router::new()
                .fallback_service(ServeDir::new(record::clips_dir()))
                .layer(middleware::from_fn(log_clip_download)),
        )
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

// ---------- 分段录制 ----------

/// 录制状态：当前是否有直播流可录 + 该 owner 是否有进行中的录制（缺省 owner 则看全局）。
async fn record_state(
    State(st): State<WebState>,
    Query(q): Query<OwnerQuery>,
) -> Json<serde_json::Value> {
    let s = st.rec.read().await;
    let recording = match &q.owner {
        Some(o) => s.recordings.iter().any(|r| r.owner == *o && r.status == "recording"),
        None => !s.stops.is_empty(),
    };
    Json(json!({ "live": s.current.is_some(), "recording": recording }))
}

/// 「开始录制」：按所选清晰度当场起 ffmpeg 录成品 mp4（有声）。返回录制 id。
async fn record_start(
    State(st): State<WebState>,
    Json(body): Json<StartBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let quality = match body.quality {
        Some(q) => q,
        None => st.cfg.read().await.default_quality.clone(),
    };
    if !st.cfg.read().await.qualities.iter().any(|x| x.name == quality) {
        return Err((StatusCode::BAD_REQUEST, format!("未知清晰度 {quality}")));
    }
    let owner = body.owner.unwrap_or_default();
    let id = record::start_recording(
        st.hub.clone(), st.rec.clone(), quality, owner, st.shutdown.clone(), st.tasks.clone(),
    )
    .await
    .map_err(|e| (StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "id": id })))
}

/// 「停止录制」：结束对应录制、写完 mp4。
async fn record_stop(
    State(st): State<WebState>,
    Json(body): Json<StopBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    record::stop_recording(&st.rec, &body.id)
        .await
        .map_err(|e| (StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "ok": true })))
}

/// 录制片段列表（最新在前）。带 owner 则只返回该浏览器的录制（「我的录制」）。
async fn list_records(
    State(st): State<WebState>,
    Query(q): Query<OwnerQuery>,
) -> Json<Vec<Recording>> {
    let s = st.rec.read().await;
    let list = match &q.owner {
        Some(o) => s.recordings.iter().filter(|r| r.owner == *o).cloned().collect(),
        None => s.recordings.clone(),
    };
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
