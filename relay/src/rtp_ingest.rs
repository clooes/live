//! RTP 注入器：把 ffmpeg 输出的裸 RTP（H264/Opus，本机 UDP 直收）作为 WebRTC 流发布进
//! streamhub，供 WHEP 播放。
//!
//! 为什么不用 ffmpeg `-f whip` 回推：whip muxer 依赖 ffmpeg 的 DTLS 后端与 webrtc-rs 完成
//! DTLS-SRTP 握手，而 Windows 的静态构建全军覆没——gyan.dev 是 GnuTLS 后端（DTLS 强走 CA 链
//! 校验，WebRTC 自签名证书必失败）、BtbN 是 SChannel 后端（不支持 use_srtp 扩展，webrtc-rs
//! 直接放弃：`SRTP support was requested but server did not respond with use_srtp extension`）。
//! 唯一能用的 OpenSSL 后端在 Windows 没有现成静态构建。
//!
//! 而 WHEP 端（xwebrtc whep.rs）本就只消费完整 RTP 包（`PacketData`），写入
//! `TrackLocalStaticRTP` 时 SSRC/PT 会按与浏览器协商的结果重写。所以让 ffmpeg 输出裸 RTP 到
//! 本机 UDP、这里收包转成 `PacketData` 发布，效果等同 WHIP 推流，却把 DTLS 整条链绕开；
//! whip muxer 不再是必需，各平台内置 ffmpeg 即可（macOS 也不用再回退 homebrew）。

use std::sync::Arc;

use bytes::BytesMut;
use streamhub::define::{
    FrameData, NotifyInfo, PacketData, PacketDataSender, PubDataType, PublishType, PublisherInfo,
    StreamHubEvent, StreamHubEventSender,
};
use streamhub::stream::StreamIdentifier;
use streamhub::utils::{RandomDigitCount, Uuid};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

/// 一路已发布进 streamhub 的 RTP 注入流：持有收包 task 与发布者身份，`stop()` 收尾。
pub struct RtpIngest {
    /// 视频 RTP 收包端口（127.0.0.1，随机分配），供拼 ffmpeg 输出 URL。
    pub video_port: u16,
    /// 音频 RTP 收包端口（同上）。
    pub audio_port: u16,
    app: String,
    stream: String,
    publisher_id: Uuid,
    hub: StreamHubEventSender,
    tasks: Vec<JoinHandle<()>>,
}

impl RtpIngest {
    /// 发布一路 WebRTC 流并开始收 RTP：video/audio 各绑一个 127.0.0.1 随机 UDP 端口。
    /// RTCP 无需处理：ffmpeg 默认把 RTCP 发到 RTP 端口 +1，我们不绑它，自然丢弃。
    pub async fn start(hub: StreamHubEventSender, app: &str, stream: &str) -> anyhow::Result<Self> {
        let video_sock = UdpSocket::bind("127.0.0.1:0").await?;
        let audio_sock = UdpSocket::bind("127.0.0.1:0").await?;
        let video_port = video_sock.local_addr()?.port();
        let audio_port = audio_sock.local_addr()?.port();

        let publisher_id = Uuid::new(RandomDigitCount::Zero);
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let publish = StreamHubEvent::Publish {
            identifier: StreamIdentifier::WebRTC {
                app_name: app.to_string(),
                stream_name: stream.to_string(),
            },
            result_sender: result_tx,
            info: PublisherInfo {
                id: publisher_id,
                pub_type: PublishType::RtpPush,
                pub_data_type: PubDataType::Both,
                notify_info: NotifyInfo {
                    request_url: String::new(),
                    remote_addr: String::new(),
                },
            },
            stream_handler: Arc::new(xwebrtc::session::WebRTCStreamHandler::default()),
        };
        hub.send(publish)
            .map_err(|_| anyhow::anyhow!("hub 事件通道已关闭"))?;
        let (frame_sender, packet_sender, _stat) = result_rx
            .await
            .map_err(|e| anyhow::anyhow!("等待 hub publish 结果失败: {e}"))?
            .map_err(|e| anyhow::anyhow!("hub publish 失败: {e}"))?;
        let packet_sender =
            packet_sender.ok_or_else(|| anyhow::anyhow!("hub 未返回 packet sender"))?;

        // WHEP/统计侧换算时间戳用的时钟率（桥固定转出 H264/90k + Opus/48k）
        if let Some(fs) = &frame_sender {
            let _ = fs.send(FrameData::MediaInfo {
                media_info: streamhub::define::MediaInfo {
                    audio_clock_rate: 48000,
                    video_clock_rate: 90000,
                    vcodec: streamhub::define::VideoCodecType::H264,
                },
            });
        }

        let tasks = vec![
            tokio::spawn(pump(video_sock, packet_sender.clone(), true)),
            tokio::spawn(pump(audio_sock, packet_sender, false)),
        ];

        Ok(Self {
            video_port,
            audio_port,
            app: app.to_string(),
            stream: stream.to_string(),
            publisher_id,
            hub,
            tasks,
        })
    }

    /// 停止收包并从 streamhub 撤掉发布（WHEP 订阅者随之收尾）。
    pub fn stop(self) {
        for t in &self.tasks {
            t.abort();
        }
        let unpublish = StreamHubEvent::UnPublish {
            identifier: StreamIdentifier::WebRTC {
                app_name: self.app,
                stream_name: self.stream,
            },
            info: PublisherInfo {
                id: self.publisher_id,
                pub_type: PublishType::RtpPush,
                pub_data_type: PubDataType::Both,
                notify_info: NotifyInfo {
                    request_url: String::new(),
                    remote_addr: String::new(),
                },
            },
        };
        if self.hub.send(unpublish).is_err() {
            log::warn!("RTP 注入流 unpublish 发送失败（hub 已关闭）");
        }
    }
}

/// 收包循环：一个 UDP 数据报 = 一个 RTP 包，原样转发进 hub。
async fn pump(sock: UdpSocket, sender: PacketDataSender, is_video: bool) {
    let kind = if is_video { "video" } else { "audio" };
    let mut buf = vec![0u8; 2048];
    let mut got_first = false;
    loop {
        match sock.recv(&mut buf).await {
            // RTP 固定头 12 字节，短于此非 RTP，丢弃
            Ok(n) if n >= 12 => {
                if !got_first {
                    got_first = true;
                    log::info!("RTP 注入 {kind} 首包已到（{n} 字节）");
                }
                let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                let data = BytesMut::from(&buf[..n]);
                let pkt = if is_video {
                    PacketData::Video { timestamp, data }
                } else {
                    PacketData::Audio { timestamp, data }
                };
                // hub 侧关闭（unpublish）即结束
                if sender.send(pkt).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("RTP 注入 {kind} 收包错误: {e}，停止该路");
                return;
            }
        }
    }
}
