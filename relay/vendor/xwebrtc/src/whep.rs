use super::errors::WebRTCError;
use super::errors::WebRTCErrorValue;

use std::sync::Arc;
use streamhub::define::PacketData;
use streamhub::define::PacketDataReceiver;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;

use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use tokio::sync::broadcast;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::TrackLocalWriter;

pub type Result<T> = std::result::Result<T, WebRTCError>;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

pub async fn handle_whep(
    offer: RTCSessionDescription,
    mut receiver: PacketDataReceiver,
    state_sender: broadcast::Sender<RTCPeerConnectionState>,
) -> Result<(RTCSessionDescription, Arc<RTCPeerConnection>)> {
    // Everything below is the WebRTC-rs API! Thanks for using it ❤️.

    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();

    m.register_default_codecs()?;

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

    // 禁用 mDNS：不再生成 xxxx.local 候选、也不启动 mDNS 收包 socket，直接用真实 IP。
    // Windows 上 mDNS 收到超大 UDP 包会返回 WSAEMSGSIZE(10040) 把收包循环搞崩，
    // 导致 .local 候选无法解析、ICE 建连失败 → 黑屏。禁用后两端都走真实 IP，平台差异消失。
    let mut setting_engine = webrtc::api::setting_engine::SettingEngine::default();
    setting_engine.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
    // 只用 IPv4 UDP 候选，剔除全部 IPv6。家用网络有公网 IPv6(240e:...)时，ICE 会优先选
    // 公网 IPv6 候选对：握手小包能过(connected)，但运营商/路由器挡入站 IPv6 媒体流 →
    // 几秒后 consent 失败掉成 disconnected/failed、画面一直不出。限定 Udp4 后走 IPv4 局域网直连。
    setting_engine.set_network_types(vec![webrtc::ice::network_type::NetworkType::Udp4]);

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();

    // Prepare the configuration
    // 不配 STUN：纯内网直连（且已限定 IPv4 host 候选），srflx 候选毫无用处；
    // 离线局域网里等 stun.l.google.com 超时会拖慢 ICE gathering → answer 迟迟不返回，白等数秒。
    let config = RTCConfiguration::default();

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    // Create Track that we send video back to browser on
    let video_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    // Create Track that we send video back to browser on
    let audio_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            ..Default::default()
        },
        "audio".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    // Add this newly created track to the PeerConnection
    let rtp_sender = peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    let _ = peer_connection
        .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // Read incoming RTCP packets
    // Before these packets are returned they are processed by interceptors. For things
    // like NACK this needs to be called.
    tokio::spawn(async move {
        let mut rtcp_buf = vec![0u8; 1500];
        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        Result::<()>::Ok(())
    });

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_ice_connection_state_change(Box::new(
        move |connection_state: RTCIceConnectionState| {
            log::info!("Connection State has changed {connection_state}");
            if connection_state == RTCIceConnectionState::Failed {
                // let _ = done_tx1.try_send(());
            }
            Box::pin(async {})
        },
    ));

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    let mut state_receiver = state_sender.subscribe();
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        log::info!("Peer Connection State has changed: {s}");

        if s == RTCPeerConnectionState::Failed {
            // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
            // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
            // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
            log::info!("Peer Connection has gone to failed exiting: Done forwarding");
            // let _ = done_tx2.try_send(());
        }
        if let Err(err) = state_sender.send(s) {
            log::error!("on_peer_connection_state_change send state err: {}", err);
        }

        Box::pin(async {})
    }));

    // Set the remote SessionDescription
    peer_connection.set_remote_description(offer).await?;

    // Create an answer
    let answer = peer_connection.create_answer(None).await?;

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection.set_local_description(answer).await?;

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    // Read RTP packets forever and send them to the WebRTC Client
    let pc_for_loop = Arc::clone(&peer_connection);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                av_data = receiver.recv() =>{
                    if let Some(data) = av_data {
                        match data {
                            PacketData::Video { timestamp: _, data } => {
                                if let Err(err) = video_track.write(&data[..]).await {
                                    log::error!("send video data error: {}", err);
                                }
                            }
                            PacketData::Audio { timestamp: _, data } => {
                                if let Err(err) = audio_track.write(&data[..]).await {
                                    log::error!("send audio data error: {}", err);
                                }
                            }
                        }
                    } else {
                        // 上游流已下线（发布者 UnPublish，如 RTMP 断推/桥重启）：recv 只会一直
                        // 返回 None。原实现在此空转（busy loop 烧 CPU），且 PC 不关——浏览器侧
                        // ICE 仍是 connected，播放端的断线自动重连永远不触发，只能手点重连。
                        // 改为：关掉 PC 让浏览器立刻感知断流走自动重连，退出本循环。
                        log::info!("WHEP 上游流已下线，关闭订阅端 peer connection 触发播放端重连");
                        if let Err(err) = pc_for_loop.close().await {
                            log::error!("close peer connection on upstream gone error: {}", err);
                        }
                        break;
                    }
                }
                pc_state = state_receiver.recv() =>{
                    if let Ok(state) = pc_state{
                        if state == RTCPeerConnectionState::Closed {
                            break;
                        }
                    }
                }
            }
        }
    });

    // Output the answer in base64 so we can paste it in browser
    if let Some(local_desc) = peer_connection.local_description().await {
        Ok((local_desc, peer_connection))
    } else {
        Err(WebRTCError {
            value: WebRTCErrorValue::CanNotGetLocalDescription,
        })
    }
}
