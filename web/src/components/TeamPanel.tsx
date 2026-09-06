import { useLayoutEffect, useMemo, useRef, useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Bot, GroupMessage, TeamEvent, TeamTask, TeamTaskState } from '../api/types'
import {
  TEAM_PHASE_LABEL,
  TEAM_ROLE_LABEL,
  TEAM_TASK_NEEDS_USER,
  TEAM_TASK_STATE_LABEL,
  TEAM_TERMINAL_PHASES,
  teamPauseLabel,
  teamPhaseTone,
} from '../api/types'
import {
  botLamp,
  composerState,
  liveReplyOf,
  teamMemberBots,
  teamShortName,
  useStore,
} from '../store/store'
import { Bubble, EmptyState, KIND_TITLE, LiveBubble } from './ChatPanel'
import { ConfirmDialog } from './ConfirmDialog'
import { CopyChip } from './CopyChip'
import { TeamDeleteDialog } from './TeamDeleteDialog'
import { QuotaStrip } from './QuotaStrip'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'

/**
 * SPEC-team §11.3 — 一個進行中 team 的視圖。骨架沿用 `GroupChatPanel`：標題列 + 成員燈號列 +
 * 合併時間軸（`Bubble` / `LiveBubble`）+ composer，差別在多了 phase chip、預算量條、
 * paused 橫幅、task 清單，以及把 daemon 代轉的訊息標成 `pm → dev-1`。
 *
 * 這一輪 task 看板做成清單（§13 第一階段允許），拖拉留給第二階段。
 */

// ---------------------------------------------------------------- helpers

/** `am-team` fenced 區塊：人看的部分留在氣泡裡，機器指令折成一顆 chip。 */
interface AmTeamBlock {
  raw: string
  action: string
  detail: string
}

const AM_TEAM_RE = /```am-team\s*\n([\s\S]*?)```/g

function summarize(obj: unknown): { action: string; detail: string } {
  if (typeof obj !== 'object' || obj === null) return { action: 'am-team', detail: '' }
  const o = obj as Record<string, unknown>
  const action = typeof o.action === 'string' ? o.action : 'am-team'
  if (action === 'dispatch' && Array.isArray(o.tasks)) return { action, detail: `×${o.tasks.length}` }
  if (action === 'report' && typeof o.status === 'string') return { action, detail: o.status }
  if (action === 'verdict' && typeof o.result === 'string') return { action, detail: o.result }
  if (action === 'done') return { action, detail: '' }
  if (action === 'ask_user') return { action, detail: '需要你回答' }
  return { action, detail: '' }
}

function splitAmTeam(content: string): { text: string; blocks: AmTeamBlock[] } {
  const blocks: AmTeamBlock[] = []
  const text = content
    .replace(AM_TEAM_RE, (_m, body: string) => {
      let parsed: unknown = null
      try {
        parsed = JSON.parse(body) as unknown
      } catch {
        parsed = null
      }
      const { action, detail } = parsed === null ? { action: 'am-team（無法解析）', detail: '' } : summarize(parsed)
      blocks.push({ raw: body.trim(), action, detail })
      return ''
    })
    .replace(/\n{3,}/g, '\n\n')
    .trim()
  return { text, blocks }
}

function AmTeamChip({ block }: { block: AmTeamBlock }) {
  return (
    <details className="am-chip">
      <summary title="daemon 讀的機器指令（點開看原始 JSON）">
        <span className="am-chip-action">{block.action}</span>
        {block.detail ? <span className="am-chip-detail">{block.detail}</span> : null}
      </summary>
      <pre className="am-chip-body">{block.raw}</pre>
    </details>
  )
}

function timeOf(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? '' : d.toLocaleTimeString([], { hour12: false })
}

/** 一件 task 屬於清單的哪一欄（看板的欄位，這一輪只做分組標題）。 */
const TASK_COLUMN: Record<TeamTaskState, string> = {
  queued: '待派',
  working: '進行中',
  rebasing: '進行中',
  reported: '審查中',
  reviewing: '審查中',
  changes_requested: '審查中',
  merging: '整合中',
  merged: '完成',
  skipped: '完成',
  failed: '完成',
  exhausted: '需要你',
  blocked_by_worker: '需要你',
}

