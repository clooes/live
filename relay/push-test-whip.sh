#!/usr/bin/env bash
# 用测试图样 WHIP 推流到 relay（第0步验证用，无需摄像头权限）。
# 用法：bash relay/push-test-whip.sh
# 前提：relay 已在跑（cd relay && cargo run），WHEP :8900 监听中。
set -euo pipefail

URL="http://localhost:8900/whip?app=live&stream=room001"

ffmpeg -re -f lavfi -i testsrc=size=640x360:rate=15 \
  -c:v libx264 -preset ultrafast -tune zerolatency -pix_fmt yuv420p -profile:v baseline \
  -x264-params "repeat-headers=1:keyint=15:min-keyint=15:scenecut=0" \
  -an \
  -f whip "$URL"
