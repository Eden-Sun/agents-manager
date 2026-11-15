/**
 * claude-api bot 整體花費概覽面板（ApiBotCostOverview）。
 *
 * 需求：
 * 1. 今日花費 / session 總花費。
 * 2. 結論先行（一句話結論 ＋ 可省建議並列；無建議不留空框）。
 * 3. 堆疊占比條（語意色票：快取寫入＝黃橙、快取讀＝綠、輸入＝藍、輸出＝紫，其餘灰）。
 * 4. 4 欄對齊表格（類別 ｜ 用量 ｜ 金額 ｜ 占比，金額與占比等寬右對齊，0 元淡列在後）。
 * 5. 最貴回合清單（點擊可跳到該回合訊息）。
 * 6. 手機版單欄可讀，文字標準 token 高對比度。
 */

import { useEffect, useState, useMemo } from 'react'
import { fetchApiBotUsage, fetchApiTurnTimeline, formatTokensK, type ApiBotUsage } from '../api/apiBotUsage'
import { useT } from '../lib/i18n'
import './apiBotCostOverview.css'

export interface OverviewCategory {
  key: string
  label: string
  tokens: number
  cost_usd: number
  percentage: number
  colorVar: string
}

export interface OverviewTopTurn {
  turn_id: string
  started_at: string | null
  requests: number
  cost_usd: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens?: number
  cache_write_tokens?: number
}

export interface ApiBotCostOverviewData {
  bot_id: string
  today_cost_usd: number
  total_cost_usd: number
  conclusion: string
  suggestions: string[]
  categories: OverviewCategory[]
  top_turns: OverviewTopTurn[]
}

export interface ApiBotCostOverviewProps {
  botId: string
  initialData?: ApiBotCostOverviewData
  onClose?: () => void
  onJumpToTurn?: (turnId: string) => void
}

