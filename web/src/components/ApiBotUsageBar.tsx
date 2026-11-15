import { useEffect, useState } from 'react'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { apiCacheChip } from '../lib/composerCost'
import { apiBotSpendUsd, fetchApiBotUsage, formatTokensK, type ApiBotUsage } from '../api/apiBotUsage'
import { balanceView, fetchClaudeApiBalance, type ClaudeApiBalance } from '../api/claudeApiBalance'
import { balanceTooltipLines } from './balanceTooltip'
import { fetchClaudeApiCap, putClaudeApiCap, putClaudeApiTurnLimits, type ClaudeApiCap } from '../api/claudeApiCap'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import './apiBotUsageBar.css'

const POLL_MS = 10_000

/**
 * claude-api bot 沒有 CLI 的 statusLine／context／pane，狀態列改放 `GET /api/bots/{id}/api-usage`：
 * 累計花費、快取命中率、最近一次的 speed。回合結束後立刻補抓一次，其餘每 10 秒輪詢（沒有 WS 事件）。
 * 若配置了 Anthropic 預付餘額（ClaudeApiBalance.configured），亦顯示預估剩餘餘額（唯讀）。
 * 若設定了總上限，顯示「上限 $Y / $X」，點擊可修改或清除；未設定時顯示「設定上限」。
 */
