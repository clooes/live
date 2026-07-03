//! 录制：整场后台常录 + 停止时按标记裁剪。
//!
//! 直播一上线（`BroadcastEvent::Publish`）就起**一路持久 ffmpeg**，把整场连续录成一个
//! fragmented mp4（原画 `-c copy`，`data/sessions/<id>/full.mp4`），并记锚点 T0（首帧墙钟）。
//! 用户点「录制」只是**记一个起点标记**（瞬时，不起 ffmpeg）；点「停止」置终点、后台起一路
//! 裁剪任务，从 full.mp4 按 `offset = 按钮墙钟 - T0` 切出成品（original 秒切、480p/720p 重编码）。
//!
//! 这样绕开了旧「HLS+PROGRAM-DATE-TIME 裁剪」的 PDT 解析/分片覆盖/开播空档三大坑：
//! 单文件、单锚点、从开播就录（天然带 SPS/PPS），墙钟↔媒体时间用 `-use_wallclock_as_timestamps` 对齐。
//! 音频：whip 已把 Opus 转 AAC 发进 frame 通道，这里补 ADTS 头走 ffmpeg 第二路输入（命名管道，仅 unix）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use streamhub::define::{
    BroadcastEvent, BroadcastEventReceiver, FrameData, FrameDataReceiver, NotifyInfo,
    StreamHubEvent, StreamHubEventSender, SubDataType, SubscribeType, SubscriberInfo,
};
use streamhub::stream::StreamIdentifier;
use streamhub::utils::{RandomDigitCount, Uuid};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{oneshot, watch, RwLock};
use tokio::task::JoinHandle;
use tokio::time::sleep;

/// 当前活跃直播流（用于「能否录制」判断）。
#[derive(Debug, Clone, Serialize)]
pub struct ActiveStream {
    pub app: String,
    pub stream: String,
}

/// 当前会话的连续录制信息（裁剪时据此定位 full.mp4 与时间锚点）。
#[derive(Debug, Clone)]
pub struct SessionRec {
    pub id: String,
    pub full: PathBuf,
    /// 首帧墙钟（ms）——full.mp4 媒体时间 0 对应的真实时间，裁剪 offset 的基准。
    pub t0_ms: u64,
    pub has_audio: bool,
}

/// 一条录制（用户标记的一段 → 裁出的成品 mp4）。
#[derive(Debug, Clone, Serialize)]
pub struct Recording {
    pub id: String,
    /// 归属浏览器（前端 localStorage 的随机 uid）：「我的录制」按用户隔离，离开再回来能停自己的。
    pub owner: String,
    pub quality: String,
    pub status: String, // recording(标记中) | cutting(裁剪中) | done | error
    pub file: Option<String>,
    pub size: Option<String>,
    pub error: Option<String>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    // ---- 裁剪所需快照（不下发前端）：标记创建时从会话取，停播/停止后仍可据此切 ----
    #[serde(skip)]
    pub session_id: String,
    #[serde(skip)]
    pub t0_ms: u64,
    #[serde(skip)]
    pub full: PathBuf,
    #[serde(skip)]
    pub has_audio: bool,
}

/// 录制共享状态。
#[derive(Default)]
pub struct RecStore {
    /// 当前正在推的直播流（None = 无流可录）。
    pub current: Option<ActiveStream>,
    /// 当前会话的连续录制（None = 录制器未就绪）。
    pub session: Option<SessionRec>,
    /// 停当前会话录制器的信号（UnPublish/新开播时 take 出来 send）。
    pub session_stop: Option<oneshot::Sender<()>>,
    /// 录制列表（最新在前）。
    pub recordings: Vec<Recording>,
}

pub type SharedRec = Arc<RwLock<RecStore>>;

/// 收集后台 task 句柄（会话录制器 + 裁剪），供优雅退出时等待收尾。
pub type RecTasks = Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>;

/// 会话连续文件保留期：停播后 full.mp4 保留供事后裁剪，超期在下次开播/启动时清理。
const RETENTION_HOURS: u64 = 24;

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// 字节数转人类可读。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut i = 0;
    while size >= 1024.0 && i < UNITS.len() - 1 {
        size /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{size:.1} {}", UNITS[i]) }
}

