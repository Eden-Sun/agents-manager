import { ApiError, type Bot, type Message, type Run } from '../api/types'
import { t } from './i18n'
import { humanMessage, plainErrMsg } from './errText'

export function canOfferRewind(msg: Message, bot: Pick<Bot, 'kind' | 'herdr_session'> | null | undefined): boolean {
  if (!bot || msg.role !== 'user' || msg.rewound_at) return false
  return bot.kind === 'claude' && bot.herdr_session !== 'default'
}

export function rewindBlocked(run: Pick<Run, 'state' | 'agent_status'> | null | undefined): string | null {
  if (!run || run.state !== 'running') return t('bot 沒在跑：倒回要接著目前這段對話')
  if (run.agent_status === 'working') return t('它正在跑這一回合，等它結束再倒回')
  if (run.agent_status === 'blocked') return t('它卡在提問，先回答或中斷再倒回')
  if (run.agent_status !== 'idle') return t('狀態不明，等它回報閒置再倒回')
  return null
}

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

export function markRewoundInGroup<T extends Message>(list: T[], messageId: string, at: string): T[] | null {
  const i = list.findIndex((m) => m.id === messageId)
  if (i < 0) return null
  const conv = list[i].conversation_id
  let changed = false
  const next = list.map((m, j) => {
    if (j < i || m.rewound_at || m.conversation_id !== conv) return m
    changed = true
    return { ...m, rewound_at: at }
  })
  return changed ? next : null
}

export function rewindErrText(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 404 && e.body.what === 'message') return t('找不到這則訊息。')
    if (e.status === 404 && e.body.what === 'bot') return t('找不到這顆 bot。')
    if (e.status === 404 || e.status === 405) return t('這顆 daemon 還沒有倒回功能（HTTP {status}），要重建並重啟 daemon。', { status: e.status })
    const human = humanMessage(e)
    if (human) return human
  }
  return plainErrMsg(e)
}
