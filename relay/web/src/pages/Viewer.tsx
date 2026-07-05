import { useEffect, useRef, useState } from 'react'
import { QRCodeSVG } from 'qrcode.react'
import { useWhep } from '../whep'
import {
  clipUrl, getLanIp, getConfig, getUid,
  recordState, recordStart, recordStop, listRecords,
  type Quality, type RecordItem, type RelayConfig,
} from '../api'

export function Viewer() {
  const videoRef = useRef<HTMLVideoElement>(null)
  const wrapRef = useRef<HTMLDivElement>(null)
  const [room, setRoom] = useState('')
  const [webrtcPort, setWebrtcPort] = useState(0)
  const [synced, setSynced] = useState(false)
  // 断流/失败全自动重连（useWhep 内含重试链 + 出画面看门狗），页面不再放手动重连按钮
  const { status, live } = useWhep(videoRef, room, webrtcPort)

  // 全屏：优先全屏容器（连同状态浮层）；iPhone Safari 不支持元素全屏，回退 video 原生全屏
  function toggleFullscreen() {
    if (document.fullscreenElement) {
      document.exitFullscreen().catch(() => {})
      return
    }
    const wrap = wrapRef.current
    const v = videoRef.current as (HTMLVideoElement & { webkitEnterFullscreen?: () => void }) | null
    if (wrap?.requestFullscreen) {
      wrap.requestFullscreen().catch(() => v?.webkitEnterFullscreen?.())
    } else {
      v?.webkitEnterFullscreen?.()
    }
  }

  // SSE 订阅配置：连接即收到当前快照，管理端一改立即收到新配置并自动切换。
  useEffect(() => {
    const es = new EventSource('/api/config/stream')
    es.onmessage = (e) => {
      try {
        const c: RelayConfig = JSON.parse(e.data)
        setRoom(c.room)
        setWebrtcPort(c.ports?.webrtc ?? 8900)
        setSynced(true)
      } catch { /* 忽略心跳/坏包 */ }
    }
    es.onerror = () => setSynced(false)
    return () => es.close()
  }, [])

  return (
    <div className="viewer">
      <div className="player-wrap" ref={wrapRef}>
        <video ref={videoRef} autoPlay playsInline muted controls className="player"
          onDoubleClick={toggleFullscreen} />
        {!live && <div className="overlay">{status}</div>}
        {/* 仅全屏时显示（CSS :fullscreen 控制）：全屏后工具栏被盖住，需页面内退出途径 */}
        <button className="fs-exit" onClick={toggleFullscreen}>✕ 退出全屏</button>
      </div>
      <div className="bar">
        <span className={live ? 'dot live' : 'dot'} />
        <span>{live ? '直播中' : status}</span>
        <span className="room">
          房间：{room || '…'}
          <span className="sync" title="配置实时同步">{synced ? ' · 已同步' : ' · 离线'}</span>
        </span>
        <button onClick={toggleFullscreen}>⛶ 全屏</button>
        <ShareButton />
      </div>
      <RecordPanel />
    </div>
  )
}

