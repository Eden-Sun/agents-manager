/**
 * claude-api bot 回合時間軸與「這回合為什麼花了 $X」拆分（可觀測性）。
 * 支援展開/收合、手機版響應式排版。
 */
import { useEffect, useState } from 'react'
import { fetchApiTurnTimeline, type ApiTurnTimelineData } from '../api/apiBotUsage'
import { useT } from '../lib/i18n'
import './apiTurnTimeline.css'

interface ApiTurnTimelineProps {
  botId: string
  turnId: string
  initialData?: ApiTurnTimelineData
  onClose?: () => void
}

export function ApiTurnTimeline({ botId, turnId, initialData, onClose }: ApiTurnTimelineProps) {
  const t = useT()
  const [data, setData] = useState<ApiTurnTimelineData | null>(initialData ?? null)
  const [loading, setLoading] = useState(!initialData)
  const [error, setError] = useState<string | null>(null)

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
  const { breakdown, largest_item, suggestions } = summary

  const formatCost = (usd: number) => `$${usd.toFixed(4)}`
  const formatMs = (ms: number | null | undefined) => (ms != null ? (ms >= 1000 ? `${(ms / 1000).toFixed(2)}s` : `${ms}ms`) : '—')

  return (
    <div className="api-timeline-panel" data-testid="api-turn-timeline">
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

      {/* 拆分分析區塊 */}
      <section className="api-breakdown-section" aria-label={t('花費拆分')}>
        {largest_item && largest_item.cost_usd > 0 ? (
          <div className="api-largest-item-badge">
            <span className="api-largest-glyph">👑</span>
            <span className="api-largest-text">
              {t('最大項：{label}（佔 {pct}% · {cost}）', {
                label: largest_item.label,
                pct: largest_item.percentage.toFixed(1),
                cost: formatCost(largest_item.cost_usd),
              })}
            </span>
          </div>
        ) : null}

        {/* 比例進度條 */}
        <div className="api-cost-bar" title={t('費用比例分布')}>
          {breakdown.cache_write.percentage > 0 ? (
            <div
              className="cost-bar-seg seg-write"
              style={{ width: `${breakdown.cache_write.percentage}%` }}
              title={`快取寫入: ${breakdown.cache_write.percentage.toFixed(1)}%`}
            />
          ) : null}
          {breakdown.input.percentage > 0 ? (
            <div
              className="cost-bar-seg seg-input"
              style={{ width: `${breakdown.input.percentage}%` }}
              title={`一般輸入: ${breakdown.input.percentage.toFixed(1)}%`}
            />
          ) : null}
          {breakdown.output.percentage > 0 ? (
            <div
              className="cost-bar-seg seg-output"
              style={{ width: `${breakdown.output.percentage}%` }}
              title={`輸出: ${breakdown.output.percentage.toFixed(1)}%`}
            />
          ) : null}
          {breakdown.cache_read.percentage > 0 ? (
            <div
              className="cost-bar-seg seg-read"
              style={{ width: `${breakdown.cache_read.percentage}%` }}
              title={`快取讀取: ${breakdown.cache_read.percentage.toFixed(1)}%`}
            />
          ) : null}
          {breakdown.compaction.percentage > 0 ? (
            <div
              className="cost-bar-seg seg-compaction"
              style={{ width: `${breakdown.compaction.percentage}%` }}
              title={`對話壓縮: ${breakdown.compaction.percentage.toFixed(1)}%`}
            />
          ) : null}
        </div>

        {/* 6 大類別明細卡片 */}
        <div className="api-breakdown-grid">
          <div className="breakdown-card">
            <span className="cat-dot dot-write" />
            <div className="card-main">
              <span className="card-label">{t('快取寫入')}</span>
              <span className="card-tokens mono">{breakdown.cache_write.tokens.toLocaleString()} tok</span>
            </div>
            <div className="card-cost mono">
              <span>{formatCost(breakdown.cache_write.cost_usd)}</span>
              <span className="card-pct">{breakdown.cache_write.percentage.toFixed(1)}%</span>
            </div>
          </div>

          <div className="breakdown-card">
            <span className="cat-dot dot-input" />
            <div className="card-main">
              <span className="card-label">{t('一般輸入')}</span>
              <span className="card-tokens mono">{breakdown.input.tokens.toLocaleString()} tok</span>
            </div>
            <div className="card-cost mono">
              <span>{formatCost(breakdown.input.cost_usd)}</span>
              <span className="card-pct">{breakdown.input.percentage.toFixed(1)}%</span>
            </div>
          </div>

          <div className="breakdown-card">
            <span className="cat-dot dot-output" />
            <div className="card-main">
              <span className="card-label">{t('輸出 (含思考)')}</span>
              <span className="card-tokens mono">{breakdown.output.tokens.toLocaleString()} tok</span>
            </div>
            <div className="card-cost mono">
              <span>{formatCost(breakdown.output.cost_usd)}</span>
              <span className="card-pct">{breakdown.output.percentage.toFixed(1)}%</span>
            </div>
          </div>

          <div className="breakdown-card">
            <span className="cat-dot dot-read" />
            <div className="card-main">
              <span className="card-label">{t('快取讀取')}</span>
              <span className="card-tokens mono">{breakdown.cache_read.tokens.toLocaleString()} tok</span>
            </div>
            <div className="card-cost mono">
              <span>{formatCost(breakdown.cache_read.cost_usd)}</span>
              <span className="card-pct">{breakdown.cache_read.percentage.toFixed(1)}%</span>
            </div>
          </div>

          <div className="breakdown-card">
            <span className="cat-dot dot-compaction" />
            <div className="card-main">
              <span className="card-label">{t('對話壓縮')}</span>
              <span className="card-tokens mono">{breakdown.compaction.tokens.toLocaleString()} tok</span>
            </div>
            <div className="card-cost mono">
              <span>{formatCost(breakdown.compaction.cost_usd)}</span>
              <span className="card-pct">{breakdown.compaction.percentage.toFixed(1)}%</span>
            </div>
          </div>

          <div className="breakdown-card">
            <span className="cat-dot dot-server-tool" />
            <div className="card-main">
              <span className="card-label">{t('伺服器工具')}</span>
              <span className="card-tokens mono">{breakdown.server_tool.tokens.toLocaleString()} 次</span>
            </div>
            <div className="card-cost mono">
              <span>{formatCost(breakdown.server_tool.cost_usd)}</span>
              <span className="card-pct">{breakdown.server_tool.percentage.toFixed(1)}%</span>
            </div>
          </div>
        </div>

        {/* 可省建議 (Suggestions) */}
        {suggestions && suggestions.length > 0 ? (
          <div className="api-suggestions-box">
            <div className="suggestions-title">💡 {t('可省建議')}</div>
            <ul className="suggestions-list">
              {suggestions.map((s, idx) => (
                <li key={idx} className="suggestion-item">
                  {s}
                </li>
              ))}
            </ul>
          </div>
        ) : null}
      </section>

      {/* 時間軸列表 (Timeline) */}
      <section className="api-timeline-section" aria-label={t('回合時間軸')}>
        <div className="timeline-title-bar">
          <span className="timeline-heading">⏳ {t('回合執行時間軸')}</span>
          <span className="timeline-count">{t('共 {n} 個步驟', { n: timeline.length })}</span>
        </div>

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
                    <div className="card-header">
                      <span className="req-num mono">#{item.ordinal + 1} {t('API 請求')}</span>
                      {isComp ? <span className="type-chip chip-comp">{t('壓縮')}</span> : <span className="type-chip chip-msg">{t('訊息')}</span>}
                      {item.speed === 'fast' ? <span className="type-chip chip-fast">fast 2×</span> : null}
                      <span className="req-cost mono">{formatCost(item.cost_usd)}</span>
                      <span className="req-dur mono">{formatMs(item.duration_ms)}</span>
                    </div>
                    <div className="card-tokens-summary mono">
                      <span>{t('輸入')} {item.input_tokens.toLocaleString()}</span>
                      <span>{t('輸出')} {item.output_tokens.toLocaleString()}</span>
                      <span>{t('快取讀')} {item.cache_read_tokens.toLocaleString()}</span>
                      <span>{t('快取寫')} {item.cache_write_tokens.toLocaleString()}</span>
                      {item.stop_reason ? <span className="stop-reason">[{item.stop_reason}]</span> : null}
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
                    <div className="card-header">
                      <span className="tool-glyph">🔧</span>
                      <span className="tool-name mono">{item.name}</span>
                      <span className={`tool-status-chip ${item.is_error ? 'chip-err' : 'chip-ok'}`}>
                        {item.is_error ? t('執行失敗') : t('成功')}
                      </span>
                      <span className="tool-dur mono">{formatMs(item.duration_ms)}</span>
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
                    <div className="card-header">
                      <span className="end-glyph">🏁</span>
                      <span className="end-label">{t('回合結束')}</span>
                      <span className="end-reason mono">{t('原因：{reason}', { reason: item.halt_reason || halt_reason })}</span>
                      <span className={`end-status-chip status-${item.status}`}>
                        {item.status}
                      </span>
                    </div>
                  </div>
                </div>
              )
            }

            return null
          })}
        </div>
      </section>
    </div>
  )
}
