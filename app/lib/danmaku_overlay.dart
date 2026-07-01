import 'package:flutter/material.dart';

class DanmakuMsg {
  final int id;
  final String text;
  final int track; // 轨道 0..4
  DanmakuMsg(this.id, this.text, this.track);
}

/// 弹幕显示层：把每条弹幕从右向左飘过。叠在播放器上层。
class DanmakuOverlay extends StatelessWidget {
  final List<DanmakuMsg> items;
  final int durationSec; // 飘过时长（越小越快）
  const DanmakuOverlay({super.key, required this.items, required this.durationSec});

  @override
  Widget build(BuildContext context) {
    return IgnorePointer(
      child: ClipRect(
        child: Stack(
          children: items
              .map((m) => _Flying(key: ValueKey(m.id), msg: m, durationSec: durationSec))
              .toList(),
        ),
      ),
    );
  }
}

class _Flying extends StatefulWidget {
  final DanmakuMsg msg;
  final int durationSec;
  const _Flying({super.key, required this.msg, required this.durationSec});
  @override
  State<_Flying> createState() => _FlyingState();
}

class _FlyingState extends State<_Flying> with SingleTickerProviderStateMixin {
  late final AnimationController _c;

  @override
  void initState() {
    super.initState();
    _c = AnimationController(vsync: this, duration: Duration(seconds: widget.durationSec))..forward();
  }

  @override
  void dispose() {
    _c.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final child = Text(
      widget.msg.text,
      style: const TextStyle(
        color: Colors.white,
        fontSize: 16,
        fontWeight: FontWeight.w600,
        shadows: [Shadow(color: Colors.black, blurRadius: 3, offset: Offset(0, 1))],
      ),
    );
    return AnimatedBuilder(
      animation: _c,
      child: child,
      builder: (context, child) {
        return LayoutBuilder(
          builder: (context, c) {
            final w = c.maxWidth;
            final x = w - _c.value * (w + 300); // 从右 w 飘到左 -300
            return Positioned(top: 8.0 + widget.msg.track * 28.0, left: x, child: child!);
          },
        );
      },
    );
  }
}
