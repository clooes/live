import 'dart:convert';
import 'package:http/http.dart' as http;
import 'config.dart';
import 'models.dart';

/// 后端裁剪 API 封装
class Api {
  static Future<Map<String, dynamic>> _post(String path, [String? token]) async {
    final headers = <String, String>{};
    if (token != null) headers['Authorization'] = 'Bearer $token';
    final r = await http.post(Uri.parse('${Config.apiBase}$path'), headers: headers);
    return jsonDecode(r.body) as Map<String, dynamic>;
  }

  static Future<Map<String, dynamic>> _get(String path) async {
    final r = await http.get(Uri.parse('${Config.apiBase}$path'));
    return jsonDecode(r.body) as Map<String, dynamic>;
  }

  /// 手机号 + 验证码登录（验证码 = 手机号后 6 位）
  static Future<Map<String, dynamic>> login(String phone, String code) async {
    final r = await http.post(
      Uri.parse('${Config.apiBase}/api/login'),
      headers: {'Content-Type': 'application/json'},
      body: jsonEncode({'phone': phone, 'code': code}),
    );
    return jsonDecode(r.body) as Map<String, dynamic>;
  }

  /// 标记开始（需登录）；返回完整响应（含 code/start_offset/msg）
  static Future<Map<String, dynamic>> clipStart(String token) => _post('/api/clip/start', token);

  /// 标记结束并按清晰度生成（需登录）；返回 { code, status, job_id }
  static Future<Map<String, dynamic>> clipEnd(String quality, String token) =>
      _post('/api/clip/end?quality=$quality', token);

  static Future<Map<String, dynamic>> clipStatus(String id) =>
      _get('/api/clip/status/$id');

  static Future<List<ClipJob>> clips() async {
    final j = await _get('/api/clips');
    final list = (j['clips'] as List?) ?? [];
    return list.map((e) => ClipJob.fromJson(e as Map<String, dynamic>)).toList();
  }

  /// 我的视频（需登录）
  static Future<List<ClipJob>> myClips(String token) async {
    final r = await http.get(
      Uri.parse('${Config.apiBase}/api/my/clips'),
      headers: {'Authorization': 'Bearer $token'},
    );
    final j = jsonDecode(r.body) as Map<String, dynamic>;
    final list = (j['clips'] as List?) ?? [];
    return list.map((e) => ClipJob.fromJson(e as Map<String, dynamic>)).toList();
  }
}
