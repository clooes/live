//! 时间切片：从某场录制的连续 HLS（index.m3u8 + .ts）按绝对时间区间裁出一段 mp4。
//!
//! 录制时每个分片写了 `#EXT-X-PROGRAM-DATE-TIME`（该片首帧的墙钟时间），
//! 因此可用「绝对时间」精确对齐——避免用「相对 session 起点的秒偏移」时，
//! Publish 事件到首个关键帧之间那 ~2s 空档造成的错位。
//!
//! 流程：解析 m3u8 建「分片→[起,止]墙钟ms」→ 选覆盖 [start_ms,end_ms] 的连续 .ts →
//! ffmpeg concat 合并 + `-ss/-t` 精修两端，`-c copy` 不重编码（秒级）。

use std::path::{Path, PathBuf};
use std::process::Stdio;

use chrono::DateTime;
use tokio::process::Command;
use tokio::time::{sleep, Duration};

/// 解析 HLS `#EXT-X-PROGRAM-DATE-TIME` 的时间为 epoch 毫秒。
/// ffmpeg 写出的偏移是 `+0800`（无冒号），不是严格 RFC3339 的 `+08:00`——
/// 先按 ffmpeg 格式解析，再回退 RFC3339，避免解析失败退化成相对时间轴。
fn parse_pdt_ms(s: &str) -> Option<u64> {
    DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z")
        .or_else(|_| DateTime::parse_from_rfc3339(s))
        .ok()
        .map(|dt| dt.timestamp_millis() as u64)
}

use crate::record::{human_size, recordings_dir, SharedRec};

/// 切片输出目录（在配置的数据根目录下，默认二进制同目录 data/）。
pub fn clips_dir() -> PathBuf {
    crate::config::data_root().join("clips")
}

/// 一个 HLS 分片在墙钟时间轴上的位置。
struct Seg {
    path: PathBuf,
    start_ms: u64,
    end_ms: u64,
}

/// 解析某 session 的 index.m3u8 → 分片墙钟时间轴。
async fn parse_segments(session_dir: &Path) -> anyhow::Result<Vec<Seg>> {
    let m3u8 = session_dir.join("index.m3u8");
    let content = tokio::fs::read_to_string(&m3u8)
        .await
        .map_err(|e| anyhow::anyhow!("读取 m3u8 失败（录制未开始或无切片）: {e}"))?;

    let mut segs: Vec<Seg> = Vec::new();
    let mut seg_dur = 0.0f64; // 最近 #EXTINF 秒
    let mut pdt_ms: Option<u64> = None; // 最近 #EXT-X-PROGRAM-DATE-TIME
    let mut cursor_ms: Option<u64> = None; // 无 PDT 时按 EXTINF 累加推算

    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            seg_dur = rest.split(',').next().unwrap_or("0").trim().parse().unwrap_or(0.0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pdt_ms = parse_pdt_ms(rest.trim());
        } else if !line.is_empty() && !line.starts_with('#') {
            let dur_ms = (seg_dur * 1000.0) as u64;
            let start = pdt_ms.or(cursor_ms).unwrap_or(0);
            segs.push(Seg {
                path: session_dir.join(line),
                start_ms: start,
                end_ms: start + dur_ms,
            });
            cursor_ms = Some(start + dur_ms);
            pdt_ms = None;
        }
    }
    if segs.is_empty() {
        anyhow::bail!("HLS 暂无切片");
    }
    Ok(segs)
}

/// 按清晰度名生成 ffmpeg 输出侧参数（R4：下载时选清晰度）。
/// `original`（或无法解析）→ 直拷 `-c copy`（秒级）；`<N>p` → scale 到高 N + libx264 重编码。
fn quality_args(quality: &str) -> Vec<String> {
    match quality.strip_suffix('p').and_then(|s| s.parse::<u32>().ok()) {
        Some(h) => vec![
            // -2 保持宽高比且宽为偶数（libx264 要求）
            "-vf".into(), format!("scale=-2:{h}"),
            "-c:v".into(), "libx264".into(),
            "-preset".into(), "veryfast".into(),
            "-crf".into(), "23".into(),
            "-c:a".into(), "aac".into(),
        ],
        None => vec!["-c".into(), "copy".into()],
    }
}

