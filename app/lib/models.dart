/// 裁剪任务（对应后端 ClipJob）
class ClipJob {
  final String id;
  final String streamId;
  final double startOffset;
  final double duration;
  final String quality;
  final String status; // Pending / Processing / Done / Error
  final String? outputFile;
  final String? fileSize;
  final String createdAt;

  ClipJob({
    required this.id,
    required this.streamId,
    required this.startOffset,
    required this.duration,
    required this.quality,
    required this.status,
    required this.outputFile,
    required this.fileSize,
    required this.createdAt,
  });

  factory ClipJob.fromJson(Map<String, dynamic> j) => ClipJob(
        id: j['id'] ?? '',
        streamId: j['stream_id'] ?? '',
        startOffset: (j['start_offset'] ?? 0).toDouble(),
        duration: (j['duration'] ?? 0).toDouble(),
        quality: j['quality'] ?? 'original',
        status: j['status'] ?? '',
        outputFile: j['output_file'],
        fileSize: j['file_size'],
        createdAt: j['created_at'] ?? '',
      );

  String get statusLower => status.toLowerCase();
}
