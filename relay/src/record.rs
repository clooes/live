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

/// AAC-LC / 48000Hz / 立体声 的 7 字节 ADTS 头。
/// whip 端 Opus→AAC 转码出的是无头 raw AAC，喂 ffmpeg `-f aac` 需要每帧带 ADTS 头。
/// 参数与 whip.rs 里 `Mpeg4Aac::new(2, 48000, 2)` 对应（object type 2 / 48k / 2ch）。
fn adts_header(payload_len: usize) -> [u8; 7] {
    let framelen = (payload_len + 7) as u32; // 含 7 字节头
    const PROFILE: u8 = 1; // AAC-LC：object type 2 → ADTS profile = 2-1
    const FREQ_IDX: u8 = 3; // 48000Hz
    const CHAN: u8 = 2; // 立体声
    [
        0xFF,
        0xF1, // syncword(12) + MPEG-4(0) + layer(00) + protection_absent(1)
        (PROFILE << 6) | (FREQ_IDX << 2) | (CHAN >> 2),
        ((CHAN & 3) << 6) | ((framelen >> 11) as u8 & 0x03),
        ((framelen >> 3) & 0xFF) as u8,
        (((framelen & 7) as u8) << 5) | 0x1F,
        0xFC, // buffer fullness 低位 + num_raw_blocks-1=0
    ]
}

