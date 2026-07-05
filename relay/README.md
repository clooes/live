# relay 开发速查

架构/使用/排障见根 [README.md](../README.md)，RTMP 桥与录制专题见
[docs/RTMP-桥与录制.md](../docs/RTMP-桥与录制.md)。

```bash
./dev.sh          # 默认 = restart：杀旧 + cargo build + 启动
./dev.sh run      # 杀旧 + 启动（用已编译的二进制）
./dev.sh kill     # 仅杀掉正在运行的 relay

# 前端改动后需重建再编（dist 由 rust-embed 嵌入二进制）
cd web && npm run build && cd .. && cargo build

# 无 OBS 时的推流测试
./push-test-whip.sh                 # WHIP 推测试图样
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -preset veryfast -g 60 -c:a aac \
  -f flv rtmp://127.0.0.1:1935/live/room001   # RTMP 推（走桥，最接近 OBS 真实链路）
```

- `vendor/` 下是打过补丁的 xwebrtc / streamhub（`[patch.crates-io]` 生效），
  **勿用 crates.io 版本覆盖**；补丁清单见根 README §5。
- `vendor/ffmpeg/<平台>/` 放静态 ffmpeg 即自动嵌入（gitignore 不入库）；
  桥只需 libx264+libopus，**不需要 whip muxer**。
