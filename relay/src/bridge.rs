//! RTMP→WHIP 内部桥：让 `rtmp://host:1935/<app>/<stream>` 也能进 WebRTC 链路（播放 + 录制）。
//!
//! 背景：本项目是纯 WebRTC 路线，WHEP 播放端与录制器都只认 `StreamIdentifier::WebRTC`，而
//! xiu/streamhub **不做 RTMP→WebRTC 转封装**。所以 RTMP 直接推上来虽然能进 hub（Rtmp 身份），
//! 却既播不出也录不到。
//!
//! 做法：streamhub 在 `hls_enabled` 下对 **RTMP 发布也广播 `Publish` 事件**。本模块监听到 RTMP
//! 发布，就起一路 ffmpeg：**拉本机该 RTMP 流 → 重编码为 WebRTC 友好的 H264 baseline + Opus →
//! WHIP 回推本机 :webrtc**。于是 hub 里多出一路**同名的 WebRTC 流** → WHEP 能播、录制器能录。
//! RTMP 停发 → 杀掉对应 ffmpeg。WHIP 回推产生的是 WebRTC 发布，本模块只认 Rtmp，故**不成环**。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use streamhub::define::{BroadcastEvent, BroadcastEventReceiver};
use streamhub::stream::StreamIdentifier;
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// 起 RTMP→WHIP 桥的监听 task：按 RTMP 发布/停发起停对应的 ffmpeg 桥。
pub fn spawn_rtmp_bridge(
    mut client_rx: BroadcastEventReceiver,
    rtmp_port: u16,
    whep_port: u16,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        // 桥依赖带 whip muxer 的 ffmpeg：内置常未编入，探测不到就不起桥（RTMP 直推仍会进 hub，
        // 只是播不出/录不到——与加桥前一致），并给出清晰告警，避免静默失败。
        // whip 探测要跑 ffmpeg（可能先释放内置二进制 + Rosetta 首启，秒级），放 spawn_blocking，
        // 且在 spawn 之内，避免堵住主启动流程（web/webrtc/rtmp 监听）。
        let ff = match tokio::task::spawn_blocking(crate::ffmpeg::whip_path).await {
            Ok(Some(p)) => p,
            _ => {
                log::warn!("未找到含 whip muxer 的 ffmpeg，RTMP→WHIP 桥禁用：RTMP 推流将无法播放/录制。\
                            请把内置 ffmpeg 换成 8.1+（含 whip）的 build，或在 PATH 提供一个支持 whip 的 ffmpeg。");
                return;
            }
        };
        log::info!("RTMP→WHIP 桥已启动（rtmp:{rtmp_port} → whip:{whep_port}，ffmpeg={}）", ff.display());
        // 每路 RTMP 流一进程，键 = (app, stream)。
        let mut bridges: HashMap<(String, String), Child> = HashMap::new();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        for (k, mut c) in bridges.drain() {
                            let _ = c.start_kill();
                            log::info!("进程退出，停 RTMP 桥 {}/{}", k.0, k.1);
                        }
                        return;
                    }
                }
                ev = client_rx.recv() => match ev {
                    Ok(BroadcastEvent::Publish {
                        identifier: StreamIdentifier::Rtmp { app_name, stream_name },
                    }) => {
                        let key = (app_name.clone(), stream_name.clone());
                        // 同名重复发布（异常未清）：先杀旧桥。
                        if let Some(mut old) = bridges.remove(&key) { let _ = old.start_kill(); }
                        match start_bridge(&ff, rtmp_port, whep_port, &app_name, &stream_name) {
                            Ok(child) => {
                                log::info!(target: "user_ops",
                                    "RTMP 推流上线，起桥转 WHIP app={app_name} stream={stream_name}");
                                bridges.insert(key, child);
                            }
                            Err(e) => log::error!("起 RTMP→WHIP 桥失败 {app_name}/{stream_name}: {e}"),
                        }
                    }
                    Ok(BroadcastEvent::UnPublish {
                        identifier: StreamIdentifier::Rtmp { app_name, stream_name },
                    }) => {
                        if let Some(mut c) = bridges.remove(&(app_name.clone(), stream_name.clone())) {
                            let _ = c.start_kill();
                            log::info!(target: "user_ops",
                                "RTMP 推流下线，停桥 app={app_name} stream={stream_name}");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("RTMP 桥 client-event 接收错误: {e}，1s 后继续");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
    });
}

/// 起一路 ffmpeg：拉本机 RTMP → 重编码为 WebRTC 友好格式 → WHIP 回推本机。
/// 视频重编码 H264 **baseline + zerolatency + repeat-headers**（每关键帧带 SPS/PPS，WebRTC 收流与
/// 录制都要），因为 RTMP 侧可能是 high profile / 带 B 帧，直拷进 WebRTC 常不兼容；音频转 **Opus
/// 48k 立体声**（WebRTC 原生音频，顺带解决 RTMP 侧 AAC 无法直进 WebRTC）。
fn start_bridge(ff: &PathBuf, rtmp_port: u16, whep_port: u16, app: &str, stream: &str) -> std::io::Result<Child> {
    let input = format!("rtmp://127.0.0.1:{rtmp_port}/{app}/{stream}");
    let output = format!("http://127.0.0.1:{whep_port}/whip?app={app}&stream={stream}");
    // ffmpeg stderr 落到 data_root/logs/bridge-<app>-<stream>.log，便于排查桥失败（whip 握手等）。
    let log_dir = crate::config::data_root().join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let ff_log = std::fs::File::create(log_dir.join(format!("bridge-{app}-{stream}.log"))).ok();
    let mut cmd = Command::new(ff);
    cmd.args(["-hide_banner", "-loglevel", "warning"])
        .args(["-i", &input])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
            "-profile:v", "baseline", "-pix_fmt", "yuv420p",
        ])
        .args(["-x264-params", "repeat-headers=1:keyint=60:min-keyint=60:scenecut=0"])
        .args(["-c:a", "libopus", "-b:a", "128k", "-ar", "48000", "-ac", "2"])
        .args(["-f", "whip", &output])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(ff_log.map(Stdio::from).unwrap_or_else(Stdio::null));
    // 独立进程组：优雅退出时按 Child 精确 kill，不误伤（与录制 ffmpeg 一致）。
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.spawn()
}
