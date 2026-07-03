//! 录制：整场后台常录 + 停止时按标记裁剪。
//!
//! 直播一上线（`BroadcastEvent::Publish`）就起**一路持久 ffmpeg**，把整场连续录成一个
//! fragmented mp4（`data/sessions/<id>/full.mp4`），并记锚点 T0（起录墙钟）。
//! 用户点「录制」只是**记一个起点标记**（瞬时，不起 ffmpeg）；点「停止」置终点、后台起一路
//! 裁剪任务，从 full.mp4 按 `offset = 按钮墙钟 - T0` 切出成品（original 秒切、480p/720p 重编码）。
//!
//! 采集（批次 10）：订阅 **packet（原始 RTP）**，把视频/音频两路 RTP 各经一个 UDP 端口
//! 转发给 ffmpeg，配一份 SDP 描述（视频 H264/90000、音频 opus/48000/2，PT 用 RTP 头里协商出的
//! 动态值）。ffmpeg 用 **RTP 原生时间戳** 复用两路：视频 `-c:v copy` 直拷；音频重编码 AAC 并加
//! `aresample=async=1` 把 DTX/丢包/抖动造成的时戳空洞拉成连续单调音频（否则每约 5s 杂音/断续）→
//! 整场 `full.mp4`（h264 copy + aac）。这样根除了旧「裸帧 + 管道 + 墙钟现打戳」的 DTS 倒退 /
//! 两路交错卡死 / ADTS→mp4 / 探测卡住等坑。RTP 无 EOF，停止靠给 ffmpeg 发 **SIGINT** 收尾写 moov。
//! 裁剪时音频已是 AAC，直接 `-c:a copy`（不重复编码）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use streamhub::define::{
    BroadcastEvent, BroadcastEventReceiver, NotifyInfo, PacketData, PacketDataReceiver,
    StreamHubEvent, StreamHubEventSender, SubDataType, SubscribeType, SubscriberInfo,
};
use streamhub::stream::StreamIdentifier;
use streamhub::utils::{RandomDigitCount, Uuid};
use tokio::net::UdpSocket;
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

/// 读 RTP 包头里的 payload type（第 2 字节低 7 位）。喂 ffmpeg 的 SDP 需声明真实 PT，
/// 否则 ffmpeg 收到 PT 与 SDP 不符的包会当未知流丢弃。
fn rtp_payload_type(pkt: &[u8]) -> Option<u8> {
    if pkt.len() < 2 { return None; }
    Some(pkt[1] & 0x7F)
}

/// 跳过 RTP 头（12 字节固定 + CSRC + 可选扩展），返回 payload 切片。
fn rtp_payload(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() < 12 { return None; }
    let cc = (pkt[0] & 0x0F) as usize;
    let has_ext = pkt[0] & 0x10 != 0;
    let mut off = 12 + cc * 4;
    if has_ext {
        if pkt.len() < off + 4 { return None; }
        let ext_words = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
        off += 4 + ext_words * 4;
    }
    if off > pkt.len() { return None; }
    Some(&pkt[off..])
}

/// 从一个视频 RTP 包里抽取 H264 参数集 NAL 字节（含 NAL 头字节）：返回该包内出现的 (SPS, PPS)。
/// SPS(type 7)+PPS(type 8) 是解码头，`-c:v copy` 必须两者俱全才能定尺寸、写出 mp4。我们把抽到的
/// SPS/PPS 通过 SDP 的 `sprop-parameter-sets` 带外喂给 ffmpeg（见 build_sdp），这样即便 RTP 里的
/// 参数集包在起步瞬间丢了，ffmpeg 也有解码头兜底，不再刷 "non-existing PPS / no frame"。
/// 处理单 NAL 与 STAP-A 聚合（WebRTC 常把 SPS/PPS 打成 STAP-A）；FU-A 分片的参数集极罕见，不处理。
fn rtp_h264_params(pkt: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let Some(payload) = rtp_payload(pkt) else { return (None, None); };
    if payload.is_empty() { return (None, None); }
    let mut sps = None;
    let mut pps = None;
    match payload[0] & 0x1F {
        7 => sps = Some(payload.to_vec()), // 单 NAL：SPS
        8 => pps = Some(payload.to_vec()), // 单 NAL：PPS
        24 => {
            // STAP-A：[NAL头][NAL大小 u16][NAL]...，逐个抽 SPS/PPS
            let mut i = 1;
            while i + 2 <= payload.len() {
                let sz = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                i += 2;
                if i + sz > payload.len() { break; }
                let nal = &payload[i..i + sz];
                match nal.first().map(|b| b & 0x1F) {
                    Some(7) => sps = Some(nal.to_vec()),
                    Some(8) => pps = Some(nal.to_vec()),
                    _ => {}
                }
                i += sz;
            }
        }
        _ => {}
    }
    (sps, pps)
}