const COLUMN_ORDER = ['需要你', '進行中', '審查中', '整合中', '待派', '完成'] as const

// ---------------------------------------------------------------- members

function MemberChip({ bot, task }: { bot: Bot; task: TeamTask | null }) {
  const lamp = useStore((s) => botLamp(s, bot.id))
  const selectBot = useStore((s) => s.selectBot)
  const role = bot.team?.role ?? 'worker'
  const short = teamShortName(bot.name)
  return (
    <button
      type="button"
      className="member team-member"
      role="listitem"
      title={`${bot.name}（${TEAM_ROLE_LABEL[role]}）：${LAMP_LABEL[lamp]}${task ? `\n目前：t${task.seq} ${task.title}` : ''}\ncwd ${bot.cwd ?? '（專案根目錄）'}\n點擊開啟它的單獨對話`}
      onClick={() => selectBot(bot.id)}
    >
      <StatusLamp lamp={lamp} />
      {/* SPEC-team §7.3：短名本身就是角色徽章（`pm` / `dev-1` / `rev`），不再重複一次角色字。 */}
      <span className={`bot-badge ${bot.kind} team-role-badge ${role}`}>{short}</span>
      {task ? <span className="team-member-task">t{task.seq}</span> : null}
    </button>
  )
}

function MemberStrip({ teamId }: { teamId: string }) {
  const members = useStore(useShallow((s) => teamMemberBots(s, teamId)))
  const tasks = useStore(useShallow((s) => s.teamDetail[teamId]?.tasks ?? []))
  const openTask = (botId: string) =>
    tasks.find((t) => t.worker_bot_id === botId && t.state !== 'merged' && t.state !== 'skipped' && t.state !== 'failed') ??
    null
  return (
    <div className="members team-members" role="list" aria-label="Team 成員">
      {members.map((b) => (
        <MemberChip key={b.id} bot={b} task={openTask(b.id)} />
      ))}
      {members.length === 0 ? <span className="hint">（成員啟動中…）</span> : null}
    </div>
  )
}

// ---------------------------------------------------------------- tasks