/// 分享按钮：点开弹二维码，手机扫码进入本直播页（http://<内网IP>:<web端口>）。
function ShareButton() {
  const [open, setOpen] = useState(false)
  const [url, setUrl] = useState('')
  const [err, setErr] = useState('')

  async function onOpen() {
    setErr(''); setOpen(true)
    try {
      const { ip, web_port } = await getLanIp()
      // 探测失败则回退当前浏览器地址（跨设备时可能是 localhost，仅本机可用）
      setUrl(ip ? `http://${ip}:${web_port}` : `${location.protocol}//${location.host}`)
    } catch (e) {
      setErr(String(e))
      setUrl(`${location.protocol}//${location.host}`)
    }
  }

  return (
    <>
      <button onClick={onOpen}>📱 分享</button>
      {open && (
        <div className="modal-mask" onClick={() => setOpen(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3>扫码进入直播</h3>
            {url && (
              <div className="qr-box">
                <QRCodeSVG value={url} size={200} includeMargin />
              </div>
            )}
            <p className="qr-url">{url || '获取地址中…'}</p>
            {err && <p className="rec-err">内网 IP 探测失败，已回退当前地址</p>}
            <p className="qr-tip">手机需与本机在同一局域网 / Wi-Fi</p>
            <button onClick={() => setOpen(false)}>关闭</button>
          </div>
        </div>
      )}
    </>
  )
}

function fmtTime(ms: number): string {
  return new Date(ms).toLocaleString()
}

/// 录制面板：选清晰度 → 开始录制（当场录成成品 mp4，有声）→ 停止即就绪，直接下载。
/// 无整场回放、无裁剪、无有声/无声之分。录制状态由后端派生（刷新页面也能看到进行中的录制并停止）。
function RecordPanel() {
  const uid = getUid() // 本浏览器身份：录制归属它，离开再回来能停自己的
  const [qualities, setQualities] = useState<Quality[]>([])
  const [quality, setQuality] = useState('')
  const [records, setRecords] = useState<RecordItem[]>([])
  const [live, setLive] = useState(false)
  const [now, setNow] = useState(Date.now())
  const [stoppingId, setStoppingId] = useState<string | null>(null) // 已点停止、等后端收尾
  const [err, setErr] = useState('')

  // 清晰度档来自 config.json，拉一次
  useEffect(() => {
    getConfig().then((c) => {
      setQualities(c.qualities)
      setQuality(c.default_quality || c.qualities[0]?.name || '')
    }).catch(() => {})
  }, [])

  // 轮询直播/录制状态 + 片段列表
  async function refresh() {
    try {
      const [st, recs] = await Promise.all([recordState(uid), listRecords(uid)])
      setLive(st.live); setRecords(recs)
    } catch { /* 忽略 */ }
  }
  useEffect(() => {
    refresh()
    const t = setInterval(refresh, 2000)
    const tick = setInterval(() => setNow(Date.now()), 1000) // 录制计时用
    return () => { clearInterval(t); clearInterval(tick) }
  }, [])

  // 收尾完成（该 id 不再处于 recording）后清掉「收尾中」标记
  useEffect(() => {
    if (stoppingId && !records.some((r) => r.id === stoppingId && r.status === 'recording')) {
      setStoppingId(null)
    }
  }, [records, stoppingId])

  // 进行中的录制由后端列表派生，刷新页面也能续上；已点停止的排除（等裁剪）
  const recording = records.find((r) => r.status === 'recording')
  const cutting = records.find((r) => r.status === 'cutting') // 停止后台裁剪中
  const active = recording && recording.id !== stoppingId ? recording : undefined
  const finalizing = (!!recording && recording.id === stoppingId) || !!cutting // 裁剪中

  async function onStart() {
    setErr('')
    try {
      await recordStart(quality, uid)
      refresh()
    } catch (e) { setErr(String(e)) }
  }
  async function onStop(id: string) {
    setErr(''); setStoppingId(id) // 立即隐藏停止按钮、显示收尾中
    try { await recordStop(id) } catch (e) { setErr(String(e)) }
    refresh()
  }

  return (
    <div className="recbar-wrap">
      <div className="recbar">
        <span className="rec-title">录制</span>
        {active ? (
          <button onClick={() => onStop(active.id)}>
            ■ 停止录制（{Math.max(0, Math.floor((now - active.started_at_ms) / 1000))}s · {active.quality}）
          </button>
        ) : finalizing ? (
          <span className="muted">裁剪中…（正在从整场录制切出片段）</span>
        ) : (
          <>
            <select className="qsel" value={quality} disabled={!live}
              onChange={(e) => setQuality(e.target.value)}>
              {qualities.map((q) => <option key={q.name} value={q.name}>{q.name}</option>)}
            </select>
            <button className="primary" onClick={onStart} disabled={!live || !quality}
              title={live ? '' : '无直播流，开播后可录'}>● 开始录制</button>
            {!live && <span className="muted">无直播流</span>}
          </>
        )}
        {err && <span className="rec-err">{err}</span>}
      </div>

      <h2 className="rec-h">我的录制</h2>
      {records.length === 0 && <p className="muted">暂无录制。选清晰度后点「开始录制」。</p>}
      {records.length > 0 && (
        <table className="qtable">
          <thead>
            <tr><th>时间</th><th>清晰度</th><th>状态</th><th>操作</th></tr>
          </thead>
          <tbody>
            {records.map((r) => (
              <tr key={r.id}>
                <td>{fmtTime(r.started_at_ms)}</td>
                <td>{r.quality}</td>
                <td>
                  {r.status === 'recording' && <span className="rec-live">● 录制中</span>}
                  {r.status === 'cutting' && <span className="muted">裁剪中…</span>}
                  {r.status === 'done' && <span className="ok">完成 {r.size}</span>}
                  {r.status === 'error' && <span className="rec-err" title={r.error || ''}>失败</span>}
                </td>
                <td>{r.status === 'done' && r.file && <a href={clipUrl(r.file)} download>下载</a>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  )
}
