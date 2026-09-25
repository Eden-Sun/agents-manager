/**
 * 更新框裡的「分析」區塊（使用者 2026-09-19：「解析結果直接在更新視窗 show 出」；2026-09-25 起 codex 也有，issue #561）。
 *
 * 兩個來源，都只讀：
 * 1. **上游分診**（`/api/release-triage`，SPEC §18.2c）：帳本早就逐條分析過 claude／codex 每一版，這裡把
 *    `(from, to]` 區間內每一版的提防／採用／值得早升與提案 issue 攤出來。帳本沒有這一版就寫「尚未分析」。
 * 2. **AGM 解析**（`/api/claude-update/review?kind=`）：使用者按「請 AGM 解析」派出去的那一筆，三種狀態：
 *    還沒派、派了還沒結論、有結論（**直接把結論印在框裡**）。
 * 兩邊都沒東西時整塊寫「尚未分析」，指向框底的「請 AGM 解析」。
 */
import { useEffect, useState } from 'react'
import * as api from '../api'
import type { UpdateReview } from '../api/types'
import { anyAnalysed, triageForRange, type TriageItem, type TriageVersion } from '../lib/releaseTriage'
import './agmReviewBox.css'

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
  const key = `${kind}|${host}|${from ?? ''}|${to ?? ''}|${refreshKey}`
  const [loaded, setLoaded] = useState<Loaded | null>(null)

  useEffect(() => {
    let alive = true
    // 任一支讀不到（舊 daemon 沒有那支 API）就只畫另一邊，不在更新框裡吵。
    const review = api.fetchUpdateReview({ kind, host, to }).catch(() => null)
    const triage = api.fetchReleaseTriage(kind).catch(() => null)
    void Promise.all([review, triage]).then(([r, t]) => {
      if (!alive) return
      const version = to || r?.version || ''
      setLoaded({ key, version, review: r?.review ?? null, triage: t ? triageForRange(t.rows, t.repo, from, version) : null })
    })
    return () => {
      alive = false
    }
  }, [kind, host, from, to, key])

  const cur = loaded?.key === key ? loaded : null
  if (!cur || (!cur.review && !cur.triage)) return null
  const { review, triage } = cur
  const analysed = (triage ? anyAnalysed(triage) : false) || (review !== null && review.state !== 'none')

  return (
    <section className="agm-review" aria-label={`${kind} 新版分析`}>
      <div className="agm-review-head">
        <strong>分析</strong>
        <span className="agm-review-who">
          {kind} {from && cur.version && from !== cur.version ? `${from} → ${cur.version}` : cur.version}
        </span>
      </div>
      {!analysed ? (
        <p className="agm-review-none">尚未分析——按下方「請 AGM 解析」，結論會回到這裡。</p>
      ) : null}
      {triage && triage.some((v) => v.state !== 'missing') ? (
        <div className="triage">
          {triage.map((v) => (
            <TriageBlock key={v.version} v={v} />
          ))}
        </div>
      ) : null}
      {review && review.state !== 'none' ? (
        <div className="agm-review-agm">
          <div className="agm-review-head">
            <strong>AGM 解析</strong>
            <span className="agm-review-who">
              {review.state === 'done'
                ? `${review.target_bot_name || 'AGM'} · ${review.answered_at ? new Date(review.answered_at).toLocaleString() : ''}`
                : `${review.target_bot_name || 'AGM'} 解析中…`}
            </span>
          </div>
          {review.state === 'done' ? (
            <pre className="agm-review-body">{review.result}</pre>
          ) : (
            <p className="hint">派出去了，結論回來就會出現在這裡。</p>
          )}
        </div>
      ) : null}
    </section>
  )
}

const STATE_TEXT: Record<TriageVersion['state'], string> = {
  judged: '',
  empty: '看過了，沒有要處理的',
  pending: '分診中…',
  failed: '分診失敗（逾時三次）',
  missing: '尚未分析',
}

function TriageBlock({ v }: { v: TriageVersion }) {
  const nothing = !v.guard.length && !v.adopt.length && !v.upgrade.length && !v.issues.length
  return (
    <div className={`triage-ver ${v.state}`}>
      <div className="triage-ver-head">
        <span className="triage-ver-no">{v.version}</span>
        {STATE_TEXT[v.state] ? <span className="triage-ver-state">{STATE_TEXT[v.state]}</span> : null}
        {v.state === 'judged' && nothing ? <span className="triage-ver-state">沒有要處理的</span> : null}
      </div>
      <Items label="提防" tone="guard" items={v.guard} />
      <Items label="採用" tone="adopt" items={v.adopt} />
      <Items label="值得早升" tone="upgrade" items={v.upgrade} />
      {v.issues.length ? (
        <ul className="triage-issues">
          {v.issues.map((i) => (
            <li key={i.title}>
              <span className={`triage-tag ${i.verdict}`}>{i.verdict === 'guard' ? '提防' : '採用'}</span>{' '}
              {i.title}
              {i.number !== null ? (
                <>
                  {' '}
                  {i.url ? (
                    <a href={i.url} target="_blank" rel="noreferrer">
                      #{i.number}
                    </a>
                  ) : (
                    <span title="帳本沒設 [release_triage] repo，連不出去">#{i.number}</span>
                  )}
                  {i.duplicate ? <span className="triage-dup">（併入既有）</span> : null}
                </>
              ) : (
                <span className="triage-dup">（尚未開票）</span>
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
