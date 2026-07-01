//! 启动横幅：ASCII art + 端口/地址表。
//! 输出到 stdout（非 TTY 如管道/重定向时自动去色，避免转义乱码）。

use std::io::IsTerminal;

use crate::config::Ports;

// ANSI 颜色（非 TTY 时全部替换为空串）
struct Palette {
    reset: &'static str,
    bold: &'static str,
    dim: &'static str,
    cyan: &'static str,
    magenta: &'static str,
    green: &'static str,
    yellow: &'static str,
}

impl Palette {
    fn pick() -> Self {
        if std::io::stdout().is_terminal() {
            Self {
                reset: "\x1b[0m",
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                cyan: "\x1b[36m",
                magenta: "\x1b[35m",
                green: "\x1b[32m",
                yellow: "\x1b[33m",
            }
        } else {
            Self { reset: "", bold: "", dim: "", cyan: "", magenta: "", green: "", yellow: "" }
        }
    }
}

/// 打印启动横幅。`ports` 决定地址表里显示的端口。
pub fn print(ports: &Ports) {
    // 传统 cmd/conhost 默认不解析 ANSI、代码页也非 UTF-8，先启用一次（现代终端调用亦无害）
    enable_windows_terminal();
    let p = Palette::pick();
    let version = env!("CARGO_PKG_VERSION");

    // ASCII art（RELAY）
    let art = [
        r"  ____  _____ _        _ __   __",
        r" |  _ \| ____| |      / \ \ / /",
        r" | |_) |  _| | |     / _ \ V / ",
        r" |  _ <| |___| |___ / ___ \| |  ",
        r" |_| \_\_____|_____/_/   \_\_|  ",
    ];

    println!();
    for line in art {
        println!("{}{}{}{}", p.bold, p.cyan, line, p.reset);
    }
    println!(
        "  {}{}内网直播 · 单二进制{}  {}v{}{}",
        p.bold, p.magenta, p.reset, p.dim, version, p.reset
    );
    println!();

    // 地址表（0.0.0.0 = 监听所有网卡；下方用 localhost 便于本机点开）
    let rows = [
        ("观看/管理", format!("http://localhost:{}", ports.web), p.green),
        ("WHIP 推流", format!("http://localhost:{}/whip?app=live&stream=<key>", ports.webrtc), p.yellow),
        ("WHEP 播放", format!("http://localhost:{}/whep?app=live&stream=<key>", ports.webrtc), p.yellow),
        ("RTMP 接收", format!("rtmp://localhost:{}/live/<key>", ports.rtmp), p.dim),
    ];
    for (label, url, color) in rows {
        println!("  {}{:<10}{} {}{}{}", p.bold, label, p.reset, color, url, p.reset);
    }
    println!(
        "  {}提示：局域网内其他设备请把 localhost 换成本机内网 IP{}",
        p.dim, p.reset
    );
    println!();
}

/// 非 Windows：无需处理，终端原生支持 ANSI + UTF-8。
#[cfg(not(windows))]
fn enable_windows_terminal() {}

/// Windows：启用 ANSI 转义解析 + 把控制台输出代码页设为 UTF-8。
/// 零依赖手写 kernel32 FFI —— 传统 cmd/conhost 默认既不解析颜色也非 UTF-8，
/// 不做这一步会把彩色转义打成 `←[36m` 乱码、中文也花屏。失败则静默降级（大不了没颜色）。
#[cfg(windows)]
fn enable_windows_terminal() {
    use std::os::raw::c_void;

    type Handle = *mut c_void;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const CP_UTF8: u32 = 65001;
    const INVALID_HANDLE_VALUE: Handle = -1i32 as isize as Handle;

    extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> Handle;
        fn GetConsoleMode(hConsoleHandle: Handle, lpMode: *mut u32) -> i32;
        fn SetConsoleMode(hConsoleHandle: Handle, dwMode: u32) -> i32;
        fn SetConsoleOutputCP(wCodePageID: u32) -> i32;
    }

    unsafe {
        // 中文：让 println! 输出的 UTF-8 字节被正确解读
        SetConsoleOutputCP(CP_UTF8);

        // ANSI：在原有 mode 上追加 VT 处理位
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return;
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}
