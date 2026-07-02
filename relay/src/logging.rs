//! 分类文件日志（R5-b）。
//!
//! 全项目（含 vendor）统一用 `log::` 宏；这里用 tracing-subscriber 接管：
//! `.init()` 装上 tracing-log 桥接，log 记录进入 tracing 管道，再按 **log target** 分流到
//! `<data_root>/logs/` 下三个按天滚动的文件，同时保留控制台（stderr）全量输出：
//!   - target = "viewers"  → viewers.log   （进入直播间 / WHEP 接入）
//!   - target = "user_ops" → user-ops.log  （用户操作：录制起止、切片/下载）
//!   - 其余（模块路径默认 target）→ system.log（系统与第三方库日志）
//!
//! 日志级别沿用 RUST_LOG（缺省 info），与原 env_logger 行为一致。

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// 用于分类的两个专用 target；其余一律归 system。
const T_VIEWERS: &str = "viewers";
const T_USER_OPS: &str = "user_ops";

/// 初始化日志。返回的 `WorkerGuard` 必须在进程存活期间保活（drop 会丢未刷盘日志），
/// 故调用方应把返回值绑定到 main 的局部变量直到退出。
pub fn init(data_root: &Path) -> Vec<WorkerGuard> {
    let logs_dir = data_root.join("logs");
    if let Err(e) = std::fs::create_dir_all(&logs_dir) {
        eprintln!("创建日志目录失败 {}：{e}", logs_dir.display());
    }

    let (sys_w, g_sys) = tracing_appender::non_blocking(
        tracing_appender::rolling::daily(&logs_dir, "system.log"),
    );
    let (ops_w, g_ops) = tracing_appender::non_blocking(
        tracing_appender::rolling::daily(&logs_dir, "user-ops.log"),
    );
    let (vie_w, g_vie) = tracing_appender::non_blocking(
        tracing_appender::rolling::daily(&logs_dir, "viewers.log"),
    );

    // 全局级别过滤（RUST_LOG，缺省 info）。压掉 WebRTC 底层库的刷屏日志——
    // 尤其 webrtc_ice 在 ICE 尚未配对候选时每 ~200ms 刷一条
    // "pingAllCandidates called with no candidate pairs" WARN，纯噪音，降到 error。
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,webrtc_ice=error,webrtc=warn,webrtc_dtls=error,webrtc_sctp=error,webrtc_srtp=error",
        )
    });

    // 控制台：全量，保留彩色（TTY 下）
    let console = fmt::layer().with_writer(std::io::stderr);
    // 文件层：关彩色（避免写入 ANSI 转义），各自按 target 过滤
    let system = fmt::layer().with_ansi(false).with_writer(sys_w).with_filter(
        filter_fn(|m| m.target() != T_VIEWERS && m.target() != T_USER_OPS),
    );
    let user_ops = fmt::layer()
        .with_ansi(false)
        .with_writer(ops_w)
        .with_filter(filter_fn(|m| m.target() == T_USER_OPS));
    let viewers = fmt::layer()
        .with_ansi(false)
        .with_writer(vie_w)
        .with_filter(filter_fn(|m| m.target() == T_VIEWERS));

    tracing_subscriber::registry()
        .with(env)
        .with(console)
        .with(system)
        .with(user_ops)
        .with(viewers)
        .init(); // 同时安装 tracing-log 桥接，接管既有 log:: 宏

    vec![g_sys, g_ops, g_vie]
}
