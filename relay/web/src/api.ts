// 配置类型 + 读写接口（对应后端 /api/config）

export interface Quality {
  name: string
  bitrate_kbps: number
}

export interface Ports {
  web: number
  webrtc: number
  rtmp: number
}

export interface RelayConfig {
  room: string
  qualities: Quality[]
  default_quality: string
  ports: Ports
}

/// 内网分享地址（R6）：本机内网 IP + web 端口，前端据此生成二维码。ip 可能为 null（探测失败）。
export async function getLanIp(): Promise<{ ip: string | null; web_port: number }> {
  const r = await fetch('/api/lan-ip')
  if (!r.ok) throw new Error('读取内网地址失败 ' + r.status)
  return r.json()
}

export async function getConfig(): Promise<RelayConfig> {
  const r = await fetch('/api/config')
  if (!r.ok) throw new Error('读取配置失败 ' + r.status)
  return r.json()
}

export async function saveConfig(cfg: RelayConfig): Promise<RelayConfig> {
  const r = await fetch('/api/config', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(cfg),
  })
  if (!r.ok) throw new Error('保存失败：' + (await r.text()))
  return r.json()
}

/// WHEP 播放地址：页面从 web 端口提供，媒体走 webrtc 端口（由 /api/config 下发）。
/// 跨设备访问自动跟随主机名；端口改配置后前端自动跟随，无需改代码。
export function whepUrl(room: string, webrtcPort: number): string {
  return `http://${location.hostname}:${webrtcPort}/whep?app=live&stream=${encodeURIComponent(room)}`
}

// ---------- 录制 / 切片 ----------

export interface ClipJob {
  id: string
  session_id: string
  start_ms: number
  end_ms: number
  status: 'processing' | 'done' | 'error'
  file: string | null
  size: string | null
  error: string | null
  created_at_ms: number
}

export interface Recording {
  id: string
  room: string
  started_at_ms: number
  ended_at_ms: number | null
  live: boolean
  playlist: string
}

/// 标记「开始录制」，返回 session_id + start_ms。
export async function clipStart(): Promise<{ session_id: string; start_ms: number }> {
  const r = await fetch('/api/clip/start', { method: 'POST' })
  if (!r.ok) throw new Error(await r.text())
  return r.json()
}

/// 标记「结束录制」，建切片任务，返回 job。
export async function clipEnd(): Promise<ClipJob> {
  const r = await fetch('/api/clip/end', { method: 'POST' })
  if (!r.ok) throw new Error(await r.text())
  return r.json()
}

/// 下载准备（R4）：按 job + 清晰度按需切片，返回可下载文件名/大小。original 秒级，720p/480p 重编码稍慢。
export async function prepareClip(id: string, quality: string): Promise<{ file: string; size: string; quality: string }> {
  const r = await fetch(`/api/clip/prepare/${id}?quality=${encodeURIComponent(quality)}`, { method: 'POST' })
  if (!r.ok) throw new Error(await r.text())
  return r.json()
}

export async function clipStatus(id: string): Promise<ClipJob> {
  const r = await fetch('/api/clip/status/' + id)
  if (!r.ok) throw new Error('查询切片失败 ' + r.status)
  return r.json()
}

export async function listClips(): Promise<ClipJob[]> {
  const r = await fetch('/api/clips')
  if (!r.ok) throw new Error('读取切片列表失败')
  return r.json()
}

export async function listRecordings(): Promise<Recording[]> {
  const r = await fetch('/api/recordings')
  if (!r.ok) throw new Error('读取录制列表失败')
  return r.json()
}

/// 切片下载地址。
export function clipUrl(file: string): string {
  return '/clips/' + file
}
