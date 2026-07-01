/*
 * @Date: 2026-06-29 21:42:28
 * @LastEditors: myclooe 994386508@qq.com
 * @LastEditTime: 2026-06-30 09:17:04
 * @FilePath: /live/app/lib/config.dart
 */
/// 服务器地址配置。
/// 真机/模拟器需访问"运行后端的那台电脑"的内网 IP（不是 localhost）。
/// ⚠️ 同时 SRS 的 candidate 必须改成这个内网 IP 并重启，否则 WebRTC 连不上。
class Config {
  static const String host = '192.168.1.10'; // 后端所在电脑内网 IP

  static String get apiBase => 'http://$host:8000';
  static String get whepUrl =>
      'http://$host:1985/rtc/v1/whep/?app=live&stream=room001';
  static String clipUrl(String file) => '$apiBase/clips/$file';
}