function TaskList({ teamId }: { teamId: string }) {
  const tasks = useStore(useShallow((s) => s.teamDetail[teamId]?.tasks ?? []))
  const maxRounds = useStore((s) => s.teams[teamId]?.budget.max_review_rounds ?? 2)
  const names = useStore(useShallow((s) => Object.fromEntries(teamMemberBots(s, teamId).map((b) => [b.id, teamShortName(b.name)]))))
  const decide = useStore((s) => s.decideTeamTask)
  const busy = useStore((s) => s.busy)
  const [open, setOpen] = useState(true)

  const grouped = useMemo(() => {
    const map = new Map<string, TeamTask[]>()
    for (const t of tasks) {
      const col = TASK_COLUMN[t.state]
      map.set(col, [...(map.get(col) ?? []), t])
    }
    return COLUMN_ORDER.filter((c) => map.has(c)).map((c) => [c, map.get(c)!] as const)
  }, [tasks])

  if (tasks.length === 0) return null

  return (
    <div className="team-tasks">
      <button type="button" className="disclosure sub" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span className="chev">{open ? '▼' : '▶'}</span> Task（{tasks.length}）
        <span className="disclosure-note">
          {grouped.map(([col, list]) => `${col} ${list.length}`).join(' ・ ')}
        </span>
      </button>
      {open ? (
        <div className="team-task-list">
          {grouped.map(([col, list]) => (
            <div key={col} className="team-task-group">
              <span className={`team-task-col${col === '需要你' ? ' urgent' : ''}`}>{col}</span>
              {list.map((t) => {
                const needsUser = TEAM_TASK_NEEDS_USER.includes(t.state)
                const key = `team:${teamId}:decide:${t.id}`
                return (
                  <div key={t.id} className={`team-task${needsUser ? ' urgent' : ''}`}>
                    <span className="team-task-id mono">t{t.seq}</span>
                    <span className="team-task-who">{names[t.worker_bot_id] ?? '—'}</span>
                    <span className="team-task-title" title={`${t.brief}\n分支 ${t.branch}`}>
                      {t.title}
                    </span>
                    <span className={`team-task-state ${t.state}`}>{TEAM_TASK_STATE_LABEL[t.state]}</span>
                    {t.round > 0 ? (
                      <span className="team-task-round" title="審查回合">
                        round {t.round}/{maxRounds}
                      </span>
                    ) : null}
                    {needsUser ? (
                      <span className="team-task-actions">
                        <button
                          type="button"
                          className="mini-btn"
                          disabled={Boolean(busy[key])}
                          title="再給一回合：把 task 送回執行者"
                          onClick={() => void decide(teamId, t.id, 'rework')}
                        >
                          再一回合
                        </button>
                        <button
                          type="button"
                          className="mini-btn"
                          disabled={Boolean(busy[key])}
                          title="不再審查，直接把這條分支合進整合分支"
                          onClick={() => void decide(teamId, t.id, 'force_merge')}
                        >
                          強制合併
                        </button>
                        <button
                          type="button"
                          className="mini-btn danger"
                          disabled={Boolean(busy[key])}
                          title="放棄這件 task（分支保留）"
                          onClick={() => void decide(teamId, t.id, 'skip')}
                        >
                          跳過
                        </button>
                      </span>
                    ) : null}
                  </div>
                )
              })}
            </div>
          ))}
        </div>
      ) : null}
    </div>
  )
}

// ---------------------------------------------------------------- timeline

type Row =
  | { key: string; sort: string; kind: 'msg'; msg: GroupMessage }
  | { key: string; sort: string; kind: 'event'; event: TeamEvent }

function eventText(ev: TeamEvent): string | null {
  const p = ev.payload
  if (ev.kind === 'merge') {
    const branch = typeof p.branch === 'string' ? p.branch : ''
    if (p.result === 'ok') return `已合併 ${branch}${typeof p.sha === 'string' ? `（${p.sha}）` : ''}`
    const files = Array.isArray(p.conflict_files) ? p.conflict_files.join('、') : ''
    return `合併衝突：${branch}${files ? ` — ${files}` : ''}`
  }
  if (ev.kind === 'phase') {
    const to = typeof p.to === 'string' ? p.to : ''
    const from = typeof p.from === 'string' ? p.from : ''
    const reason = typeof p.reason === 'string' && p.reason ? `（${teamPauseLabel(p.reason)}）` : ''
    return `${from} → ${to}${reason}`
  }
  if (ev.kind === 'note') return typeof p.text === 'string' ? p.text : null
  return null
}