/// 裁剪成品输出目录。
pub fn clips_dir() -> PathBuf {
    crate::config::data_root().join("clips")
}

/// 整场连续录制目录（每个会话一个子目录）。
pub fn sessions_dir() -> PathBuf {
    crate::config::data_root().join("sessions")
}

/// 清理超过保留期的会话目录（在开播/启动时调用，同步阻塞、量小）。
fn clean_old_sessions() {
    let dir = sessions_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else { return; };
    let now = std::time::SystemTime::now();
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() { continue; }
        let expired = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs() > RETENTION_HOURS * 3600)
            .unwrap_or(false);
        if expired {
            let _ = std::fs::remove_dir_all(&p);
            log::info!("清理过期会话录制 {}", p.display());
        }
    }
}

/// AAC-LC / 48000Hz / 立体声 的 7 字节 ADTS 头。
/// whip 端 Opus→AAC 转码出的是无头 raw AAC，喂 ffmpeg `-f aac` 需要每帧带 ADTS 头。
fn adts_header(payload_len: usize) -> [u8; 7] {
    let framelen = (payload_len + 7) as u32;
    const PROFILE: u8 = 1; // AAC-LC：object type 2 → ADTS profile = 2-1
    const FREQ_IDX: u8 = 3; // 48000Hz
    const CHAN: u8 = 2; // 立体声
    [
        0xFF,
        0xF1,
        (PROFILE << 6) | (FREQ_IDX << 2) | (CHAN >> 2),
        ((CHAN & 3) << 6) | ((framelen >> 11) as u8 & 0x03),
        ((framelen >> 3) & 0xFF) as u8,
        (((framelen & 7) as u8) << 5) | 0x1F,
        0xFC,
    ]
}

