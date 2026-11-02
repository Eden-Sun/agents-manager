import { useEffect, useState } from 'react'
import { apiBotSpendUsd, fetchApiBotUsage, type ApiBotUsage } from '../api/apiBotUsage'
import { fetchClaudeApiBalance, type ClaudeApiBalance } from '../api/claudeApiBalance'
import { fetchClaudeApiCap, putClaudeApiCap, type ClaudeApiCap } from '../api/claudeApiCap'
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
  const [usage, setUsage] = useState<ApiBotUsage | null>(null)
  const [balance, setBalance] = useState<ClaudeApiBalance | null>(null)
  const [cap, setCap] = useState<ClaudeApiCap | null>(null)
  const [failed, setFailed] = useState(false)
  const [editingCap, setEditingCap] = useState(false)
  const [newCapVal, setNewCapVal] = useState('')
  const [savingCap, setSavingCap] = useState(false)

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

  let balanceTooltip = ''
  if (balance?.configured && balance.anchor) {
    const parts = [
      t('錨點：${usd}（{at}）', { usd: balance.anchor.usd.toFixed(2), at: balance.anchor.at }),
    ]
    if (balance.spent_usd !== undefined) {
      parts.push(t('錨點以來花費：${spent}', { spent: balance.spent_usd.toFixed(4) }))
    }
    if (balance.report_through) {
      parts.push(t('官方報表涵蓋至：{dt}', { dt: balance.report_through }))
    }
    if (balance.local_since) {
      parts.push(t('本機帳計算自：{dt}', { dt: balance.local_since }))
    }
    if (balance.error) {
      parts.push(t('警告：{err}', { err: balance.error }))
    }
    balanceTooltip = parts.join('\n')
  }

  const isCapExceededOrClose =
    cap?.cap_usd !== null &&
    cap?.cap_usd !== undefined &&
    cap.cap_usd > 0 &&
    cap.spent_usd >= cap.cap_usd * 0.9

  return (
    <span className="api-usage-bar" data-testid="api-usage-bar">
      {balance?.configured && balance.remaining_usd !== undefined ? (
        <span
          className="sl-item api-balance-chip readonly"
          data-k="balance"
          title={balanceTooltip}
        >
          <span className="sl-k">Balance</span>
          <span className="sl-v">≈ ${balance.remaining_usd.toFixed(2)}</span>
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

      <span className="sl-item" data-k="cost" title={t('累計花費＝已結算 ${rec}＋預留 ${res}＋結果不明 ${unk}（預留與不明先當花掉算）', { rec: (u.recorded / 1e6).toFixed(4), res: (u.reserved / 1e6).toFixed(2), unk: (u.unknown / 1e6).toFixed(2) })}>
        <span className="sl-k">{t('累計')}</span>
        <span className="sl-v">${spend.toFixed(2)}</span>
        {pending > 0 ? <span className="api-usage-hold">{t('含預留')}</span> : null}
      </span>
      <span className="sl-item" data-k="cache" title={t('快取命中率＝cache_read ÷ (input＋cache_read＋cache_creation)，只算已結算的回合')}>
        <span className="sl-k">{t('快取命中')}</span>
        <span className="sl-v">{rate === null ? '—' : `${Math.round(rate * 100)}%`}</span>
      </span>
      <span className={`sl-item${usage.last?.speed === 'fast' ? ' api-usage-fast' : ''}`} data-k="speed" title={t('最近一次回覆的 usage.speed')}>
        <span className="sl-k">speed</span>
        <span className="sl-v">{usage.last?.speed ?? '—'}</span>
      </span>
      <span className="sl-item" data-k="calls" title={t('已結算 {rec} 次、預留中 {res} 次、結果不明 {unk} 次、失敗 {fail} 次', { rec: usage.attempts.recorded, res: usage.attempts.reserved, unk: usage.attempts.unknown, fail: usage.attempts.failed })}>
        <span className="sl-k">{t('請求')}</span>
        <span className="sl-v">{usage.attempts.total}</span>
      </span>
    </span>
  )
}
