import { useMemo, useState } from 'react'
import type { Bot } from '../api/types'
import type { Schedule } from '../api/schedules'
import { LAMP_LABEL } from '../components/lampLabel'
import { ModelTag } from '../components/ModelTag'
import { KIND_LABEL } from '../components/kindMeta'
import { msg, useT } from '../lib/i18n'
import { botLamp, useStore } from '../store/store'

/** 不能接排程的原因（daemon 也會再驗一次）；`null` 就是可選。 */
export function ineligibleReason(b: Bot, projectId: string): string | null {
  if (b.project_id !== projectId) return msg('屬於其他專案')
  if (b.kind === 'claude-api') return msg('claude-api 不能接排程')
  if (b.managed_by !== 'user' || b.parent_bot_id) return msg('子 agent 不能接排程')
  if (b.share_profile) return msg('分享用 Bot 不能接排程')
  return null
}

const ROLE_LABEL: Record<string, string> = { manager: msg('管'), executor: msg('執'), advisor: msg('顧'), specialist: msg('專'), scheduler: msg('排') }

/**
 * 執行的 Bot 清單（取代原生 select）：每列有名稱、kind、模型＋強度、身份、角色、狀態燈，
 * 已被其他排程用的標「已有 N 個排程」，不合格的灰掉寫原因。目前選的置頂。手機上每列 ≥ 44px。
 */
export function ScheduleBotPicker({
  projectId,
  bots,
  selectedId,
  usageOf,
  onSelect,
  disabled,
}: {
  projectId: string
  bots: Bot[]
  selectedId: string
  /** 其他（不是這份）排程用了這顆 Bot 的列表。 */
  usageOf: (botId: string) => Schedule[]
  onSelect: (id: string) => void
  disabled?: boolean
}) {
  const t = useT()
  const state = useStore((s) => s)
  const [query, setQuery] = useState('')
  const rows = useMemo(() => {
    const q = query.trim().toLowerCase()
    const list = bots.filter((b) => !q || b.name.toLowerCase().includes(q) || (b.identity ?? '').toLowerCase().includes(q))
    const reasonOf = (b: Bot) => ineligibleReason(b, projectId)
    return list.slice().sort((a, b) => {
      const rank = (x: Bot) => (x.id === selectedId ? 0 : reasonOf(x) ? 2 : 1)
      return rank(a) - rank(b) || a.name.localeCompare(b.name)
    })
  }, [bots, projectId, query, selectedId])

  return (
    <div className="sched-bot-picker" role="group" aria-label={t('執行的 Bot')}>
      {bots.length > 6 ? (
        <input
          className="sched-bot-search"
          type="search"
          placeholder={t('搜尋 Bot 名稱或身份')}
          aria-label={t('搜尋 Bot')}
          value={query}
          disabled={disabled}
          onChange={(e) => setQuery(e.target.value)}
        />
      ) : null}
      {rows.length === 0 ? <p className="hint">{t('（這個專案沒有 Bot）')}</p> : null}
      <ul className="sched-bot-list">
        {rows.map((b) => {
          const reason = ineligibleReason(b, projectId)
          const selected = b.id === selectedId
          const used = usageOf(b.id)
          const lamp = botLamp(state, b.id)
          return (
            <li key={b.id}>
              <button
                type="button"
                data-bot-id={b.id}
                className={`sched-bot-row${selected ? ' selected' : ''}${reason ? ' ineligible' : ''}`}
                aria-pressed={selected}
                disabled={disabled || reason !== null}
                onClick={() => onSelect(b.id)}
              >
                <span className="sched-bot-name">
                  {selected ? <span className="sched-bot-check" aria-hidden="true">✓ </span> : null}
                  {b.name}
                  <span className="sched-bot-kind">{KIND_LABEL[b.kind]}</span>
                </span>
                <span className="sched-bot-meta">
                  <ModelTag botId={b.id} />
                  {b.identity ? <span className="sched-bot-identity">{b.identity}</span> : null}
                  {b.role ? <span className="sched-bot-role">{t.tr(ROLE_LABEL[b.role] ?? b.role)}</span> : null}
                </span>
                <span className="sched-bot-state">
                  <span className="sched-bot-dot" data-lamp={lamp} aria-hidden="true" />
                  {t.tr(LAMP_LABEL[lamp])}
                  {used.length > 0 ? <span className="sched-bot-used">{t('已有 {n} 個排程', { n: used.length })}</span> : null}
                </span>
                {reason ? <span className="sched-bot-reason">{t.tr(reason)}</span> : null}
              </button>
            </li>
          )
        })}
      </ul>
    </div>
  )
}