/// 创建 Unix 命名管道（给 ffmpeg 的第二路音频输入）。Windows 无 mkfifo，R10 首版仅 mac/Linux（D15）。
#[cfg(unix)]
fn mkfifo_at(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let r = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// 标记某场录制已结束（live=false + 结束时刻）。
async fn mark_ended(rec: &SharedRec, session_id: &str) {
    let mut s = rec.write().await;
    if let Some(sess) = s.sessions.iter_mut().find(|x| x.id == session_id) {
        sess.live = false;
        sess.ended_at_ms = Some(now_ms());
    }
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

    let m3u8 = dir.join("index.m3u8");
    let seg = dir.join("%d.ts");

    // 登记 session（ffmpeg 延后到「探测出有无音频」后再启动，故先登记为直播中）
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
        // ---- 预备阶段：探测这路流有无音频，同时缓冲视频帧 ----
        // 音频一般与视频同时到达；最多等 PREAMBLE_MS，仍无音频即判定纯视频，
        // 避免给 ffmpeg 挂一路「永远没数据的音频输入」导致它空等卡死（D14 静默出片的前提）。
        const PREAMBLE_MS: u64 = 1500;
        let preamble_start = now_ms();
        let mut has_audio = false;
        let mut ended_in_preamble = false;
        loop {
            let elapsed = now_ms().saturating_sub(preamble_start);
            if elapsed >= PREAMBLE_MS {
                break;
            }
            let remaining = Duration::from_millis(PREAMBLE_MS - elapsed);
            tokio::select! {
                frame = frame_rx.recv() => match frame {
                    // 探测期视频「直接丢弃」不缓冲：若缓冲后再一次性 flush，wallclock 会把这批帧挤成
                    // 同一时刻 → DTS 非单调 → HLS 时间轴错乱、切片按 PDT 墙钟对齐会选不到片。
                    // 音频通常与视频同时到达，故丢弃的开头极短（<探测时长），可接受。
                    Some(FrameData::Video { .. }) => {}
                    // 首个音频帧是 AAC 配置头(ASC)，丢弃即可（ADTS 自带配置）；见到即判定有音频
                    Some(FrameData::Audio { .. }) => { has_audio = true; break; }
                    Some(_) => {}
                    None => { ended_in_preamble = true; break; }
                },
                _ = shutdown.changed() => { ended_in_preamble = true; break; }
                _ = tokio::time::sleep(remaining) => break,
            }
        }
        log::info!("录制探测完成 session={session_id} 音频={}", if has_audio { "有" } else { "无" });

        // ---- 据探测结果拉起 ffmpeg：有音频=视频(stdin)+音频(fifo)双路；无音频=纯视频单路 ----
        let audio_fifo = dir.join("audio.aac");
        let ff_log = std::fs::File::create(dir.join("ffmpeg.log")).ok();
        let mut cmd = Command::new(crate::ffmpeg::path());
        cmd.args(["-hide_banner", "-loglevel", "warning"])
            .args(["-analyzeduration", "10000000", "-probesize", "10000000"])
            // -thread_queue_size：多输入 mux 时给每路足够队列，避免一路读取阻塞另一路
            .args(["-thread_queue_size", "512"])
            .args(["-use_wallclock_as_timestamps", "1"])
            .args(["-f", "h264", "-i", "pipe:0"]);
        if has_audio {
            let _ = std::fs::remove_file(&audio_fifo);
            #[cfg(unix)]
            {
                if let Err(e) = mkfifo_at(&audio_fifo) {
                    log::warn!("创建音频管道失败，降级为纯视频 session={session_id}: {e}");
                    has_audio = false;
                }
            }
            #[cfg(not(unix))]
            {
                log::warn!("非 Unix 暂不支持音频录制（R10 待补 Windows 命名管道），降级为纯视频");
                has_audio = false;
            }
            if has_audio {
                // 音频第二路也按墙钟打时间戳，与视频近似同步（D13：先墙钟近似）
                cmd.args(["-thread_queue_size", "512"])
                    .args(["-use_wallclock_as_timestamps", "1"])
                    .args(["-f", "aac", "-i"]).arg(&audio_fifo);
            }
        }
        if has_audio {
            cmd.args(["-c:v", "copy", "-c:a", "copy"]);
        } else {
            cmd.args(["-c:v", "copy"]);
        }
        cmd.args(["-f", "hls"])
            .args(["-hls_time", &SEGMENT_SECS.to_string()])
            .args(["-hls_list_size", "0"]) // 保留全部片（录制）
            .args(["-hls_flags", "append_list+program_date_time"])
            .arg("-hls_segment_filename").arg(&seg)
            .arg(&m3u8)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(ff_log.map(Stdio::from).unwrap_or_else(Stdio::null));
        // 脱离前台进程组：关终端的 SIGHUP 不直接杀它，改由 relay 收到信号后关管道、等它写完 ENDLIST（见 main.rs）
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                log::error!("拉起 ffmpeg 失败（PATH 是否有 ffmpeg?）session={session_id}: {e}");
                mark_ended(&rec, &session_id).await;
                write_meta(&stream, &session_id, started, Some(now_ms())).await;
                return;
            }
        };
        let mut stdin = child.stdin.take().expect("ffmpeg stdin");

        // 打开音频管道写端。关键：用 O_RDWR（read+write）打开，避免只写打开阻塞——
        // ffmpeg 是顺序探测输入的（先探视频 pipe:0，探完才开音频 fifo 读端），
        // 若这里阻塞等 ffmpeg 开读端，而 ffmpeg 又在等视频数据才探完视频，就会死锁、HLS 永不产出。
        // O_RDWR 打开命名管道不阻塞（自身即读端），relay 得以立刻喂视频、打破死锁。
        let mut audio_w: Option<tokio::fs::File> = None;
        if has_audio {
            match std::fs::OpenOptions::new().read(true).write(true).open(&audio_fifo) {
                Ok(f) => audio_w = Some(tokio::fs::File::from_std(f)),
                Err(e) => log::warn!("打开音频管道失败，音频将缺失 session={session_id}: {e}"),
            }
        }

        // ---- 主收帧循环（视频从此刻实时喂，wallclock 单调，不再有 flush 突变）----
        let mut nv = 0u64;
        let mut na = 0u64;
        if !ended_in_preamble {
            loop {
                tokio::select! {
                    frame = frame_rx.recv() => match frame {
                        Some(FrameData::Video { data, .. }) => {
                            nv += 1;
                            if let Err(e) = stdin.write_all(&data).await {
                                log::warn!("写视频到 ffmpeg 失败（已退出?）session={session_id}: {e}");
                                break;
                            }
                        }
                        Some(FrameData::Audio { data, .. }) => {
                            if let Some(w) = audio_w.as_mut() {
                                na += 1;
                                // whip 发的是 raw AAC，加 ADTS 头再喂 ffmpeg
                                let hdr = adts_header(data.len());
                                if w.write_all(&hdr).await.is_err() || w.write_all(&data).await.is_err() {
                                    log::warn!("写音频管道失败，后续只录视频 session={session_id}");
                                    audio_w = None;
                                }
                            }
                        }
                        Some(_) => {}
                        None => break, // 停流：所有发布者已 drop
                    },
                    _ = shutdown.changed() => {
                        log::info!("收到退出信号，收尾录制 session={session_id}");
                        break;
                    }
                }
            }
        }
        log::info!("录制收帧结束 video={nv} audio={na} session={session_id}");

        // 关两路写端 → ffmpeg 收尾写 ENDLIST 成 VOD
        drop(stdin);
        drop(audio_w);
        let _ = child.wait().await;
        let _ = std::fs::remove_file(&audio_fifo);

        mark_ended(&rec, &session_id).await;
        write_meta(&stream, &session_id, started, Some(now_ms())).await;
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