export function ApiBotCostOverview({
  botId,
  initialData,
  onClose,
  onJumpToTurn,
}: ApiBotCostOverviewProps) {
  const t = useT()
  const [data, setData] = useState<ApiBotCostOverviewData | null>(initialData ?? null)
  const [loading, setLoading] = useState<boolean>(!initialData)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (initialData) {
      setData(initialData)
      setLoading(false)
      return
    }

    let alive = true
    setLoading(true)
    setError(null)

    void (async () => {
      try {
        const usage: ApiBotUsage = await fetchApiBotUsage(botId)
        if (!alive) return

        const now = new Date()
        const todayLocal = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}-${String(now.getDate()).padStart(2, '0')}`
        const todayUtc = now.toISOString().slice(0, 10)
        const isToday = (ts: string | null) => {
          if (!ts) return false
          const p = ts.slice(0, 10)
          return p === todayLocal || p === todayUtc
        }

        const turns = usage.turns ?? []
        const todayTurns = turns.filter((x) => isToday(x.started_at))
        const todayCost = todayTurns.reduce((sum, x) => sum + (x.cost_usd || 0), 0)
        const totalCost = usage.cost_microusd.recorded > 0
          ? usage.cost_microusd.recorded / 1e6
          : turns.reduce((sum, x) => sum + (x.cost_usd || 0), 0)

        // 最貴的幾個回合（取前 5 名）
        const sortedTurns = [...turns]
          .filter((x) => !x.in_flight && (x.cost_usd > 0 || x.requests > 0))
          .sort((a, b) => b.cost_usd - a.cost_usd)
          .slice(0, 5)
          .map((x): OverviewTopTurn => ({
            turn_id: x.turn_id,
            started_at: x.started_at,
            requests: x.requests,
            cost_usd: x.cost_usd,
            input_tokens: x.input_tokens,
            output_tokens: x.output_tokens,
            cache_read_tokens: x.cache_read_tokens,
            cache_write_tokens: x.cache_write_tokens,
          }))

        // 若最貴回合存在，抓其 timeline 取得潛在 suggestions / compaction
        let topTimelineSuggestions: string[] = []
        if (sortedTurns.length > 0) {
          try {
            const tl = await fetchApiTurnTimeline(botId, sortedTurns[0].turn_id)
            if (tl?.summary?.suggestions) {
              topTimelineSuggestions = tl.summary.suggestions
            }
          } catch {
            // timeline 抓取失敗不阻礙主資料呈現
          }
        }

        // 分類拆分估算
        const inTokens = usage.tokens.input
        const writeTokens = usage.tokens.cache_creation
        const readTokens = usage.tokens.cache_read
        const outTokens = usage.tokens.output

        // Claude 定價加權比重
        const wIn = inTokens * 3.0
        const wWrite = writeTokens * 3.75
        const wRead = readTokens * 0.30
        const wOut = outTokens * 15.0
        const wTotal = wIn + wWrite + wRead + wOut

        const inCost = wTotal > 0 ? totalCost * (wIn / wTotal) : 0
        const writeCost = wTotal > 0 ? totalCost * (wWrite / wTotal) : 0
        const readCost = wTotal > 0 ? totalCost * (wRead / wTotal) : 0
        const outCost = wTotal > 0 ? totalCost * (wOut / wTotal) : 0

        const calcPct = (c: number) => (totalCost > 0 ? (c / totalCost) * 100 : 0)

        const categories: OverviewCategory[] = [
          {
            key: 'cache_write',
            label: t('快取寫入'),
            tokens: writeTokens,
            cost_usd: writeCost,
            percentage: calcPct(writeCost),
            colorVar: '#d97706',
          },
          {
            key: 'input',
            label: t('一般輸入'),
            tokens: inTokens,
            cost_usd: inCost,
            percentage: calcPct(inCost),
            colorVar: '#2563eb',
          },
          {
            key: 'output',
            label: t('輸出(含思考)'),
            tokens: outTokens,
            cost_usd: outCost,
            percentage: calcPct(outCost),
            colorVar: '#7c3aed',
          },
          {
            key: 'cache_read',
            label: t('快取讀取'),
            tokens: readTokens,
            cost_usd: readCost,
            percentage: calcPct(readCost),
            colorVar: '#16a34a',
          },
          {
            key: 'compaction',
            label: t('對話壓縮'),
            tokens: 0,
            cost_usd: 0,
            percentage: 0,
            colorVar: '#64748b',
          },
          {
            key: 'server_tool',
            label: t('伺服器工具'),
            tokens: 0,
            cost_usd: 0,
            percentage: 0,
            colorVar: '#64748b',
          },
        ]

        // 結論：找出最大項
        let largest = categories[0]
        for (const cat of categories) {
          if (cat.cost_usd > largest.cost_usd) {
            largest = cat
          }
        }

        let conclusionText = t('目前尚無花費記錄')
        if (totalCost > 0) {
          const pctInt = Math.round(largest.percentage)
          conclusionText = t('總花費 {cost}，{pct}% 是{category}', {
            cost: `$${totalCost.toFixed(2)}`,
            pct: pctInt,
            category: largest.label,
          })
        }

        // 建議
        const suggestions: string[] = []
        if (topTimelineSuggestions.length > 0) {
          suggestions.push(...topTimelineSuggestions)
        } else if (writeCost > 0 && largest.key === 'cache_write' && largest.percentage >= 30) {
          suggestions.push(
            t('快取冷掉重寫占了 {pct}%；若在快取保留期（5分鐘/1小時）內送出，快取讀取費用僅約 10%，可省下大部分快取開銷。', {
              pct: Math.round(largest.percentage),
            }),
          )
        }

        if (usage.last?.speed === 'fast' && totalCost > 0.01) {
          suggestions.push(t('目前使用 fast 模式（2× 費率），切換至 standard 模式可省下約 50% 費用。'))
        }

        setData({
          bot_id: botId,
          today_cost_usd: todayCost,
          total_cost_usd: totalCost,
          conclusion: conclusionText,
          suggestions,
          categories,
          top_turns: sortedTurns,
        })
      } catch (err) {
        if (!alive) return
        setError(err instanceof Error ? err.message : String(err))
      } finally {
        if (alive) setLoading(false)
      }
    })()

    return () => {
      alive = false
    }
  }, [botId, initialData, t])

  const sortedCategories = useMemo(() => {
    if (!data?.categories) return []
    const nonZero = data.categories.filter((c) => c.cost_usd > 0 || c.tokens > 0)
    const zero = data.categories.filter((c) => c.cost_usd === 0 && c.tokens === 0)
    nonZero.sort((a, b) => b.cost_usd - a.cost_usd)
    return [...nonZero, ...zero]
  }, [data?.categories])

  if (loading) {
    return (
      <section className="api-overview-panel loading" aria-busy="true">
        <span className="api-overview-loading-text">{t('讀取花費概覽中...')}</span>
      </section>
    )
  }

  if (error || !data) {
    return (
      <section className="api-overview-panel error" role="alert">
        <span className="api-overview-error-text">{t('載入花費概覽失敗')}: {error}</span>
        {onClose ? (
          <button type="button" className="api-overview-close" onClick={onClose} aria-label={t('關閉花費概覽')}>
            ✕
          </button>
        ) : null}
      </section>
    )
  }

  const { today_cost_usd, total_cost_usd, conclusion, suggestions, top_turns } = data

  return (
    <section className="api-overview-panel" aria-label={t('Claude API 花費概覽')}>
      {/* 頂部標題列 */}
      <div className="api-overview-header">
        <div className="api-overview-title-row">
          <span className="api-overview-title">{t('Claude API 花費概覽')}</span>
          <span className="api-overview-chip today" title={t('今日花費')}>
            <span className="chip-k">{t('今日花費')}</span>
            <span className="chip-v mono">${today_cost_usd.toFixed(2)}</span>
          </span>
          <span className="api-overview-chip total" title={t('總計')}>
            <span className="chip-k">{t('總計')}</span>
            <span className="chip-v mono">${total_cost_usd.toFixed(2)}</span>
          </span>
        </div>
        {onClose ? (
          <button
            type="button"
            className="api-overview-close"
            onClick={onClose}
            aria-label={t('關閉花費概覽')}
            title={t('關閉花費概覽')}
          >
            ✕
          </button>
        ) : null}
      </div>

      {/* 結論與建議（建議並列，無建議不顯示框） */}
      <div className="api-overview-takeaway-card">
        <div className="api-overview-conclusion">
          <span className="api-overview-conclusion-icon" aria-hidden="true">💡</span>
          <span className="api-overview-conclusion-text">{conclusion}</span>
        </div>
        {suggestions && suggestions.length > 0 ? (
          <div className="api-overview-suggestion" role="note">
            <span className="api-overview-suggestion-icon" aria-hidden="true">⚡</span>
            <span className="api-overview-suggestion-text">{suggestions[0]}</span>
          </div>
        ) : null}
      </div>

      {/* 堆疊百分比長條 */}
      <div
        className="api-overview-bar"
        role="progressbar"
        aria-label={t('花費分類占比')}
      >
        {sortedCategories
          .filter((c) => c.percentage > 0.5)
          .map((c) => (
            <div
              key={c.key}
              className={`api-overview-bar-seg seg-${c.key}`}
              style={{
                width: `${c.percentage}%`,
                backgroundColor: c.colorVar,
              }}
              title={`${c.label}: ${c.percentage.toFixed(1)}% ($${c.cost_usd.toFixed(4)})`}
            />
          ))}
      </div>

      {/* 對齊表格：類別 ｜ 用量 ｜ 金額 ｜ 占比 */}
      <div className="api-overview-table-wrap">
        <table className="api-overview-table">
          <thead>
            <tr>
              <th className="th-cat">{t('類別')}</th>
              <th className="th-num">{t('用量')}</th>
              <th className="th-num">{t('金額')}</th>
              <th className="th-num">{t('占比')}</th>
            </tr>
          </thead>
          <tbody>
            {sortedCategories.map((c) => {
              const isZero = c.cost_usd === 0 && c.tokens === 0
              return (
                <tr key={c.key} className={`tr-cat tr-${c.key}${isZero ? ' is-zero' : ''}`}>
                  <td className="td-cat">
                    <span
                      className="cat-bullet"
                      style={{ backgroundColor: c.colorVar }}
                      aria-hidden="true"
                    />
                    <span className="cat-label">{c.label}</span>
                  </td>
                  <td className="td-num mono">
                    {c.tokens > 0 ? formatTokensK(c.tokens) : '—'}
                  </td>
                  <td className="td-num mono">${c.cost_usd.toFixed(4)}</td>
                  <td className="td-num mono">
                    {c.percentage > 0 ? `${c.percentage.toFixed(1)}%` : '0%'}
                  </td>
                </tr>
              )
            })}
          </tbody>
        </table>
      </div>

      {/* 最貴回合清單（點擊可跳到該回合訊息） */}
      <div className="api-overview-top-turns">
        <div className="api-overview-turns-title">{t('最貴回合清單')}</div>
        {top_turns.length === 0 ? (
          <div className="api-overview-no-turns">{t('目前尚無已完成的回合記錄')}</div>
        ) : (
          <div className="api-overview-turns-list">
            {top_turns.map((turn, idx) => {
              const timeStr = turn.started_at ? formatTimeStr(turn.started_at) : '—'
              const totalTokens = (turn.input_tokens || 0) + (turn.output_tokens || 0)
              return (
                <div key={turn.turn_id} className="api-overview-turn-row" data-turn-id={turn.turn_id}>
                  <span className="turn-rank">#{idx + 1}</span>
                  <div className="turn-meta">
                    <span className="turn-time">{timeStr}</span>
                    <span className="turn-details">
                      {t('{n} 次請求 · {tokens} tokens', {
                        n: turn.requests,
                        tokens: formatTokensK(totalTokens),
                      })}
                    </span>
                  </div>
                  <span className="turn-cost mono">${turn.cost_usd.toFixed(4)}</span>
                  {onJumpToTurn ? (
                    <button
                      type="button"
                      className="api-overview-jump-btn"
                      onClick={() => onJumpToTurn(turn.turn_id)}
                      title={t('點擊跳至該回合訊息')}
                    >
                      {t('跳至該回合 →')}
                    </button>
                  ) : null}
                </div>
              )
            })}
          </div>
        )}
      </div>
    </section>
  )
}

function formatTimeStr(isoStr: string): string {
  try {
    const d = new Date(isoStr)
    if (Number.isNaN(d.getTime())) return isoStr
    const hh = String(d.getHours()).padStart(2, '0')
    const mm = String(d.getMinutes()).padStart(2, '0')
    const ss = String(d.getSeconds()).padStart(2, '0')
    return `${hh}:${mm}:${ss}`
  } catch {
    return isoStr
  }
}
