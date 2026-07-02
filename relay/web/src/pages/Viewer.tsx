import { useEffect, useRef, useState } from 'react'
import { QRCodeSVG } from 'qrcode.react'
import { useWhep } from '../whep'
import {
  clipStart, clipEnd, clipUrl, prepareClip, getLanIp, getConfig,
  listClips, listRecordings,
  type ClipJob, type Quality, type Recording, type RelayConfig,
} from '../api'

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
      <Library />
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

/// 录制条：看直播时点「开始/结束录制」标记一段区间。切片延到下载时按清晰度生成（R4），
/// 结束后片段出现在下方「片段切片」列表，在那里选清晰度下载。
function RecordBar({ live }: { live: boolean }) {
  const [marking, setMarking] = useState(false)
  const [startAt, setStartAt] = useState<number | null>(null)
  const [saved, setSaved] = useState(false)
  const [err, setErr] = useState('')
  const [elapsed, setElapsed] = useState(0)

  // 标记中每秒刷新已录时长
  useEffect(() => {
    if (!marking || startAt == null) return
    const t = setInterval(() => setElapsed(Math.floor((Date.now() - startAt) / 1000)), 1000)
    return () => clearInterval(t)
  }, [marking, startAt])

  async function onStart() {
    setErr(''); setSaved(false)
    try {
      await clipStart()
      setStartAt(Date.now()); setElapsed(0); setMarking(true)
    } catch (e) { setErr(String(e)) }
  }

  async function onEnd() {
    setMarking(false)
    try {
      await clipEnd()
      setSaved(true)
    } catch (e) { setErr(String(e)) }
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
      {saved && <span className="rec-status">已保存，见下方「片段切片」选清晰度下载 ↓</span>}
      {err && <span className="rec-err">{err}</span>}
    </div>
  )
}

function fmtTime(ms: number): string {
  return new Date(ms).toLocaleString()
}
function fmtDur(a: number, b: number | null): string {
  if (!b) return '进行中'
  const s = Math.round((b - a) / 1000)
  const m = Math.floor(s / 60)
  return m > 0 ? `${m}分${s % 60}秒` : `${s}秒`
}

