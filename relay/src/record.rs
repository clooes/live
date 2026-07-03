//! 分段录制：用户点「开始录制」时，当场起一路 ffmpeg，把当前直播的视频+音频
//! 按所选清晰度直接录成一个成品 mp4；点「停止」即结束、文件就绪、直接下载。
//!
//! 不再全程录 HLS、不再按墙钟裁剪（那套依赖 program_date_time 对齐，脆弱易错）。
//! 音频：whip 已把 Opus 转成 AAC 发进 frame 通道，这里接住 + 补 ADTS 头，走 ffmpeg 第二路输入（命名管道）。

use std::collections::HashMap;
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

/// 当前活跃直播流（用于「能否录制」判断 + 录制时订阅）。
#[derive(Debug, Clone, Serialize)]
pub struct ActiveStream {
    pub app: String,
    pub stream: String,
}

/// 一个录制片段（成品 mp4）。
#[derive(Debug, Clone, Serialize)]
pub struct Recording {
    pub id: String,
    /// 归属浏览器（前端 localStorage 里的随机 uid）；用于「我的录制」按用户隔离、离开再回来能停自己的。
    pub owner: String,
    pub quality: String,
    pub status: String, // recording | done | error
    pub file: Option<String>,
    pub size: Option<String>,
    pub error: Option<String>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
}

/// 录制共享状态。
#[derive(Default)]
pub struct RecStore {
    /// 当前正在推的直播流（None = 无流可录）。
    pub current: Option<ActiveStream>,
    /// 录制片段列表（最新在前）。
    pub recordings: Vec<Recording>,
    /// 进行中录制的停止信号（id → sender）；点「停止」时 take 出来 send。
    pub stops: HashMap<String, oneshot::Sender<()>>,
}

pub type SharedRec = Arc<RwLock<RecStore>>;

/// 收集录制 task 句柄，供优雅退出时等待各段录制写完 moov 收尾。
pub type RecTasks = Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>;

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

/// 录制片段输出目录（配置的数据根目录下，默认二进制同目录 data/clips）。
pub fn clips_dir() -> PathBuf {
    crate::config::data_root().join("clips")
}

