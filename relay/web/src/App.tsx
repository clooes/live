import { useEffect, useState } from 'react'
import { Viewer } from './pages/Viewer'
import { Admin } from './pages/Admin'
import { Recordings } from './pages/Recordings'

// 极简 hash 路由：#/admin → 管理页，#/recordings → 录制页，其余 → 观看页。
function useHashRoute(): string {
  const [hash, setHash] = useState(location.hash)
  useEffect(() => {
    const on = () => setHash(location.hash)
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return hash
}

export function App() {
  const hash = useHashRoute()
  const isAdmin = hash.startsWith('#/admin')
  const isRec = hash.startsWith('#/recordings')

  return (
    <div className="app">
      <header className="nav">
        <span className="brand">🎥 内网直播</span>
        <nav>
          <a href="#/" className={!isAdmin && !isRec ? 'active' : ''}>观看</a>
          <a href="#/recordings" className={isRec ? 'active' : ''}>录制</a>
          <a href="#/admin" className={isAdmin ? 'active' : ''}>管理</a>
        </nav>
      </header>
      <main>{isAdmin ? <Admin /> : isRec ? <Recordings /> : <Viewer />}</main>
    </div>
  )
}
