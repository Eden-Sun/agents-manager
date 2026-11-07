/** 四種 CLI 投影分診帳本；herdr 保留原影響分析路徑。 */
import { useEffect, useState } from 'react'
import * as api from '../api'
import { fetchChangelog } from '../api/changelog'
import type { UpdateReview } from '../api/types'
import { anyAnalysed, triageForRange, type TriageItem, type TriageVersion } from '../lib/releaseTriage'
import { loadReviewOnOpen } from '../lib/autoReview'
import { scheduleReviewRefresh } from '../lib/reviewRefresh'
import { useStore } from '../store/store'
import './agmReviewBox.css'
import { safeHttpUrl } from '../lib/safeUrl'
import { msg, useT } from '../lib/i18n'

type Loaded = { key: string; version: string; review: UpdateReview | null; triage: TriageVersion[] | null }

export function AgmReviewBox({
  kind,
  host,
  from,
  to = null,
  refreshKey,
}: {
  kind: string
  host: string
  from: string | null
  /** codex 的新版還沒裝，要從通知讀；claude 給 `null` 讓 daemon 讀磁碟。 */
  to?: string | null
  refreshKey: number
}) {
  const usesTriage = kind !== 'herdr'
  const key = `${kind}|${host}|${from ?? ''}|${to ?? ''}|${refreshKey}`
  const [loaded, setLoaded] = useState<Loaded | null>(null)

  useEffect(() => {
    let alive = true
    // 任一支讀不到（舊 daemon 沒有那支 API）就只畫另一邊，不在更新框裡吵。
    const review = !usesTriage ? loadReviewOnOpen({ kind, host, from, to }, api).catch(() => null) : !to ? fetchChangelog(kind, host, from).then((r) => ({ version: r.installedVersion || '', review: null })) : Promise.resolve(null)
    const triage = api.fetchReleaseTriage(kind).catch(() => null)
    void Promise.all([review, triage]).then(([r, t]) => {
      if (!alive) return
      const version = to || r?.version || ''
      setLoaded({ key, version, review: usesTriage ? null : r?.review ?? null, triage: t ? triageForRange(t.rows, t.repo, from, version) : null })
    })
    return () => {
      alive = false
    }
  }, [kind, host, from, to, key, usesTriage])

  // 帳本目前沒有專用 SSE；開框期間輪詢，包含判完後 publish 的狀態變更。
  useEffect(() => {
    if (!usesTriage) return
    let alive = true
    let running = false
    const timer = setInterval(async () => {
      if (running) return
      running = true
      try {
        const data = await api.fetchReleaseTriage(kind)
        if (alive) setLoaded((cur) => cur?.key === key ? { ...cur, triage: triageForRange(data.rows, data.repo, from, cur.version) } : cur)
      } catch { /* 保留上次成功結果，下輪再讀。 */ }
      finally { running = false }
    }, 5000)
    return () => { alive = false; clearInterval(timer) }
  }, [usesTriage, kind, from, key])

  // 分診／publish 與 herdr review 都共用 supervisor_changed；完成後仍需刷新 publish 結果。
  const rev = useStore((s) => s.supervisorRev)
  useEffect(() => {
    let alive = true
    const cancel = scheduleReviewRefresh({
      rev,
      state: undefined,
      run: () => {
        void Promise.all([
          api.fetchUpdateReview({ kind, host, from, to }).catch(() => null),
          api.fetchReleaseTriage(kind).catch(() => null),
        ]).then(([r, triage]) => {
          if (!alive) return
          setLoaded((cur) => cur?.key === key ? {
            ...cur,
            review: usesTriage ? null : r?.review ?? cur.review,
            triage: triage ? triageForRange(triage.rows, triage.repo, from, cur.version) : cur.triage,
          } : cur)
        })
      },
    })
    return () => { alive = false; cancel() }
  }, [rev, kind, host, from, to, key, usesTriage])

  const t = useT()
  const [retrying, setRetrying] = useState(false)
  const [retryError, setRetryError] = useState('')
  const retry = async (version?: string, publish = false) => {
    setRetrying(true)
    setRetryError('')
    try {
      if (version && publish) await api.publishReleaseTriage(kind, version)
      else if (version) await api.retryReleaseTriage(kind, version)
      else await api.requestUpdateReview({ kind, host, from, to: to ?? loaded?.version ?? null })
      const [r, triage] = await Promise.all([usesTriage ? Promise.resolve(null) : api.fetchUpdateReview({ kind, host, from, to }), api.fetchReleaseTriage(kind).catch(() => null)])
      setLoaded((cur) => cur && cur.key === key ? { ...cur, review: r?.review ?? null, triage: triage ? triageForRange(triage.rows, triage.repo, from, cur.version) : cur.triage } : cur)
    } catch (e) {
      setRetryError(e instanceof Error ? e.message : String(e))
    } finally {
      setRetrying(false)
    }
  }

  const cur = loaded?.key === key ? loaded : null
  if (!cur || (!cur.review && !cur.triage)) return null
  const { review, triage } = cur
  const analysed = (triage ? anyAnalysed(triage) : false) || (review !== null && review.state !== 'none')

  return (
    <section className="agm-review" aria-label={t('{kind} 新版分析', { kind })}>
      <div className="agm-review-head">
        <strong>{t('影響分析')}</strong>
        <span className="agm-review-who">
          {kind} {from && cur.version && from !== cur.version ? `${from} → ${cur.version}` : cur.version}
        </span>
      </div>
      {!analysed ? (
        <p className="agm-review-none">{t('尚未分析——按下方「分析影響」，結論會回到這裡。')}</p>
      ) : null}
      {triage && triage.some((v) => v.state !== 'missing') ? (
        <div className="triage">
          {triage.map((v) => (
            <TriageBlock key={v.version} v={v} retry={() => void retry(v.version)} publish={() => void retry(v.version, true)} retrying={retrying} />
          ))}
        </div>
      ) : null}
      {review && review.state !== 'none' ? (
        <div className="agm-review-agm">
          <div className="agm-review-head">
            <strong>{t('影響分析')}</strong>
            <span className="agm-review-who">
              {review.source === 'api'
                ? (review.model ? t('Claude API（{model}）', { model: review.model }) : 'Claude API')
                : review.target_bot_name || 'AGM'}

            </span>
          </div>
          {review.state === 'done' ? (
            <pre className="agm-review-body">{review.result}</pre>
          ) : review.state === 'failed' ? (
            <p role="alert">
              {t('影響分析失敗：{reason}', { reason: review.error || t('原因不明') })}{' '}
              <button type="button" disabled={retrying} onClick={() => void retry()}>{t('重試')}</button>
            </p>
          ) : (
            <p className="hint">{t('分析中，結論回來就會出現在這裡。')}</p>
          )}

        </div>
      ) : null}
      {retryError ? <p role="alert">{retryError}</p> : null}
    </section>
  )
}

