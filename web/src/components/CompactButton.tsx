import { useState } from 'react'
import * as api from '../api'
import { ApiError } from '../api/types'
import { useStore } from '../store/store'
import { compactRefusal } from '../lib/compactRefusal'
import './compactButton.css'

/**
 * context 旁的「壓縮」鈕（2026-10-04 使用者：「可以對某個 bot 下 compact，按鈕做在 context 旁邊」）：對閒著的 bot 送 `/compact`。
 * 只在 claude／codex、bot 在跑而且閒著時能按；壓完的新用量由 statusLine 回報，這裡不等。
 */
export function CompactButton({ botId }: { botId: string }) {
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  const idle = useStore((s) => {
    const r = s.runs[botId]
    return Boolean(r && r.state === 'running' && r.agent_status === 'idle')
  })
  const notify = useStore((s) => s.notify)
  const [busy, setBusy] = useState(false)
  if (kind !== 'claude' && kind !== 'codex') return null
  const go = async () => {
    setBusy(true)
    try {
      await api.compactBot(botId)
      notify('info', '已送出 /compact，壓縮完 context 用量會更新')
    } catch (e) {
      const reason = e instanceof ApiError ? String(e.body.reason ?? e.message) : String(e)
      notify('error', `沒有送出：${compactRefusal(reason)}`)
    } finally {
      setBusy(false)
    }
  }
  return (
    <button
      type="button"
      className="compact-btn"
      disabled={!idle || busy}
      title={idle ? '對這顆 bot 送 /compact，壓縮對話脈絡' : '要等它閒下來才能壓縮'}
      onClick={(e) => {
        e.stopPropagation()
        void go()
      }}
    >
      {busy ? '送出中…' : '壓縮'}
    </button>
  )
}
