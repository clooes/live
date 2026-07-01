import { useEffect, useRef, useState } from 'react'
import {
  listClips, listRecordings, clipUrl,
  type ClipJob, type Recording,
} from '../api'

function fmtTime(ms: number): string {
  return new Date(ms).toLocaleString()
}
function fmtDur(a: number, b: number | null): string {
  if (!b) return '进行中'
  const s = Math.round((b - a) / 1000)
  const m = Math.floor(s / 60)
  return m > 0 ? `${m}分${s % 60}秒` : `${s}秒`
}

export function Recordings() {
  const [clips, setClips] = useState<ClipJob[]>([])
  const [recs, setRecs] = useState<Recording[]>([])
  const [playing, setPlaying] = useState<string | null>(null) // 正在回放的 playlist url

  async function refresh() {
    try {
      const [c, r] = await Promise.all([listClips(), listRecordings()])
      setClips(c); setRecs(r)
    } catch { /* 忽略 */ }
  }
  useEffect(() => {
    refresh()
    const t = setInterval(refresh, 3000) // 有切片进行中时自动刷新进度
    return () => clearInterval(t)
  }, [])

  return (
    <div className="recordings">
      <h2>整场录制</h2>
      {recs.length === 0 && <p className="muted">暂无录制。直播开始后自动全程录制。</p>}
      <ul className="rec-list">
        {recs.map((r) => (
          <li key={r.id}>
            <div className="rec-row">
              <span className={r.live ? 'dot live' : 'dot'} />
              <span>{fmtTime(r.started_at_ms)}</span>
              <span className="muted">时长 {fmtDur(r.started_at_ms, r.ended_at_ms)}</span>
              <button onClick={() => setPlaying(playing === r.playlist ? null : r.playlist)}>
                {playing === r.playlist ? '收起' : '▶ 回放'}
              </button>
            </div>
            {playing === r.playlist && <HlsPlayer src={r.playlist} />}
          </li>
        ))}
      </ul>

      <h2>片段切片</h2>
      {clips.length === 0 && <p className="muted">暂无切片。观看页点「开始/结束录制」生成片段。</p>}
      <table className="qtable">
        <thead>
          <tr><th>时间</th><th>区间</th><th>状态</th><th>操作</th></tr>
        </thead>
        <tbody>
          {clips.map((c) => (
            <tr key={c.id}>
              <td>{fmtTime(c.created_at_ms)}</td>
              <td>{Math.round((c.end_ms - c.start_ms) / 1000)}s</td>
              <td>
                {c.status === 'processing' && '切片中…'}
                {c.status === 'done' && <span className="ok">完成 {c.size}</span>}
                {c.status === 'error' && <span className="rec-err" title={c.error || ''}>失败</span>}
              </td>
              <td>
                {c.status === 'done' && c.file && (
                  <a href={clipUrl(c.file)} download>下载</a>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

/// HLS 回放：Safari 原生支持 m3u8；其它浏览器动态加载 hls.js。
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

  return <video ref={ref} controls playsInline className="player replay" />
}