/// 片段/录制库（并入观看页）：上「我的片段」下载，下「整场录制」回放（弹窗 VOD）。
/// 每 3s 刷新，切片进行中能自动更新进度。
function Library() {
  const [clips, setClips] = useState<ClipJob[]>([])
  const [recs, setRecs] = useState<Recording[]>([])
  const [qualities, setQualities] = useState<Quality[]>([])
  const [replay, setReplay] = useState<Recording | null>(null) // 正在回放的场次（null=关闭弹窗）

  async function refresh() {
    try {
      const [c, r] = await Promise.all([listClips(), listRecordings()])
      setClips(c); setRecs(r)
    } catch { /* 忽略 */ }
  }
  useEffect(() => {
    refresh()
    const t = setInterval(refresh, 3000)
    // 清晰度档来自 config.json（R4 下载时选），拉一次即可
    getConfig().then((c) => setQualities(c.qualities)).catch(() => {})
    return () => clearInterval(t)
  }, [])

  // R3：回放只列**已结束**（含 ENDLIST 的 VOD）的场次；直播中的场次不给回放入口，
  // 避免对 live playlist 追最新帧、观感等同直播（这正是「回放却是直播」的根因）。
  const ended = recs.filter((r) => !r.live)
  const liveCount = recs.length - ended.length

  return (
    <div className="library">
      <h2>片段切片</h2>
      {clips.length === 0 && <p className="muted">暂无切片。上方点「开始/结束录制」生成片段。</p>}
      {clips.length > 0 && (
        <table className="qtable">
          <thead>
            <tr><th>时间</th><th>时长</th><th>下载（选清晰度）</th></tr>
          </thead>
          <tbody>
            {clips.map((c) => (
              <tr key={c.id}>
                <td>{fmtTime(c.created_at_ms)}</td>
                <td>{Math.round((c.end_ms - c.start_ms) / 1000)}s</td>
                <td><ClipDownload clip={c} qualities={qualities} /></td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h2>整场录制</h2>
      {ended.length === 0 && (
        <p className="muted">
          暂无可回放录制。{liveCount > 0 ? '当前直播结束后此场即可回放。' : '直播开始后自动全程录制。'}
        </p>
      )}
      <ul className="rec-list">
        {ended.map((r) => (
          <li key={r.id}>
            <div className="rec-row">
              <span className="dot" />
              <span>{fmtTime(r.started_at_ms)}</span>
              <span className="muted">时长 {fmtDur(r.started_at_ms, r.ended_at_ms)}</span>
              <button onClick={() => setReplay(r)}>▶ 回放</button>
            </div>
          </li>
        ))}
      </ul>

      {replay && <ReplayModal rec={replay} onClose={() => setReplay(null)} />}
    </div>
  )
}

/// 单个片段的下载（R4 清晰度 + R10 有声/无声）：清晰度 × {有声,无声} = 6 个入口。
/// 点击后按需切片（original 秒级 / 720p·480p 重编码；无声版 -an），就绪即触发浏览器下载。
/// 同一 (清晰度,音频) 切好后服务端缓存，再点即秒下。
function ClipDownload({ clip, qualities }: { clip: ClipJob; qualities: Quality[] }) {
  const [busy, setBusy] = useState('') // 正在准备的 key（quality+音频）
  const [err, setErr] = useState('')

  async function onPick(q: string, withAudio: boolean) {
    const key = q + (withAudio ? '·snd' : '·mute')
    setErr(''); setBusy(key)
    try {
      const { file } = await prepareClip(clip.id, q, withAudio)
      // 触发下载（download 属性 + 程序化点击）
      const a = document.createElement('a')
      a.href = clipUrl(file); a.download = ''
      document.body.appendChild(a); a.click(); a.remove()
    } catch (e) {
      setErr(String(e))
    } finally {
      setBusy('')
    }
  }

  return (
    <span className="clip-dl">
      {([[true, '有声'], [false, '无声']] as const).map(([withAudio, label]) => (
        <span key={label} className="clip-dl-row">
          <span className="clip-dl-label">{label}</span>
          {qualities.map((q) => {
            const key = q.name + (withAudio ? '·snd' : '·mute')
            return (
              <button key={key} onClick={() => onPick(q.name, withAudio)} disabled={!!busy}>
                {busy === key ? '生成中…' : q.name}
              </button>
            )
          })}
        </span>
      ))}
      {err && <span className="rec-err">{err}</span>}
    </span>
  )
}

/// 回放弹窗（D7）：独立 HLS VOD 播放器，与直播 WHEP 的 <video> 完全解耦。
function ReplayModal({ rec, onClose }: { rec: Recording; onClose: () => void }) {
  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal modal-wide" onClick={(e) => e.stopPropagation()}>
        <h3>回放 · {fmtTime(rec.started_at_ms)}</h3>
        <HlsPlayer src={rec.playlist} />
        <button onClick={onClose}>关闭</button>
      </div>
    </div>
  )
}

/// HLS 回放：Safari 原生支持 m3u8；其它浏览器动态加载 hls.js。播的是录制 VOD，非直播流。
function HlsPlayer({ src }: { src: string }) {
  const ref = useRef<HTMLVideoElement>(null)

  useEffect(() => {
    const video = ref.current
    if (!video) return
    // Safari / iOS 原生 HLS
    if (video.canPlayType('application/vnd.apple.mpegurl')) {
      video.src = src
      return
    }
    // 其它浏览器：动态 import hls.js（按需，不进主包）
    let hls: any
    let cancelled = false
    import('hls.js').then(({ default: Hls }) => {
      if (cancelled) return
      if (Hls.isSupported()) {
        hls = new Hls()
        hls.loadSource(src)
        hls.attachMedia(video)
      } else {
        video.src = src // 兜底
      }
    })
    return () => {
      cancelled = true
      if (hls) hls.destroy()
    }
  }, [src])

  return <video ref={ref} controls playsInline autoPlay className="player replay" />
}