function Timeline({ teamId }: { teamId: string }) {
  // The rows come from the project's merged timeline (SPEC-team §11.3). Select the *stable*
  // array from the store and filter in a memo — a selector that builds a new array on every
  // call re-renders forever (React #185).
  const projectId = useStore((s) => s.teams[teamId]?.project_id ?? s.teamDetail[teamId]?.project_id ?? null)
  const all = useStore((s) => (projectId ? s.groupMessages[projectId] : undefined))
  const messages = useMemo(() => (all ? all.filter((m) => m.team_id === teamId) : null), [all, teamId])
  const events = useStore((s) => s.teamEvents[teamId])
  const members = useStore(useShallow((s) => teamMemberBots(s, teamId)))
  const kinds = useMemo(() => Object.fromEntries(members.map((b) => [b.id, b.kind])), [members])
  const shortNames = useMemo(() => Object.fromEntries(members.map((b) => [b.id, teamShortName(b.name)])), [members])
  // 還在回答的成員：一人一顆打字氣泡（沿用單一 bot / 群組的同一套 live 狀態）。
  const typing = useStore(
    useShallow((s) =>
      members.filter((b) => s.runs[b.id]?.agent_status === 'working' || composerState(s, b.id).inFlightTurnId !== null),
    ),
  )
  const liveText = useStore(useShallow((s) => Object.fromEntries(typing.map((b) => [b.id, liveReplyOf(s, b.id)?.text ?? null]))))
  const liveActivity = useStore(
    useShallow((s) => Object.fromEntries(typing.map((b) => [b.id, liveReplyOf(s, b.id)?.activity ?? null]))),
  )
  const liveAlert = useStore(useShallow((s) => Object.fromEntries(typing.map((b) => [b.id, liveReplyOf(s, b.id)?.alert ?? null]))))
  const ref = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

  const rows = useMemo<Row[]>(() => {
    const out: Row[] = []
    for (const m of messages ?? []) out.push({ key: `m:${m.id}`, sort: m.created_at || m.id, kind: 'msg', msg: m })
    for (const e of events ?? []) {
      // relay / user 事件本身就是時間軸上的訊息，不重複畫一次。
      if (e.kind === 'relay' || e.kind === 'user') continue
      if (!eventText(e)) continue
      out.push({ key: `e:${e.id}`, sort: e.created_at || e.id, kind: 'event', event: e })
    }
    return out.sort((a, b) => a.sort.localeCompare(b.sort) || a.key.localeCompare(b.key))
  }, [messages, events])

  useLayoutEffect(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [rows, typing.length, liveText, liveActivity])

  return (
    <div
      className="msg-list group team-timeline"
      ref={ref}
      onScroll={(e) => {
        const el = e.currentTarget
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80
      }}
    >
      {rows.length === 0 ? (
        <EmptyState loading={messages === null} title={messages === null ? undefined : '等待第一則轉送'} icon={messages === null ? undefined : '⚙'}>
          {messages === null ? '載入 Team 時間軸中…' : 'daemon 會在成員都就緒後把 issue 交給 PM。'}
        </EmptyState>
      ) : (
        rows.map((r) => {
          if (r.kind === 'event') {
            return (
              <div key={r.key} className={`team-sys ${r.event.kind}`} role="note">
                <span className="team-sys-kind">{r.event.kind}</span>
                <span className="team-sys-text">{eventText(r.event)}</span>
                <time className="msg-time" dateTime={r.event.created_at}>
                  {timeOf(r.event.created_at)}
                </time>
              </div>
            )
          }
          const m = r.msg
          const { text, blocks } = splitAmTeam(m.content)
          const shown = blocks.length ? { ...m, content: text } : m
          const to = shortNames[m.bot_id] ?? m.bot_name
          const from =
            m.role === 'user' ? (
              m.relay_from ? (
                // `relay_from` 對得到成員 = bot→bot 的轉送；對不到（daemon 自己發的首則指派 /
                // 合併通知 / 修復提示）就標成 daemon。
                <span
                  className={`msg-targets relay${shortNames[m.relay_from] ? '' : ' daemon'}`}
                  title="daemon 代為轉送的訊息（不是你送的）"
                >
                  {shortNames[m.relay_from] ?? 'daemon'} → {to}
                </span>
              ) : (
                <span className="msg-targets" title="你直接對這個成員說的話">
                  你 → {to}
                </span>
              )
            ) : m.role === 'assistant' ? (
              <span className="msg-speaker">
                {to}
                {kinds[m.bot_id] ? ` · ${KIND_TITLE[kinds[m.bot_id]]}` : ''}
              </span>
            ) : undefined
          return (
            <div key={r.key} className="team-msg-row">
              <Bubble msg={shown} kind={m.role === 'assistant' ? kinds[m.bot_id] : undefined} from={from} />
              {blocks.length ? (
                <div className="am-chips">
                  {blocks.map((b, i) => (
                    <AmTeamChip key={i} block={b} />
                  ))}
                </div>
              ) : null}
            </div>
          )
        })
      )}
      {typing.map((t) => (
        <LiveBubble
          key={`typing-${t.id}`}
          text={liveText[t.id] ?? null}
          activity={liveActivity[t.id] ?? null}
          alert={liveAlert[t.id] ?? null}
          kind={t.kind}
          from={
            <span className="msg-speaker">
              {teamShortName(t.name)} · {KIND_TITLE[t.kind]}
            </span>
          }
        />
      ))}
    </div>
  )
}

