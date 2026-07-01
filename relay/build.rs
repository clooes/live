//! 构建脚本：若 `vendor/ffmpeg/<平台>/ffmpeg[.exe]` 存在，则把它嵌入二进制
//! （设置 cfg `embed_ffmpeg` + env `EMBED_FFMPEG_PATH`，供 src/ffmpeg.rs 的 include_bytes! 使用）。
//! 不存在则什么都不做——运行时回退到 PATH 的 ffmpeg，始终可编译。

use std::path::Path;

fn main() {
    // 允许使用自定义 cfg，消除新版 rustc 的 unexpected_cfgs 警告
    println!("cargo::rustc-check-cfg=cfg(embed_ffmpeg)");
    println!("cargo:rerun-if-changed=vendor/ffmpeg");

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let subdir = match (os.as_str(), arch.as_str()) {
        ("macos", "aarch64") => "macos-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("windows", "x86_64") => "windows-x64",
        ("linux", "x86_64") => "linux-x64",
        _ => return, // 未知平台：不嵌入，回退 PATH
    };
    let name = if os == "windows" { "ffmpeg.exe" } else { "ffmpeg" };
    let path = Path::new("vendor/ffmpeg").join(subdir).join(name);

    if path.exists() {
        let abs = path.canonicalize().unwrap_or(path.clone());
        println!("cargo:rustc-cfg=embed_ffmpeg");
        println!("cargo:rustc-env=EMBED_FFMPEG_PATH={}", abs.display());
        println!("cargo:rerun-if-changed={}", path.display());
        println!("cargo:warning=已嵌入内置 ffmpeg: {}", abs.display());
    }
}
