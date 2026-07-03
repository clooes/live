<!--
 * @Date: 2026-07-01 10:19:23
 * @LastEditors: myclooe 994386508@qq.com
 * @LastEditTime: 2026-07-03 10:54:34
 * @FilePath: /live/relay/vendor/xwebrtc/README.md
-->

./dev.sh kill # 仅杀掉正在运行的 relay
./dev.sh run # 杀旧 + 启动（用已编译的二进制）
./dev.sh restart # 杀旧 + cargo build + 启动（默认，不带参数也是它）