// ---------------------------------------------------------------- composer

function TeamComposer({ teamId }: { teamId: string }) {
  const members = useStore(useShallow((s) => teamMemberBots(s, teamId)))
  const pm = members.find((b) => b.team?.role === 'pm') ?? members[0] ?? null
  const askUser = useStore((s) => {
    const t = s.teams[teamId]
    return t?.phase === 'paused' && t.pause_reason === 'ask_user'
  })
  const question = useStore((s) => {
    if (!askUser) return null
    const list = s.teamEvents[teamId] ?? []
    for (let i = list.length - 1; i >= 0; i -= 1) {
      const q = list[i].payload.question
      if (typeof q === 'string' && q) return q
    }
    return null
  })
  const sayToTeam = useStore((s) => s.sayToTeam)
  const answerTeam = useStore((s) => s.answerTeam)
  const draftKey = `team:${teamId}` as const
  const text = useStore((s) => s.drafts[draftKey] ?? '')
  const setDraft = useStore((s) => s.setDraft)
  const setDraftCursor = useStore((s) => s.setDraftCursor)
  const [to, setTo] = useState<string | null>(null)
  const [sending, setSending] = useState(false)
  const ref = useRef<HTMLTextAreaElement>(null)

  // Keep the same draft-selection behavior as bot and group composers. Do not focus here:
  // opening a Team view should not steal focus from another control.
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const currentText = useStore.getState().drafts[draftKey] ?? ''
    const saved = useStore.getState().draftCursors[draftKey]
    const max = currentText.length
    const start = Math.max(0, Math.min(max, saved?.start ?? max))
    const end = Math.max(start, Math.min(max, saved?.end ?? start))
    el.setSelectionRange(start, end)
  }, [draftKey, ref])

  const target = to && members.some((b) => b.id === to) ? to : (pm?.id ?? null)

  const submit = () => {
    const body = text.trim()
    if (!body || sending || !target) return
    setSending(true)
    const run = askUser ? answerTeam(teamId, body) : sayToTeam(teamId, body, target)
    void run.then((ok) => {
      setSending(false)
      if (ok) setDraft(draftKey, '')
    })
  }

  return (
    <div className="composer group-composer team-composer">
      {askUser ? (
        <div className="team-ask" role="status">
          <span className="team-ask-mark" aria-hidden="true">
            ?
          </span>
          <span>PM 需要你的答覆{question ? `：${question}` : '（詳見上方時間軸）'}</span>
        </div>
      ) : null}
      <div className="recipient-row" role="group" aria-label="收件者">
        {members.map((b) => {
          const on = target === b.id
          return (
            <button
              key={b.id}
              type="button"
              className={`recipient-chip ${b.kind}${on ? ' on' : ''}`}
              aria-pressed={on}
              disabled={sending || askUser}
              title={askUser ? '回答 PM 的問題時只會送給 PM' : `送給 ${b.name}`}
              onClick={() => setTo(b.id)}
            >
              @{teamShortName(b.name)}
            </button>
          )
        })}
        {members.length === 0 ? <span className="hint">（沒有可對話的成員）</span> : null}
      </div>
      <div className="composer-box">
        <textarea
          ref={ref}
          value={text}
          disabled={sending || members.length === 0}
          placeholder={askUser ? '回答 PM 的問題（送出後 team 會自動繼續）…' : '對成員插話（不計轉送預算）…'}
          title="Enter 送出，Shift+Enter 換行"
          onChange={(e) => {
            setDraft(draftKey, e.target.value)
            setDraftCursor(draftKey, e.target.selectionStart, e.target.selectionEnd)
          }}
          onSelect={() => {
            const el = ref.current
            if (el) setDraftCursor(draftKey, el.selectionStart, el.selectionEnd)
          }}
          onClick={() => {
            const el = ref.current
            if (el) setDraftCursor(draftKey, el.selectionStart, el.selectionEnd)
          }}
          onBlur={() => {
            const el = ref.current
            if (el) setDraftCursor(draftKey, el.selectionStart, el.selectionEnd)
          }}
          onKeyUp={() => {
            const el = ref.current
            if (el) setDraftCursor(draftKey, el.selectionStart, el.selectionEnd)
          }}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing) return
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              submit()
            }
          }}
        />
        <button
          type="button"
          className="send-btn"
          disabled={sending || !text.trim() || !target}
          title={askUser ? '回答 PM 並繼續' : `送給 @${target ? teamShortName(members.find((b) => b.id === target)?.name ?? '') : ''}`}
          onClick={submit}
        >
          {sending ? '送出中…' : askUser ? '回答並繼續' : '送出'}
        </button>
      </div>
      <div className="composer-hint group-hint">
        <span className="group-targets">
          {askUser
            ? '→ PM（會同時解除暫停）'
            : `→ @${teamShortName(members.find((b) => b.id === target)?.name ?? '')}・使用者插話不計入 max_relays`}
        </span>
      </div>
    </div>
  )
}

