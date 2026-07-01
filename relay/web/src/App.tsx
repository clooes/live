import { Viewer } from './pages/Viewer'

// 单页面（R2）：砍掉管理页/录制页与 hash 路由，观看 + 录制条 + 片段/回放全并入观看页。
// 配置（room/清晰度/端口）纯看 config.json，改后重启生效（决策 D6），前端不再提供改配置入口。
export function App() {
  return (
    <div className="app">
      <header className="nav">
        <span className="brand">🎥 内网直播</span>
      </header>
      <main><Viewer /></main>
    </div>
  )
}