/// 按清晰度名生成 ffmpeg 视频编码参数。`original`（或无法解析）→ 直拷 `-c:v copy`；
/// `<N>p` → scale 到高 N + libx264 重编码。
fn video_enc_args(quality: &str) -> Vec<String> {
    match quality.strip_suffix('p').and_then(|s| s.parse::<u32>().ok()) {
        Some(h) => vec![
            "-vf".into(), format!("scale=-2:{h}"), // -2 保持宽高比且宽为偶数
            "-c:v".into(), "libx264".into(),
            "-preset".into(), "veryfast".into(),
            "-crf".into(), "23".into(),
        ],
        None => vec!["-c:v".into(), "copy".into()],
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
/// 中途订阅录制时，只有从带 SPS/PPS 的关键帧起步，`-c:v copy` 才能干净解码/拷贝。
/// 兼容 3 字节(00 00 01)与 4 字节(00 00 00 01)起始码。
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
fn mkfifo_at(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let r = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// 监听 client-event，维护「当前活跃直播流」：目标 room 开播即记录、断流即清空。
pub fn spawn_monitor(mut client_rx: BroadcastEventReceiver, rec: SharedRec, room: String) {
    tokio::spawn(async move {
        log::info!("直播流监听已启动，目标房间 {room}");
        loop {
            match client_rx.recv().await {
                Ok(BroadcastEvent::Publish { identifier }) => {
                    if let StreamIdentifier::WebRTC { app_name, stream_name } = identifier {
                        if stream_name == room {
                            log::info!("直播流上线 app={app_name} stream={stream_name}");
                            rec.write().await.current =
                                Some(ActiveStream { app: app_name, stream: stream_name });
                        }
                    }
                }
                Ok(BroadcastEvent::UnPublish { identifier }) => {
                    if let StreamIdentifier::WebRTC { stream_name, .. } = identifier {
                        if stream_name == room {
                            log::info!("直播流下线 stream={stream_name}");
                            rec.write().await.current = None;
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

/// 点「开始录制」：订阅当前直播流 → 起 ffmpeg 按 `quality` 录成品 mp4（有声）。返回录制 id。
/// 无直播流时返回 Err。
pub async fn start_recording(
    hub: StreamHubEventSender,
    rec: SharedRec,
    quality: String,
    owner: String,
    shutdown: watch::Receiver<bool>,
    tasks: RecTasks,
) -> Result<String, String> {
    let Some(active) = rec.read().await.current.clone() else {
        return Err("当前没有正在直播的流，无法录制".into());
    };

    // 订阅 Frame（同 WHEP 拉流，拿 Annex-B 视频帧 + AAC 音频帧）
    let sub_info = SubscriberInfo {
        id: Uuid::new(RandomDigitCount::Four),
        sub_type: SubscribeType::WhepPull,
        notify_info: NotifyInfo { request_url: String::new(), remote_addr: String::new() },
        sub_data_type: SubDataType::Frame,
    };
    let identifier = StreamIdentifier::WebRTC {
        app_name: active.app.clone(),
        stream_name: active.stream.clone(),
    };
    let (tx, rx) = oneshot::channel();
    if hub
        .send(StreamHubEvent::Subscribe { identifier, info: sub_info, result_sender: tx })
        .is_err()
    {
        return Err("订阅直播流失败（流可能已断）".into());
    }
    let frame_rx = match rx.await {
        Ok(Ok(data)) => match data.0.frame_receiver {
            Some(r) => r,
            None => return Err("订阅无 frame_receiver".into()),
        },
        _ => return Err("订阅结果错误（流可能已断）".into()),
    };

    let started = now_ms();
    let id = started.to_string();
    let file_name = format!("rec_{id}_{quality}.mp4");
    let output = clips_dir().join(&file_name);
    if let Some(parent) = output.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    {
        let mut s = rec.write().await;
        s.recordings.insert(0, Recording {
            id: id.clone(),
            owner: owner.clone(),
            quality: quality.clone(),
            status: "recording".into(),
            file: None,
            size: None,
            error: None,
            started_at_ms: started,
            ended_at_ms: None,
        });
        s.stops.insert(id.clone(), stop_tx);
    }
    log::info!(target: "user_ops", "开始录制 id={id} quality={quality} stream={} → {}", active.stream, output.display());

    let handle = tokio::spawn(record_task(
        frame_rx, quality, output, file_name, id.clone(), rec.clone(), stop_rx, shutdown,
    ));
    tasks.lock().await.push(handle);
    Ok(id)
}

/// 点「停止录制」：发停止信号给对应任务，任务收尾写完 mp4。
/// 幂等：若该录制已自然结束（流断/已收尾），只要它在列表里就当作已停止、返回成功，不报错。
pub async fn stop_recording(rec: &SharedRec, id: &str) -> Result<(), String> {
    let mut s = rec.write().await;
    if let Some(tx) = s.stops.remove(id) {
        let _ = tx.send(());
        return Ok(());
    }
    if s.recordings.iter().any(|r| r.id == id) {
        return Ok(()); // 已结束（流断/收尾完成），幂等成功
    }
    Err("无此录制".into())
}

/// 录制任务：探测有无音频 → 起 ffmpeg（视频 stdin + 可选音频 fifo）录 mp4 → 收尾标记 done/error。
#[allow(clippy::too_many_arguments)]
async fn record_task(
    mut frame_rx: streamhub::define::FrameDataReceiver,
    quality: String,
    output: PathBuf,
    file_name: String,
    id: String,
    rec: SharedRec,
    mut stop_rx: oneshot::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
) {
    let dir = output.parent().map(|p| p.to_path_buf()).unwrap_or_else(clips_dir);
    let audio_fifo = dir.join(format!("audio_{id}.aac"));

    // ---- 预备阶段：先攒够「一个能自解码的起点」再开 ffmpeg。----
    // 录制是中途订阅：H.264 的 SPS/PPS(解码头)只在关键帧(GOP 边界)随帧带出，
    // 若从任意 P 帧开录，`-c:v copy`(original) 会一直 non-existing PPS / no frame、不消费 stdin，
    // relay 写视频被背压阻塞、3s 超时被迫早停（480p 走 libx264 解码容错故看似正常）。
    // 对策：缓冲视频直到收到含 SPS 的帧，并从该帧起灌（丢弃之前的无头帧），让 copy 从关键帧干净起步；
    // 同时用 [MIN,MAX] 窗口判定有无音频（不再一见音频就 break，否则可能一帧视频/解码头都没攒到）。
    const PREAMBLE_MIN_MS: u64 = 500;  // 至少等这么久，够判定有无音频
    const PREAMBLE_MAX_MS: u64 = 3000; // 最多等这么久去等一个含 SPS 的关键帧（覆盖常见 GOP）
    let preamble_start = now_ms();
    let mut pending_video = Vec::new();
    let mut sps_at: Option<usize> = None; // pending_video 中最后一个含 SPS 的帧下标
    let mut has_audio = false;
    let mut ended_early = false;
    loop {
        let elapsed = now_ms().saturating_sub(preamble_start);
        if elapsed >= PREAMBLE_MAX_MS { break; }
        // 已拿到解码头且过了最短判定窗 → 可以开录
        if sps_at.is_some() && elapsed >= PREAMBLE_MIN_MS { break; }
        let remaining = Duration::from_millis(PREAMBLE_MAX_MS - elapsed);
        tokio::select! {
            frame = frame_rx.recv() => match frame {
                Some(FrameData::Video { data, .. }) => {
                    if annexb_has_sps(&data) { sps_at = Some(pending_video.len()); }
                    pending_video.push(data);
                }
                Some(FrameData::Audio { .. }) => has_audio = true, // 记下有音频，但继续等 SPS
                Some(_) => {}
                None => { ended_early = true; break; }
            },
            _ = &mut stop_rx => { ended_early = true; break; }
            _ = shutdown.changed() => { ended_early = true; break; }
            _ = tokio::time::sleep(remaining) => break,
        }
    }
    // 从最后一个含 SPS 的帧起（丢弃它之前的无头帧）；没等到 SPS 就原样灌（尽力而为）。
    if let Some(idx) = sps_at { pending_video.drain(0..idx); }
    log::info!(
        "录制探测完成 id={id} 音频={} 起点SPS={} 缓冲视频帧={}",
        if has_audio { "有" } else { "无" },
        if sps_at.is_some() { "已捕获" } else { "未捕获" },
        pending_video.len(),
    );

    // ---- 拉起 ffmpeg：视频 pipe + 可选音频 fifo，按清晰度编码，输出成品 mp4 ----
    let ff_log = std::fs::File::create(dir.join(format!("ffmpeg_{id}.log"))).ok();
    let mut cmd = Command::new(crate::ffmpeg::path());
    // analyzeduration/probesize 取小值：让 ffmpeg 尽快探测完视频、开始读音频 fifo，
    // 否则探测期不读音频 → fifo 缓冲(64KB)写满 → relay 写音频阻塞、主循环卡死收不到停止。
    cmd.args(["-hide_banner", "-loglevel", "warning"])
        .args(["-analyzeduration", "1000000", "-probesize", "1000000"])
        .args(["-thread_queue_size", "512"])
        .args(["-use_wallclock_as_timestamps", "1"])
        .args(["-f", "h264", "-i", "pipe:0"]);
    if has_audio {
        let _ = std::fs::remove_file(&audio_fifo);
        #[cfg(unix)]
        {
            if let Err(e) = mkfifo_at(&audio_fifo) {
                log::warn!("创建音频管道失败，降级为纯视频 id={id}: {e}");
                has_audio = false;
            }
        }
        #[cfg(not(unix))]
        { has_audio = false; }
        if has_audio {
            cmd.args(["-thread_queue_size", "512"])
                .args(["-use_wallclock_as_timestamps", "1"])
                .args(["-f", "aac", "-i"]).arg(&audio_fifo);
        }
    }
    // 视频按清晰度编码；音频源已是 AAC，直拷即可（不额外重编码）
    for a in video_enc_args(&quality) { cmd.arg(a); }
    if has_audio { cmd.args(["-c:a", "copy"]); }
    cmd.args(["-movflags", "+faststart"]) // moov 前置，边下边播/网页播放友好
        .arg(&output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(ff_log.map(Stdio::from).unwrap_or_else(Stdio::null));
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log::error!("拉起 ffmpeg 失败 id={id}: {e}");
            finish(&rec, &id, Err(format!("拉起 ffmpeg 失败: {e}")), &file_name, &output).await;
            return;
        }
    };
    let mut stdin = child.stdin.take().expect("ffmpeg stdin");

    // 音频管道写端用 O_RDWR 打开（不阻塞，避免与 ffmpeg 顺序探测输入死锁）
    let mut audio_w: Option<tokio::fs::File> = None;
    if has_audio {
        match std::fs::OpenOptions::new().read(true).write(true).open(&audio_fifo) {
            Ok(f) => audio_w = Some(tokio::fs::File::from_std(f)),
            Err(e) => log::warn!("打开音频管道失败，音频将缺失 id={id}: {e}"),
        }
    }

    // 先灌入预备阶段缓冲的视频帧（含 SPS/PPS/首关键帧），copy 模式据此才能正常解码/拷贝
    let mut nv = 0u64;
    for data in pending_video.drain(..) {
        nv += 1;
        match tokio::time::timeout(Duration::from_secs(3), stdin.write_all(&data)).await {
            Ok(Ok(())) => {}
            _ => { log::warn!("灌入缓冲视频失败/超时 id={id}"); break; }
        }
    }

    // ---- 主收帧循环：视频→stdin，音频→fifo；停止/停流/退出即收尾 ----
    let mut na = 0u64;
    if !ended_early {
        loop {
            tokio::select! {
                frame = frame_rx.recv() => match frame {
                    Some(FrameData::Video { data, .. }) => {
                        nv += 1;
                        // 写视频加超时：ffmpeg 若卡住不读 stdin，不至于永久阻塞主循环、收不到停止。
                        match tokio::time::timeout(Duration::from_secs(3), stdin.write_all(&data)).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => { log::warn!("写视频失败（ffmpeg 已退出?）id={id}: {e}"); break; }
                            Err(_) => { log::warn!("写视频超时 3s，ffmpeg 疑似卡住，结束录制 id={id}"); break; }
                        }
                    }
                    Some(FrameData::Audio { data, .. }) => {
                        if let Some(w) = audio_w.as_mut() {
                            na += 1;
                            let hdr = adts_header(data.len());
                            // 音频写超时保护：ffmpeg 若一时没读音频（探测/卡顿），fifo 满不至于
                            // 永久阻塞主循环、收不到停止；超时则丢弃该帧继续（正常不触发）。
                            let write = async {
                                w.write_all(&hdr).await?;
                                w.write_all(&data).await
                            };
                            match tokio::time::timeout(Duration::from_millis(800), write).await {
                                Ok(Ok(())) => {}
                                Ok(Err(_)) => {
                                    log::warn!("写音频管道失败，后续只录视频 id={id}");
                                    audio_w = None;
                                }
                                Err(_) => { /* 超时：丢弃该音频帧，避免卡死 */ }
                            }
                        }
                    }
                    Some(_) => {}
                    None => { log::info!("直播流已断，录制结束 id={id}"); break; }
                },
                _ = &mut stop_rx => { log::info!("收到停止，收尾录制 id={id}"); break; }
                _ = shutdown.changed() => { log::info!("进程退出，收尾录制 id={id}"); break; }
            }
        }
    }
    log::info!("录制收帧结束 video={nv} audio={na} id={id}");

    // 关两路写端 → ffmpeg EOF 收尾写 moov。加超时强杀兜底：
    // 流异常时 ffmpeg 可能不自行退出，若不兜底任务会永久卡在这、状态一直「录制中」。
    drop(stdin);
    drop(audio_w);
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            log::warn!("ffmpeg 收尾超时(10s)，强制结束 id={id}");
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
    }
    let _ = std::fs::remove_file(&audio_fifo);

    let result = if nv == 0 {
        Err("未录到任何视频帧（区间过短或流已断）".to_string())
    } else {
        Ok(())
    };
    finish(&rec, &id, result, &file_name, &output).await;
}

/// 收尾：更新该 recording 的状态（done + 文件/大小，或 error），并清理 stop 信号。
async fn finish(rec: &SharedRec, id: &str, result: Result<(), String>, file_name: &str, output: &std::path::Path) {
    let mut s = rec.write().await;
    s.stops.remove(id);
    if let Some(r) = s.recordings.iter_mut().find(|x| x.id == id) {
        r.ended_at_ms = Some(now_ms());
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
                log::error!("录制失败 id={id}: {e}");
            }
        }
    }
}
