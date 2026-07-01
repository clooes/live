import 'dart:async';
import 'package:flutter/material.dart';
import 'package:web_socket_channel/web_socket_channel.dart';
import 'api.dart';
import 'config.dart';
import 'danmaku_overlay.dart';
import 'models.dart';
import 'user_center.dart';
import 'webrtc_player.dart';

void main() => runApp(const LiveClipApp());

class LiveClipApp extends StatelessWidget {
  const LiveClipApp({super.key});
  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: '内网直播录制裁剪',
      debugShowCheckedModeBanner: false,
      theme: ThemeData.dark(useMaterial3: true).copyWith(
        scaffoldBackgroundColor: const Color(0xFF0E0F13),
        colorScheme: const ColorScheme.dark(primary: Color(0xFF2B6CFF)),
      ),
      home: const HomeScreen(),
    );
  }
}

class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key});
  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  bool _recording = false;
  int _elapsed = 0;
  String _quality = 'original';
  String _toast = '点「开始录制」标记起点，再点「停止」按清晰度生成。';
  Timer? _timer;
  DateTime? _start;
  List<ClipJob> _clips = [];
  Timer? _refreshTimer;

  // 弹幕
  bool _danmakuOn = true;
  WebSocketChannel? _ws;
  final List<DanmakuMsg> _danmaku = [];
  int _dmId = 0, _dmTrack = 0;
  double _dmSpeed = 6; // 1~10，越大越快
  final TextEditingController _dmInput = TextEditingController();

  // 登录态（内存）
  String? _token;
  String? _phone;

  int get _dmDuration => 14 - _dmSpeed.round(); // 秒，越小越快

  static const _qLabels = {'original': '原画', '720p': '720p', '480p': '480p'};
  static const _stLabels = {'pending': '⏳ 待生成', 'processing': '⚙️ 处理中', 'done': '✅ 完成', 'error': '❌ 失败'};

  @override
  void initState() {
    super.initState();
    _loadClips();
    _refreshTimer = Timer.periodic(const Duration(seconds: 5), (_) => _loadClips());
    _connectDanmaku();
  }

  @override
  void dispose() {
    _timer?.cancel();
    _refreshTimer?.cancel();
    _ws?.sink.close();
    _dmInput.dispose();
    super.dispose();
  }

  // ---------- 弹幕 ----------
  void _connectDanmaku() {
    try {
      _ws = WebSocketChannel.connect(Uri.parse('ws://${Config.host}:8000/ws/danmaku'));
      _ws!.stream.listen(
        (data) { if (data is String && data.trim().isNotEmpty) _addDanmaku(data); },
        onError: (_) {}, onDone: () {},
      );
    } catch (_) {}
  }

  void _addDanmaku(String text) {
    if (!_danmakuOn) return;
    final id = ++_dmId;
    final track = _dmTrack++ % 5;
    setState(() => _danmaku.add(DanmakuMsg(id, text, track)));
    Future.delayed(Duration(seconds: _dmDuration), () {
      if (mounted) setState(() => _danmaku.removeWhere((m) => m.id == id));
    });
  }

  void _sendDanmaku() {
    final t = _dmInput.text.trim();
    if (t.isNotEmpty && _ws != null) {
      _ws!.sink.add(t);
      _dmInput.clear();
    }
  }

  void _toggleDanmaku() {
    setState(() {
      _danmakuOn = !_danmakuOn;
      if (!_danmakuOn) _danmaku.clear();
    });
  }

  Future<void> _loadClips() async {
    try {
      final list = await Api.clips();
      if (mounted) setState(() => _clips = list);
    } catch (_) {}
  }

  Future<void> _startRec() async {
    if (_token == null) { _showLogin(); return; }
    final r = await Api.clipStart(_token!);
    if (r['code'] == 401) { setState(() { _token = null; }); _showLogin(); return; }
    if (r['code'] != 0) { setState(() => _toast = '⚠️ ${r['msg'] ?? '开始失败'}'); return; }
    _start = DateTime.now();
    setState(() { _recording = true; _elapsed = 0; _toast = '🔴 录制中…'; });
    _timer = Timer.periodic(const Duration(milliseconds: 250), (_) {
      setState(() => _elapsed = DateTime.now().difference(_start!).inSeconds);
    });
  }

  Future<void> _stopRec() async {
    _timer?.cancel();
    setState(() => _recording = false);
    final r = await Api.clipEnd(_quality, _token ?? '');
    if (r['code'] == 401) { setState(() { _token = null; }); _showLogin(); return; }
    if (r['code'] != 0) { setState(() => _toast = '⚠️ ${r['msg'] ?? '结束失败'}'); return; }
    final ql = _qLabels[_quality];
    if (r['status'] == 'pending') {
      setState(() => _toast = '⏳ 已记录，停止推流后自动生成');
    } else {
      setState(() => _toast = '⚙️ 裁剪中…（$ql${_quality == 'original' ? '' : '，转码较慢'}）');
      _poll(r['job_id'] as String);
    }
    _loadClips();
  }

  Future<void> _poll(String id) async {
    for (var i = 0; i < 40; i++) {
      await Future.delayed(const Duration(milliseconds: 1500));
      final r = await Api.clipStatus(id);
      if (r['status'] == 'done') { if (mounted) setState(() => _toast = '✅ 裁剪完成'); _loadClips(); return; }
      if (r['status'] == 'error') { if (mounted) setState(() => _toast = '❌ 裁剪失败'); _loadClips(); return; }
    }
  }

  String _fmt(int s) => '${(s ~/ 60).toString().padLeft(2, '0')}:${(s % 60).toString().padLeft(2, '0')}';

  // ---------- 登录 ----------
  Future<void> _showLogin() async {
    final phoneCtl = TextEditingController();
    final codeCtl = TextEditingController();
    String? err;
    await showDialog<void>(
      context: context,
      builder: (ctx) => StatefulBuilder(
        builder: (ctx, setLocal) => AlertDialog(
          backgroundColor: const Color(0xFF16181F),
          title: const Text('手机号登录'),
          content: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              TextField(controller: phoneCtl, keyboardType: TextInputType.phone,
                  decoration: const InputDecoration(hintText: '手机号')),
              TextField(controller: codeCtl, keyboardType: TextInputType.number,
                  decoration: const InputDecoration(hintText: '验证码（手机号后 6 位）')),
              if (err != null) Padding(
                padding: const EdgeInsets.only(top: 8),
                child: Text(err!, style: const TextStyle(color: Color(0xFFE23B3B), fontSize: 13)),
              ),
              const Padding(padding: EdgeInsets.only(top: 8),
                child: Text('Demo：验证码 = 手机号后 6 位', style: TextStyle(color: Color(0xFF8A8F99), fontSize: 12))),
            ],
          ),
          actions: [
            TextButton(onPressed: () => Navigator.pop(ctx), child: const Text('取消')),
            ElevatedButton(
              onPressed: () async {
                final r = await Api.login(phoneCtl.text.trim(), codeCtl.text.trim());
                if (r['code'] == 0) {
                  setState(() { _token = r['token']; _phone = r['phone']; });
                  if (ctx.mounted) Navigator.pop(ctx);
                } else {
                  setLocal(() => err = r['msg'] ?? '登录失败');
                }
              },
              child: const Text('登录'),
            ),
          ],
        ),
      ),
    );
  }

  void _logout() => setState(() { _token = null; _phone = null; });

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('📺 内网直播 · room001'),
        backgroundColor: const Color(0xFF16181F),
        actions: [
          IconButton(
            tooltip: '我的视频',
            icon: const Icon(Icons.video_library_outlined),
            onPressed: () {
              if (_token == null) { _showLogin(); return; }
              Navigator.of(context).push(MaterialPageRoute(
                builder: (_) => UserCenterPage(token: _token!, phone: _phone ?? ''),
              ));
            },
          ),
          if (_token != null)
            TextButton(onPressed: _logout, child: Text('退出(${_phone ?? ''})',
                style: const TextStyle(fontSize: 12)))
          else
            TextButton(onPressed: _showLogin, child: const Text('登录')),
        ],
      ),
      body: SafeArea(
        child: ListView(
          padding: const EdgeInsets.all(16),
          children: [
            Stack(
              children: [
                WebRTCPlayer(danmakuOn: _danmakuOn, onToggleDanmaku: _toggleDanmaku),
                if (_danmakuOn)
                  Positioned.fill(child: DanmakuOverlay(items: _danmaku, durationSec: _dmDuration)),
              ],
            ),
            if (_danmakuOn) _buildDanmakuBar(),
            const SizedBox(height: 20),
            _buildRecordBar(),
            const SizedBox(height: 8),
            Text(_toast, style: const TextStyle(color: Color(0xFFB9C0CC), fontSize: 13)),
            const SizedBox(height: 20),
            const Text('🎬 我的片段', style: TextStyle(fontSize: 15, fontWeight: FontWeight.w600)),
            const SizedBox(height: 10),
            ..._clips.map(_buildClipCard),
            if (_clips.isEmpty)
              const Padding(padding: EdgeInsets.all(24), child: Center(child: Text('暂无片段', style: TextStyle(color: Colors.grey)))),
          ],
        ),
      ),
    );
  }

  Widget _buildDanmakuBar() {
    return Padding(
      padding: const EdgeInsets.only(top: 12),
      child: Column(
        children: [
          Row(
            children: [
              Expanded(
                child: TextField(
                  controller: _dmInput,
                  maxLength: 100,
                  onSubmitted: (_) => _sendDanmaku(),
                  decoration: const InputDecoration(
                    hintText: '发条弹幕…',
                    counterText: '',
                    isDense: true,
                    filled: true,
                    fillColor: Color(0xFF16181F),
                    border: OutlineInputBorder(borderSide: BorderSide.none),
                  ),
                ),
              ),
              const SizedBox(width: 8),
              ElevatedButton(onPressed: _sendDanmaku, child: const Text('发送')),
            ],
          ),
          Row(
            children: [
              const Text('弹幕速度', style: TextStyle(color: Color(0xFF8A8F99), fontSize: 13)),
              const Text(' 慢', style: TextStyle(color: Color(0xFF8A8F99), fontSize: 12)),
              Expanded(
                child: Slider(
                  min: 1, max: 10, divisions: 9, value: _dmSpeed,
                  onChanged: (v) => setState(() => _dmSpeed = v),
                ),
              ),
              const Text('快 ', style: TextStyle(color: Color(0xFF8A8F99), fontSize: 12)),
            ],
          ),
        ],
      ),
    );
  }

  Widget _buildRecordBar() {
    return Wrap(
      spacing: 12, runSpacing: 12, crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        ElevatedButton(
          onPressed: _recording ? _stopRec : _startRec,
          style: ElevatedButton.styleFrom(
            backgroundColor: _recording ? const Color(0xFFE23B3B) : const Color(0xFF1F9D55),
            foregroundColor: Colors.white,
            padding: const EdgeInsets.symmetric(horizontal: 22, vertical: 14),
          ),
          child: Text(_recording ? '⏹ 停止录制 ${_fmt(_elapsed)}' : '⏺ 开始录制',
              style: const TextStyle(fontSize: 16, fontWeight: FontWeight.w600)),
        ),
        Row(mainAxisSize: MainAxisSize.min, children: [
          const Text('清晰度 ', style: TextStyle(color: Color(0xFF8A8F99))),
          DropdownButton<String>(
            value: _quality,
            dropdownColor: const Color(0xFF23252E),
            onChanged: _recording ? null : (v) => setState(() => _quality = v!),
            items: const [
              DropdownMenuItem(value: 'original', child: Text('原画（最快）')),
              DropdownMenuItem(value: '720p', child: Text('720p 高清')),
              DropdownMenuItem(value: '480p', child: Text('480p 标清')),
            ],
          ),
        ]),
      ],
    );
  }

  Widget _buildClipCard(ClipJob c) {
    final st = c.statusLower;
    final canDownload = st == 'done' && c.outputFile != null;
    return Card(
      color: const Color(0xFF16181F),
      shape: RoundedRectangleBorder(side: const BorderSide(color: Color(0xFF20232B)), borderRadius: BorderRadius.circular(10)),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Row(mainAxisAlignment: MainAxisAlignment.spaceBetween, children: [
            Text(_time(c.createdAt), style: const TextStyle(color: Color(0xFF8A8F99), fontSize: 13)),
            Text(_stLabels[st] ?? st, style: const TextStyle(fontSize: 12)),
          ]),
          const SizedBox(height: 8),
          Wrap(spacing: 14, children: [
            Text('⏱ ${c.duration.toStringAsFixed(1)}s', style: const TextStyle(fontSize: 13)),
            Text('🎞 ${_qLabels[c.quality] ?? c.quality}', style: const TextStyle(fontSize: 13)),
            Text('💾 ${c.fileSize ?? '—'}', style: const TextStyle(fontSize: 13)),
          ]),
          const SizedBox(height: 8),
          if (canDownload)
            SelectableText('下载：${Config.clipUrl(c.outputFile!)}',
                style: const TextStyle(color: Color(0xFF4F8CFF), fontSize: 12)),
        ]),
      ),
    );
  }

  String _time(String iso) {
    try {
      final d = DateTime.parse(iso).toLocal();
      return '${d.hour.toString().padLeft(2, '0')}:${d.minute.toString().padLeft(2, '0')}:${d.second.toString().padLeft(2, '0')}';
    } catch (_) { return iso; }
  }
}
