import { useEffect, useMemo, useState } from 'react'
import { fetchClaudeApiCap, putClaudeApiCap, putClaudeApiTurnLimits, type ClaudeApiCap } from '../api/claudeApiCap'
import { apiBotConfirmedUsd, fetchApiBotUsage, type ApiBotUsage } from '../api/apiBotUsage'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import './claudeApiBotLimits.css'

export interface ClaudeApiBotLimitsProps {
  /** 若指定 botId，顯示此 bot 的累計用量統計（用於 BotSettingsPanel） */
  botId?: string
  /** 點擊 bot 連結時的額外回呼（例如關閉 Settings Modal） */
  onSelectBot?: (botId: string) => void
}

/**
 * claude-api bot 的總花費上限與每回合時限／呼叫次數設定面板。
 * 同時共用於：
 * 1. 環境設定（Settings → Claude API）：設定全域上限與時限，並列出專案中的 claude-api bot 連結。
 * 2. Bot 設定（BotSettingsPanel）：讓手機與桌機在 bot 設定面板中直接檢視與調整總上限與每回合時限，並查看該 bot 用量。
 */
export function ClaudeApiBotLimits({ botId, onSelectBot }: ClaudeApiBotLimitsProps) {
  const t = useT()
  const [cap, setCap] = useState<ClaudeApiCap | null>(null)
  const [usage, setUsage] = useState<ApiBotUsage | null>(null)
  const [editingCap, setEditingCap] = useState(false)
  const [newCapVal, setNewCapVal] = useState('')
  const [savingCap, setSavingCap] = useState(false)
  const [capError, setCapError] = useState('')

  const [editingLimits, setEditingLimits] = useState(false)
  const [newTimeout, setNewTimeout] = useState('')
  const [newCalls, setNewCalls] = useState('')
  const [savingLimits, setSavingLimits] = useState(false)
  const [limitsError, setLimitsError] = useState('')

  const selectBot = useStore((s) => s.selectBot)
  const bots = useStore((s) => s.bots)
  const allApiBots = useMemo(() => bots.filter((b) => b.kind === 'claude-api'), [bots])

  const loadCap = () => {
    fetchClaudeApiCap().then(setCap).catch(() => setCap(null))
  }

  useEffect(() => {
    loadCap()
    if (botId) {
      fetchApiBotUsage(botId).then(setUsage).catch(() => setUsage(null))
    }
  }, [botId])

  const handleStartEditCap = () => {
    setNewCapVal(cap?.cap_usd !== null && cap?.cap_usd !== undefined ? String(cap.cap_usd) : '')
    setCapError('')
    setEditingCap(true)
  }

  const handleSaveCap = async (e: React.FormEvent) => {
    e.preventDefault()
    const val = parseFloat(newCapVal)
    if (Number.isNaN(val) || val < 0) {
      setCapError(t('上限金額必須是大於等於 0 的數字'))
      return
    }
    setSavingCap(true)
    setCapError('')
    try {
      const updated = await putClaudeApiCap(val)
      setCap(updated)
      setEditingCap(false)
    } catch (err) {
      setCapError(err instanceof Error ? err.message : String(err))
    } finally {
      setSavingCap(false)
    }
  }

  const handleClearCap = async () => {
    setSavingCap(true)
    setCapError('')
    try {
      const updated = await putClaudeApiCap(null)
      setCap(updated)
      setEditingCap(false)
    } catch (err) {
      setCapError(err instanceof Error ? err.message : String(err))
    } finally {
      setSavingCap(false)
    }
  }

  const handleStartEditLimits = () => {
    setNewTimeout(String(cap?.turn_timeout_secs ?? '1800'))
    setNewCalls(String(cap?.max_api_calls_per_turn ?? '20'))
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
    setSavingLimits(true)
    setLimitsError('')
    try {
      const updated = await putClaudeApiTurnLimits({ turn_timeout_secs: secs, max_api_calls_per_turn: calls })
      setCap(updated)
      setEditingLimits(false)
    } catch (err) {
      setLimitsError(err instanceof Error ? err.message : String(err))
    } finally {
      setSavingLimits(false)
    }
  }

  const isCapExceededOrClose =
    cap?.cap_usd !== null &&
    cap?.cap_usd !== undefined &&
    cap.cap_usd > 0 &&
    cap.spent_usd >= cap.cap_usd * 0.9

  return (
    <fieldset className="claude-api-bot-limits" data-testid="claude-api-bot-limits">
      <legend className="claude-api-bot-limits-title">{t('Claude API Bot（用量與上限）')}</legend>
      <p className="claude-api-bot-limits-desc">
        {t('所有 claude-api bot 共用的總花費上限與每回合設定（時間與 API 呼叫次數）。亦可在各 bot 的狀態列或設定面板修改。')}
      </p>

      {/* 若是在單一 bot 設定面板，顯示該 bot 的統計 */}
      {botId && usage ? (
        <div className="claude-api-bot-usage-stats" data-testid="bot-usage-stats">
          <div className="stat-item">
            <span className="stat-k">{t('本 Bot 累計花費：')}</span>
            <span className="stat-v" title={t('已結算的估算花費；預留與結果不明的另外顯示在狀態列')}>${apiBotConfirmedUsd(usage).toFixed(2)}</span>
          </div>
          <div className="stat-item">
            <span className="stat-k">{t('快取命中率：')}</span>
            <span className="stat-v">{usage.cache_hit_rate === null ? '—' : `${Math.round(usage.cache_hit_rate * 100)}%`}</span>
          </div>
          <div className="stat-item">
            <span className="stat-k">{t('請求次數：')}</span>
            <span className="stat-v">
              {t('共 {total} 次（已結算 {rec} 次、預留中 {res} 次、失敗 {fail} 次）', { total: usage.attempts.total, rec: usage.attempts.recorded, res: usage.attempts.reserved, fail: usage.attempts.failed })}
            </span>
          </div>
        </div>
      ) : null}

      {/* 總花費上限 */}
      <div className="claude-api-bot-limits-section">
        {editingCap ? (
          <form className="claude-api-bot-limits-form" data-testid="cap-edit-form" onSubmit={(e) => void handleSaveCap(e)}>
            <label htmlFor="bot-cap-input">{t('總花費上限 $')}</label>
            <input
              id="bot-cap-input"
              type="number"
              step="0.01"
              min="0"
              autoFocus
              value={newCapVal}
              disabled={savingCap}
              onChange={(e) => setNewCapVal(e.target.value)}
              placeholder="0.00"
            />
            <button className="claude-api-bot-limits-btn" type="submit" disabled={savingCap}>
              {t('儲存')}
            </button>
            {cap?.cap_usd !== null && cap?.cap_usd !== undefined ? (
              <button
                className="claude-api-bot-limits-btn clear"
                type="button"
                disabled={savingCap}
                onClick={() => void handleClearCap()}
              >
                {t('清除上限')}
              </button>
            ) : null}
            <button
              className="claude-api-bot-limits-btn cancel"
              type="button"
              disabled={savingCap}
              onClick={() => setEditingCap(false)}
            >
              {t('取消')}
            </button>
            {capError ? <span className="warn" role="alert">{capError}</span> : null}
          </form>
        ) : (
          <div className={`claude-api-bot-limits-row${isCapExceededOrClose ? ' warn' : ''}`}>
            <span className="k">{t('總花費上限：')}</span>
            <span className="v">
              {cap
                ? cap.cap_usd !== null && cap.cap_usd !== undefined
                  ? t('從 {since} 起算已花 ${spent}，上限 ${cap}', {
                      since: cap.since ?? '—',
                      spent: cap.spent_usd.toFixed(2),
                      cap: cap.cap_usd.toFixed(2),
                    })
                  : t('已花 ${spent}，未設定上限', { spent: cap.spent_usd.toFixed(2) })
                : t('讀取中…')}
            </span>
            <button
              type="button"
              className="claude-api-bot-limits-btn"
              onClick={handleStartEditCap}
            >
              {cap?.cap_usd !== null && cap?.cap_usd !== undefined ? t('修改上限') : t('設定上限')}
            </button>
            {cap?.cap_usd !== null && cap?.cap_usd !== undefined ? (
              <button
                type="button"
                className="claude-api-bot-limits-btn clear"
                disabled={savingCap}
                onClick={() => void handleClearCap()}
              >
                {t('清除上限')}
              </button>
            ) : null}
          </div>
        )}
      </div>

      {/* 每回合上限 */}
      <div className="claude-api-bot-limits-section">
        {editingLimits ? (
          <form className="claude-api-bot-limits-form" data-testid="turn-limits-form" onSubmit={(e) => void handleSaveLimits(e)}>
            <label htmlFor="bot-turn-timeout-input">{t('回合時間（秒，1–3600）')}</label>
            <input
              id="bot-turn-timeout-input"
              type="number"
              min="1"
              max="3600"
              step="1"
              autoFocus
              value={newTimeout}
              disabled={savingLimits}
              onChange={(e) => setNewTimeout(e.target.value)}
            />
            <label htmlFor="bot-turn-calls-input">{t('呼叫次數（1–64）')}</label>
            <input
              id="bot-turn-calls-input"
              type="number"
              min="1"
              max="64"
              step="1"
              value={newCalls}
              disabled={savingLimits}
              onChange={(e) => setNewCalls(e.target.value)}
            />
            <button className="claude-api-bot-limits-btn" type="submit" disabled={savingLimits}>
              {t('儲存')}
            </button>
            <button
              className="claude-api-bot-limits-btn cancel"
              type="button"
              disabled={savingLimits}
              onClick={() => setEditingLimits(false)}
            >
              {t('取消')}
            </button>
            {limitsError ? <span className="warn" role="alert">{limitsError}</span> : null}
          </form>
        ) : (
          <div className="claude-api-bot-limits-row">
            <span className="k">{t('每回合上限：')}</span>
            <span className="v">
              {t('每回合時間：{secs} 秒（1–3600） · API 呼叫：{calls} 次（1–64）', {
                secs: cap?.turn_timeout_secs ?? 1800,
                calls: cap?.max_api_calls_per_turn ?? 20,
              })}
            </span>
            <button
              type="button"
              className="claude-api-bot-limits-btn"
              onClick={handleStartEditLimits}
            >
              {t('修改每回合上限')}
            </button>
          </div>
        )}
        <p className="claude-api-bot-limits-desc">
          {t('下一回合起生效；時間到會先請模型收尾寫摘要，不是直接中止。')}
        </p>
      </div>

      {/* 若在全域環境設定中，列出專案中的 claude-api bot 連結 */}
      {!botId ? (
        <div className="claude-api-bot-links-section">
          {allApiBots.length > 0 ? (
            <div className="claude-api-bot-links">
              <span className="k">{t('專案中的 claude-api bot：')}</span>
              {allApiBots.map((b) => (
                <button
                  key={b.id}
                  type="button"
                  className="bot-link-btn"
                  onClick={() => {
                    selectBot(b.id)
                    onSelectBot?.(b.id)
                  }}
                >
                  {b.name}
                </button>
              ))}
            </div>
          ) : (
            <p className="claude-api-bot-limits-desc">
              {t('目前專案尚未建立 claude-api bot；可在側欄「新增 Bot」選擇「claude-api」建立。')}
            </p>
          )}
        </div>
      ) : null}
    </fieldset>
  )
}