const STATE_TEXT: Record<TriageVersion['state'], string> = {
  judged: '',
  empty: msg('看過了，沒有要處理的'),
  pending: msg('分診中…'),
  failed: msg('影響分析失敗'),
  missing: msg('尚未分析'),
}

function TriageBlock({ v, retry, publish, retrying }: { v: TriageVersion; retry: () => void; publish: () => void; retrying: boolean }) {
  const t = useT()
  const nothing = !v.guard.length && !v.adopt.length && !v.upgrade.length && !v.issues.length
  return (
    <div className={`triage-ver ${v.state}`}>
      <div className="triage-ver-head">
        <span className="triage-ver-no">{v.version}</span>
        {STATE_TEXT[v.state] ? <span className="triage-ver-state">{t.tr(STATE_TEXT[v.state])}</span> : null}
        {v.state === 'judged' && nothing ? <span className="triage-ver-state">{t('沒有要處理的')}</span> : null}
      </div>
      {v.analysis?.source === 'api' ? <p className="hint">{v.analysis.model ? t('Claude API（{model}）', { model: v.analysis.model }) : 'Claude API'}</p> : null}
      {v.state === 'failed' ? <p role="alert">{v.analysis?.error || t('原因不明')}{' '}<button type="button" disabled={retrying} onClick={retry}>{t('重試')}</button></p> : null}
      {v.publishError ? <p role="alert">{t('發布失敗：{reason}', { reason: v.publishError })}{' '}<button type="button" disabled={retrying} onClick={publish}>{t('重試')}</button></p> : null}
      <Items label={t('提防')} tone="guard" items={v.guard} />
      <Items label={t('採用')} tone="adopt" items={v.adopt} />
      <Items label={t('值得早升')} tone="upgrade" items={v.upgrade} />
      {v.issues.length ? (
        <ul className="triage-issues">
          {v.issues.map((i) => (
            <li key={i.title}>
              <span className={`triage-tag ${i.verdict}`}>{i.verdict === 'guard' ? t('提防') : t('採用')}</span>{' '}
              {i.title}
              {i.number !== null ? (
                <>
                  {' '}
                  {safeHttpUrl(i.url) ? (
                    <a href={safeHttpUrl(i.url)} target="_blank" rel="noopener noreferrer">
                      {i.duplicate ? (i.published ? t('留言於 #{number}', { number: i.number }) : t('待留言於 #{number}', { number: i.number })) : `#${i.number}`}
                    </a>
                  ) : (
                    <span title={t('帳本沒設 [release_triage] repo，連不出去')}>#{i.number}</span>
                  )}

                </>
              ) : (
                <span className="triage-dup">{t('（尚未開票）')}</span>
              )}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  )
}

function Items({ label, tone, items }: { label: string; tone: string; items: TriageItem[] }) {
  if (!items.length) return null
  return (
    <ul className={`triage-items ${tone}`}>
      {items.map((it) => (
        <li key={it.id} title={it.text}>
          <span className={`triage-tag ${tone}`}>{label}</span> {it.reason || it.text}
          {it.module ? <code className="triage-mod">{it.module}</code> : null}
        </li>
      ))}
    </ul>
  )
}
