import { useEffect, useRef, useState } from 'react'
import { useStore } from '../store/store'
import { BlockedModal } from './BlockedModal'

/**
 * team 停在 `member_blocked:<name>`（SPEC-team §11：成員卡在 trust 提示、權限詢問、codex 升級選單）
 * 時，暫停橫幅上的那顆「回應 <成員>」按鈕（2026-09-10）。
 *
 * 之前橫幅只寫「請先處理該成員（啟動 / 回應終端提示），再按『繼續』」——要處理得先在側欄找到
 * 那個成員、點進去、等 blocked 面板彈出來、回完再切回 team 按「繼續」，四步。這裡把成員的整張
 * 終端畫面（同 `BlockedModal`，鍵盤直通）直接從 team 畫面拉出來，回完就地收尾：
 *
 * - 成員一離開 `blocked`（daemon 推 `bot_status`）就自動 `resume`，不用再按「繼續」。規格本來就說
 *   這是唯一會自動 resume 的原因，但 2026-09-10 實測 daemon 沒有做到，UI 這邊補上這一手。
 * - 只在這顆按鈕開的視窗仍開著、或剛關掉時接手；使用者從別處處理掉的，仍照原本流程按「繼續」。
 */
export function TeamMemberBlocked({ teamId, memberName }: { teamId: string; memberName: string }) {
  const bot = useStore((s) => {
    const ids = new Set((s.teams[teamId]?.members ?? []).map((m) => m.bot_id))
    return s.bots.find((b) => ids.has(b.id) && b.name === memberName) ?? null
  })
  const blocked = useStore((s) => (bot ? s.runs[bot.id]?.agent_status === 'blocked' : false))
  const stillPaused = useStore((s) => s.teams[teamId]?.pause_reason === `member_blocked:${memberName}`)
  const controlTeam = useStore((s) => s.controlTeam)
  const [open, setOpen] = useState(false)
  // 「使用者是從這裡回的」——按過按鈕之後才替他按繼續；視窗先關掉也照樣接手。
  const armed = useRef(false)

  useEffect(() => {
    if (!armed.current || blocked) return
    armed.current = false
    setOpen(false)
    if (stillPaused) void controlTeam(teamId, 'resume')
  }, [blocked, stillPaused, controlTeam, teamId])

  if (!bot) return null
  return (
    <>
      <button
        type="button"
        className="mini-btn team-paused-go"
        title={blocked ? `打開 ${memberName} 的終端畫面回應提示，回完自動繼續` : `${memberName} 已不在 blocked，按「繼續」即可`}
        disabled={!blocked}
        onClick={() => {
          armed.current = true
          setOpen(true)
        }}
      >
        回應 {memberName}
      </button>
      {open ? <BlockedModal key={bot.id} botId={bot.id} onClose={() => setOpen(false)} /> : null}
    </>
  )
}
