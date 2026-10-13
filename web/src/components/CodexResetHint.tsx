import { useEffect, useState } from 'react'
import { useCodexReset } from '../store/codexReset'
import './codexResetHint.css'

export function CodexResetHint() {
  const info = useCodexReset((s) => s.info)
  const [now, setNow] = useState(Date.now)
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 60_000); return () => clearInterval(timer) }, [])
  if (!info) return null
  const stale = now - Date.parse(info.checked_at) > 70 * 60_000
  const label = stale ? 'Codex 重置資料過期' : info.upcoming ? 'Codex 即將重置' : 'Codex 重置消息'
  const last = info.last_reset_at ? new Date(info.last_reset_at).toLocaleString([], { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false }) : '尚無紀錄'
  const title = `${label}\n${info.upcoming && !stale ? '公告預告，尚未確認生效；個人額度仍以 /status 為準。\n' : ''}最近全域重置：${last}\n${info.latest?.text ?? ''}\n每 30 分鐘刷新；最後成功讀取：${new Date(info.checked_at).toLocaleString()}\n資料：codex-reset.com（第三方追蹤）`
  return <a className={`codex-reset-hint${info.upcoming && !stale ? ' upcoming' : ''}${stale ? ' stale' : ''}`} href={info.source} target="_blank" rel="noopener noreferrer" title={title} aria-label={title}>
    <span>{label}</span><small>codex-reset.com</small>
  </a>
}
