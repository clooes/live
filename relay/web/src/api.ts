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

// ---------- 分段录制（点击录制即录成品 mp4）----------

export interface RecordItem {
  id: string
  quality: string
  status: 'recording' | 'done' | 'error'
  file: string | null
  size: string | null
  error: string | null
  started_at_ms: number
  ended_at_ms: number | null
}

/// 录制状态：当前是否有直播流可录 + 是否有进行中的录制。
export async function recordState(): Promise<{ live: boolean; recording: boolean }> {
  const r = await fetch('/api/record/state')
  if (!r.ok) throw new Error('读取录制状态失败')
  return r.json()
}

/// 开始录制（选清晰度），返回录制 id。
export async function recordStart(quality: string): Promise<{ id: string }> {
  const r = await fetch('/api/record/start', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ quality }),
  })
  if (!r.ok) throw new Error(await r.text())
  return r.json()
}

/// 停止录制。
export async function recordStop(id: string): Promise<void> {
  const r = await fetch('/api/record/stop', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id }),
  })
  if (!r.ok) throw new Error(await r.text())
}

/// 录制片段列表（最新在前）。
export async function listRecords(): Promise<RecordItem[]> {
  const r = await fetch('/api/records')
  if (!r.ok) throw new Error('读取录制列表失败')
  return r.json()
}

/// 录制片段下载地址。
export function clipUrl(file: string): string {
  return '/clips/' + file
}
