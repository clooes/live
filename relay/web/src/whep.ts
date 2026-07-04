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
//
// 自愈设计（修「进页面不自动播、要手点多次重连」）：
// - 单一重试定时器：失败/断开/看门狗都走 scheduleRetry，先清旧定时器，避免多条重试链叠加，
//   多个 setTimeout 同时醒来互相 cleanup 对方刚建的连接。
// - 出画面看门狗：WHEP 握手成功 ≠ 有画面（订阅可能夹在流重建间隙、或等关键帧超时），
//   握手后 8s 内没进入 playing 就整个重来。
// - 服务端配合：上游断流时会主动关订阅端 PC（xwebrtc whep.rs），这里 connectionState
//   变 failed/disconnected/closed 即自动重连，无需人工。
export function useWhep(videoRef: RefObject<HTMLVideoElement>, room: string, webrtcPort: number): WhepApi {
  const [status, setStatus] = useState('未连接')
  const [live, setLive] = useState(false)
  const pcRef = useRef<RTCPeerConnection | null>(null)
  const stoppedRef = useRef(false)
  const playingRef = useRef(false)
  const retryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const watchdogRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const playRef = useRef<() => void>(() => {})

  const clearTimers = useCallback(() => {
    if (retryTimerRef.current) { clearTimeout(retryTimerRef.current); retryTimerRef.current = null }
    if (watchdogRef.current) { clearTimeout(watchdogRef.current); watchdogRef.current = null }
  }, [])

  const cleanup = useCallback(() => {
    clearTimers()
    if (pcRef.current) {
      try { pcRef.current.close() } catch { /* noop */ }
      pcRef.current = null
    }
    const v = videoRef.current
    if (v) v.srcObject = null
  }, [videoRef, clearTimers])

  // 统一入口：清掉已排的重试再排新的，保证任意时刻至多一条重试链
  const scheduleRetry = useCallback((delayMs: number) => {
    if (stoppedRef.current) return
    if (retryTimerRef.current) clearTimeout(retryTimerRef.current)
    retryTimerRef.current = setTimeout(() => playRef.current(), delayMs)
  }, [])

  const play = useCallback(async () => {
    if (!room || !webrtcPort) return
    cleanup()
    setLive(false)
    playingRef.current = false
    setStatus('WebRTC 连接中…')
    try {
      const pc = new RTCPeerConnection()
      pcRef.current = pc
      pc.addTransceiver('video', { direction: 'recvonly' })
      pc.addTransceiver('audio', { direction: 'recvonly' })
      pc.ontrack = (e) => {
        if (e.streams && e.streams[0] && videoRef.current) {
          const v = videoRef.current
          v.srcObject = e.streams[0]
          // R1：流一就绪即显式播放；muted 必须为 true 浏览器才允许无手势自动播
          v.muted = true
          v.play().catch(() => {})
        }
      }
      pc.onconnectionstatechange = () => {
        if (pcRef.current !== pc) return // 已被新一轮连接替换，旧 PC 的事件不作数
        const s = pc.connectionState
        if (s === 'failed' || s === 'disconnected' || s === 'closed') {
          setStatus('连接断开，3 秒后重连…')
          scheduleRetry(3000)
        }
      }
      const offer = await pc.createOffer()
      await pc.setLocalDescription(offer)
      const resp = await fetch(whepUrl(room, webrtcPort), {
        method: 'POST',
        headers: { 'Content-Type': 'application/sdp' },
        body: offer.sdp,
        // 兜底：信令端若挂起（历史上有连接复用坑）不至于卡死在「连接中」，超时走重试
        signal: AbortSignal.timeout(8000),
      })
      if (!resp.ok) throw new Error('WHEP ' + resp.status)
      if (pcRef.current !== pc) return // 等响应期间被手动重连/新一轮替换
      await pc.setRemoteDescription({ type: 'answer', sdp: await resp.text() })
      videoRef.current?.play().catch(() => {})
      // 出画面看门狗：握手成功但 8s 内没 playing（等关键帧超时/订到僵尸流）→ 整个重来
      if (watchdogRef.current) clearTimeout(watchdogRef.current)
      watchdogRef.current = setTimeout(() => {
        if (!playingRef.current && pcRef.current === pc && !stoppedRef.current) {
          setStatus('画面迟迟未到，重连中…')
          playRef.current()
        }
      }, 8000)
    } catch (err) {
      setStatus('连接失败：' + (err as Error).message + '（等待推流…）')
      scheduleRetry(3000)
    }
  }, [cleanup, room, videoRef, webrtcPort, scheduleRetry])
  playRef.current = play

  useEffect(() => {
    stoppedRef.current = false
    const v = videoRef.current
    const onPlaying = () => {
      playingRef.current = true
      if (watchdogRef.current) { clearTimeout(watchdogRef.current); watchdogRef.current = null }
      setLive(true); setStatus('直播中')
    }
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
