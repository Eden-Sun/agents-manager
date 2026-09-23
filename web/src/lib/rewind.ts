import { ApiError, type Bot, type Message, type Run } from '../api/types'

/**
 * 對話倒回（SPEC §6.13）的前端規則。daemon 才是權威（它會再查一次、不行就 409）；這裡只決定按鈕要不要出現、能不能按，
 * 以及收到 `messages_rewound` 時怎麼標清單。
 */

/** 這則訊息要不要有「倒回這裡」：claude（含子 agent）、不是 default session 匯入的（那是使用者自己的 pane，只看不代打）、還沒被倒掉的使用者訊息。 */
export function canOfferRewind(msg: Message, bot: Pick<Bot, 'kind' | 'herdr_session'> | null | undefined): boolean {
  if (!bot || msg.role !== 'user' || msg.rewound_at) return false
  return bot.kind === 'claude' && bot.herdr_session !== 'default'
}

/** 現在不能倒的理由（按鈕 disabled 的 tooltip）；`null`＝可以。倒回會重啟 bot，正在跑的回合會被砍掉，所以只在閒著時給按。 */
export function rewindBlocked(run: Pick<Run, 'state' | 'agent_status'> | null | undefined): string | null {
  if (!run || run.state !== 'running') return 'bot 沒在跑：倒回要接著目前這段對話'
  if (run.agent_status === 'working') return '它正在跑這一回合，等它結束再倒回'
  if (run.agent_status === 'blocked') return '它卡在提問，先回答或中斷再倒回'
  if (run.agent_status !== 'idle') return '狀態不明，等它回報閒置再倒回'
  return null
}

/** 這則與之後的都標成倒回（清單照時間排）。找不到那則回 `null`＝不用動。 */
export function markRewound(list: Message[], messageId: string, at: string): Message[] | null {
  const i = list.findIndex((m) => m.id === messageId)
  if (i < 0) return null
  let changed = false
  const next = list.map((m, j) => {
    if (j < i || m.rewound_at) return m
    changed = true
    return { ...m, rewound_at: at }
  })
  return changed ? next : null
}

/** 失敗時給使用者看的一句：daemon 的 409 帶寫好的 `message`，直接用；舊 daemon 沒這支 API 講清楚要重建。 */
export function rewindErrText(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 404 && e.body.what === 'message') return '找不到這則訊息。'
    if (e.status === 404 && e.body.what === 'bot') return '找不到這顆 bot。'
    // 舊 daemon：POST 掉進只收 GET 的前端 catch-all（405），或路由不存在（404）。
    if (e.status === 404 || e.status === 405) return `這顆 daemon 還沒有倒回功能（HTTP ${e.status}），要重建並重啟 daemon。`
    const human = e.body.message
    if (typeof human === 'string' && human.trim()) return human.trim()
  }
  return e instanceof Error ? e.message : String(e)
}
