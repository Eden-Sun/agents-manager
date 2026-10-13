import { useCodexReset } from '../store/codexReset'
import './codexResetHint.css'

/** Public announcements belong to the Codex cell; account windows stay untouched. */
export function CodexResetHint({ now, detail = false, compact = false }: { now: number; detail?: boolean; compact?: boolean }) {
  const info = useCodexReset((s) => s.info)
  if (!info) return null
  const stale = now - Date.parse(info.checked_at) > 70 * 60_000
  const upcoming = info.upcoming && !stale
  if (!detail && !upcoming) return null
  const last = info.last_reset_at ? new Date(info.last_reset_at).toLocaleString([], { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false }) : '尚無紀錄'
  const title = `${stale ? '資料過期' : upcoming ? '即將重置（公告預告，尚未確認生效）' : 'Codex 重置紀錄'}\n個人額度仍以 /status 為準。\n最近全域重置：${last}\n${info.latest?.text ?? ''}\n每 30 分鐘刷新；最後成功讀取：${new Date(info.checked_at).toLocaleString()}\n資料：codex-reset.com（第三方追蹤）`
  return <a className={`codex-reset-${detail ? 'detail' : 'badge'}${upcoming ? ' upcoming' : ''}${stale ? ' stale' : ''}${compact ? ' compact' : ''}`} href={info.source} target="_blank" rel="noopener noreferrer" title={title} aria-label={title}>
    {detail ? <>
      {stale ? <span>資料過期 · </span> : null}
      <span className="codex-reset-record">最近全域重置：{last}</span>
      {upcoming ? <span className="codex-reset-announcement">即將重置（公告預告，個人額度以 /status 為準）</span> : null}
      <span> · codex-reset.com ↗</span>
    </> : <><span className="codex-reset-dot" aria-hidden="true" /><span className="codex-reset-label">即將重置</span></>}
  </a>
}
