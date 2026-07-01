import { useEffect, useState } from 'react'
import { getConfig, saveConfig, whepUrl, type RelayConfig, type Quality } from '../api'

export function Admin() {
  const [cfg, setCfg] = useState<RelayConfig | null>(null)
  const [msg, setMsg] = useState('')
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    getConfig().then(setCfg).catch((e) => setMsg('加载失败：' + e.message))
  }, [])

  if (!cfg) return <div className="admin">{msg || '加载中…'}</div>

  const setQuality = (i: number, patch: Partial<Quality>) => {
    const qs = cfg.qualities.map((q, idx) => (idx === i ? { ...q, ...patch } : q))
    setCfg({ ...cfg, qualities: qs })
  }
  const addQuality = () =>
    setCfg({ ...cfg, qualities: [...cfg.qualities, { name: 'new', bitrate_kbps: 2000 }] })
  const removeQuality = (i: number) =>
    setCfg({ ...cfg, qualities: cfg.qualities.filter((_, idx) => idx !== i) })

  const onSave = async () => {
    setSaving(true)
    setMsg('')
    try {
      const saved = await saveConfig(cfg)
      setCfg(saved)
      setMsg('✅ 已保存')
    } catch (e) {
      setMsg('❌ ' + (e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="admin">
      <h2>直播配置</h2>

      <label className="field">
        <span>房间 / 流名</span>
        <input value={cfg.room} onChange={(e) => setCfg({ ...cfg, room: e.target.value })} />
      </label>

      <div className="field">
        <span>清晰度档（原画直通，码率为对推流端 OBS 的建议值，服务端不重编码）</span>
        <table className="qtable">
          <thead>
            <tr><th>默认</th><th>名称</th><th>建议码率(kbps)</th><th></th></tr>
          </thead>
          <tbody>
            {cfg.qualities.map((q, i) => (
              <tr key={i}>
                <td>
                  <input
                    type="radio"
                    name="dq"
                    checked={cfg.default_quality === q.name}
                    onChange={() => setCfg({ ...cfg, default_quality: q.name })}
                  />
                </td>
                <td><input value={q.name} onChange={(e) => setQuality(i, { name: e.target.value })} /></td>
                <td>
                  <input
                    type="number"
                    value={q.bitrate_kbps}
                    onChange={(e) => setQuality(i, { bitrate_kbps: Number(e.target.value) })}
                  />
                </td>
                <td><button onClick={() => removeQuality(i)}>删除</button></td>
              </tr>
            ))}
          </tbody>
        </table>
        <button onClick={addQuality}>+ 添加档位</button>
      </div>

      <div className="actions">
        <button onClick={onSave} disabled={saving} className="primary">
          {saving ? '保存中…' : '保存'}
        </button>
        <span className="msg">{msg}</span>
      </div>

      <div className="tips">
        <h3>推流指引（OBS WHIP）</h3>
        <ol>
          <li>OBS → 设置 → 直播：服务选 <b>WHIP</b>，Bearer 留空</li>
          <li>服务器填：<code>{`http://${location.hostname}:8900/whip?app=live&stream=${cfg.room}`}</code></li>
          <li>输出用 x264，关键帧间隔 <b>1s</b>、profile <b>baseline</b>、附加 <code>repeat-headers=1</code></li>
          <li>推荐码率参考上表默认档：
            <b>{cfg.qualities.find((q) => q.name === cfg.default_quality)?.bitrate_kbps ?? '-'} kbps</b></li>
        </ol>
        <p>观看地址（WHEP）：<code>{whepUrl(cfg.room)}</code></p>
      </div>
    </div>
  )
}
