//! relay 运行配置：清晰度/码率档（原画直通，码率为对推流端的建议值），
//! 持久化到二进制同目录 `config.json`（无数据库，符合单二进制目标）。

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// 一个清晰度档：名称 + 建议码率（kbps）。直通模式下码率仅作 OBS 推流建议，服务端不重编码。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quality {
    pub name: String,       // "original" / "720p" / "480p"
    pub bitrate_kbps: u32,  // 建议码率
}

/// 监听端口配置。端口被占用时可在 config.json 改此三项（改后重启生效）。
/// 前端 WHEP 播放端口由 /api/config 下发，改端口后前端自动跟随，无需另改代码。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ports {
    /// 内网页面 + API（axum）
    pub web: u16,
    /// WHIP 推流 + WHEP 播放（WebRTC 信令，同端口）
    pub webrtc: u16,
    /// RTMP 接收（可选保留）
    pub rtmp: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Self { web: 8000, webrtc: 8900, rtmp: 1935 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfig {
    /// 直播间/流名（对应 WHIP/WHEP 的 stream 参数）
    pub room: String,
    /// 可选清晰度档
    pub qualities: Vec<Quality>,
    /// 默认清晰度名（须存在于 qualities 中）
    pub default_quality: String,
    /// 监听端口（缺省用内置默认：web 8000 / webrtc 8900 / rtmp 1935）。
    /// 属启动期配置：改后需重启进程生效（不像 room/清晰度那样 SSE 热更新）。
    #[serde(default)]
    pub ports: Ports,
    /// 数据目录（录制/切片存放）。留空 = 二进制同目录下的 `data/`。
    /// 绝对路径按原样使用；`~/…` 相对家目录；其余相对二进制所在目录。改后需重启生效。
    #[serde(default)]
    pub data_dir: Option<String>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            room: "room001".into(),
            qualities: vec![
                Quality { name: "original".into(), bitrate_kbps: 4000 },
                Quality { name: "720p".into(), bitrate_kbps: 2500 },
                Quality { name: "480p".into(), bitrate_kbps: 1000 },
            ],
            default_quality: "original".into(),
            ports: Ports::default(),
            data_dir: None,
        }
    }
}

pub type SharedConfig = Arc<RwLock<RelayConfig>>;

/// 二进制所在目录（取不到则退回当前工作目录）。config 与默认 data 均以此为基准，
/// 避免「从哪启动就写到哪」——不随启动时的工作目录 CWD 变化。
pub fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// config.json 路径：二进制所在目录。
pub fn config_path() -> PathBuf {
    base_dir().join("config.json")
}

static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// 把 `data_dir` 配置解析成绝对路径：绝对路径原样；`~/…` 相对家目录；
/// 其余相对二进制目录；留空 = `<二进制目录>/data`。
fn resolve_data_dir(cfg: &RelayConfig) -> PathBuf {
    match cfg.data_dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => {
            if let Some(rest) = d.strip_prefix("~/") {
                if let Ok(home) = std::env::var("HOME") {
                    return PathBuf::from(home).join(rest);
                }
            }
            let p = PathBuf::from(d);
            if p.is_absolute() {
                p
            } else {
                base_dir().join(p)
            }
        }
        None => base_dir().join("data"),
    }
}

/// 启动时确定数据根目录（据 config），创建并缓存。录制/切片统一以此为基准。
pub fn init_data_root(cfg: &RelayConfig) -> PathBuf {
    let root = resolve_data_dir(cfg);
    if let Err(e) = std::fs::create_dir_all(&root) {
        log::warn!("创建数据目录失败 {}：{e}", root.display());
    }
    let _ = DATA_ROOT.set(root.clone());
    root
}

/// 数据根目录（录制/切片存放）。未初始化时回退二进制目录下 `data/`。
pub fn data_root() -> PathBuf {
    DATA_ROOT
        .get()
        .cloned()
        .unwrap_or_else(|| base_dir().join("data"))
}

/// 启动时加载：文件存在则读，否则用默认并落盘一份。
pub fn load() -> RelayConfig {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<RelayConfig>(&s) {
            Ok(cfg) => {
                log::info!("已加载配置 {}", path.display());
                cfg
            }
            Err(e) => {
                log::warn!("配置解析失败（用默认）：{e}");
                RelayConfig::default()
            }
        },
        Err(_) => {
            let cfg = RelayConfig::default();
            if let Err(e) = save(&cfg) {
                log::warn!("写入默认配置失败：{e}");
            } else {
                log::info!("已生成默认配置 {}", path.display());
            }
            cfg
        }
    }
}

/// 保存配置到 config.json（美化 JSON）。
pub fn save(cfg: &RelayConfig) -> anyhow::Result<()> {
    let path = config_path();
    let s = serde_json::to_string_pretty(cfg)?;
    std::fs::write(&path, s)?;
    Ok(())
}
