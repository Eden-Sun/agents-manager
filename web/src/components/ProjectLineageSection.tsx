import { useCallback, useEffect, useState } from 'react'
import { fetchLineageStats, type LineageLineStat } from '../api/lineage'
import { compactTokens } from '../lib/composerCost'
import { useT } from '../lib/i18n'
import { msgTimeLabel, msgTimeTitle } from '../lib/msgTime'
import './projectLineageSection.css'

export function ProjectLineageSection({ projectId }: { projectId: string }) {
  const t = useT()
  const [stats, setStats] = useState<LineageLineStat[] | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const reload = useCallback(() => {
    setLoading(true)
    setError(null)
    fetchLineageStats(projectId)
      .then((res) => {
        setStats(res.lines)
      })
      .catch((err) => {
        setError(err instanceof Error ? err.message : String(err))
      })
      .finally(() => {
        setLoading(false)
      })
  }, [projectId])

  useEffect(() => {
    reload()
  }, [reload])

  return (
    <section className="project-lineage-section" aria-label={t('Session 血統')}>
      <div className="project-lineage-header">
        <div className="project-lineage-title-row">
          <h3>{t('Session 血統與快取節省')}</h3>
          <button
            type="button"
            className="btn small project-lineage-refresh"
            onClick={reload}
            disabled={loading}
            aria-label={t('重新整理血統資料')}
          >
            {loading ? t('載入中…') : t('重新整理')}
          </button>
        </div>
        <p className="hint">
          {t('按模組切線的 codex 血統檢查點、進行中認領與快取節省統計。')}
        </p>
      </div>

      {error ? (
        <div className="project-lineage-error" role="alert">
          <span>{t('載入失敗：{error}', { error })}</span>
          <button type="button" className="btn small" onClick={reload}>
            {t('重試')}
          </button>
        </div>
      ) : null}

      {!stats && loading ? (
        <div className="project-lineage-loading">{t('載入中…')}</div>
      ) : null}

      {stats && stats.length === 0 ? (
        <div className="project-lineage-empty">
          <p>{t('目前尚無血統紀錄。由 codex 派工或 agm lineage 建立檢查點後會顯示於此。')}</p>
        </div>
      ) : null}

      {stats && stats.length > 0 ? (
        <div className="project-lineage-list" role="table" aria-label={t('血統線清單')}>
          <div className="project-lineage-head-row" role="row">
            <span className="col-line" role="columnheader">{t('線名')}</span>
            <span className="col-checkpoint" role="columnheader">{t('檢查點')}</span>
            <span className="col-sha" role="columnheader">{t('HEAD')}</span>
            <span className="col-updated" role="columnheader">{t('最後更新')}</span>
            <span className="col-claims" role="columnheader">{t('認領中')}</span>
            <span className="col-ratio" role="columnheader">{t('快取命中')}</span>
            <span className="col-saved" role="columnheader">{t('省下 Token')}</span>
          </div>
          {stats.map((item) => {
            const cp = item.checkpoint
            const sid = cp ? cp.session_id : ''
            const shortSid = sid ? (sid.length > 8 ? sid.slice(0, 8) : sid) : '—'
            const sha = cp?.head_sha ? cp.head_sha.slice(0, 7) : '—'
            const updated = cp?.updated_at ?? item.recent_claims[0]?.updated_at ?? null
            const hasTokens = item.tokens.input_tokens > 0
            const ratioPct = hasTokens ? `${(item.tokens.hit_ratio * 100).toFixed(1)}%` : '—'
            const savedStr = hasTokens ? compactTokens(item.tokens.saved_input_tokens) : '—'

            return (
              <div key={item.line} className="project-lineage-row" role="row" data-line={item.line}>
                <div className="col-line" role="cell">
                  <span className="lineage-line-badge">{item.line}</span>
                </div>
                <div className="col-checkpoint" role="cell">
                  {sid ? (
                    <code className="lineage-sid" title={sid}>
                      {shortSid}
                    </code>
                  ) : (
                    <span className="lineage-empty-val">—</span>
                  )}
                </div>
                <div className="col-sha" role="cell">
                  {cp?.head_sha ? (
                    <code className="lineage-sha" title={cp.head_sha}>
                      {sha}
                    </code>
                  ) : (
                    <span className="lineage-empty-val">—</span>
                  )}
                </div>
                <div className="col-updated" role="cell">
                  {updated ? (
                    <time dateTime={updated} title={msgTimeTitle(updated)}>
                      {msgTimeLabel(updated)}
                    </time>
                  ) : (
                    <span className="lineage-empty-val">—</span>
                  )}
                </div>
                <div className="col-claims" role="cell">
                  {item.open_claims_count > 0 ? (
                    <span className="lineage-claim-chip active">
                      {t('{n} 筆', { n: item.open_claims_count })}
                    </span>
                  ) : (
                    <span className="lineage-claim-chip zero">0</span>
                  )}
                </div>
                <div className="col-ratio" role="cell">
                  <span className={`lineage-ratio ${hasTokens && item.tokens.hit_ratio > 0.5 ? 'high' : ''}`}>
                    {ratioPct}
                  </span>
                </div>
                <div className="col-saved" role="cell">
                  <span
                    className="lineage-saved"
                    title={
                      hasTokens
                        ? t('快取省下 {n} input tokens（共 {total} tokens）', {
                            n: item.tokens.saved_input_tokens.toLocaleString(),
                            total: item.tokens.input_tokens.toLocaleString(),
                          })
                        : undefined
                    }
                  >
                    {savedStr}
                  </span>
                </div>
              </div>
            )
          })}
        </div>
      ) : null}
    </section>
  )
}
