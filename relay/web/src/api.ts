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

/// 浏览器身份：进入页面即读/生成一个随机 uid 存 localStorage。
/// 录制归属该 uid，「我的录制」按它隔离；离开页面再回来（uid 仍在）能看到并停止自己的录制。
export function getUid(): string {
  const KEY = 'relay_uid'
  let uid = localStorage.getItem(KEY)
  if (!uid) {
    uid = (crypto.randomUUID?.() ?? String(Date.now()) + Math.random().toString(16).slice(2))
    localStorage.setItem(KEY, uid)
  }
  return uid
}

export interface RecordItem {
  id: string
  owner: string
  quality: string
  status: 'recording' | 'done' | 'error'
  file: string | null
  size: string | null
  error: string | null
  started_at_ms: number
  ended_at_ms: number | null
}

/// 录制状态：当前是否有直播流可录 + 该浏览器是否有进行中的录制。
export async function recordState(owner: string): Promise<{ live: boolean; recording: boolean }> {
  const r = await fetch('/api/record/state?owner=' + encodeURIComponent(owner))
  if (!r.ok) throw new Error('读取录制状态失败')
  return r.json()
}

/// 开始录制（选清晰度 + 归属浏览器 uid），返回录制 id。
export async function recordStart(quality: string, owner: string): Promise<{ id: string }> {
  const r = await fetch('/api/record/start', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ quality, owner }),
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

/// 「我的录制」列表（最新在前）：只返回该浏览器 uid 的录制。
export async function listRecords(owner: string): Promise<RecordItem[]> {
  const r = await fetch('/api/records?owner=' + encodeURIComponent(owner))
  if (!r.ok) throw new Error('读取录制列表失败')
  return r.json()
}

/// 录制片段下载地址。
export function clipUrl(file: string): string {
  return '/clips/' + file
}
