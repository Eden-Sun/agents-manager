import { useEffect, useRef, useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
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

/** pane 還在就算活著（同 Sidebar / BotSettingsPanel 的判準）；daemon 的 `active_run` 也是這個意思。 */
function alive(run: { state: string } | null | undefined): boolean {
  return Boolean(run) && run!.state !== 'stopped' && run!.state !== 'exited'
}

/**
 * team 停在 `member_lost:<name>`（成員 pane 沒了：使用者手動 stop、pane 被關、daemon 重啟後
 * 找不到）時的「啟動並繼續」（2026-09-10）。
 *
 * 之前橫幅只寫「請先處理該成員（啟動）再按繼續」；daemon 的 `resume` 又要求**每個**未刪除成員都有
 * 活著的 run（§10.5），少一個就 409 `member not running`，使用者得一顆一顆去側欄啟動。這顆按鈕把
 * 沒在跑的成員全部 `start`，等它們都有 run 之後自動 `resume`。
 */
export function TeamMemberLost({ teamId }: { teamId: string }) {
  const missing = useStore(
    useShallow((s) => {
      const ids = new Set((s.teams[teamId]?.members ?? []).filter((m) => !m.deleted).map((m) => m.bot_id))
      return s.bots.filter((b) => ids.has(b.id) && !alive(s.runs[b.id])).map((b) => b.id)
    }),
  )
  const stillPaused = useStore((s) => (s.teams[teamId]?.pause_reason ?? '').startsWith('member_lost'))
  const startBot = useStore((s) => s.startBot)
  const controlTeam = useStore((s) => s.controlTeam)
  const [busy, setBusy] = useState(false)
  const armed = useRef(false)

  // 成員都回來了（`bot_status` 推過來、`missing` 變空）才按繼續；沒按過按鈕的不代勞。
  useEffect(() => {
    if (!armed.current || missing.length > 0) return
    armed.current = false
    setBusy(false)
    if (stillPaused) void controlTeam(teamId, 'resume')
  }, [missing, stillPaused, controlTeam, teamId])

  const go = () => {
    if (busy) return
    if (missing.length === 0) {
      void controlTeam(teamId, 'resume')
      return
    }
    setBusy(true)
    armed.current = true
    void Promise.allSettled(missing.map((id) => startBot(id))).then((rs) => {
      // 有一顆起不來就停在這裡讓使用者看側欄的錯誤；其餘的照常等 resume。
      if (rs.some((r) => r.status === 'rejected')) {
        armed.current = false
        setBusy(false)
      }
    })
  }

  return (
    <button
      type="button"
      className="mini-btn team-paused-go"
      title={
        missing.length
          ? `啟動 ${missing.length} 個沒在跑的成員，都起來後自動繼續`
          : '成員都在跑了，直接繼續'
      }
      disabled={busy}
      onClick={go}
    >
      {busy ? '啟動中…' : missing.length ? `啟動並繼續（${missing.length}）` : '繼續'}
    </button>
  )
}
