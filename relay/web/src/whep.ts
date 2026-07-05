import { useCallback, useEffect, useRef, useState, type RefObject } from 'react'
import { whepUrl } from './api'

export interface WhepApi {
  status: string
  live: boolean
  /** 自动播放被浏览器拦截（iOS 低电量模式等），需要用户点一下恢复 */
  needTap: boolean
  /** 在用户手势里调用：恢复播放（配合 needTap 的「点击播放」浮层） */
  resume: () => void
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
//
// iOS 自动播放（修「iPhone 扫码进来一直黑屏，要手点」）：
// - muted/playsinline 用 setAttribute 直挂 DOM（React 对 muted 属性的渲染有历史怪癖，
//   iOS 判定自动播放资格看的是元素属性，双保险）。
// - play() 被拒（NotAllowedError，典型是 iOS 低电量/省流量模式禁一切自动播）→ 置 needTap，
//   页面显示「点击播放」浮层；用户手势里调 resume() 即恢复。此状态下看门狗不能重连——
//   流是好的，只是播放被拦，重连只会无限循环。
// - loadedmetadata 后再补一次 play()：iOS 上 ontrack 时可能还没数据，首次 play() 会白试。
export function useWhep(videoRef: RefObject<HTMLVideoElement>, room: string, webrtcPort: number): WhepApi {
  const [status, setStatus] = useState('未连接')
  const [live, setLive] = useState(false)
  const [needTap, setNeedTap] = useState(false)
  const pcRef = useRef<RTCPeerConnection | null>(null)
  const stoppedRef = useRef(false)
  const playingRef = useRef(false)
  const needTapRef = useRef(false)
  const retryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const watchdogRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const playRef = useRef<() => void>(() => {})

  const clearTimers = useCallback(() => {
    if (retryTimerRef.current) { clearTimeout(retryTimerRef.current); retryTimerRef.current = null }
    if (watchdogRef.current) { clearTimeout(watchdogRef.current); watchdogRef.current = null }
  }, [])

  const cleanup = useCallback(() => {
    clearTimers()
    // 先摘 srcObject 再关 PC：关 PC 可能触发 pause 事件，onPause 以 srcObject 为「活跃连接」
    // 标志，先摘掉避免重连瞬间误亮「已暂停」浮层
    const v = videoRef.current
    if (v) v.srcObject = null
    if (pcRef.current) {
      try { pcRef.current.close() } catch { /* noop */ }
      pcRef.current = null
    }
  }, [videoRef, clearTimers])

  // 统一入口：清掉已排的重试再排新的，保证任意时刻至多一条重试链
  const scheduleRetry = useCallback((delayMs: number) => {
    if (stoppedRef.current) return
    if (retryTimerRef.current) clearTimeout(retryTimerRef.current)
    retryTimerRef.current = setTimeout(() => playRef.current(), delayMs)
  }, [])

  // 尝试自动播放：被拦截（NotAllowedError）时亮出「点击播放」，其余错误静默（等下次时机再试）
  const tryPlay = useCallback(() => {
    const v = videoRef.current
    if (!v) return
    v.muted = true
    v.play().then(() => {
      needTapRef.current = false
      setNeedTap(false)
    }).catch((err: unknown) => {
      if ((err as DOMException)?.name === 'NotAllowedError') {
        needTapRef.current = true
        setNeedTap(true)
        setStatus('点击画面开始播放')
      }
    })
  }, [videoRef])

  // 用户手势里恢复播放（浮层点击）。必须同步调 play() 才带手势资格。
  const resume = useCallback(() => {
    const v = videoRef.current
    if (!v) return
    v.muted = true
    needTapRef.current = false
    setNeedTap(false)
    v.play().catch(() => { needTapRef.current = true; setNeedTap(true) })
  }, [videoRef])

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
          tryPlay()
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
      tryPlay()
      // 出画面看门狗：握手成功但 8s 内没 playing（等关键帧超时/订到僵尸流）→ 整个重来。
      // 但自动播放被拦（needTap）不算故障——流是好的，重连只会无限循环，等用户点。
      if (watchdogRef.current) clearTimeout(watchdogRef.current)
      watchdogRef.current = setTimeout(() => {
        if (!playingRef.current && !needTapRef.current && pcRef.current === pc && !stoppedRef.current) {
          setStatus('画面迟迟未到，重连中…')
          playRef.current()
        }
      }, 8000)
    } catch (err) {
      setStatus('连接失败：' + (err as Error).message + '（等待推流…）')
      scheduleRetry(3000)
    }
  }, [cleanup, room, videoRef, webrtcPort, scheduleRetry, tryPlay])
  playRef.current = play

  useEffect(() => {
    stoppedRef.current = false
    const v = videoRef.current
    // iOS 自动播放资格看元素属性；React 对 muted 的渲染有历史怪癖，直挂 DOM 双保险
    if (v) {
      v.muted = true
      v.setAttribute('muted', '')
      v.setAttribute('playsinline', '')
      v.setAttribute('autoplay', '')
    }
    const onPlaying = () => {
      playingRef.current = true
      needTapRef.current = false
      if (watchdogRef.current) { clearTimeout(watchdogRef.current); watchdogRef.current = null }
      setNeedTap(false)
      setLive(true); setStatus('直播中')
    }
    // iOS 上 ontrack 时常还没媒体数据，首次 play() 可能白试；拿到元数据后再补一次
    const onLoadedMeta = () => { if (!playingRef.current && !needTapRef.current) tryPlay() }
    // 播放中被暂停（iOS 切后台回来被系统暂停 / 用户点了暂停）：直播暂停没有意义且 UI 会
    // 停留在「直播中」假象，亮「点击播放」引导恢复。重连 cleanup 时 srcObject 已置空，跳过。
    const onPause = () => {
      if (stoppedRef.current || !v?.srcObject) return
      playingRef.current = false
      needTapRef.current = true
      setLive(false)
      setNeedTap(true)
      setStatus('已暂停，点击播放')
    }
    v?.addEventListener('playing', onPlaying)
    v?.addEventListener('loadedmetadata', onLoadedMeta)
    v?.addEventListener('pause', onPause)
    play()
    return () => {
      stoppedRef.current = true
      v?.removeEventListener('playing', onPlaying)
      v?.removeEventListener('loadedmetadata', onLoadedMeta)
      v?.removeEventListener('pause', onPause)
      cleanup()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [room, webrtcPort])

  return { status, live, needTap, resume, reconnect: play }
}
