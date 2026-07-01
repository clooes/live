import { useEffect, useRef, useState } from 'react'
import { QRCodeSVG } from 'qrcode.react'
import { useWhep } from '../whep'
import { clipStart, clipEnd, clipStatus, clipUrl, getLanIp, type ClipJob, type RelayConfig } from '../api'

export function Viewer() {
  const videoRef = useRef<HTMLVideoElement>(null)
  const [room, setRoom] = useState('')
  const [webrtcPort, setWebrtcPort] = useState(0)
  const [synced, setSynced] = useState(false)
  const { status, live, reconnect } = useWhep(videoRef, room, webrtcPort)

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
      <div className="player-wrap">
        <video ref={videoRef} autoPlay playsInline muted controls className="player" />
        {!live && <div className="overlay">{status}</div>}
      </div>
      <div className="bar">
        <span className={live ? 'dot live' : 'dot'} />
        <span>{live ? '直播中' : status}</span>
        <span className="room">
          房间：{room || '…'}
          <span className="sync" title="配置实时同步">{synced ? ' · 已同步' : ' · 离线'}</span>
        </span>
        <button onClick={reconnect}>重连</button>
        <ShareButton />
      </div>
      <RecordBar live={live} />
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

/// 录制条：看直播时点「开始/结束录制」标记一段区间，据起止时间切片下载。
function RecordBar({ live }: { live: boolean }) {
  const [marking, setMarking] = useState(false)
  const [startAt, setStartAt] = useState<number | null>(null)
  const [job, setJob] = useState<ClipJob | null>(null)
  const [err, setErr] = useState('')
  const [elapsed, setElapsed] = useState(0)

  // 标记中每秒刷新已录时长
  useEffect(() => {
    if (!marking || startAt == null) return
    const t = setInterval(() => setElapsed(Math.floor((Date.now() - startAt) / 1000)), 1000)
    return () => clearInterval(t)
  }, [marking, startAt])

  async function onStart() {
    setErr(''); setJob(null)
    try {
      await clipStart()
      setStartAt(Date.now()); setElapsed(0); setMarking(true)
    } catch (e) { setErr(String(e)) }
  }

  async function onEnd() {
    setMarking(false)
    try {
      const j = await clipEnd()
      setJob(j)
      pollJob(j.id)
    } catch (e) { setErr(String(e)) }
  }

  // 轮询切片进度直到 done/error
  function pollJob(id: string) {
    const timer = setInterval(async () => {
      try {
        const j = await clipStatus(id)
        setJob(j)
        if (j.status !== 'processing') clearInterval(timer)
      } catch { clearInterval(timer) }
    }, 1000)
  }

  return (
    <div className="recbar">
      <span className="rec-title">录制片段</span>
      {!marking ? (
        <button className="primary" onClick={onStart} disabled={!live} title={live ? '' : '开播后可录'}>
          ● 开始录制
        </button>
      ) : (
        <button onClick={onEnd}>■ 结束录制（{elapsed}s）</button>
      )}
      {job && (
        <span className="rec-status">
          {job.status === 'processing' && '切片中…'}
          {job.status === 'error' && <span className="rec-err">失败：{job.error}</span>}
          {job.status === 'done' && job.file && (
            <a href={clipUrl(job.file)} download>⬇ 下载片段（{job.size}）</a>
          )}
        </span>
      )}
      {err && <span className="rec-err">{err}</span>}
      <a className="rec-link" href="#/recordings">全部录制 →</a>
    </div>
  )
}