/// 扫描 Annex-B 码流里是否含 SPS（NAL type 7）——即该帧是否携带解码头。
/// 只有从带 SPS/PPS 的关键帧起步，`-c:v copy` 才能干净解码/拷贝。兼容 3/4 字节起始码。
fn annexb_has_sps(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if data[i + 3] & 0x1F == 7 { return true; } // NAL type 7 = SPS
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// 创建 Unix 命名管道（给 ffmpeg 的第二路音频输入）。Windows 无 mkfifo，音频仅 mac/Linux。
#[cfg(unix)]
fn mkfifo_at(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let r = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// 订阅某流的 Frame 通道，返回 (接收端, 退订用 identifier, 退订用 info)。
async fn subscribe_frames(
    hub: &StreamHubEventSender,
    app: &str,
    stream: &str,
) -> Result<(FrameDataReceiver, StreamIdentifier, SubscriberInfo), String> {
    let sub_info = SubscriberInfo {
        id: Uuid::new(RandomDigitCount::Four),
        sub_type: SubscribeType::WhepPull,
        notify_info: NotifyInfo { request_url: String::new(), remote_addr: String::new() },
        sub_data_type: SubDataType::Frame,
    };
    let identifier = StreamIdentifier::WebRTC {
        app_name: app.to_string(),
        stream_name: stream.to_string(),
    };
    let unsub_id = identifier.clone();
    let unsub_info = sub_info.clone();
    let (tx, rx) = oneshot::channel();
    if hub
        .send(StreamHubEvent::Subscribe { identifier, info: sub_info, result_sender: tx })
        .is_err()
    {
        return Err("订阅直播流失败（流可能已断）".into());
    }
    let frame_rx = match rx.await {
        Ok(Ok(data)) => data.0.frame_receiver.ok_or("订阅无 frame_receiver")?,
        _ => return Err("订阅结果错误（流可能已断）".into()),
    };
    Ok((frame_rx, unsub_id, unsub_info))
}

/// 监听 client-event：目标 room 开播 → 起整场连续录制器；断流 → 停录制器（收尾 + 收尾挂起标记）。
pub fn spawn_monitor(
    mut client_rx: BroadcastEventReceiver,
    rec: SharedRec,
    room: String,
    hub: StreamHubEventSender,
    shutdown: watch::Receiver<bool>,
    tasks: RecTasks,
) {
    tokio::spawn(async move {
        log::info!("直播流监听已启动，目标房间 {room}");
        loop {
            match client_rx.recv().await {
                Ok(BroadcastEvent::Publish { identifier }) => {
                    if let StreamIdentifier::WebRTC { app_name, stream_name } = identifier {
                        if stream_name == room {
                            log::info!("直播流上线 app={app_name} stream={stream_name}");
                            on_publish(&rec, &hub, &shutdown, &tasks, app_name, stream_name).await;
                        }
                    }
                }
                Ok(BroadcastEvent::UnPublish { identifier }) => {
                    if let StreamIdentifier::WebRTC { stream_name, .. } = identifier {
                        if stream_name == room {
                            log::info!("直播流下线 stream={stream_name}");
                            let mut s = rec.write().await;
                            s.current = None;
                            if let Some(stop) = s.session_stop.take() {
                                let _ = stop.send(()); // 通知录制器收尾（它会收尾 full.mp4 再切挂起标记）
                            }
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

/// 开播：清过期会话 → 订阅 → 起整场连续录制器。
async fn on_publish(
    rec: &SharedRec,
    hub: &StreamHubEventSender,
    shutdown: &watch::Receiver<bool>,
    tasks: &RecTasks,
    app: String,
    stream: String,
) {
    let _ = tokio::task::spawn_blocking(clean_old_sessions);

    let (frame_rx, unsub_id, unsub_info) = match subscribe_frames(hub, &app, &stream).await {
        Ok(v) => v,
        Err(e) => {
            log::error!("整场录制订阅失败：{e}");
            return;
        }
    };

    let session_id = now_ms().to_string();
    let dir = sessions_dir().join(&session_id);
    let full = dir.join("full.mp4");
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    {
        let mut s = rec.write().await;
        s.current = Some(ActiveStream { app, stream });
        // 若还有上一场录制器（异常未清），先停掉
        if let Some(old) = s.session_stop.take() {
            let _ = old.send(());
        }
        s.session_stop = Some(stop_tx);
    }

    let handle = tokio::spawn(session_recorder(
        frame_rx, session_id, dir, full, rec.clone(), stop_rx, shutdown.clone(),
        hub.clone(), unsub_id, unsub_info, tasks.clone(),
    ));
    tasks.lock().await.push(handle);
}

/// 点「开始录制」：仅记一个起点标记（瞬时，不起 ffmpeg）。无就绪会话时返回 Err。
pub async fn start_recording(rec: &SharedRec, quality: String, owner: String) -> Result<String, String> {
    let mut s = rec.write().await;
    let Some(session) = s.session.clone() else {
        return Err("录制器尚未就绪（无直播流或刚开播），请稍候再试".into());
    };
    let started = now_ms();
    let id = format!("{started}{}", Uuid::new(RandomDigitCount::Four));
    s.recordings.insert(0, Recording {
        id: id.clone(),
        owner,
        quality: quality.clone(),
        status: "recording".into(),
        file: None,
        size: None,
        error: None,
        started_at_ms: started,
        ended_at_ms: None,
        session_id: session.id,
        t0_ms: session.t0_ms,
        full: session.full,
        has_audio: session.has_audio,
    });
    log::info!(target: "user_ops", "标记录制起点 id={id} quality={quality}");
    Ok(id)
}

/// 点「停止录制」：置终点 → 后台起裁剪任务。幂等：已在裁剪/完成则直接成功。
pub async fn stop_recording(rec: &SharedRec, tasks: &RecTasks, id: &str) -> Result<(), String> {
    {
        let mut s = rec.write().await;
        let Some(r) = s.recordings.iter_mut().find(|r| r.id == id) else {
            return Err("无此录制".into());
        };
        if r.status != "recording" {
            return Ok(()); // 已在裁剪/完成，幂等成功
        }
        r.ended_at_ms = Some(now_ms());
        r.status = "cutting".into();
    }
    log::info!(target: "user_ops", "停止录制，转裁剪 id={id}");
    spawn_cut(rec.clone(), tasks.clone(), id.to_string()).await;
    Ok(())
}

/// 起一路裁剪任务（后台），并把句柄收进 tasks 供优雅退出等待。
async fn spawn_cut(rec: SharedRec, tasks: RecTasks, id: String) {
    let handle = tokio::spawn(cut_mark(rec, id));
    tasks.lock().await.push(handle);
}

/// 从 full.mp4 裁出该标记区间的成品，并更新状态。
async fn cut_mark(rec: SharedRec, id: String) {
    let snap = {
        let s = rec.read().await;
        s.recordings.iter().find(|r| r.id == id).map(|r| {
            (
                r.full.clone(),
                r.t0_ms,
                r.started_at_ms,
                r.ended_at_ms.unwrap_or_else(now_ms),
                r.quality.clone(),
                r.has_audio,
            )
        })
    };
    let Some((full, t0, start_ms, end_ms, quality, has_audio)) = snap else { return; };

    // 等尾部落盘：停止时切的是直播边缘，fragmented mp4 最后 ~1-2s 尚未成形（partial file），
    // 直接切会短一截。等一下让覆盖 [start,end] 的分片刷完（会话已结束的 full.mp4 已 finalize，等亦无害）。
    tokio::time::sleep(Duration::from_millis(2000)).await;

    let start_s = start_ms.saturating_sub(t0) as f64 / 1000.0;
    let dur_s = end_ms.saturating_sub(start_ms) as f64 / 1000.0;
    let file_name = format!("clip_{id}_{quality}.mp4");
    let output = clips_dir().join(&file_name);
    log::info!(target: "user_ops", "开始裁剪 id={id} quality={quality} start={start_s:.1}s dur={dur_s:.1}s");

    let res = cut_clip(&full, start_s, dur_s, &quality, has_audio, &output).await;
    finish(&rec, &id, res, &file_name, &output).await;
}

/// 从连续 full.mp4 切 [start_s, start_s+dur_s) → output。
/// original → `-c copy`（输入侧 seek，关键帧吸附，秒级）；`<N>p` → scale + libx264 重编码。
async fn cut_clip(
    full: &Path,
    start_s: f64,
    dur_s: f64,
    quality: &str,
    has_audio: bool,
    output: &Path,
) -> Result<(), String> {
    if dur_s <= 0.2 {
        return Err("录制时长过短".into());
    }
    if !full.exists() {
        return Err("整场文件不存在（录制器未就绪或已清理）".into());
    }
    if let Some(p) = output.parent() {
        let _ = tokio::fs::create_dir_all(p).await;
    }

    let mut cmd = Command::new(crate::ffmpeg::path());
    cmd.args(["-hide_banner", "-loglevel", "warning", "-y"])
        .args(["-ss", &format!("{start_s:.3}")]) // 输入侧 seek：快，落到 <=start 的关键帧
        .arg("-i")
        .arg(full)
        .args(["-t", &format!("{dur_s:.3}")]);
    match quality.strip_suffix('p').and_then(|s| s.parse::<u32>().ok()) {
        Some(h) => {
            cmd.args(["-vf", &format!("scale=-2:{h}")]) // -2 保持宽高比且宽为偶数
                .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "23"]);
            if has_audio { cmd.args(["-c:a", "aac"]); }
        }
        None => { cmd.args(["-c", "copy"]); } // 原画直拷
    }
    cmd.args(["-movflags", "+faststart"]) // moov 前置，网页/边下边播友好
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let out = cmd.output().await.map_err(|e| format!("起 ffmpeg 失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffmpeg 裁剪失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// 整场连续录制器：整场录成 fragmented mp4（原画 copy）；记 T0；停播/退出收尾 + 收尾挂起标记。
#[allow(clippy::too_many_arguments)]
async fn session_recorder(
    mut frame_rx: FrameDataReceiver,
    session_id: String,
    dir: PathBuf,
    full: PathBuf,
    rec: SharedRec,
    mut stop_rx: oneshot::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
    hub: StreamHubEventSender,
    unsub_id: StreamIdentifier,
    unsub_info: SubscriberInfo,
    tasks: RecTasks,
) {
    let _ = tokio::fs::create_dir_all(&dir).await;
    let audio_fifo = dir.join("audio.aac");

    // ---- 预备阶段：攒到含 SPS 的关键帧再开 ffmpeg，并判定有无音频（[MIN,MAX] 窗口）----
    // 判定窗从「首帧到达」起算，而非从 Publish 起算：WHIP 握手(ICE/DTLS)要 ~1s，
    // 媒体帧才开始流；若从 Publish 计时，首个视频帧到时早已过 min 窗 → 一见 SPS 就开录、
    // 根本没等到音频（opus→aac 首帧比视频稍晚）。收到首帧后再宽限一段看有没有音频。
    const AUDIO_GRACE_MS: u64 = 500; // 收到首帧后等这么久判定有无音频
    const PREAMBLE_MAX_MS: u64 = 6000; // 从 Publish 起的总上限（容忍慢握手）
    let preamble_start = now_ms();
    let mut first_media_ms: Option<u64> = None; // 首帧(视频/音频)到达时刻
    let mut pending_video: Vec<_> = Vec::new();
    let mut sps_at: Option<usize> = None;
    let mut has_audio = false;
    let mut ended_early = false;
    loop {
        if now_ms().saturating_sub(preamble_start) >= PREAMBLE_MAX_MS { break; }
        // 拿到解码头 + （已见音频 或 首帧后已等够判定窗）→ 开录
        if let (Some(_), Some(fm)) = (sps_at, first_media_ms) {
            if has_audio || now_ms().saturating_sub(fm) >= AUDIO_GRACE_MS { break; }
        }
        tokio::select! {
            frame = frame_rx.recv() => match frame {
                Some(FrameData::Video { data, .. }) => {
                    if first_media_ms.is_none() { first_media_ms = Some(now_ms()); }
                    if annexb_has_sps(&data) { sps_at = Some(pending_video.len()); }
                    pending_video.push(data);
                }
                Some(FrameData::Audio { .. }) => {
                    if first_media_ms.is_none() { first_media_ms = Some(now_ms()); }
                    has_audio = true;
                }
                Some(_) => {}
                None => { ended_early = true; break; }
            },
            _ = &mut stop_rx => { ended_early = true; break; }
            _ = shutdown.changed() => { ended_early = true; break; }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {} // 定期复查退出条件
        }
    }
    if let Some(idx) = sps_at { pending_video.drain(0..idx); }

    if ended_early && pending_video.is_empty() {
        log::info!("整场录制未起播即结束 session={session_id}");
        let _ = hub.send(StreamHubEvent::UnSubscribe { identifier: unsub_id, info: unsub_info });
        let mut s = rec.write().await;
        if s.session.as_ref().map(|x| x.id == session_id).unwrap_or(false) {
            s.session = None;
        }
        return;
    }

    // ---- 拉起持久 ffmpeg：视频 stdin + 可选音频 fifo → fragmented mp4（原画直拷）----
    let ff_log = std::fs::File::create(dir.join("ffmpeg.log")).ok();
    let mut cmd = Command::new(crate::ffmpeg::path());
    cmd.args(["-hide_banner", "-loglevel", "warning"])
        .args(["-analyzeduration", "500000", "-probesize", "500000"])
        // 视频 pipe 缓冲调大：ffmpeg 打开/探测第二路(音频 fifo)时不读视频，
        // 缓冲小会瞬间塞满 → relay 写视频阻塞超时。给足缓冲扛过探测窗口。
        .args(["-thread_queue_size", "4096"])
        .args(["-use_wallclock_as_timestamps", "1"]) // PTS 跟真实时间，墙钟↔媒体时间不漂移
        .args(["-f", "h264", "-i", "pipe:0"]);
    if has_audio {
        let _ = std::fs::remove_file(&audio_fifo);
        #[cfg(unix)]
        {
            if let Err(e) = mkfifo_at(&audio_fifo) {
                log::warn!("创建音频管道失败，整场降级为纯视频 session={session_id}: {e}");
                has_audio = false;
            }
        }
        #[cfg(not(unix))]
        { has_audio = false; }
        if has_audio {
            // 音频输入极小 probe：-f aac 格式已知，不需长探测；否则默认探测 ~5s，
            // 期间 ffmpeg 只读音频不读视频 → 视频 pipe 塞满、写视频超时卡死。
            cmd.args(["-analyzeduration", "0", "-probesize", "32"])
                .args(["-thread_queue_size", "4096"])
                .args(["-use_wallclock_as_timestamps", "1"])
                .args(["-f", "aac", "-i"]).arg(&audio_fifo);
        }
    }
    cmd.args(["-c:v", "copy"]); // 视频无损直拷（原画）
    // 音频重编码 AAC：wallclock 毫秒戳下音频帧成串到达会 DTS 倒退，两路 copy 的 muxer 为交错
    // 而卡住 → 交错缓冲写满 → ffmpeg 停读视频 stdin → 写视频超时。重编码让编码器按采样数
    // 重生成单调时戳（开销极小），根除音频 DTS 非单调 + ADTS→ASC 问题。
    // aresample=async=1000：wallclock 戳有抖动/倒退（Queue input is backward in time），
    // 用异步重采样按输出时钟补/丢样本，产出连续单调音频，消除卡死。
    if has_audio { cmd.args(["-af", "aresample=async=1000", "-c:a", "aac", "-b:a", "128k"]); }
    // -max_interleave_delta 0：不为音视频交错而阻塞，杜绝两路输入互相等待的卡死。
    cmd.args(["-max_interleave_delta", "0"]);
    // fragmented mp4：边写边可裁、崩溃可播（不需 faststart）；
    // frag_duration 0.5s：每 0.5s 强制刷一个分片，让可读边缘紧贴直播（否则尾部数秒未成形无法裁）。
    cmd.args(["-frag_duration", "500000"])
        // flush_packets：每写一包就 flush AVIO 缓冲到盘，否则分片攒在 ffmpeg 写缓冲里、
        // 裁剪读端看不到最新数据 → 停止时切的成品仍短一截。
        .args(["-flush_packets", "1"])
        .args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof"])
        .arg(&full)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(ff_log.map(Stdio::from).unwrap_or_else(Stdio::null));
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log::error!("拉起整场 ffmpeg 失败 session={session_id}: {e}");
            let _ = hub.send(StreamHubEvent::UnSubscribe { identifier: unsub_id, info: unsub_info });
            return;
        }
    };
    let mut stdin = child.stdin.take().expect("ffmpeg stdin");

    let mut audio_w: Option<tokio::fs::File> = None;
    if has_audio {
        match std::fs::OpenOptions::new().read(true).write(true).open(&audio_fifo) {
            Ok(f) => audio_w = Some(tokio::fs::File::from_std(f)),
            Err(e) => log::warn!("打开音频管道失败，音频将缺失 session={session_id}: {e}"),
        }
    }

    // T0：full.mp4 媒体时间 0 对应的墙钟。就在灌首帧前取，与用户标记的墙钟同一时钟基准。
    let t0_ms = now_ms();
    {
        let mut s = rec.write().await;
        s.session = Some(SessionRec { id: session_id.clone(), full: full.clone(), t0_ms, has_audio });
    }
    log::info!(target: "user_ops", "整场录制已开始 session={session_id} 音频={} → {}", if has_audio { "有" } else { "无" }, full.display());

    // 灌入预备缓冲（含 SPS/PPS/首关键帧）
    let mut nv = 0u64;
    for data in pending_video.drain(..) {
        nv += 1;
        match tokio::time::timeout(Duration::from_secs(3), stdin.write_all(&data)).await {
            Ok(Ok(())) => {}
            _ => { log::warn!("灌入缓冲视频失败/超时 session={session_id}"); break; }
        }
    }

    // ---- 主收帧循环 ----
    let mut na = 0u64;
    if !ended_early {
        loop {
            tokio::select! {
                frame = frame_rx.recv() => match frame {
                    Some(FrameData::Video { data, .. }) => {
                        nv += 1;
                        match tokio::time::timeout(Duration::from_secs(3), stdin.write_all(&data)).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => { log::warn!("整场写视频失败（ffmpeg 退出?）session={session_id}: {e}"); break; }
                            Err(_) => { log::warn!("整场写视频超时 3s session={session_id}"); break; }
                        }
                    }
                    Some(FrameData::Audio { data, .. }) => {
                        if let Some(w) = audio_w.as_mut() {
                            na += 1;
                            let hdr = adts_header(data.len());
                            let write = async {
                                w.write_all(&hdr).await?;
                                w.write_all(&data).await
                            };
                            match tokio::time::timeout(Duration::from_millis(800), write).await {
                                Ok(Ok(())) => {}
                                Ok(Err(_)) => { log::warn!("整场写音频失败，后续只录视频 session={session_id}"); audio_w = None; }
                                Err(_) => { /* 超时：丢该音频帧 */ }
                            }
                        }
                    }
                    Some(_) => {}
                    None => { log::info!("直播流已断，整场录制结束 session={session_id}"); break; }
                },
                _ = &mut stop_rx => { log::info!("收到停止，整场录制收尾 session={session_id}"); break; }
                _ = shutdown.changed() => { log::info!("进程退出，整场录制收尾 session={session_id}"); break; }
            }
        }
    }

    // 退订 → 关写端 → 等 ffmpeg 收尾（超时强杀兜底）
    let _ = hub.send(StreamHubEvent::UnSubscribe { identifier: unsub_id, info: unsub_info });
    drop(stdin);
    drop(audio_w);
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            log::warn!("整场 ffmpeg 收尾超时(10s)，强制结束 session={session_id}");
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
    }
    let _ = std::fs::remove_file(&audio_fifo);
    let end_ms = now_ms();
    log::info!("整场录制收帧结束 video={nv} audio={na} session={session_id}");

    // 清会话 + 收尾此会话仍打开的标记（full.mp4 已收尾，可安全裁剪）。
    let pending: Vec<String> = {
        let mut s = rec.write().await;
        if s.session.as_ref().map(|x| x.id == session_id).unwrap_or(false) {
            s.session = None;
            s.session_stop = None;
        }
        s.recordings
            .iter_mut()
            .filter(|r| r.session_id == session_id && r.status == "recording")
            .map(|r| {
                r.ended_at_ms = Some(end_ms);
                r.status = "cutting".into();
                r.id.clone()
            })
            .collect()
    };
    // 会话结束顺带把挂起标记切掉——inline await（不 spawn），使本任务句柄在优雅退出时覆盖这些裁剪。
    for id in pending {
        log::info!(target: "user_ops", "会话结束，收尾挂起标记 id={id} session={session_id}");
        cut_mark(rec.clone(), id).await;
    }
    let _ = tasks; // 句柄由调用方（on_publish）收集
}

/// 收尾：更新该 recording 的状态（done + 文件/大小，或 error）。
async fn finish(rec: &SharedRec, id: &str, result: Result<(), String>, file_name: &str, output: &Path) {
    let mut s = rec.write().await;
    if let Some(r) = s.recordings.iter_mut().find(|x| x.id == id) {
        r.ended_at_ms = r.ended_at_ms.or_else(|| Some(now_ms()));
        match result {
            Ok(()) => {
                r.status = "done".into();
                r.file = Some(file_name.to_string());
                r.size = std::fs::metadata(output).map(|m| human_size(m.len())).ok();
                log::info!(target: "user_ops", "录制完成可下载 id={id} file={file_name} size={:?}", r.size);
            }
            Err(e) => {
                r.status = "error".into();
                r.error = Some(e.clone());
                let _ = std::fs::remove_file(output);
                log::error!("裁剪失败 id={id}: {e}");
            }
        }
    }
}
