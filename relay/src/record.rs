//! 录制管理器：把 WHIP 进来的 WebRTC 流录成连续 HLS（天然按时间分片）。
//!
//! xiu 的 WHIP 把视频解成 Annex-B 裸 H.264（帧数据在 FrameData::Video，起始码 00000001）。
//! 我们订阅该流的 FrameData，把裸流管道喂给 ffmpeg，由 ffmpeg 切成 HLS：
//!   ffmpeg -f h264 -i pipe:0 -c copy -f hls ... → data/recordings/<room>/<session>/index.m3u8 + <n>.ts
//! `-use_wallclock_as_timestamps 1` 让分片按墙钟时间，`program_date_time` 写入每片起始钟点，切片对齐更准。
//! need_record 语义由 `-hls_list_size 0`（保留全部片）实现；停流后 ffmpeg 写 ENDLIST 成 VOD。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use streamhub::define::{
    BroadcastEvent, BroadcastEventReceiver, FrameData, NotifyInfo, StreamHubEvent,
    StreamHubEventSender, SubDataType, SubscribeType, SubscriberInfo,
};
use streamhub::stream::StreamIdentifier;
use streamhub::utils::{RandomDigitCount, Uuid};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{oneshot, watch, RwLock};
use tokio::task::JoinHandle;
use tokio::time::sleep;

/// HLS 分片时长（秒）。
const SEGMENT_SECS: u32 = 2;

/// 一场录制。
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub room: String,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub live: bool,
}

/// 切片任务。
#[derive(Debug, Clone, Serialize)]
pub struct ClipJob {
    pub id: String,
    pub session_id: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub status: String, // processing | done | error
    pub file: Option<String>,
    pub size: Option<String>,
    pub error: Option<String>,
    pub created_at_ms: u64,
}

/// 「开始录制」标记：记住是在哪场、什么墙钟时刻按下的。
#[derive(Debug, Clone, Serialize)]
pub struct ClipMark {
    pub session_id: String,
    pub start_ms: u64,
}

/// 录制/切片共享状态。
#[derive(Default)]
pub struct RecStore {
    pub sessions: Vec<Session>, // 追加在尾
    pub mark: Option<ClipMark>, // 当前「开始录制」标记（未结束）
    pub jobs: Vec<ClipJob>,     // 最新在前
}

impl RecStore {
    /// 当前正在直播的 session。
    pub fn current_session(&self) -> Option<&Session> {
        self.sessions.iter().rev().find(|s| s.live)
    }
    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }
}

pub type SharedRec = Arc<RwLock<RecStore>>;

/// 收集所有录制 task 的句柄，供优雅退出时等待各场录制写完收尾。
pub type RecTasks = Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>;

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// 字节数转人类可读（切片文件大小展示用）。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut i = 0;
    while size >= 1024.0 && i < UNITS.len() - 1 {
        size /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[i])
    }
}

/// 某房间的录制根目录（在配置的数据根目录下，默认二进制同目录 data/）。
pub fn recordings_dir(room: &str) -> PathBuf {
    crate::config::data_root().join("recordings").join(room)
}