/// 标准 base64 编码（仅用于 SDP 的 sprop-parameter-sets，量小，内联省一个依赖）。
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((*c.get(1).unwrap_or(&0) as u32) << 8)
            | (*c.get(2).unwrap_or(&0) as u32);
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// 挑一个空闲 UDP 端口（bind 到 :0 拿到内核分配的端口后立即释放，交给 ffmpeg 去 bind）。
/// localhost 上从释放到 ffmpeg 抢占有极小竞争窗口，实测可接受。
fn pick_udp_port() -> std::io::Result<u16> {
    let s = std::net::UdpSocket::bind("127.0.0.1:0")?;
    Ok(s.local_addr()?.port())
}

/// 生成喂 ffmpeg 的 SDP：视频（H264/90000）恒有；有音频再加一路 opus/48000/2。
/// PT 用 RTP 头里协商出的动态值；packetization-mode=1 让 ffmpeg 正确重组分片的 H264 NAL。
/// `sprop`（"<b64 SPS>,<b64 PPS>"）经 sprop-parameter-sets 把解码头带外交给 ffmpeg，
/// 免得起步时 RTP 里的 SPS/PPS 包一丢就整段解不出帧、录不下视频。
fn build_sdp(vport: u16, vpt: u8, sprop: &str, audio: Option<(u16, u8)>) -> String {
    let mut sdp = format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 127.0.0.1\r\n\
         s=relay\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=video {vport} RTP/AVP {vpt}\r\n\
         a=rtpmap:{vpt} H264/90000\r\n\
         a=fmtp:{vpt} packetization-mode=1;sprop-parameter-sets={sprop}\r\n"
    );
    if let Some((aport, apt)) = audio {
        sdp.push_str(&format!(
            "m=audio {aport} RTP/AVP {apt}\r\n\
             a=rtpmap:{apt} opus/48000/2\r\n"
        ));
    }
    sdp
}

