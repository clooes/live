#!/usr/bin/env bash
# relay 开发脚本：杀旧进程 / 启动 / 重编译并启动。
#
#   ./dev.sh kill      仅杀掉正在运行的 relay
#   ./dev.sh run       杀旧 + 启动（用已编译的二进制）
#   ./dev.sh restart   杀旧 + cargo build + 启动（默认）
#
# 说明：relay 从 config.json 读端口（默认 web=8000 / webrtc=8900 / rtmp=1935）。
set -euo pipefail

cd "$(dirname "$0")"                 # 切到 relay/ 目录，脚本可从任意路径调用
BIN="target/debug/relay"
PATTERN="target/debug/relay"        # 用于匹配进程；不含自身脚本

# 杀掉所有在跑的 relay，并等端口释放
kill_old() {
  local pids
  pids="$(pgrep -f "$PATTERN" || true)"
  if [ -n "$pids" ]; then
    echo "杀掉旧 relay：$(echo "$pids" | tr '\n' ' ')"
    # shellcheck disable=SC2086
    kill -9 $pids 2>/dev/null || true
    sleep 1
  else
    echo "无正在运行的 relay"
  fi
  # 端口占用检查（仅提示，不阻塞）
  for p in 8000 8900 1935; do
    if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
      echo "  警告：端口 $p 仍被占用"
    fi
  done
}

case "${1:-restart}" in
  kill)
    kill_old
    ;;
  run)
    kill_old
    [ -x "$BIN" ] || { echo "未找到 $BIN，请先 cargo build"; exit 1; }
    echo "启动 $BIN …"
    exec "$BIN"
    ;;
  restart)
    kill_old
    echo "编译中 …"
    cargo build
    echo "启动 $BIN …"
    exec "$BIN"
    ;;
  *)
    echo "用法：$0 {kill|run|restart}"
    exit 1
    ;;
esac
