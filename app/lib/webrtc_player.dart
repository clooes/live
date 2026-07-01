import 'dart:async';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_webrtc/flutter_webrtc.dart';
import 'package:http/http.dart' as http;
import 'config.dart';

/// WebRTC(WHEP) 播放器：只收不发，对接 SRS /rtc/v1/whep/。
/// YouTube 风格：底部悬浮控制栏，点画面显隐、3 秒自动隐藏。
class WebRTCPlayer extends StatefulWidget {
  final bool danmakuOn;
  final VoidCallback onToggleDanmaku;
  const WebRTCPlayer({super.key, required this.danmakuOn, required this.onToggleDanmaku});
  @override
  State<WebRTCPlayer> createState() => _WebRTCPlayerState();
}

class _WebRTCPlayerState extends State<WebRTCPlayer> {
  final RTCVideoRenderer _renderer = RTCVideoRenderer();
  RTCPeerConnection? _pc;
  String _status = '初始化…';
  bool _live = false;
  bool _paused = false;
  bool _controlsVisible = true;
  Timer? _hideTimer;

  @override
  void initState() {
    super.initState();
    _init();
    _scheduleHide();
  }

  void _toggleControls() {
    setState(() => _controlsVisible = !_controlsVisible);
    if (_controlsVisible) _scheduleHide();
  }

  void _scheduleHide() {
    _hideTimer?.cancel();
    _hideTimer = Timer(const Duration(seconds: 3), () {
      if (mounted) setState(() => _controlsVisible = false);
    });
  }

  Future<void> _init() async {
    await _renderer.initialize();
    _connect();
  }

  Future<void> _connect() async {
    await _cleanupPc();
    setState(() {
      _status = 'WebRTC 连接中…';
      _live = false;
    });
    try {
      final pc = await createPeerConnection({'iceServers': []});
      _pc = pc;

      pc.onTrack = (RTCTrackEvent e) {
        if (e.streams.isNotEmpty) {
          _renderer.srcObject = e.streams[0];
          if (mounted) setState(() => _live = true);
        }
      };
      pc.onConnectionState = (RTCPeerConnectionState s) {
        if (s == RTCPeerConnectionState.RTCPeerConnectionStateFailed ||
            s == RTCPeerConnectionState.RTCPeerConnectionStateDisconnected) {
          if (_paused) return; // 暂停态不自动重连
          if (mounted) setState(() => _status = 'WebRTC 断开，3 秒后重连…');
          Future.delayed(const Duration(seconds: 3), () { if (mounted && !_paused) _connect(); });
        } else if (s == RTCPeerConnectionState.RTCPeerConnectionStateConnected) {
          if (mounted) setState(() => _status = '直播中');
        }
      };

      // 只收：视频 + 音频
      await pc.addTransceiver(
        kind: RTCRtpMediaType.RTCRtpMediaTypeVideo,
        init: RTCRtpTransceiverInit(direction: TransceiverDirection.RecvOnly),
      );
      await pc.addTransceiver(
        kind: RTCRtpMediaType.RTCRtpMediaTypeAudio,
        init: RTCRtpTransceiverInit(direction: TransceiverDirection.RecvOnly),
      );

      final offer = await pc.createOffer();
      await pc.setLocalDescription(offer);

      final resp = await http.post(
        Uri.parse(Config.whepUrl),
        headers: {'Content-Type': 'application/sdp'},
        body: offer.sdp,
      );
      if (resp.statusCode == 200 || resp.statusCode == 201) {
        await pc.setRemoteDescription(RTCSessionDescription(resp.body, 'answer'));
      } else {
        throw Exception('WHEP ${resp.statusCode}');
      }
    } catch (e) {
      if (_paused) return;
      if (mounted) setState(() => _status = 'WebRTC 失败：$e');
      Future.delayed(const Duration(seconds: 3), () { if (mounted && !_paused) _connect(); });
    }
  }

  Future<void> _cleanupPc() async {
    try { await _pc?.close(); } catch (_) {}
    _pc = null;
  }

  /// 暂停：断开拉流（省带宽），清空画面
  Future<void> _pause() async {
    setState(() { _paused = true; _live = false; _status = '已暂停'; });
    await _cleanupPc();
    _renderer.srcObject = null;
  }

  /// 播放：重新建立 WebRTC 连接
  void _resume() {
    setState(() => _paused = false);
    _connect();
  }