export function ApiBotUsageBar({ botId }: { botId: string }) {
  const t = useT()
  const phone = useMediaQuery(PHONE_QUERY)
  const [mobileExpanded, setMobileExpanded] = useState(false)
  const [usage, setUsage] = useState<ApiBotUsage | null>(null)
  const [balance, setBalance] = useState<ClaudeApiBalance | null>(null)
  const [cap, setCap] = useState<ClaudeApiCap | null>(null)
  const [failed, setFailed] = useState(false)
  const [editingCap, setEditingCap] = useState(false)
  const [newCapVal, setNewCapVal] = useState('')
  const [savingCap, setSavingCap] = useState(false)
  const [editingLimits, setEditingLimits] = useState(false)
  const [newTimeout, setNewTimeout] = useState('')
  const [newCalls, setNewCalls] = useState('')
  const [newAutoLowerCost, setNewAutoLowerCost] = useState('')
  const [savingLimits, setSavingLimits] = useState(false)
  const [limitsError, setLimitsError] = useState('')

  // 快取倒數每秒重算（時間點是 daemon 給的精確值，這裡只減「現在」）。
  const [nowMs, setNowMs] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNowMs(Date.now()), 1000)
    return () => clearInterval(id)
  }, [])
  const botModel = useStore((s) => s.bots.find((b) => b.id === botId))

  // 最後一個回合的狀態改變（in_flight → completed）＝該重抓了。
  const turnSig = useStore((s) => {
    const turns = Object.values(s.turns[botId] ?? {})
    return `${turns.length}:${turns.filter((x) => x.status === 'in_flight').length}`
  })

  useEffect(() => {
    let alive = true
    const loadUsage = () =>
      fetchApiBotUsage(botId).then(
        (u) => alive && (setUsage(u), setFailed(false)),
        () => alive && setFailed(true),
      )
    const loadBalance = () =>
      fetchClaudeApiBalance().then(
        (b) => alive && setBalance(b),
        () => alive && setBalance(null),
      )
    const loadCap = () =>
      fetchClaudeApiCap().then(
        (c) => alive && setCap(c),
        () => alive && setCap(null),
      )

    void loadUsage()
    void loadBalance()
    void loadCap()

    const id = setInterval(() => {
      void loadUsage()
      void loadBalance()
      void loadCap()
    }, POLL_MS)
    return () => {
      alive = false
      clearInterval(id)
    }
  }, [botId, turnSig])

  const handleStartEditCap = () => {
    setNewCapVal(cap?.cap_usd !== null && cap?.cap_usd !== undefined ? cap.cap_usd.toString() : '')
    setEditingCap(true)
  }

  const handleSaveCap = async (e: React.FormEvent) => {
    e.preventDefault()
    const val = parseFloat(newCapVal)
    if (Number.isNaN(val) || val < 0) return
    setSavingCap(true)
    try {
      const updated = await putClaudeApiCap(val)
      setCap(updated)
      setEditingCap(false)
    } catch {
      // 保持編輯狀態讓使用者重試或取消
    } finally {
      setSavingCap(false)
    }
  }

  const handleStartEditLimits = () => {
    setNewTimeout(String(cap?.turn_timeout_secs ?? ''))
    setNewCalls(String(cap?.max_api_calls_per_turn ?? ''))
    setNewAutoLowerCost(
      cap?.auto_lower_effort_turn_cost_microusd !== null && cap?.auto_lower_effort_turn_cost_microusd !== undefined
        ? (cap.auto_lower_effort_turn_cost_microusd / 1e6).toString()
        : '',
    )
    setLimitsError('')
    setEditingLimits(true)
  }

  const handleSaveLimits = async (e: React.FormEvent) => {
    e.preventDefault()
    const secs = Number.parseInt(newTimeout, 10)
    const calls = Number.parseInt(newCalls, 10)
    if (!Number.isInteger(secs) || secs < 1 || secs > 3600 || !Number.isInteger(calls) || calls < 1 || calls > 64) {
      setLimitsError(t('時間要在 1–3600 秒，呼叫次數要在 1–64'))
      return
    }
    const payload: {
      turn_timeout_secs: number
      max_api_calls_per_turn: number
      auto_lower_effort_turn_cost_microusd?: number | null
    } = {
      turn_timeout_secs: secs,
      max_api_calls_per_turn: calls,
    }
    const costVal = parseFloat(newAutoLowerCost)
    if (!Number.isNaN(costVal) && costVal > 0) {
      payload.auto_lower_effort_turn_cost_microusd = Math.round(costVal * 1e6)
    } else if (cap?.auto_lower_effort_turn_cost_microusd) {
      payload.auto_lower_effort_turn_cost_microusd = null
    }
    setSavingLimits(true)
    try {
      setCap(await putClaudeApiTurnLimits(payload))
      setEditingLimits(false)
    } catch (err) {
      // 保持編輯狀態讓使用者改正（例如時間小於單一指令逾時）
      setLimitsError(err instanceof Error ? err.message : String(err))
    } finally {
      setSavingLimits(false)
    }
  }

  const handleClearCap = async () => {
    setSavingCap(true)
    try {
      const updated = await putClaudeApiCap(null)
      setCap(updated)
      setEditingCap(false)
    } catch {
      // 保持編輯狀態
    } finally {
      setSavingCap(false)
    }
  }

  if (!usage) return failed ? <span className="api-usage-bar warn">{t('讀不到 API 用量')}</span> : null
  const spend = apiBotSpendUsd(usage)
  const u = usage.cost_microusd
  const pending = u.reserved + u.unknown
  const rate = usage.cache_hit_rate

  const bal = balanceView(balance)
  const balanceTooltip = balanceTooltipLines(balance, bal, t).join('\n')

  const isCapExceededOrClose =
    cap?.cap_usd !== null &&
    cap?.cap_usd !== undefined &&
    cap.cap_usd > 0 &&
    cap.spent_usd >= cap.cap_usd * 0.9

  const cacheChip = apiCacheChip(usage.cache, nowMs, usage.context.tokens, botModel?.model ?? null, botModel?.fast === true)
  const showFullDetails = !phone || mobileExpanded || editingCap || editingLimits

  return (
    <span className="api-usage-bar" data-testid="api-usage-bar">
      {usage.stuck ? (
        <span
          className="sl-item warn api-stuck-chip"
          data-k="stuck"
          title={t('Bot 卡住：{reason}', { reason: usage.stuck.message || usage.stuck.reason })}
        >
          <span className="sl-k">{t('卡住')}</span>
          <span className="sl-v">{usage.stuck.reason}</span>
        </span>
      ) : null}
      {showFullDetails && (bal.usd !== undefined || bal.loggedOut) ? (
        <span
          className={`sl-item api-balance-chip readonly${bal.expiringSoon || bal.loggedOut ? ' warn' : ''}`}
          data-k="balance"
          data-source={bal.source}
          title={balanceTooltip}
        >
          <span className="sl-k">Balance</span>
          <span className="sl-v">
            {bal.usd === undefined ? '—' : `${bal.source === 'console' ? '' : '≈ '}$${bal.usd.toFixed(2)}`}
            {bal.loggedOut ? ' ⚠' : ''}
          </span>
        </span>
      ) : null}

      {cap ? (
        editingCap ? (
          <form className="api-balance-form" onSubmit={(e) => void handleSaveCap(e)}>
            <label className="sl-k" htmlFor="cap-input">{t('上限 $')}</label>
            <input
              id="cap-input"
              className="api-balance-input"
              type="number"
              step="0.01"
              min="0"
              autoFocus
              value={newCapVal}
              disabled={savingCap}
              onChange={(e) => setNewCapVal(e.target.value)}
              placeholder="0.00"
            />
            <button className="api-balance-btn" type="submit" disabled={savingCap}>
              {t('儲存')}
            </button>
            {cap.cap_usd !== null ? (
              <button
                className="api-balance-btn clear"
                type="button"
                disabled={savingCap}
                onClick={() => void handleClearCap()}
              >
                {t('清除')}
              </button>
            ) : null}
            <button
              className="api-balance-btn cancel"
              type="button"
              disabled={savingCap}
              onClick={() => setEditingCap(false)}
            >
              {t('取消')}
            </button>
          </form>
        ) : cap.cap_usd === null || cap.cap_usd === undefined ? (
          <button
            type="button"
            className="sl-item api-balance-chip"
            data-k="cap-setup"
            title={t('設定總花費上限（所有 claude-api bot 合計，從設定時起算）')}
            onClick={handleStartEditCap}
          >
            <span className="sl-k">{t('設定上限')}</span>
          </button>
        ) : (
          <button
            type="button"
            className={`sl-item api-balance-chip${isCapExceededOrClose ? ' warn' : ''}`}
            data-k="cap"
            title={t('總花費上限：從 {since} 起算已花 ${spent}，上限 ${cap}', {
              since: cap.since ?? '—',
              spent: cap.spent_usd.toFixed(2),
              cap: cap.cap_usd.toFixed(2),
            })}
            onClick={handleStartEditCap}
          >
            <span className="sl-k">{t('上限')}</span>
            <span className="sl-v">${cap.spent_usd.toFixed(2)} / ${cap.cap_usd.toFixed(2)}</span>
          </button>
        )
      ) : null}

      {cap && cap.turn_timeout_secs !== null && cap.max_api_calls_per_turn !== null ? (
        editingLimits ? (
          <form className="api-balance-form" data-k="turn-limits-form" onSubmit={(e) => void handleSaveLimits(e)}>
            <label className="sl-k" htmlFor="turn-timeout-input">{t('回合時間（秒，1–3600）')}</label>
            <input id="turn-timeout-input" className="api-balance-input" type="number" min="1" max="3600" step="1" autoFocus value={newTimeout} disabled={savingLimits} onChange={(e) => setNewTimeout(e.target.value)} />
            <label className="sl-k" htmlFor="turn-calls-input">{t('呼叫次數（1–64）')}</label>
            <input id="turn-calls-input" className="api-balance-input" type="number" min="1" max="64" step="1" value={newCalls} disabled={savingLimits} onChange={(e) => setNewCalls(e.target.value)} />
            <label className="sl-k" htmlFor="turn-auto-lower-input">{t('降級門檻（美元，空＝關閉）')}</label>
            <input
              id="turn-auto-lower-input"
              className="api-balance-input"
              type="number"
              min="0"
              step="0.01"
              value={newAutoLowerCost}
              disabled={savingLimits}
              onChange={(e) => setNewAutoLowerCost(e.target.value)}
              placeholder="0.00"
            />
            <span className="api-limits-hint" title={t('下一回合起生效；時間到會先請模型收尾寫摘要，不是直接中止')}>{t('下一回合起生效')}</span>
            <button className="api-balance-btn" type="submit" disabled={savingLimits}>{t('儲存')}</button>
            <button className="api-balance-btn cancel" type="button" disabled={savingLimits} onClick={() => setEditingLimits(false)}>{t('取消')}</button>
            {limitsError ? <span className="sl-v warn" role="alert">{limitsError}</span> : null}
          </form>
        ) : (
          <button
            type="button"
            className="sl-item api-balance-chip"
            data-k="turn-limits"
            title={t('每回合上限：時間 {secs} 秒、API 呼叫 {calls} 次{lower}。時間到會先請模型收尾寫摘要，不是直接中止；點擊可修改（下一回合起生效）', {
              secs: cap.turn_timeout_secs,
              calls: cap.max_api_calls_per_turn,
              lower: cap.auto_lower_effort_turn_cost_microusd ? t('、回合成本滿 ${cost} 降 effort', { cost: (cap.auto_lower_effort_turn_cost_microusd / 1e6).toFixed(2) }) : '',
            })}
            onClick={handleStartEditLimits}
          >
            <span className="sl-k">{t('回合')}</span>
            <span className="sl-v">
              {Math.round(cap.turn_timeout_secs / 60)}m · {cap.max_api_calls_per_turn} calls
              {cap.auto_lower_effort_turn_cost_microusd ? ` · ↓$${(cap.auto_lower_effort_turn_cost_microusd / 1e6).toFixed(2)}` : ''}
            </span>
          </button>
        )
      ) : null}

      <span className="sl-item" data-k="cost" title={t('累計花費＝已結算 ${rec}＋預留 ${res}＋結果不明 ${unk}（預留與不明先當花掉算）', { rec: (u.recorded / 1e6).toFixed(4), res: (u.reserved / 1e6).toFixed(2), unk: (u.unknown / 1e6).toFixed(2) })}>
        <span className="sl-k">{t('累計')}</span>
        <span className="sl-v">${spend.toFixed(2)}</span>
        {pending > 0 ? <span className="api-usage-hold">{t('含預留')}</span> : null}
      </span>
      {showFullDetails ? (
        <>
          {usage.context.tokens !== null ? (
            <span
              className={`sl-item${usage.context.compact_at !== null && usage.context.tokens >= usage.context.compact_at * 0.9 ? ' warn' : ''}`}
              data-k="context"
              title={t('目前 context＝最近一次請求最後一個 iteration 的 input＋cache_read＋cache_creation；到壓縮門檻會觸發伺服器端壓縮，壓縮後下降')}
            >
              <span className="sl-k">context</span>
              <span className="sl-v">
                {formatTokensK(usage.context.tokens)}
                {usage.context.compact_at !== null ? ` / ${t('壓縮門檻 {n}', { n: formatTokensK(usage.context.compact_at) })}` : ''}
              </span>
            </span>
          ) : null}
          {cacheChip ? (
            <span className={`sl-item api-cache-${cacheChip.level}`} data-k="cache-countdown" title={cacheChip.title}>
              <span className="sl-k">{t('快取')}</span>
              <span className="sl-v">{cacheChip.label}</span>
            </span>
          ) : null}
          <span className="sl-item" data-k="cache" title={t('快取命中率＝cache_read ÷ (input＋cache_read＋cache_creation)，只算已結算的回合')}>
            <span className="sl-k">{t('快取命中')}</span>
            <span className="sl-v">{rate === null ? '—' : `${Math.round(rate * 100)}%`}</span>
          </span>
          <span className={`sl-item${usage.last?.speed === 'fast' ? ' api-usage-fast' : ''}`} data-k="speed" title={t('上一次呼叫實際用的 speed（不是目前設定；speed 設定改了要下一回合才生效）')}>
            <span className="sl-k">{t('上一次 speed')}</span>
            <span className="sl-v">{usage.last?.speed ?? '—'}</span>
          </span>
          {usage.last?.effort ? (
            <span
              className={`sl-item${usage.last.auto_lowered_effort ? ' warn' : ''}`}
              data-k="last-effort"
              title={
                usage.last.auto_lowered_effort
                  ? t('回合花費超過門檻，已自動降一級 effort')
                  : t('上一次呼叫使用的 effort')
              }
            >
              <span className="sl-k">{t('上一次 effort')}</span>
              <span className="sl-v">
                {usage.last.effort}
                {usage.last.auto_lowered_effort ? ` (${t('降級')})` : ''}
              </span>
            </span>
          ) : null}
          <span className="sl-item" data-k="calls" title={t('已結算 {rec} 次、預留中 {res} 次、結果不明 {unk} 次、失敗 {fail} 次', { rec: usage.attempts.recorded, res: usage.attempts.reserved, unk: usage.attempts.unknown, fail: usage.attempts.failed })}>
            <span className="sl-k">{t('請求')}</span>
            <span className="sl-v">{usage.attempts.total}</span>
          </span>
        </>
      ) : null}
      {phone ? (
        <button
          type="button"
          className="sl-item api-usage-expand-btn"
          aria-label={mobileExpanded ? t('收合用量') : t('展開用量')}
          title={mobileExpanded ? t('收合用量') : t('展開用量')}
          onClick={() => setMobileExpanded((v) => !v)}
        >
          {mobileExpanded ? '▴' : '▾'}
        </button>
      ) : null}
    </span>
  )
}