// ---------------------------------------------------------------- panel

function BudgetMeter({ teamId }: { teamId: string }) {
  const team = useStore((s) => s.teams[teamId] ?? null)
  if (!team) return null
  const { relays, elapsed_min } = team.usage
  const pct = Math.min(100, Math.round((relays / Math.max(1, team.budget.max_relays)) * 100))
  const level = pct >= 100 ? 'crit' : pct >= 75 ? 'warn' : 'ok'
  return (
    <span
      className={`team-budget-meter ${level}`}
      title={`轉送 ${relays}/${team.budget.max_relays}・已跑 ${elapsed_min}/${team.budget.max_wall_clock_min} 分鐘・審查回合合計 ${team.usage.review_rounds_total}`}
    >
      <span className="team-budget-bar">
        <span className="team-budget-fill" style={{ width: `${pct}%` }} />
      </span>
      <span className="team-budget-text mono">
        {relays}/{team.budget.max_relays} · {elapsed_min} 分
      </span>
    </span>
  )
}

export function TeamPanel({ teamId, onOpenSidebar }: { teamId: string; onOpenSidebar: () => void }) {
  const team = useStore((s) => s.teams[teamId] ?? null)
  const detail = useStore((s) => s.teamDetail[teamId] ?? null)
  const project = useStore((s) => s.projects.find((p) => p.id === (team?.project_id ?? '')) ?? null)
  const controlTeam = useStore((s) => s.controlTeam)
  const patchTeam = useStore((s) => s.patchTeam)
  const selectProject = useStore((s) => s.selectProject)
  const busy = useStore((s) => s.busy)
  // 額度列把成員用的 kind（PM 優先）排到最前面。選字串，所以引用是穩定的。
  const memberKind = useStore((s) => {
    const members = teamMemberBots(s, teamId)
    return members.find((b) => b.team?.role === 'pm')?.kind ?? members[0]?.kind ?? null
  })
  const [confirm, setConfirm] = useState<'abort' | 'cleanup' | 'delete' | null>(null)

  if (!team) {
    return (
      <div className="main-head">
        <button type="button" className="btn menu-btn" onClick={onOpenSidebar} aria-label="開啟側邊欄">
          ☰
        </button>
        <span className="main-status">找不到這個 Team（可能已清理）</span>
      </div>
    )
  }

  const terminal = TEAM_TERMINAL_PHASES.includes(team.phase)
  const paused = team.phase === 'paused'
  const gated = paused && (team.pause_reason ?? '').startsWith('gate:')
  const budgetPause = paused && (team.pause_reason === 'budget_relays' || team.pause_reason === 'budget_time')

  return (
    <>
      <div className="main-head group-head team-head">
        <button
          type="button"
          className="btn menu-btn icon-tip"
          onClick={onOpenSidebar}
          aria-label="開啟側邊欄"
          title="開啟側邊欄"
          data-tip="開啟側邊欄"
        >
          ☰
        </button>
        <div className="main-title">
          <span className="team-icon" aria-hidden="true">
            ⚙
          </span>
          <strong title={team.issue_title}>
            Team · #{team.issue_number} {team.issue_title}
          </strong>
          <span className={`team-phase ${teamPhaseTone(team.phase)}`} title={team.pause_reason ? teamPauseLabel(team.pause_reason) : undefined}>
            {TEAM_PHASE_LABEL[team.phase]}
            {paused && team.pause_reason ? ` · ${teamPauseLabel(team.pause_reason)}` : ''}
          </span>
          <span className={`team-deliver ${team.deliver}`} title={team.deliver === 'pr' ? '完成時會 push 到 origin 並開 PR' : '完成時只留下整合分支，不 push'}>
            {team.deliver === 'pr' ? '交付：PR' : '交付：留分支'}
          </span>
        </div>
        <span className="spacer" />
        <BudgetMeter teamId={teamId} />
        <QuotaStrip focusKind={memberKind} />
        <div className="head-actions">
          {gated ? (
            <button
              type="button"
              className="mini-btn primary"
              disabled={Boolean(busy[`team:${teamId}:approve`])}
              title="放行這一個 supervised 閘門"
              onClick={() => void controlTeam(teamId, 'approve')}
            >
              放行
            </button>
          ) : null}
          {paused && !gated ? (
            <>
              {budgetPause ? (
                <button
                  type="button"
                  className="mini-btn"
                  disabled={Boolean(busy[`team:${teamId}:patch`])}
                  title="把轉送上限與時間上限各加一倍，然後繼續"
                  onClick={() =>
                    void patchTeam(teamId, {
                      budget: {
                        max_relays: team.budget.max_relays * 2,
                        max_wall_clock_min: team.budget.max_wall_clock_min * 2,
                      },
                    })
                  }
                >
                  加碼預算
                </button>
              ) : null}
              <button
                type="button"
                className="mini-btn primary"
                disabled={Boolean(busy[`team:${teamId}:resume`])}
                title="從暫停的地方繼續（會重送待送的轉送）"
                onClick={() => void controlTeam(teamId, 'resume')}
              >
                繼續
              </button>
            </>
          ) : null}
          {!paused && !terminal ? (
            <button
              type="button"
              className="mini-btn"
              disabled={Boolean(busy[`team:${teamId}:pause`])}
              title="暫停：成員 pane 仍留著，隨時可以繼續"
              onClick={() => void controlTeam(teamId, 'pause')}
            >
              暫停
            </button>
          ) : null}
          {terminal ? (
            <button type="button" className="mini-btn danger" title="移除 worktree 與成員 bot（分支保留）" onClick={() => setConfirm('cleanup')}>
              清理
            </button>
          ) : (
            <button type="button" className="mini-btn danger" title="中止：停掉所有成員，不可逆" onClick={() => setConfirm('abort')}>
              中止
            </button>
          )}
          <button
            type="button"
            className="mini-btn danger"
            disabled={Boolean(busy[`team:${teamId}:delete`])}
            title="刪除：連 Team 紀錄一起移除（進行中會先停成員；訊息與分支預設保留）"
            onClick={() => setConfirm('delete')}
          >
            刪除
          </button>
          <button type="button" className="mini-btn" title="回到這個 Project 的群組聊天" onClick={() => selectProject(team.project_id)}>
            關閉
          </button>
        </div>
      </div>

      <div className="team-subhead">
        {/* 成員燈號列自成一列：標題列放不下（issue 標題很長），而且成員是常看的東西。 */}
        <MemberStrip teamId={teamId} />
        <span className="team-sep" aria-hidden="true" />
        <CopyChip label="整合分支" value={team.branch} title="daemon 把通過審查的 task 合併到這條分支" />
        <CopyChip label="worktree" value={detail?.worktree_root ?? ''} title="成員的工作目錄根（在 daemon 資料目錄下，不在你的 checkout 裡）" />
        {detail?.base_ref ? (
          <span className="team-base mono" title={detail.base_sha}>
            base {detail.base_ref}
          </span>
        ) : null}
        {team.pr_url ? (
          <a className="team-pr" href={team.pr_url} target="_blank" rel="noreferrer">
            開啟 PR
          </a>
        ) : null}
        {team.issue_url ? (
          <a className="team-pr" href={team.issue_url} target="_blank" rel="noreferrer">
            issue #{team.issue_number}
          </a>
        ) : null}
        <span className="spacer" />
        {project ? (
          <span className="team-project" title={project.path}>
            {project.label}
          </span>
        ) : null}
      </div>

      {paused ? (
        <div className="team-paused" role="alert">
          <span className="team-paused-mark" aria-hidden="true">
            ‖
          </span>
          <span className="team-paused-text">
            已暫停{team.pause_reason ? `：${teamPauseLabel(team.pause_reason)}` : ''}。
            {budgetPause
              ? '加碼預算後即可繼續，成員都還活著。'
              : gated
                ? '這是 supervised 閘門，確認後按「放行」。'
                : (team.pause_reason ?? '').startsWith('member_')
                  ? '請先處理該成員（啟動 / 回應終端提示），再按「繼續」。'
                  : '處理完上面的原因後按「繼續」。'}
          </span>
        </div>
      ) : null}

      {detail?.summary && terminal ? (
        <div className="team-summary" role="note">
          <strong>PM 總結</strong>
          <span>{detail.summary}</span>
        </div>
      ) : null}

      <div className="chat team-chat">
        <TaskList teamId={teamId} />
        <Timeline teamId={teamId} />
        {terminal ? (
          <div className="composer group-composer team-composer">
            <div className="composer-lock" role="status">
              <span>⛔ Team {TEAM_PHASE_LABEL[team.phase]}，成員已停止。需要保留現場就先看 worktree，處理完再按「清理」。</span>
            </div>
          </div>
        ) : (
          <TeamComposer teamId={teamId} />
        )}
      </div>

      <ConfirmDialog
        open={confirm === 'abort'}
        title="中止這個 Team？"
        body={
          <>
            <p>
              會停掉 <strong>{team.members.length}</strong> 個成員的 pane。已合併的工作留在{' '}
              <code>{team.branch}</code>，worktree 也會留著。<strong>中止不可逆</strong>——只是想喘口氣請用「暫停」。
            </p>
            <p className="hint">
              Team #{team.issue_number} {team.issue_title}
              {project ? ` · ${project.label}` : ''}
            </p>
          </>
        }
        confirmLabel="中止"
        danger
        onCancel={() => setConfirm(null)}
        onConfirm={() => {
          setConfirm(null)
          void controlTeam(teamId, 'abort')
        }}
      />
      <ConfirmDialog
        open={confirm === 'cleanup'}
        title="清理這個 Team？"
        body={
          <>
            <p>
              會移除 {team.members.length} 個成員 bot 與它們的 worktree（<code>{detail?.worktree_root ?? '資料目錄下的 team 目錄'}</code>）。
              <strong>分支一律保留</strong>，訊息歷史也保留。
            </p>
            <p className="hint">
              Team #{team.issue_number} {team.issue_title}
              {project ? ` · ${project.label}` : ''}
            </p>
          </>
        }
        confirmLabel="清理"
        danger
        onCancel={() => setConfirm(null)}
        onConfirm={() => {
          setConfirm(null)
          void controlTeam(teamId, 'cleanup')
        }}
      />
      {confirm === 'delete' ? <TeamDeleteDialog teamId={teamId} onClose={() => setConfirm(null)} /> : null}
    </>
  )
}
