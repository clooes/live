//! RTMP→WebRTC 内部桥：让 `rtmp://host:1935/<app>/<stream>` 也能进 WebRTC 链路（播放 + 录制）。
//!
//! 背景：本项目是纯 WebRTC 路线，WHEP 播放端与录制器都只认 `StreamIdentifier::WebRTC`，而
//! xiu/streamhub **不做 RTMP→WebRTC 转封装**。所以 RTMP 直接推上来虽然能进 hub（Rtmp 身份），
//! 却既播不出也录不到。
//!
//! 做法：streamhub 在 `hls_enabled` 下对 **RTMP 发布也广播 `Publish` 事件**。本模块监听到 RTMP
//! 发布，就起一路 ffmpeg：**拉本机该 RTMP 流 → 重编码为 WebRTC 友好的 H264 baseline + Opus →
//! 裸 RTP 输出到本机 UDP**，由 `rtp_ingest` 收包并以 WebRTC 身份发布进 hub → WHEP 能播、
//! 录制器能录。RTMP 停发 → 杀 ffmpeg + 撤发布。桥只认 Rtmp 身份，不成环。
//!
//! 曾经的做法是 ffmpeg `-f whip` 回推本机 :8900，已废弃：whip 依赖 ffmpeg 的 DTLS 后端，
//! Windows 静态构建（gyan=GnuTLS、BtbN=SChannel）都无法与 webrtc-rs 完成 DTLS-SRTP 握手，
//! macOS 又没有含 whip 的静态构建（只能回退 homebrew）。改裸 RTP 后 whip muxer 不再是必需，
//! 各平台内置 ffmpeg 即可。详见 rtp_ingest.rs 模块注释与 docs/RTMP-桥与录制.md。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use streamhub::define::{BroadcastEvent, BroadcastEventReceiver, StreamHubEventSender};
use streamhub::stream::StreamIdentifier;
use tokio::process::Command;
use tokio::sync::{oneshot, watch};

use crate::rtp_ingest::RtpIngest;

/// 起 RTMP→WebRTC 桥的监听 task：按 RTMP 发布/停发起停对应的 ffmpeg + RTP 注入流。
///
/// 注意：streamhub 只广播 `Publish`、**从不广播 `UnPublish`**（vendor lib.rs 的 unpublish
/// 不发 client event）。所以「RTMP 停推 → 收桥」不能靠事件，靠的是桥 ffmpeg 拉流读到 EOF
/// 自行退出，由每桥的守护 task（`child.wait()`）负责撤发布，顺带把 ffmpeg 异常死亡也
/// 监控住（否则注入流成幽灵发布，下次推流 publish 撞 Exists 失败）。
pub fn spawn_rtmp_bridge(
    mut client_rx: BroadcastEventReceiver,
    hub_sender: StreamHubEventSender,
    rtmp_port: u16,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let ff = crate::ffmpeg::path();
        log::info!("RTMP→WebRTC 桥已启动（rtmp:{rtmp_port} → 裸 RTP 注入，ffmpeg={}）", ff.display());
        // 每路 RTMP 流一个守护 task，键 = (app, stream)；发 stop（或 drop sender）即收桥。
        let mut bridges: HashMap<(String, String), oneshot::Sender<()>> = HashMap::new();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        for (k, stop) in bridges.drain() {
                            let _ = stop.send(());
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
                        // 同名重复发布（异常未清）：先停旧桥，稍候片刻让旧注入流撤干净。
                        if let Some(old_stop) = bridges.remove(&key) {
                            let _ = old_stop.send(());
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                        match start_bridge(&ff, &hub_sender, rtmp_port, &app_name, &stream_name).await {
                            Ok(stop) => {
                                log::info!(target: "user_ops",
                                    "RTMP 推流上线，起桥转 WebRTC app={app_name} stream={stream_name}");
                                bridges.insert(key, stop);
                            }
                            Err(e) => log::error!("起 RTMP→WebRTC 桥失败 {app_name}/{stream_name}: {e}"),
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

/// 先发布注入流拿到收包端口，再起 ffmpeg：拉本机 RTMP → 重编码 → 两路裸 RTP 回送。
/// 视频重编码 H264 **baseline + zerolatency + repeat-headers**（每关键帧带 SPS/PPS，浏览器
/// 中途进场靠它），因为 RTMP 侧可能是 high profile / 带 B 帧，直拷进 WebRTC 常不兼容；音频转
/// **Opus 48k 立体声**（WebRTC 原生音频；RTMP 侧的 AAC 无法直进 WebRTC）。
/// pkt_size=1200 给 WHEP 侧 SRTP 封装留够 MTU 余量。
async fn start_bridge(
    ff: &Path,
    hub_sender: &StreamHubEventSender,
    rtmp_port: u16,
    app: &str,
    stream: &str,
) -> anyhow::Result<oneshot::Sender<()>> {
    let ingest = RtpIngest::start(hub_sender.clone(), app, stream).await?;

    let input = format!("rtmp://127.0.0.1:{rtmp_port}/{app}/{stream}");
    let video_out = format!("rtp://127.0.0.1:{}?pkt_size=1200", ingest.video_port);
    let audio_out = format!("rtp://127.0.0.1:{}?pkt_size=1200", ingest.audio_port);
    // ffmpeg stderr 落到 data_root/logs/bridge-<app>-<stream>.log，便于排查桥失败。
    let log_dir = crate::config::data_root().join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let ff_log = std::fs::File::create(log_dir.join(format!("bridge-{app}-{stream}.log"))).ok();
    let mut cmd = Command::new(ff);
    cmd.args(["-hide_banner", "-loglevel", "warning"])
        .args(["-i", &input])
        .args([
            "-map", "0:v:0",
            "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
            "-profile:v", "baseline", "-pix_fmt", "yuv420p",
        ])
        .args(["-x264-params", "repeat-headers=1:keyint=60:min-keyint=60:scenecut=0"])
        .args(["-payload_type", "96", "-f", "rtp", &video_out])
        .args(["-map", "0:a:0"])
        .args(["-c:a", "libopus", "-b:a", "128k", "-ar", "48000", "-ac", "2"])
        .args(["-payload_type", "111", "-f", "rtp", &audio_out])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(ff_log.map(Stdio::from).unwrap_or_else(Stdio::null));
    // 独立进程组：优雅退出时按 Child 精确 kill，不误伤（与录制 ffmpeg 一致）。
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            ingest.stop();
            return Err(e.into());
        }
    };

    // 守护 task：ffmpeg 退出（RTMP 断流的 EOF / 异常死亡）或收到 stop 信号，都撤发布收尾。
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let (app, stream) = (app.to_string(), stream.to_string());
    tokio::spawn(async move {
        tokio::select! {
            status = child.wait() => {
                // RTMP 正常断流 ffmpeg 也以非零码退出（demux io error），与异常死亡无法从
                // 退出码区分，统一提示去 bridge log 甄别。
                match status {
                    Ok(s) => log::info!(target: "user_ops",
                        "桥 ffmpeg 退出（{s}）app={app} stream={stream}，正常断流可忽略；若直播中断查 logs/bridge-{app}-{stream}.log"),
                    Err(e) => log::warn!("等待桥 ffmpeg 退出失败 app={app} stream={stream}: {e}"),
                }
            }
            _ = &mut stop_rx => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                log::info!(target: "user_ops", "停桥 app={app} stream={stream}");
            }
        }
        ingest.stop();
    });
    Ok(stop_tx)
}
