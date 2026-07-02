//! relay —— 内网直播单二进制（路线 A：纯 Rust + WebRTC）。
//! 一个进程同时提供：
//!   - WHIP 推流入口 + WHEP 播放出口（xwebrtc，:8900，OBS 全程 WebRTC）
//!   - 内网页面 + 配置接口（axum，:8000）
//!   - RTMP 接收（:1935，可选保留，首版 WebRTC 链路不依赖）
//!
//! 媒体路由由 streamhub 撮合。第 0 步已验证 WHIP→WHEP H.264 直通链路。

mod banner;
mod config;
mod ffmpeg;
mod logging;
mod record;
mod web;

use std::sync::Arc;
use std::time::Duration;

use rtmp::rtmp::RtmpServer;
use streamhub::StreamsHub;
use tokio::sync::{watch, RwLock};
use xwebrtc::webrtc::WebRTCServer;

const GOP_NUM: usize = 1;

#[tokio::main]
async fn main() {
    // 加载/生成配置（config.json）+ 配置变更广播通道
    let loaded = config::load();
    // 监听地址由 config.ports 决定（端口被占用时改 config.json 即可，无需改代码）
    let rtmp_addr = format!("0.0.0.0:{}", loaded.ports.rtmp);
    let whep_addr = format!("0.0.0.0:{}", loaded.ports.webrtc);
    let web_addr = format!("0.0.0.0:{}", loaded.ports.web);
    // 启动横幅：ASCII art + 端口/地址表（先于各服务日志打印，用 println 不依赖 logger）
    banner::print(&loaded.ports);
    // 确定数据根目录（录制/切片），以二进制目录为基准或 config.data_dir，不随 CWD 变化
    let data_root = config::init_data_root(&loaded);
    // 装日志：控制台 + data_root/logs 下 system/user-ops/viewers 三个滚动文件。
    // _guards 必须存活到进程结束（drop 会丢未刷盘日志），故绑定到 main 局部变量。
    let _log_guards = logging::init(&data_root);
    log::info!("数据目录：{}（录制/切片存放于此）", data_root.display());
    let cfg = Arc::new(RwLock::new(loaded));
    let (cfg_tx, _) = tokio::sync::broadcast::channel(16);
    let room = cfg.read().await.room.clone();

    // 媒体路由中心：发布者(WHIP/RTMP) ←→ 订阅者(WHEP) 在此撮合
    let mut stream_hub = StreamsHub::new(None);
    // 打开 hls 开关，streamhub 才会在推流时广播 BroadcastEvent::Publish（录制管理器据此开录）
    stream_hub.set_hls_enabled(true);

    // ffmpeg 自检：录制/切片依赖它。内置则首次调用释放，否则回退 PATH。
    log::info!(
        "ffmpeg：{}（路径 {}）",
        if ffmpeg::is_embedded() { "内置(嵌入二进制)" } else { "外部 PATH" },
        ffmpeg::path().display()
    );

    // 优雅退出：收到信号时广播 shutdown，进行中的录制据此收尾（写完 mp4 moov）；tasks 收集句柄供等待
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let rec_tasks: record::RecTasks = Arc::new(tokio::sync::Mutex::new(Vec::new()));

    // 直播流监听：维护「当前活跃流」，供「点击录制」时判断能否录 + 订阅（不再自动全程录制）
    let rec: record::SharedRec = Arc::new(RwLock::new(record::RecStore::default()));
    record::spawn_monitor(stream_hub.get_client_event_consumer(), rec.clone(), room);
    let hub_sender = stream_hub.get_hub_event_sender();

    // RTMP 接收端（可选保留）
    let mut rtmp_server = RtmpServer::new(
        rtmp_addr.clone(),
        stream_hub.get_hub_event_sender(),
        GOP_NUM,
        None,
    );
    tokio::spawn(async move {
        if let Err(e) = rtmp_server.run().await {
            log::error!("RTMP 服务退出: {e}");
        }
    });
    log::info!("RTMP 接收已启动 rtmp://{rtmp_addr}/live/<streamKey>（可选）");

    // WebRTC 端：同端口处理 WHIP(推) 与 WHEP(播)
    let mut webrtc_server = WebRTCServer::new(
        whep_addr.clone(),
        stream_hub.get_hub_event_sender(),
        None,
    );
    tokio::spawn(async move {
        if let Err(e) = webrtc_server.run().await {
            log::error!("WebRTC 服务退出: {e}");
        }
    });
    log::info!("WHIP 推流 http://{whep_addr}/whip?app=live&stream=<key>");
    log::info!("WHEP 播放 http://{whep_addr}/whep?app=live&stream=<key>");

    // 内网页面 + 配置接口 + 录制接口（axum :8000）
    let web_app = web::router(web::WebState {
        cfg: cfg.clone(),
        tx: cfg_tx.clone(),
        rec: rec.clone(),
        hub: hub_sender,
        shutdown: shutdown_rx,
        tasks: rec_tasks.clone(),
    });
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(&web_addr).await {
            Ok(listener) => {
                log::info!("内网页面已启动 http://{web_addr}（管理页 + 观看页）");
                if let Err(e) = axum::serve(listener, web_app).await {
                    log::error!("web 服务退出: {e}");
                }
            }
            Err(e) => log::error!("绑定 {web_addr} 失败: {e}"),
        }
    });

    // 事件循环 vs 退出信号：任一先到即结束主流程
    tokio::select! {
        _ = stream_hub.run() => log::warn!("StreamsHub 事件循环退出，进程结束"),
        _ = wait_for_signal() => log::info!("收到退出信号，开始优雅关闭…"),
    }

    // 广播 shutdown → 各录制 task 关 stdin、等 ffmpeg 写完 ENDLIST 收尾；最多等 8s 兜底
    let _ = shutdown_tx.send(true);
    let handles: Vec<_> = rec_tasks.lock().await.drain(..).collect();
    if !handles.is_empty() {
        log::info!("等待 {} 场录制写完收尾…", handles.len());
        let _ = tokio::time::timeout(Duration::from_secs(8), futures::future::join_all(handles)).await;
    }
    log::info!("已优雅退出");
}

/// 等待退出信号，注册后覆盖默认「直接终止」、改走优雅收尾：
/// - unix：SIGINT(Ctrl+C)/SIGTERM/SIGHUP(关终端窗口)
/// - windows：Ctrl+C / Ctrl+Break / 关闭控制台窗口(CTRL_CLOSE) / 注销 / 关机
///   注意 windows 的 CTRL_CLOSE 只给约 5s 宽限即强杀，故收尾要快（录制收尾通常 <1s）。
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT");
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM");
        let mut sighup = signal(SignalKind::hangup()).expect("SIGHUP");
        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
            _ = sighup.recv() => {},
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let mut ctrl_c = windows::ctrl_c().expect("ctrl_c");
        let mut ctrl_break = windows::ctrl_break().expect("ctrl_break");
        let mut ctrl_close = windows::ctrl_close().expect("ctrl_close");
        let mut ctrl_logoff = windows::ctrl_logoff().expect("ctrl_logoff");
        let mut ctrl_shutdown = windows::ctrl_shutdown().expect("ctrl_shutdown");
        tokio::select! {
            _ = ctrl_c.recv() => {},
            _ = ctrl_break.recv() => {},
            _ = ctrl_close.recv() => {},
            _ = ctrl_logoff.recv() => {},
            _ = ctrl_shutdown.recv() => {},
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