/// 启动录制管理器：监听 client-event，自动为目标 room 开录。
pub fn spawn(
    hub_sender: StreamHubEventSender,
    mut client_rx: BroadcastEventReceiver,
    rec: SharedRec,
    room: String,
    shutdown: watch::Receiver<bool>,
    tasks: RecTasks,
) {
    tokio::spawn(async move {
        log::info!("录制管理器已启动，目标房间 {room}");
        loop {
            match client_rx.recv().await {
                Ok(BroadcastEvent::Publish { identifier }) => {
                    if let StreamIdentifier::WebRTC { app_name, stream_name } = identifier {
                        if stream_name == room {
                            start_session(
                                hub_sender.clone(),
                                rec.clone(),
                                app_name,
                                stream_name,
                                shutdown.clone(),
                                tasks.clone(),
                            )
                            .await;
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log::warn!("client-event 接收错误: {e}，1s 后继续");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
}

/// 开一场录制：订阅 WebRTC 流的 FrameData(Annex-B) → 管道喂 ffmpeg 写 HLS。
async fn start_session(
    hub: StreamHubEventSender,
    rec: SharedRec,
    app: String,
    stream: String,
    mut shutdown: watch::Receiver<bool>,
    tasks: RecTasks,
) {
    let started = now_ms();
    let session_id = started.to_string();
    let dir = recordings_dir(&stream).join(&session_id);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        log::error!("建录制目录失败 {}: {e}", dir.display());
        return;
    }

    // 订阅 Frame：xiu WHIP 已把 RTP 解成 Annex-B 裸 H.264（FrameData::Video，含在带 SPS/PPS），
    // identifier 必须精确匹配发布者 WebRTC{app, stream}。
    let sub_info = SubscriberInfo {
        id: Uuid::new(RandomDigitCount::Four),
        sub_type: SubscribeType::WhepPull,
        notify_info: NotifyInfo { request_url: String::new(), remote_addr: String::new() },
        sub_data_type: SubDataType::Frame,
    };
    let identifier = StreamIdentifier::WebRTC { app_name: app, stream_name: stream.clone() };
    let (tx, rx) = oneshot::channel();
    if hub
        .send(StreamHubEvent::Subscribe { identifier, info: sub_info, result_sender: tx })
        .is_err()
    {
        log::error!("录制订阅发送失败 session={session_id}");
        return;
    }
    let mut frame_rx = match rx.await {
        Ok(Ok(data)) => match data.0.frame_receiver {
            Some(r) => r,
            None => { log::error!("录制订阅无 frame_receiver"); return; }
        },
        _ => { log::error!("录制订阅结果错误"); return; }
    };

    // 拉起 ffmpeg：裸 H264(Annex-B) 进 → HLS 出
    let m3u8 = dir.join("index.m3u8");
    let seg = dir.join("%d.ts");
    let ff_log = std::fs::File::create(dir.join("ffmpeg.log")).ok();
    let mut cmd = Command::new(crate::ffmpeg::path());
    cmd.args(["-hide_banner", "-loglevel", "warning"])
        .args(["-analyzeduration", "10000000", "-probesize", "10000000"])
        .args(["-use_wallclock_as_timestamps", "1"])
        .args(["-f", "h264", "-i", "pipe:0"])
        .args(["-c:v", "copy", "-f", "hls"])
        .args(["-hls_time", &SEGMENT_SECS.to_string()])
        .args(["-hls_list_size", "0"]) // 保留全部片（录制）
        .args(["-hls_flags", "append_list+program_date_time"])
        .arg("-hls_segment_filename").arg(&seg)
        .arg(&m3u8)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(ff_log.map(Stdio::from).unwrap_or_else(Stdio::null));
    // 让 ffmpeg 脱离 relay 的前台进程组：关终端窗口发给进程组的 SIGHUP 不会直接杀它，
    // 改由 relay 收到信号后主动关它 stdin、等它写完 ENDLIST 再退出（优雅收尾，见 main.rs）。
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => { log::error!("拉起 ffmpeg 失败（PATH 是否有 ffmpeg?）: {e}"); return; }
    };
    let mut stdin = child.stdin.take().expect("ffmpeg stdin");

    // 登记 session
    {
        let mut s = rec.write().await;
        s.sessions.push(Session {
            id: session_id.clone(),
            room: stream.clone(),
            started_at_ms: started,
            ended_at_ms: None,
            live: true,
        });
    }
    log::info!("▶ 开始录制 session={session_id} room={stream} → {}/", dir.display());

    let handle = tokio::spawn(async move {
        let mut nv = 0u64;
        // 收帧 → 写 ffmpeg stdin。两种收尾：frame_rx 返回 None（停流），或收到进程退出信号。
        loop {
            tokio::select! {
                frame = frame_rx.recv() => match frame {
                    Some(FrameData::Video { data, .. }) => {
                        nv += 1;
                        if nv <= 3 || nv.is_multiple_of(300) {
                            log::info!("录制写入视频帧 #{nv} ({}B) session={session_id}", data.len());
                        }
                        if let Err(e) = stdin.write_all(&data).await {
                            log::warn!("写 ffmpeg stdin 失败（ffmpeg 退出?）: {e}");
                            break;
                        }
                    }
                    Some(_) => {} // 首版只录视频；音频/MediaInfo 暂忽略
                    None => break, // 停流：所有发布者已 drop
                },
                _ = shutdown.changed() => {
                    log::info!("收到退出信号，收尾录制 session={session_id}");
                    break;
                }
            }
        }
        log::info!("录制收帧结束 video={nv} session={session_id}");
        // 关闭 stdin → ffmpeg 收尾写 ENDLIST，成 VOD
        drop(stdin);
        let _ = child.wait().await;

        let ended = now_ms();
        {
            let mut s = rec.write().await;
            if let Some(sess) = s.sessions.iter_mut().find(|x| x.id == session_id) {
                sess.live = false;
                sess.ended_at_ms = Some(ended);
            }
        }
        write_meta(&stream, &session_id, started, Some(ended)).await;
        log::info!("⏹ 录制结束 session={session_id}");
    });
    tasks.lock().await.push(handle);
}

/// 写 meta.json（回放/列表用）。
async fn write_meta(room: &str, session_id: &str, started: u64, ended: Option<u64>) {
    let dir = recordings_dir(room).join(session_id);
    let meta = serde_json::json!({
        "id": session_id,
        "room": room,
        "started_at_ms": started,
        "ended_at_ms": ended,
        "status": if ended.is_some() { "ended" } else { "live" },
        "playlist": "index.m3u8",
    });
    let _ = tokio::fs::create_dir_all(&dir).await;
    let _ = tokio::fs::write(dir.join("meta.json"), meta.to_string()).await;
}
