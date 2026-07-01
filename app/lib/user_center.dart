import 'package:flutter/material.dart';
import 'package:url_launcher/url_launcher.dart';
import 'package:video_player/video_player.dart';
import 'api.dart';
import 'config.dart';
import 'models.dart';

const _qLabels = {'original': '原画', '720p': '720p', '480p': '480p'};
const _stLabels = {'pending': '⏳ 待生成', 'processing': '⚙️ 处理中', 'done': '✅ 完成', 'error': '❌ 失败'};

/// 用户管理 / 我的视频：查看自己录制的片段，可播放、下载。
class UserCenterPage extends StatefulWidget {
  final String token;
  final String phone;
  const UserCenterPage({super.key, required this.token, required this.phone});
  @override
  State<UserCenterPage> createState() => _UserCenterPageState();
}

class _UserCenterPageState extends State<UserCenterPage> {
  List<ClipJob> _clips = [];
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    try {
      final list = await Api.myClips(widget.token);
      if (mounted) setState(() { _clips = list; _loading = false; });
    } catch (_) {
      if (mounted) setState(() => _loading = false);
    }
  }

  Future<void> _download(String file) async {
    final uri = Uri.parse(Config.clipUrl(file));
    if (!await launchUrl(uri, mode: LaunchMode.externalApplication)) {
      if (mounted) ScaffoldMessenger.of(context).showSnackBar(const SnackBar(content: Text('打开下载失败')));
    }
  }

  void _play(String file) {
    Navigator.of(context).push(MaterialPageRoute(
      builder: (_) => VideoPlayerPage(url: Config.clipUrl(file)),
    ));
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: Text('👤 ${widget.phone} 的视频'),
        backgroundColor: const Color(0xFF16181F),
        actions: [IconButton(onPressed: _load, icon: const Icon(Icons.refresh))],
      ),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : _clips.isEmpty
              ? const Center(child: Text('还没有录制的视频', style: TextStyle(color: Colors.grey)))
              : RefreshIndicator(
                  onRefresh: _load,
                  child: ListView.builder(
                    padding: const EdgeInsets.all(12),
                    itemCount: _clips.length,
                    itemBuilder: (_, i) => _card(_clips[i]),
                  ),
                ),
    );
  }

  Widget _card(ClipJob c) {
    final st = c.statusLower;
    final ready = st == 'done' && c.outputFile != null;
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
          const SizedBox(height: 10),
          if (ready)
            Row(children: [
              Expanded(child: ElevatedButton.icon(
                onPressed: () => _play(c.outputFile!),
                icon: const Icon(Icons.play_arrow), label: const Text('播放'),
              )),
              const SizedBox(width: 8),
              Expanded(child: OutlinedButton.icon(
                onPressed: () => _download(c.outputFile!),
                icon: const Icon(Icons.download), label: const Text('下载'),
              )),
            ])
          else
            const Text('处理中…', style: TextStyle(color: Color(0xFF8A8F99))),
        ]),
      ),
    );
  }

  String _time(String iso) {
    try {
      final d = DateTime.parse(iso).toLocal();
      String p(int n) => n.toString().padLeft(2, '0');
      return '${d.month}/${d.day} ${p(d.hour)}:${p(d.minute)}';
    } catch (_) { return iso; }
  }
}

/// 视频播放页（video_player）
class VideoPlayerPage extends StatefulWidget {
  final String url;
  const VideoPlayerPage({super.key, required this.url});
  @override
  State<VideoPlayerPage> createState() => _VideoPlayerPageState();
}

class _VideoPlayerPageState extends State<VideoPlayerPage> {
  late final VideoPlayerController _c;
  bool _ready = false;

  @override
  void initState() {
    super.initState();
    _c = VideoPlayerController.networkUrl(Uri.parse(widget.url))
      ..initialize().then((_) {
        if (mounted) { setState(() => _ready = true); _c.play(); }
      });
  }

  @override
  void dispose() {
    _c.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: Colors.black,
      appBar: AppBar(backgroundColor: Colors.black, foregroundColor: Colors.white),
      body: Center(
        child: _ready
            ? AspectRatio(aspectRatio: _c.value.aspectRatio, child: VideoPlayer(_c))
            : const CircularProgressIndicator(),
      ),
      floatingActionButton: _ready
          ? FloatingActionButton(
              onPressed: () => setState(() => _c.value.isPlaying ? _c.pause() : _c.play()),
              child: Icon(_c.value.isPlaying ? Icons.pause : Icons.play_arrow),
            )
          : null,
    );
  }
}
