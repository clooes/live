//! 内置 ffmpeg：把静态 ffmpeg 嵌入二进制，首次运行释放到临时目录再调用，实现「双击即用、不装外部依赖」。
//!
//! 是否内置由 `build.rs` 决定：`vendor/ffmpeg/<平台>/ffmpeg[.exe]` 存在即嵌入（cfg `embed_ffmpeg`）；
//! 不存在则 `EMBEDDED = None`，运行时回退到 PATH 的 `ffmpeg`（保持原行为，始终可编译）。
//!
//! 放置二进制的位置（需静态构建，不依赖外部动态库）：
//!   - macOS arm64:  vendor/ffmpeg/macos-arm64/ffmpeg
//!   - macOS x64:    vendor/ffmpeg/macos-x64/ffmpeg
//!   - Windows x64:  vendor/ffmpeg/windows-x64/ffmpeg.exe
//!   - Linux x64:    vendor/ffmpeg/linux-x64/ffmpeg

use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(embed_ffmpeg)]
static EMBEDDED: Option<&[u8]> = Some(include_bytes!(env!("EMBED_FFMPEG_PATH")));
#[cfg(not(embed_ffmpeg))]
static EMBEDDED: Option<&[u8]> = None;

/// 可用的 ffmpeg 可执行路径。内置优先（首次调用释放并缓存），失败或未内置回退 PATH 的 `ffmpeg`。
pub fn path() -> PathBuf {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(resolve).clone()
}

/// 是否使用了内置 ffmpeg（供启动日志/自检提示）。
pub fn is_embedded() -> bool {
    EMBEDDED.is_some()
}

/// 探测某 ffmpeg 是否带 `whip` muxer（RTMP→WHIP 桥必需；不少内置/静态 build 未编入）。
fn has_whip(exe: &std::path::Path) -> bool {
    std::process::Command::new(exe)
        .args(["-hide_banner", "-muxers"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.split_whitespace().any(|t| t == "whip"))
        })
        .unwrap_or(false)
}

/// 返回一个支持 `whip` muxer 的 ffmpeg 路径（RTMP→WHIP 桥用）。内置优先；内置不含 whip 则回退
/// PATH 的 `ffmpeg`（homebrew/较新发行版的 8.1+ 自带 whip）。都不支持返回 None（桥不启，仅告警）。
pub fn whip_path() -> Option<PathBuf> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| {
        let primary = path();
        if has_whip(&primary) {
            return Some(primary);
        }
        let fallback = PathBuf::from("ffmpeg");
        if fallback != primary && has_whip(&fallback) {
            log::warn!("内置 ffmpeg 不含 whip muxer，RTMP→WHIP 桥回退 PATH 的 ffmpeg（{}）", fallback.display());
            return Some(fallback);
        }
        None
    })
    .clone()
}

fn resolve() -> PathBuf {
    match EMBEDDED {
        Some(bytes) => match extract(bytes) {
            Ok(p) => {
                log::info!("使用内置 ffmpeg：{}", p.display());
                p
            }
            Err(e) => {
                log::warn!("释放内置 ffmpeg 失败，回退 PATH 的 ffmpeg：{e}");
                PathBuf::from("ffmpeg")
            }
        },
        None => PathBuf::from("ffmpeg"),
    }
}

/// 把内置字节释放到临时目录并赋可执行权限，返回路径。用字节长度做版本区分，升级后自动换新文件。
fn extract(bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;

    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let dir = std::env::temp_dir().join("relay-ffmpeg");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}-{}", bytes.len(), name));

    // 已释放且大小一致 → 直接复用，省去重复写盘
    if let Ok(m) = std::fs::metadata(&path) {
        if m.len() == bytes.len() as u64 {
            return Ok(path);
        }
    }

    // 原子写：先写临时文件再 rename，避免并发/半写留下坏文件
    let tmp = dir.join(format!(".{}-{}.tmp", std::process::id(), bytes.len()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}
