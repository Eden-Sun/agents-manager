/**
 * bot 上的「Jev」徽章（issue #1328）：近 24 小時有超過門檻的第二意見時才出現，點開看場景、機率、時間與遮罩後的命中行。
 * 只是提示：寫明「不會自動動作」。關掉的那一則記在這台瀏覽器，不再顯示。
 */
import { useEffect, useState } from 'react'
import { JUDGE_SCENES, probabilityText } from '../lib/judgeStats'
import { judgeBadgeFor } from '../lib/judgeBadge'
import { useJudgeLedger } from '../store/judgeLedger'
import { useStore } from '../store/store'
import './judgeBadge.css'

export function JudgeBadge({ botId }: { botId: string }) {
  const visible = useJudgeLedger((s) => s.visible)
  const rows = useJudgeLedger((s) => s.rows)
  const dismissed = useJudgeLedger((s) => s.dismissed)
  const snapshotAt = useJudgeLedger((s) => s.lastFetchAt)
  const dismiss = useJudgeLedger((s) => s.dismiss)
  /** 開著浮層的那一則 id：那一則不在了（關掉、換了、過期），浮層自然不顯示，不用 effect 收。 */
  const [openId, setOpenId] = useState<string | null>(null)
  // 「近 24 小時」以快照的時間為準（每次重抓都會更新），渲染只依賴 store，不讀牆鐘。
  const row = visible ? judgeBadgeFor(rows, botId, snapshotAt, dismissed) : null
  if (!row) return null
  const open = openId === row.id
  const scene = JUDGE_SCENES.find((s) => s.id === row.regex_verdict)?.name ?? row.regex_verdict
  const prob = probabilityText(row)
  const at = Date.parse(row.at)
  return (
    <span className="judge-badge-wrap" onClick={(e) => e.stopPropagation()}>
      <button
        type="button"
        className="judge-badge"
        aria-expanded={open}
        aria-label={`Jev 第二意見：${scene}`}
        title={`Jev：${scene}（機率 ${prob}）`}
        onClick={() => setOpenId(open ? null : row.id)}
      >
        Jev
      </button>
      {open ? (
        <div className="judge-badge-pop" role="dialog" aria-label="Jev 第二意見">
          <p>
            <b>{scene}</b>・機率 {prob}
          </p>
          <p className="hint">{Number.isNaN(at) ? row.at : new Date(at).toLocaleString('zh-TW', { hour12: false })}</p>
          {row.matched_line ? <pre className="judge-badge-line">{row.matched_line}</pre> : null}
          <p className="hint">這只是第二意見，不會自動動作。</p>
          <button type="button" className="btn" onClick={() => dismiss(row.id)}>
            關掉這則
          </button>
        </div>
      ) : null}
    </span>
  )
}

/** 開畫面讀一次；之後每次 `supervisor_changed`（`supervisorRev` 變）重抓，節流在 store 裡（最短 60 秒）。 */
export function JudgeLedgerWatch() {
  const rev = useStore((s) => s.supervisorRev)
  useEffect(() => {
    void useJudgeLedger.getState().refresh()
  }, [rev])
  return null
}