/// 订阅某流的 Packet（原始 RTP）通道，返回 (接收端, 退订用 identifier, 退订用 info)。
async fn subscribe_packets(
    hub: &StreamHubEventSender,
    app: &str,
    stream: &str,
) -> Result<(PacketDataReceiver, StreamIdentifier, SubscriberInfo), String> {
    let sub_info = SubscriberInfo {
        id: Uuid::new(RandomDigitCount::Four),
        sub_type: SubscribeType::WhepPull,
        notify_info: NotifyInfo { request_url: String::new(), remote_addr: String::new() },
        sub_data_type: SubDataType::Packet,
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
    let packet_rx = match rx.await {
        Ok(Ok(data)) => data.0.packet_receiver.ok_or("订阅无 packet_receiver")?,
        _ => return Err("订阅结果错误（流可能已断）".into()),
    };
    Ok((packet_rx, unsub_id, unsub_info))
}

/// 监听 client-event：目标 room 开播 → 起整场连续录制器；断流 → 停录制器（收尾 + 收尾挂起标记）。
///
/// 两种源分流录制：
/// - **RTMP 源**（OBS 推 RTMP，桥转 WHIP 供直播）：录制**直接从 RTMP 拉、`-c copy` 无损**（音质最好、
///   无哧哧底噪）。此时桥会另发一个同名 **WebRTC** 发布事件——那只供直播，录制要**忽略**它避免录两遍。
/// - **WHIP 源**（OBS 直接 WHIP，无 RTMP）：走原 RTP 录制路（订阅 packet + SPS/PPS + Opus→AAC）。
pub fn spawn_monitor(
    mut client_rx: BroadcastEventReceiver,
    rec: SharedRec,
    room: String,
    hub: StreamHubEventSender,
    shutdown: watch::Receiver<bool>,
    tasks: RecTasks,
    rtmp_port: u16,
) {
    tokio::spawn(async move {
        log::info!("直播流监听已启动，目标房间 {room}");
        // 本 room 当前是否有 RTMP 源在推：决定 WebRTC 发布是「桥的产物（忽略）」还是「WHIP 直推（要录）」。
        let mut rtmp_active = false;
        loop {
            match client_rx.recv().await {
                Ok(BroadcastEvent::Publish { identifier }) => match identifier {
                    StreamIdentifier::Rtmp { app_name, stream_name } if stream_name == room => {
                        log::info!("RTMP 直播流上线 app={app_name} stream={stream_name}（直录）");
                        rtmp_active = true;
                        on_publish_rtmp(&rec, &shutdown, &tasks, rtmp_port, app_name, stream_name).await;
                    }
                    StreamIdentifier::WebRTC { app_name, stream_name } if stream_name == room => {
                        if rtmp_active {
                            // 桥把 RTMP 转成的 WebRTC 流：仅供直播，录制已 RTMP 直录，跳过避免录两遍。
                            log::info!("跳过桥 WebRTC 发布（RTMP 直录中）stream={stream_name}");
                        } else {
                            log::info!("直播流上线 app={app_name} stream={stream_name}");
                            on_publish(&rec, &hub, &shutdown, &tasks, app_name, stream_name).await;
                        }
                    }
                    _ => {}
                },
                Ok(BroadcastEvent::UnPublish { identifier }) => match identifier {
                    StreamIdentifier::Rtmp { stream_name, .. } if stream_name == room => {
                        log::info!("RTMP 直播流下线 stream={stream_name}");
                        rtmp_active = false;
                        stop_session(&rec).await;
                    }
                    StreamIdentifier::WebRTC { stream_name, .. } if stream_name == room => {
                        if rtmp_active {
                            // 桥的 WebRTC 撤销（RTMP 仍在或刚下线）：以 RTMP 下线为准，忽略。
                            log::info!("跳过桥 WebRTC 撤销（RTMP 直录）stream={stream_name}");
                        } else {
                            log::info!("直播流下线 stream={stream_name}");
                            stop_session(&rec).await;
                        }
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(e) => {
                    log::warn!("client-event 接收错误: {e}，1s 后继续");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
}

/// 断流：清 current + 通知录制器收尾（它会收尾 full.mp4 再切挂起标记）。
async fn stop_session(rec: &SharedRec) {
    let mut s = rec.write().await;
    s.current = None;
    if let Some(stop) = s.session_stop.take() {
        let _ = stop.send(());
    }
}

/// RTMP 源开播：清过期会话 → 起 RTMP 直录任务（不订阅 hub、不走 RTP）。
async fn on_publish_rtmp(
    rec: &SharedRec,
    shutdown: &watch::Receiver<bool>,
    tasks: &RecTasks,
    rtmp_port: u16,
    app: String,
    stream: String,
) {
    let _ = tokio::task::spawn_blocking(clean_old_sessions);

    let session_id = now_ms().to_string();
    let dir = sessions_dir().join(&session_id);
    let full = dir.join("full.mp4");
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    {
        let mut s = rec.write().await;
        s.current = Some(ActiveStream { app: app.clone(), stream: stream.clone() });
        if let Some(old) = s.session_stop.take() { let _ = old.send(()); } // 停上一场（异常未清）
        s.session_stop = Some(stop_tx);
    }

    let handle = tokio::spawn(session_recorder_rtmp(
        session_id, dir, full, rec.clone(), stop_rx, shutdown.clone(), rtmp_port, app, stream,
    ));
    tasks.lock().await.push(handle);
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

    let (packet_rx, unsub_id, unsub_info) = match subscribe_packets(hub, &app, &stream).await {
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
        packet_rx, session_id, dir, full, rec.clone(), stop_rx, shutdown.clone(),
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
    // 视频：original 直拷、<N>p 重编码；音频整场 full.mp4 已是连续 AAC，一律直拷、不重复编码
    //（避免二次编码降质）。
    match quality.strip_suffix('p').and_then(|s| s.parse::<u32>().ok()) {
        Some(h) => {
            cmd.args(["-vf", &format!("scale=-2:{h}")]) // -2 保持宽高比且宽为偶数
                .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "23"]);
        }
        None => { cmd.args(["-c:v", "copy"]); } // 原画直拷
    }
    if has_audio {
        cmd.args(["-c:a", "copy"]);
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
    mut pkt_rx: PacketDataReceiver,
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

    // ---- 预备阶段：等到带 SPS 的关键帧才起 ffmpeg，并判定有无音频 ----
    // `-c:v copy` 必须从带 SPS/PPS 的关键帧起步：否则起步是引用不到解码头的 P 帧，
    // ffmpeg 在探测窗里定不了尺寸 → 写不出 mp4 头（整段录不下来）；即便侥幸探到也开头花屏。
    // WHIP 每 3s 发 PLI 催关键帧，故这里最多等一个关键帧间隔即可拿到 SPS。
    // SPS 之前的视频/音频包全丢弃（不可解码的 lead-in）；从 SPS 起缓存，ffmpeg 起来后一次性转发。
    const AUDIO_GRACE_MS: u64 = 500; // 收齐 SPS+PPS 后再等这么久判定有无音频
    const PREAMBLE_MAX_MS: u64 = 12000; // 总上限（容忍慢握手 + 长关键帧间隔）
    let preamble_start = now_ms();
    let mut params_at_ms: Option<u64> = None; // SPS 与 PPS 都收齐的时刻
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    let mut caching = false; // 见到首个参数集即开始缓存（丢弃之前不可解码的 lead-in P 帧）
    let mut pending: Vec<PacketData> = Vec::new();
    let mut video_pt: Option<u8> = None;
    let mut audio_pt: Option<u8> = None;
    let mut ended_early = false;
    loop {
        if now_ms().saturating_sub(preamble_start) >= PREAMBLE_MAX_MS { break; }
        // 解码头(SPS+PPS)俱全 + （已见音频 或 已等够判定窗）→ 起 ffmpeg
        if let Some(t) = params_at_ms {
            if audio_pt.is_some() || now_ms().saturating_sub(t) >= AUDIO_GRACE_MS { break; }
        }
        tokio::select! {
            pkt = pkt_rx.recv() => match pkt {
                Some(p @ PacketData::Video { .. }) => {
                    if let PacketData::Video { data, .. } = &p {
                        if video_pt.is_none() { video_pt = rtp_payload_type(data); }
                        // 抽本包内的 SPS/PPS；见到任一参数集即开始缓存（丢弃之前的 lead-in P 帧）
                        let (s, pp) = rtp_h264_params(data);
                        if s.is_some() { if sps.is_none() { sps = s; } caching = true; }
                        if pp.is_some() { if pps.is_none() { pps = pp; } caching = true; }
                        if params_at_ms.is_none() && sps.is_some() && pps.is_some() {
                            params_at_ms = Some(now_ms());
                        }
                    }
                    if caching { pending.push(p); }
                }
                Some(p @ PacketData::Audio { .. }) => {
                    if let PacketData::Audio { data, .. } = &p {
                        if audio_pt.is_none() { audio_pt = rtp_payload_type(data); }
                    }
                    // 只保留参数集之后的音频，避免音频超前视频一大截
                    if caching { pending.push(p); }
                }
                None => { ended_early = true; break; }
            },
            _ = &mut stop_rx => { ended_early = true; break; }
            _ = shutdown.changed() => { ended_early = true; break; }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {} // 定期复查退出条件
        }
    }

    let (Some(vpt), Some(sps), Some(pps)) = (video_pt, sps, pps) else {
        log::info!("整场录制未起播即结束（未收齐 H264 SPS+PPS）session={session_id}");
        let _ = hub.send(StreamHubEvent::UnSubscribe { identifier: unsub_id, info: unsub_info });
        let mut s = rec.write().await;
        if s.session.as_ref().map(|x| x.id == session_id).unwrap_or(false) {
            s.session = None;
        }
        return;
    };
    let sprop = format!("{},{}", base64_encode(&sps), base64_encode(&pps));
    let mut has_audio = audio_pt.is_some();

    // ---- 挑 UDP 端口、写 SDP、拉起 ffmpeg（RTP 输入 → fragmented mp4，h264 直拷 + 音频转 AAC）----
    let (vport, aport) = match (pick_udp_port(), pick_udp_port()) {
        (Ok(v), Ok(a)) => (v, a),
        _ => {
            log::error!("分配 UDP 端口失败 session={session_id}");
            let _ = hub.send(StreamHubEvent::UnSubscribe { identifier: unsub_id, info: unsub_info });
            return;
        }
    };
    let sdp_path = dir.join("session.sdp");
    let audio_spec = if has_audio { Some((aport, audio_pt.unwrap())) } else { None };
    if let Err(e) = tokio::fs::write(&sdp_path, build_sdp(vport, vpt, &sprop, audio_spec)).await {
        log::error!("写 SDP 失败 session={session_id}: {e}");
        let _ = hub.send(StreamHubEvent::UnSubscribe { identifier: unsub_id, info: unsub_info });
        return;
    }

    let ff_log = std::fs::File::create(dir.join("ffmpeg.log")).ok();
    let mut cmd = Command::new(crate::ffmpeg::path());
    cmd.args(["-hide_banner", "-loglevel", "warning"])
        .args(["-protocol_whitelist", "file,rtp,udp"])
        // 加大 UDP 接收缓冲 + reorder 队列：H264 关键帧是一串突发大包，localhost UDP 默认
        // 接收缓冲会被瞬间挤爆丢包 → 画面花/糊、音频周期性杂音断续；reorder_queue 处理乱序到达。
        .args(["-buffer_size", "4194304"]) // 4MB（受 kern.ipc.maxsockbuf 上限约束）
        .args(["-reorder_queue_size", "2048"])
        // 探测窗放大到 10s 作兜底：我们从检测到 SPS 的那一个关键帧包起步转发，若该包在
        // ffmpeg 刚起（端口未 bind）时发丢，需等下一个关键帧的 SPS 才能定尺寸/写 mp4 头；
        // 长关键帧间隔（如 5s）下 2s 探测窗会超时录不下来，故给足 10s 容错。
        .args(["-analyzeduration", "10000000", "-probesize", "10000000"])
        .args(["-i"]).arg(&sdp_path)
        .args(["-c:v", "copy"]); // 视频无损直拷（原画）
    // 音频重编码 AAC + aresample=async：真实编码器/网络会因 DTX（静音期不连续传输）/丢包/抖动在
    // Opus RTP 时间戳上留空洞，直拷会把空洞原样落盘 → 每隔约 5s 杂音/断续。async 按输出采样时钟
    // 补/丢样本产出连续单调音频（>0.1s 的空洞插静音硬补偿，保持 A/V 同步），顺带避开 opus-in-mp4
    // 的 frame-size/时戳警告。native RTP 时戳单调，无旧路 wallclock 的「Queue backward / DTS 倒退」。
    if has_audio { cmd.args(["-af", "aresample=async=1", "-c:a", "aac", "-b:a", "128k"]); }
    // -max_interleave_delta 0：不为音视频交错而阻塞（一路暂时无包时不卡住 muxer）。
    cmd.args(["-max_interleave_delta", "0"]);
    // fragmented mp4：边写边可裁、崩溃可播（不需 faststart）；
    // frag_duration 0.5s：每 0.5s 强制刷一个分片，让可读边缘紧贴直播（否则尾部数秒未成形无法裁）。
    cmd.args(["-frag_duration", "500000"])
        // flush_packets：每写一包就 flush AVIO 缓冲到盘，否则分片攒在 ffmpeg 写缓冲里、
        // 裁剪读端看不到最新数据 → 停止时切的成品仍短一截。
        .args(["-flush_packets", "1"])
        .args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof"])
        .arg(&full)
        .stdin(Stdio::null())
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

    // 转发用 UDP 套接字（本地任意端口）；两路目标为 ffmpeg 在 SDP 里 bind 的端口。
    let sock = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => {
            log::error!("绑定转发 UDP 失败 session={session_id}: {e}");
            let _ = child.start_kill();
            let _ = hub.send(StreamHubEvent::UnSubscribe { identifier: unsub_id, info: unsub_info });
            return;
        }
    };
    let vaddr: std::net::SocketAddr = ([127, 0, 0, 1], vport).into();
    let aaddr: std::net::SocketAddr = ([127, 0, 0, 1], aport).into();

    // 给 ffmpeg 时间解析 SDP、setsockopt 缓冲并 bind 两个 UDP 端口，之后再灌包：
    // 我们的首个关键帧(SPS)只发一次，若在 ffmpeg 端口未就绪时发出会丢，长关键帧间隔下要等很久
    // 才有下一个 SPS。1.2s 覆盖常见启动耗时；即便偶发更慢，也有 10s 探测窗兜底等下一个关键帧。
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // T0：full.mp4 媒体时间 0 对应的墙钟。就在灌首包前取，与用户标记的墙钟同一时钟基准。
    let t0_ms = now_ms();
    {
        let mut s = rec.write().await;
        s.session = Some(SessionRec { id: session_id.clone(), full: full.clone(), t0_ms, has_audio });
    }
    log::info!(target: "user_ops", "整场录制已开始 session={session_id} 音频={} vpt={vpt} → {}", if has_audio { "有" } else { "无" }, full.display());

    // 转发一个 RTP 包到对应端口；send 失败（ffmpeg 已退出）返回 false。
    let mut nv = 0u64;
    let mut na = 0u64;
    async fn forward(sock: &UdpSocket, addr: std::net::SocketAddr, data: &[u8]) -> bool {
        sock.send_to(data, addr).await.is_ok()
    }

    // 灌入预备缓冲
    let mut alive = true;
    for p in pending.drain(..) {
        let ok = match &p {
            PacketData::Video { data, .. } => { nv += 1; forward(&sock, vaddr, data).await }
            PacketData::Audio { data, .. } => { na += 1; forward(&sock, aaddr, data).await }
        };
        if !ok { alive = false; break; }
    }

    // ---- 主收包循环 ----
    if !ended_early && alive {
        loop {
            tokio::select! {
                pkt = pkt_rx.recv() => match pkt {
                    Some(PacketData::Video { data, .. }) => {
                        nv += 1;
                        if !forward(&sock, vaddr, &data).await { log::warn!("整场转发视频失败（ffmpeg 退出?）session={session_id}"); break; }
                    }
                    Some(PacketData::Audio { data, .. }) => {
                        if has_audio {
                            na += 1;
                            if !forward(&sock, aaddr, &data).await { log::warn!("整场转发音频失败 session={session_id}"); has_audio = false; }
                        }
                    }
                    None => { log::info!("直播流已断，整场录制结束 session={session_id}"); break; }
                },
                _ = &mut stop_rx => { log::info!("收到停止，整场录制收尾 session={session_id}"); break; }
                _ = shutdown.changed() => { log::info!("进程退出，整场录制收尾 session={session_id}"); break; }
            }
        }
    }

    // 退订 → 给 ffmpeg 发 SIGINT 求干净收尾（RTP 无 EOF，close socket 不触发结束）。
    // 但 ffmpeg 阻塞在 UDP 读时对 SIGINT 响应慢（实测 ~9s）；而本文件是 fragmented mp4
    // （empty_moov + frag_keyframe），moov 前置、每分片自包含，即便 SIGKILL 也留下可播文件。
    // 故只给短暂宽限走干净路径，超时即强杀，让停止响应快、最多丢尾部半个分片。
    let _ = hub.send(StreamHubEvent::UnSubscribe { identifier: unsub_id, info: unsub_info });
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as i32, libc::SIGINT); }
    }
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            let _ = child.start_kill(); // fragmented mp4 强杀亦可播
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
    }
    let _ = std::fs::remove_file(&sdp_path);
    let end_ms = now_ms();
    log::info!("整场录制收帧结束 video={nv} audio={na} session={session_id}");
    finalize_session(&rec, &session_id, end_ms).await;
    let _ = tasks; // 句柄由调用方（on_publish）收集
}

/// 会话结束通用收尾：清 session、把此会话仍「recording」的标记转「cutting」，并 inline 裁剪
/// （不 spawn，使本录制任务句柄在优雅退出时覆盖这些裁剪）。full.mp4 已收尾，可安全裁剪。
async fn finalize_session(rec: &SharedRec, session_id: &str, end_ms: u64) {
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
    for id in pending {
        log::info!(target: "user_ops", "会话结束，收尾挂起标记 id={id} session={session_id}");
        cut_mark(rec.clone(), id).await;
    }
}

/// RTMP 源整场直录：直接从本机 RTMP 拉原始流、**音视频 `-c copy` 无损落盘**。
/// RTMP 走 TCP 可靠传输、FLV 自带 SPS/PPS 与 AAC，故不需 RTP 转发 / SPS-PPS 探测 / Opus 转码 /
/// aresample——彻底避开 RTP 录制路那条会引入哧哧底噪的音频加工链。桥另起一路负责直播(WHIP)。
#[allow(clippy::too_many_arguments)]
async fn session_recorder_rtmp(
    session_id: String,
    dir: PathBuf,
    full: PathBuf,
    rec: SharedRec,
    mut stop_rx: oneshot::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
    rtmp_port: u16,
    app: String,
    stream: String,
) {
    let _ = tokio::fs::create_dir_all(&dir).await;
    let input = format!("rtmp://127.0.0.1:{rtmp_port}/{app}/{stream}");
    let ff_log = std::fs::File::create(dir.join("ffmpeg.log")).ok();
    let mut cmd = Command::new(crate::ffmpeg::path());
    cmd.args(["-hide_banner", "-loglevel", "warning"])
        .args(["-i", &input])
        .args(["-c", "copy"]) // 视频 + 音频全部无损直拷（FLV→MP4 的 AVC/AAC 直通，无需 bsf）
        .args(["-max_interleave_delta", "0"]) // 一路暂时无包时不卡住 muxer
        // fragmented mp4：边写边可裁、崩溃可播；frag 0.5s 让可读边缘紧贴直播；flush 立即落盘供裁剪读端。
        .args(["-frag_duration", "500000"])
        .args(["-flush_packets", "1"])
        .args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof"])
        .arg(&full)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(ff_log.map(Stdio::from).unwrap_or_else(Stdio::null));
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log::error!("拉起 RTMP 直录 ffmpeg 失败 session={session_id}: {e}");
            let mut s = rec.write().await;
            if s.session.as_ref().map(|x| x.id == session_id).unwrap_or(false) { s.session = None; }
            return;
        }
    };

    // T0：full.mp4 媒体时间 0 对应的墙钟。localhost RTMP 连接即时，起 ffmpeg 后立刻取。
    // RTMP 恒带 AAC 音轨（OBS 总有音频），has_audio=true；即便无音轨，裁剪的 -c:a copy 亦无害。
    let t0_ms = now_ms();
    {
        let mut s = rec.write().await;
        s.session = Some(SessionRec { id: session_id.clone(), full: full.clone(), t0_ms, has_audio: true });
    }
    log::info!(target: "user_ops", "整场录制已开始（RTMP 直录）session={session_id} → {}", full.display());

    // 等 停止 / 退出 / ffmpeg 自行退出（RTMP 断流时 ffmpeg 收到 EOF 会自己结束）。
    tokio::select! {
        _ = &mut stop_rx => { log::info!("收到停止，RTMP 直录收尾 session={session_id}"); }
        _ = shutdown.changed() => { log::info!("进程退出，RTMP 直录收尾 session={session_id}"); }
        _ = child.wait() => { log::info!("RTMP 断流，直录结束 session={session_id}"); }
    }

    // 给 ffmpeg 发 SIGINT 求干净收尾（写 moov）；超时强杀（fragmented mp4 强杀亦可播）。
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as i32, libc::SIGINT); }
    }
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
    }
    let end_ms = now_ms();
    log::info!("RTMP 直录收尾结束 session={session_id}");
    finalize_session(&rec, &session_id, end_ms).await;
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
