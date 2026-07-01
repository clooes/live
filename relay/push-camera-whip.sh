#!/usr/bin/env bash
# 用 mac 摄像头 WHIP 推流到 relay（第0步验证用）。
# 用法：bash relay/push-camera-whip.sh
# 前提：relay 已在跑（cd relay && cargo run），WHEP :8900 监听中。
set -euo pipefail

URL="http://localhost:8900/whip?app=live&stream=room001"

ffmpeg -f avfoundation -framerate 30 -video_size 1280x720 -i "0" \
  -vf format=yuv420p \
  -c:v libx264 -preset ultrafast -tune zerolatency -profile:v baseline \
  -x264-params "repeat-headers=1:keyint=30:min-keyint=30:scenecut=0" \
  -an \
  -f whip "$URL"