  /// 进入全屏：横屏 + 沉浸式，复用同一 renderer（不重连）；pop 回来后恢复竖屏。
  Future<void> _enterFullscreen() async {
    await SystemChrome.setPreferredOrientations(
        [DeviceOrientation.landscapeLeft, DeviceOrientation.landscapeRight]);
    await SystemChrome.setEnabledSystemUIMode(SystemUiMode.immersiveSticky);
    if (!mounted) return;
    await Navigator.of(context).push(MaterialPageRoute(
      fullscreenDialog: true,
      builder: (_) => _FullscreenView(renderer: _renderer),
    ));
    await SystemChrome.setPreferredOrientations([DeviceOrientation.portraitUp]);
    await SystemChrome.setEnabledSystemUIMode(SystemUiMode.edgeToEdge);
  }

  @override
  void dispose() {
    _hideTimer?.cancel();
    _cleanupPc();
    _renderer.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return AspectRatio(
      aspectRatio: 16 / 9,
      child: GestureDetector(
        onTap: _toggleControls,
        child: Container(
          decoration: BoxDecoration(
            color: Colors.black,
            borderRadius: BorderRadius.circular(12),
          ),
          clipBehavior: Clip.antiAlias,
          child: Stack(
            children: [
              Positioned.fill(
                child: RTCVideoView(_renderer, objectFit: RTCVideoViewObjectFit.RTCVideoViewObjectFitContain),
              ),
              // 左上：直播状态
              Positioned(left: 10, top: 10, child: _statusChip()),
              // 底部 YouTube 风格控制栏
              Positioned(
                left: 0, right: 0, bottom: 0,
                child: AnimatedOpacity(
                  opacity: _controlsVisible ? 1 : 0,
                  duration: const Duration(milliseconds: 200),
                  child: IgnorePointer(ignoring: !_controlsVisible, child: _controlBar()),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }

  Widget _statusChip() {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
      decoration: BoxDecoration(color: Colors.black54, borderRadius: BorderRadius.circular(20)),
      child: Row(mainAxisSize: MainAxisSize.min, children: [
        Icon(Icons.circle, size: 8, color: _live ? Colors.redAccent : Colors.grey),
        const SizedBox(width: 6),
        Text(_status, style: const TextStyle(color: Colors.white, fontSize: 12)),
      ]),
    );
  }

  Widget _controlBar() {
    return Container(
      padding: const EdgeInsets.fromLTRB(4, 18, 4, 2),
      decoration: const BoxDecoration(
        gradient: LinearGradient(
          begin: Alignment.bottomCenter,
          end: Alignment.topCenter,
          colors: [Colors.black87, Colors.transparent],
        ),
      ),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.spaceBetween,
        children: [
          IconButton(
            icon: Icon(_paused ? Icons.play_arrow : Icons.pause, color: Colors.white, size: 28),
            tooltip: _paused ? '播放' : '暂停',
            onPressed: () { (_paused ? _resume : _pause)(); _scheduleHide(); },
          ),
          Row(mainAxisSize: MainAxisSize.min, children: [
            IconButton(
              icon: Icon(widget.danmakuOn ? Icons.comment : Icons.comments_disabled_outlined,
                  color: Colors.white, size: 24),
              tooltip: widget.danmakuOn ? '关闭弹幕' : '开启弹幕',
              onPressed: () { widget.onToggleDanmaku(); _scheduleHide(); },
            ),
            IconButton(
              icon: const Icon(Icons.fullscreen, color: Colors.white, size: 28),
              tooltip: '全屏',
              onPressed: _enterFullscreen,
            ),
          ]),
        ],
      ),
    );
  }
}

/// 全屏播放页：复用传入的 renderer，横屏铺满，右上角退出。
class _FullscreenView extends StatelessWidget {
  final RTCVideoRenderer renderer;
  const _FullscreenView({required this.renderer});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: Colors.black,
      body: Stack(
        children: [
          Positioned.fill(
            child: RTCVideoView(renderer,
                objectFit: RTCVideoViewObjectFit.RTCVideoViewObjectFitContain),
          ),
          Positioned(
            top: 8, right: 8,
            child: SafeArea(
              child: IconButton(
                icon: const Icon(Icons.fullscreen_exit, color: Colors.white70, size: 30),
                tooltip: '退出全屏',
                onPressed: () => Navigator.of(context).pop(),
              ),
            ),
          ),
        ],
      ),
    );
  }
}
