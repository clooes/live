# 内置 ffmpeg（静态二进制）

把**静态构建**的 ffmpeg 放到对应平台子目录，`build.rs` 会自动把它嵌入 relay 二进制，
首次运行时释放到临时目录调用（见 `src/ffmpeg.rs`）。不放则运行时回退 PATH 的 `ffmpeg`。

```
vendor/ffmpeg/
├── macos-arm64/ffmpeg        # Apple Silicon
├── macos-x64/ffmpeg          # Intel Mac
├── windows-x64/ffmpeg.exe
└── linux-x64/ffmpeg
```

## 要求

- 必须是**静态**构建（不依赖外部 `.dylib`/`.so`/`.dll`），否则目标机缺库跑不起来。
  - 验证 macOS：`otool -L vendor/ffmpeg/macos-arm64/ffmpeg` 不应出现 `/opt/homebrew/...` 之类第三方库。
  - Homebrew 装的 ffmpeg 是**动态**链接，**不能**直接用。
- 放好后 `cargo build`，构建日志会打印 `已嵌入内置 ffmpeg: ...`；启动日志显示 `ffmpeg：内置(嵌入二进制)`。

## 获取静态构建（参考来源，请自行确认可信）

- macOS arm64/x64：osxexperts.net 的 `ffmpegXXarm.zip` / `ffmpegXXintel.zip`
- Windows x64：gyan.dev 或 BtbN 的 static build
- Linux x64：johnvansickle.com 的 static build

> 这些二进制体积较大（每个 ~40–80MB），已在 `.gitignore` 忽略，不入库；
> 交付/CI 时按平台放入对应子目录再编译。
