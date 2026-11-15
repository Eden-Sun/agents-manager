/**
 * claude-api bot 回合時間軸與花費拆分面板（可觀測性）。
 *
 * 版面配置：
 * 1. 標題與結論（一句話結論例如「這回合 $5.01，82% 是快取冷掉重寫」，旁邊若有可省建議則並列顯示，無則不顯示框）。
 * 2. 堆疊條（4-5 色語意色票）。
 * 3. 花費拆分對齊表格（類別｜Token/次數｜金額｜占比，右對齊等寬數字，0 元類別排在最後淡化）。
 * 4. 可折疊的執行時間軸（每步一列，時間與花費垂直右對齊）。
 */
import { useEffect, useMemo, useState } from 'react'
import { fetchApiTurnTimeline, type ApiTurnTimelineData } from '../api/apiBotUsage'
import { useT } from '../lib/i18n'
import './apiTurnTimeline.css'

interface ApiTurnTimelineProps {
  botId: string
  turnId: string
  initialData?: ApiTurnTimelineData
  onClose?: () => void
}

interface CategoryRow {
  key: string
  label: string
  tokens: number
  unit: string
  cost_usd: number
  percentage: number
  colorClass: string
}

export function ApiTurnTimeline({ botId, turnId, initialData, onClose }: ApiTurnTimelineProps) {
  const t = useT()
  const [data, setData] = useState<ApiTurnTimelineData | null>(initialData ?? null)
  const [loading, setLoading] = useState(!initialData)
  const [error, setError] = useState<string | null>(null)
  const [timelineOpen, setTimelineOpen] = useState(true)

  useEffect(() => {
    if (initialData) {
      setData(initialData)
      setLoading(false)
      return
    }
    let active = true
    setLoading(true)
    setError(null)
    fetchApiTurnTimeline(botId, turnId)
      .then((d) => {
        if (active) {
          setData(d)
          setLoading(false)
        }
      })
      .catch((err) => {
        if (active) {
          setError(err?.message || '載入失敗')
          setLoading(false)
        }
      })
    return () => {
      active = false
    }
  }, [botId, turnId, initialData])

  const formatCost = (usd: number) => `$${usd.toFixed(4)}`
  const formatMs = (ms: number | null | undefined) => (ms != null ? (ms >= 1000 ? `${(ms / 1000).toFixed(2)}s` : `${ms}ms`) : '—')

  // 將 6 大類別轉為陣列，並將有花費的排在前面、0 元的淡列在最後
  const categories = useMemo((): CategoryRow[] => {
    if (!data) return []
    const { breakdown } = data.summary
    const raw: CategoryRow[] = [
      {
        key: 'cache_write',
        label: t('快取寫入'),
        tokens: breakdown.cache_write.tokens,
        unit: 'tok',
        cost_usd: breakdown.cache_write.cost_usd,
        percentage: breakdown.cache_write.percentage,
        colorClass: 'cat-cache-write',
      },
      {
        key: 'output',
        label: t('輸出 (含思考)'),
        tokens: breakdown.output.tokens,
        unit: 'tok',
        cost_usd: breakdown.output.cost_usd,
        percentage: breakdown.output.percentage,
        colorClass: 'cat-output',
      },
      {
        key: 'cache_read',
        label: t('快取讀取'),
        tokens: breakdown.cache_read.tokens,
        unit: 'tok',
        cost_usd: breakdown.cache_read.cost_usd,
        percentage: breakdown.cache_read.percentage,
        colorClass: 'cat-cache-read',
      },
      {
        key: 'input',
        label: t('一般輸入'),
        tokens: breakdown.input.tokens,
        unit: 'tok',
        cost_usd: breakdown.input.cost_usd,
        percentage: breakdown.input.percentage,
        colorClass: 'cat-input',
      },
      {
        key: 'compaction',
        label: t('對話壓縮'),
        tokens: breakdown.compaction.tokens,
        unit: 'tok',
        cost_usd: breakdown.compaction.cost_usd,
        percentage: breakdown.compaction.percentage,
        colorClass: 'cat-compaction',
      },
      {
        key: 'server_tool',
        label: t('伺服器工具'),
        tokens: breakdown.server_tool.tokens,
        unit: t('次'),
        cost_usd: breakdown.server_tool.cost_usd,
        percentage: breakdown.server_tool.percentage,
        colorClass: 'cat-server-tool',
      },
    ]

    // 排序：金額大者在前；若金額為 0，淡列在最後
    return raw.sort((a, b) => {
      if (a.cost_usd > 0 && b.cost_usd === 0) return -1
      if (a.cost_usd === 0 && b.cost_usd > 0) return 1
      return b.cost_usd - a.cost_usd
    })
  }, [data, t])

  if (loading) {
    return (
      <div className="api-timeline-panel loading">
        <span className="spinner-sm" /> {t('載入時間軸與花費拆分...')}
      </div>
    )
  }

  if (error || !data) {
    return (
      <div className="api-timeline-panel error">
        <span>{t('無法載入時間軸：{err}', { err: error ?? '未知錯誤' })}</span>
        {onClose ? (
          <button type="button" className="api-timeline-close-btn" onClick={onClose}>
            {t('關閉')}
          </button>
        ) : null}
      </div>
    )
  }

  const { summary, timeline, total_cost_usd, duration_ms, halt_reason, status } = data
  const { largest_item, suggestions } = summary

  const hasSuggestions = Boolean(suggestions && suggestions.length > 0)
  const hasLargest = Boolean(largest_item && largest_item.cost_usd > 0)

  const conclusionSentence = hasLargest
    ? t('這回合 {cost}，{pct}% 是{label}', {
        cost: formatCost(total_cost_usd),
        pct: largest_item.percentage.toFixed(1),
        label: largest_item.label,
      })
    : t('這回合花費 {cost}', { cost: formatCost(total_cost_usd) })

  return (
    <div className="api-timeline-panel" data-testid="api-turn-timeline">
      {/* 頂部標題列 */}
      <div className="api-timeline-header">
        <div className="api-timeline-title-row">
          <span className="api-timeline-title">
            {t('這回合為什麼花了 {cost}', { cost: `$${total_cost_usd.toFixed(4)}` })}
          </span>
          {duration_ms != null ? (
            <span className="api-timeline-duration mono">
              ⏱ {formatMs(duration_ms)}
            </span>
          ) : null}
          <span className={`api-timeline-badge status-${status}`}>
            {status}
          </span>
        </div>
        {onClose ? (
          <button
            type="button"
            className="api-timeline-close-btn"
            onClick={onClose}
            aria-label={t('關閉時間軸')}
            title={t('關閉')}
          >
            ✕
          </button>
        ) : null}
      </div>

      {/* 結論與建議區塊（結論先行、建議在側；沒有建議不顯示框） */}
      <div className="api-conclusion-card">
        <div className="api-conclusion-main">
          <span className="api-conclusion-badge">{t('結論')}</span>
          <span className="api-conclusion-text">{conclusionSentence}</span>
          {hasLargest ? (
            <span className="api-largest-chip">
              {t('最大項：{label}（佔 {pct}% · {cost}）', {
                label: largest_item.label,
                pct: largest_item.percentage.toFixed(1),
                cost: formatCost(largest_item.cost_usd),
              })}
            </span>
          ) : null}
        </div>

        {hasSuggestions ? (
          <div className="api-conclusion-suggestions" role="note" aria-label={t('可省建議')}>
            <span className="suggestions-icon" aria-hidden="true">💡</span>
            <div className="suggestions-body">
              {suggestions.map((s, idx) => (
                <div key={idx} className="suggestion-item">
                  {s}
                </div>
              ))}
            </div>
          </div>
        ) : null}
      </div>

      {/* 比例堆疊長條 */}
      <div className="api-cost-bar-wrap" aria-label={t('費用比例分布')}>
        <div className="api-cost-bar">
          {categories
            .filter((cat) => cat.percentage > 0)
            .map((cat) => (
              <div
                key={cat.key}
                className={`cost-bar-seg seg-${cat.key}`}
                style={{ width: `${cat.percentage}%` }}
                title={`${cat.label}: ${cat.percentage.toFixed(1)}% (${formatCost(cat.cost_usd)})`}
              />
            ))}
        </div>
      </div>

      {/* 費用拆分對齊表格 */}
      <section className="api-breakdown-section" aria-label={t('花費拆分')}>
        <table className="api-breakdown-table">
          <thead>
            <tr>
              <th className="th-label">{t('類別')}</th>
              <th className="th-tokens mono text-right">{t('用量')}</th>
              <th className="th-cost mono text-right">{t('金額')}</th>
              <th className="th-pct mono text-right">{t('占比')}</th>
            </tr>
          </thead>
          <tbody>
            {categories.map((cat) => {
              const isZero = cat.cost_usd === 0 && cat.tokens === 0
              return (
                <tr key={cat.key} className={`breakdown-row ${cat.colorClass}${isZero ? ' is-zero' : ''}`}>
                  <td className="td-label">
                    <span className={`cat-dot dot-${cat.key}`} />
                    <span className="card-label">{cat.label}</span>
                  </td>
                  <td className="td-tokens mono text-right">
                    {cat.tokens.toLocaleString()} {cat.unit}
                  </td>
                  <td className="td-cost mono text-right">
                    {formatCost(cat.cost_usd)}
                  </td>
                  <td className="td-pct mono text-right">
                    {cat.percentage.toFixed(1)}%
                  </td>
                </tr>
              )
            })}
          </tbody>
        </table>
      </section>

      {/* 可折疊的回合執行時間軸 */}
      <section className="api-timeline-section" aria-label={t('回合時間軸')}>
        <div className="timeline-header-bar">
          <button
            type="button"
            className="timeline-toggle-btn"
            onClick={() => setTimelineOpen((v) => !v)}
            aria-expanded={timelineOpen}
            title={timelineOpen ? t('收合時間軸') : t('展開時間軸')}
          >
            <span className="timeline-chev" aria-hidden="true">{timelineOpen ? '▼' : '▶'}</span>
            <span className="timeline-heading">⏳ {t('回合執行時間軸')}</span>
            <span className="timeline-count">{t('共 {n} 個步驟', { n: timeline.length })}</span>
          </button>
        </div>

        {timelineOpen ? (
          <div className="timeline-list">
            {timeline.map((item, idx) => {
              if (item.type === 'request') {
                const isComp = item.iteration_type === 'compaction'
                return (
                  <div key={idx} className={`timeline-row timeline-request${isComp ? ' is-compaction' : ''}`}>
                    <div className="timeline-line-marker">
                      <span className="marker-dot req-dot" />
                    </div>
                    <div className="timeline-card">
                      <div className="timeline-card-content">
                        <div className="step-ident">
                          <span className="req-num mono">#{item.ordinal + 1} {t('API 請求')}</span>
                          {isComp ? <span className="type-chip chip-comp">{t('壓縮')}</span> : <span className="type-chip chip-msg">{t('訊息')}</span>}
                          {item.speed === 'fast' ? <span className="type-chip chip-fast">fast 2×</span> : null}
                        </div>
                        <div className="card-tokens-summary mono">
                          <span>{t('輸入')} {item.input_tokens.toLocaleString()}</span>
                          <span>{t('輸出')} {item.output_tokens.toLocaleString()}</span>
                          <span>{t('快取讀')} {item.cache_read_tokens.toLocaleString()}</span>
                          <span>{t('快取寫')} {item.cache_write_tokens.toLocaleString()}</span>
                          {item.stop_reason ? <span className="stop-reason">[{item.stop_reason}]</span> : null}
                        </div>
                        <div className="step-metrics mono">
                          <span className="step-dur">{formatMs(item.duration_ms)}</span>
                          <span className="step-cost">{formatCost(item.cost_usd)}</span>
                        </div>
                      </div>
                    </div>
                  </div>
                )
              }

              if (item.type === 'tool') {
                return (
                  <div key={idx} className="timeline-row timeline-tool">
                    <div className="timeline-line-marker">
                      <span className={`marker-dot tool-dot${item.is_error ? ' is-err' : ''}`} />
                    </div>
                    <div className="timeline-card tool-card">
                      <div className="timeline-card-content">
                        <div className="step-ident">
                          <span className="tool-glyph">🔧</span>
                          <span className="tool-name mono">{item.name}</span>
                          <span className={`tool-status-chip ${item.is_error ? 'chip-err' : 'chip-ok'}`}>
                            {item.is_error ? t('執行失敗') : t('成功')}
                          </span>
                        </div>
                        <div className="step-metrics mono">
                          <span className="step-dur">{formatMs(item.duration_ms)}</span>
                        </div>
                      </div>
                    </div>
                  </div>
                )
              }

              if (item.type === 'turn_end') {
                return (
                  <div key={idx} className="timeline-row timeline-end">
                    <div className="timeline-line-marker">
                      <span className="marker-dot end-dot" />
                    </div>
                    <div className="timeline-card end-card">
                      <div className="timeline-card-content">
                        <div className="step-ident">
                          <span className="end-glyph">🏁</span>
                          <span className="end-label">{t('回合結束')}</span>
                          <span className="end-reason mono">{t('原因：{reason}', { reason: item.halt_reason || halt_reason })}</span>
                        </div>
                        <div className="step-metrics">
                          <span className={`end-status-chip status-${item.status}`}>
                            {item.status}
                          </span>
                        </div>
                      </div>
                    </div>
                  </div>
                )
              }

              return null
            })}
          </div>
        ) : null}
      </section>
    </div>
  )
}