/// 从 session 的 HLS 裁出 [start_ms, end_ms] → output(mp4)，按 `quality` 直拷或重编码。
/// `with_audio=false` 时加 `-an` 去掉音频（R10 下载「无声」版）。
pub async fn clip_session(
    session_dir: &Path,
    start_ms: u64,
    end_ms: u64,
    output: &Path,
    quality: &str,
    with_audio: bool,
) -> anyhow::Result<()> {
    if end_ms <= start_ms {
        anyhow::bail!("结束时间需晚于开始时间");
    }
    let segs = parse_segments(session_dir).await?;

    // 选覆盖 [start_ms, end_ms) 的连续分片
    let chosen: Vec<&Seg> = segs
        .iter()
        .filter(|s| s.end_ms > start_ms && s.start_ms < end_ms)
        .collect();
    if chosen.is_empty() {
        anyhow::bail!("HLS 切片未覆盖该区间（区间太新或已过期），稍等重试");
    }

    // 相对首片的精修偏移 + 时长（秒）
    let seek = (start_ms.saturating_sub(chosen[0].start_ms)) as f64 / 1000.0;
    let duration = (end_ms - start_ms) as f64 / 1000.0;

    // concat 列表（临时文件）
    let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let list_path = std::env::temp_dir().join(format!("relay_concat_{stem}.txt"));
    let mut list = String::new();
    for s in &chosen {
        // concat demuxer 按「list 文件所在目录」解析相对路径，而 list 放在临时目录，
        // 故必须写绝对路径（canonicalize 需文件存在，失败则用 CWD 拼接兜底）。
        let abs = std::fs::canonicalize(&s.path).unwrap_or_else(|_| {
            std::env::current_dir().unwrap_or_default().join(&s.path)
        });
        list.push_str(&format!("file '{}'\n", abs.display()));
    }
    tokio::fs::write(&list_path, &list).await?;
    if let Some(parent) = output.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    // concat + 输出侧 seek（-ss 在 -i 后）精修；.ts 以关键帧起头，落点较准
    let mut cmd = Command::new(crate::ffmpeg::path());
    cmd.args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list_path)
        .args(["-ss", &seek.to_string(), "-t", &duration.to_string()])
        .args(&quality_args(quality));
    if !with_audio {
        cmd.arg("-an"); // 无声版：丢弃音频轨（覆盖 quality_args 里的 -c:a aac）
    }
    let out = cmd
        .args(["-avoid_negative_ts", "make_zero"])
        .arg(output)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let _ = tokio::fs::remove_file(&list_path).await;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg concat 失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// 按需切片（R4 清晰度 + R10 有声/无声）。确保 job 区间在指定 `quality`、`with_audio` 下的 mp4 已生成，
/// 返回 (文件名, 大小)。同一 (job, quality, 音频) 的产物按文件名缓存，命中直接返回，不重复切。
/// 由 web 层的下载准备接口调用（原画秒级直拷，720p/480p 重编码稍慢）。
pub async fn ensure_clip(
    rec: &SharedRec,
    job_id: &str,
    quality: &str,
    with_audio: bool,
) -> anyhow::Result<(String, String)> {
    // 取 job 区间 + 定位 session 目录
    let (room, session_id, start_ms, end_ms) = {
        let s = rec.read().await;
        let job = s
            .jobs
            .iter()
            .find(|j| j.id == job_id)
            .ok_or_else(|| anyhow::anyhow!("无此片段 job={job_id}"))?;
        let room = s
            .session(&job.session_id)
            .map(|x| x.room.clone())
            .ok_or_else(|| anyhow::anyhow!("找不到对应录制 session"))?;
        (room, job.session_id.clone(), job.start_ms, job.end_ms)
    };

    let session_dir = recordings_dir(&room).join(&session_id);
    let aud = if with_audio { "snd" } else { "mute" }; // 音频维度进缓存文件名，有声/无声互不覆盖
    let file_name = format!("clip_{job_id}_{quality}_{aud}.mp4");
    let output = clips_dir().join(&file_name);

    // 缓存命中：已切过该 (清晰度,音频)，直接复用
    if let Ok(m) = std::fs::metadata(&output) {
        return Ok((file_name, human_size(m.len())));
    }

    log::info!(
        "切片开始 job={job_id} quality={quality} audio={aud} session={session_id} [{start_ms}, {end_ms}] ({}s)",
        (end_ms - start_ms) as f64 / 1000.0
    );
    // 有限重试：刚开录时目标分片可能尚未落盘（m3u8 未写/区间太新），等待后重试；切片幂等（输出覆盖）
    let mut r = clip_session(&session_dir, start_ms, end_ms, &output, quality, with_audio).await;
    let mut tries = 0;
    while r.is_err() && tries < 8 {
        tries += 1;
        sleep(Duration::from_millis(500)).await;
        r = clip_session(&session_dir, start_ms, end_ms, &output, quality, with_audio).await;
    }
    r?;

    let size = std::fs::metadata(&output).map(|m| human_size(m.len())).unwrap_or_default();
    log::info!(target: "user_ops", "切片完成可下载 job={job_id} quality={quality} audio={aud} file={file_name} size={size}");
    Ok((file_name, size))
}
