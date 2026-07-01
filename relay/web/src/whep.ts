import { useCallback, useEffect, useRef, useState, type RefObject } from 'react'
import { whepUrl } from './api'

export interface WhepApi {
  status: string
  live: boolean
  reconnect: () => void
}

// WHEP 播放：createOffer → POST /whep → setRemoteDescription；断流自动重连。
// 复用自 frontend/src/hooks/usePlayer.ts 的 WebRTC 分支，去掉 FLV，信令走 relay webrtc 端口。
// webrtcPort 由 /api/config 下发（端口可配，见 R8）；为 0 时表示配置未就绪，暂不连接。
export function useWhep(videoRef: RefObject<HTMLVideoElement>, room: string, webrtcPort: number): WhepApi {
  const [status, setStatus] = useState('未连接')
  const [live, setLive] = useState(false)
  const pcRef = useRef<RTCPeerConnection | null>(null)
  const stoppedRef = useRef(false)

  const cleanup = useCallback(() => {
    if (pcRef.current) {
      try { pcRef.current.close() } catch { /* noop */ }
      pcRef.current = null
    }
    const v = videoRef.current
    if (v) v.srcObject = null
  }, [videoRef])

  const play = useCallback(async () => {
    if (!room || !webrtcPort) return
    cleanup()
    setLive(false)
    setStatus('WebRTC 连接中…')
    try {
      const pc = new RTCPeerConnection()
      pcRef.current = pc
      pc.addTransceiver('video', { direction: 'recvonly' })
      pc.addTransceiver('audio', { direction: 'recvonly' })
      pc.ontrack = (e) => {
        if (e.streams && e.streams[0] && videoRef.current) {
          videoRef.current.srcObject = e.streams[0]
          // R1：流一就绪即显式播放（muted 下浏览器允许自动播），避免需手动点
          videoRef.current.play().catch(() => {})
        }
      }
      pc.onconnectionstatechange = () => {
        const s = pc.connectionState
        if (s === 'failed' || s === 'disconnected') {
          setStatus('WebRTC 断开，3 秒后重连…')
          if (!stoppedRef.current) setTimeout(play, 3000)
        }
      }
      const offer = await pc.createOffer()
      await pc.setLocalDescription(offer)
      const resp = await fetch(whepUrl(room, webrtcPort), {
        method: 'POST',
        headers: { 'Content-Type': 'application/sdp' },
        body: offer.sdp,
      })
      if (!resp.ok) throw new Error('WHEP ' + resp.status)
      await pc.setRemoteDescription({ type: 'answer', sdp: await resp.text() })
      videoRef.current?.play().catch(() => {})
    } catch (err) {
      setStatus('连接失败：' + (err as Error).message + '（等待推流…）')
      if (!stoppedRef.current) setTimeout(play, 3000)
    }
  }, [cleanup, room, videoRef, webrtcPort])

  useEffect(() => {
    stoppedRef.current = false
    const v = videoRef.current
    const onPlaying = () => { setLive(true); setStatus('直播中') }
    v?.addEventListener('playing', onPlaying)
    play()
    return () => {
      stoppedRef.current = true
      v?.removeEventListener('playing', onPlaying)
      cleanup()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [room, webrtcPort])

  return { status, live, reconnect: play }
}
