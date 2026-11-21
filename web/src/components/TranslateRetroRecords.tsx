import { useCallback, useEffect, useState } from 'react'
import { createPortal } from 'react-dom'
import {
  fetchMessageTranslateRecord,
  fetchTranslateRecords,
  type RetroAttempt,
  type TranslateLocalChecks,
  type TranslateRecord,
  type TranslateRecordFilter,
} from '../api/translateRetro'
import { useT } from '../lib/i18n'
import { botLabel } from '../lib/judgeStats'
import { msgTimeLabel } from '../lib/msgTime'
import { useStore } from '../store/store'
import { Modal } from './Modal'
import { isLocalTranslation, translationModelName, usd } from './messageTranslationConstants'
import './translateRetro.css'

type T = ReturnType<typeof import('../lib/i18n').useT>

const PAGE = 50

function failedChecks(t: T, c: TranslateLocalChecks): string[] {
  const out: string[] = []
  if (c.untranslated) out.push(t('未翻譯'))
  if (c.partiallyUntranslated) out.push(t('部分未翻'))
  if (c.markdownStructure) out.push(t('Markdown 結構'))
  if (c.japaneseOutput) out.push(t('出現日文'))
  return out
}

function reasonLabel(t: T, reason: string): string {
  switch (reason) {
    case 'untranslated':
      return t('未翻譯')
    case 'partially_untranslated':
      return t('部分未翻')
    case 'markdown_structure':
      return t('Markdown 結構')
    case 'japanese_output':
      return t('出現日文')
    default:
      return reason
  }
}

function AttemptLine({ t, a }: { t: T; a: RetroAttempt }) {
  const name = translationModelName(a.model) || a.model
  const parts = [a.reason ? t('未過：{reason}', { reason: reasonLabel(t, a.reason) }) : t('通過')]
  if (a.jevProb !== null) parts.push(`Jev ${a.jevProb.toFixed(2)}`)
  return (
    <li>
      <span title={a.model}>{name}</span>
      {a.effort ? ` · ${a.effort}` : ''}
      {'｜'}
      {parts.join('｜')}
    </li>
  )
}

export function TranslateRecordCard({
  r,
  bots,
  defaultOpenTexts = false,
}: {
  r: TranslateRecord
  bots: readonly { id: string; name: string }[]
  defaultOpenTexts?: boolean
}) {
  const t = useT()
  const modelLabel = r.model ? translationModelName(r.model) : '—'
  const failed = r.localChecks ? failedChecks(t, r.localChecks) : null
  const attemptCount = r.attempts.length || r.rejectDetail?.attempts || 0
  const retriedText =
    attemptCount > 0
      ? r.retried
        ? t('是（{n} 次嘗試）', { n: attemptCount })
        : t('否')
      : '—'
  const costText = isLocalTranslation(r.model) ? '$0' : r.costUsd !== null ? usd(r.costUsd) : '—'

  return (
    <article className="retro-item" data-k="retro-item">
      <header className="retro-head">
        {r.at ? <time dateTime={r.at} className="retro-time">{msgTimeLabel(r.at)}</time> : null}
        <strong className="retro-model" data-k="retro-model" title={r.model || undefined}>
          {modelLabel}
        </strong>
        {r.effort ? <span className="hint">{r.effort}</span> : null}
        {r.status === 'rejected' ? (
          <span className="retro-badge rejected" data-k="retro-rejected-badge">{t('被擋')}</span>
        ) : null}
        {r.retried ? <span className="retro-badge retried">{t('重翻過')}</span> : null}
        {r.botId ? <span className="hint retro-bot">{botLabel(r.botId, bots)}</span> : null}
      </header>
      <dl className="retro-facts">
        {r.rejected ? (
          <div>
            <dt>{t('被擋原因')}</dt>
            <dd data-k="retro-rejected">{reasonLabel(t, r.rejected)}</dd>
          </div>
        ) : null}
        <div>
          <dt>{t('重翻過')}</dt>
          <dd data-k="retro-retried">{retriedText}</dd>
        </div>
        {r.attempts.length > 0 ? (
          <div>
            <dt>{t('嘗試')}</dt>
            <dd>
              <ol className="retro-attempts" data-k="retro-attempts">
                {r.attempts.map((a, i) => (
                  <AttemptLine key={i} t={t} a={a} />
                ))}
              </ol>
            </dd>
          </div>
        ) : r.rejectDetail && r.rejectDetail.models.length > 0 ? (
          <div>
            <dt>{t('嘗試')}</dt>
            <dd data-k="retro-attempts">
              {r.rejectDetail.models.map((m) => translationModelName(m) || m).join(' → ')}
            </dd>
          </div>
        ) : (
          <div>
            <dt>{t('嘗試')}</dt>
            <dd className="hint">{t('（舊快取，沒有嘗試紀錄）')}</dd>
          </div>
        )}
        <div>
          <dt>{t('檢查')}</dt>
          <dd data-k="retro-checks">
            {failed === null ? '—' : failed.length === 0 ? t('全部通過') : t('未過：{items}', { items: failed.join('、') })}
          </dd>
        </div>
        <div>
          <dt>{t('Jev 機率')}</dt>
          <dd>
            {r.jevProb !== null ? r.jevProb.toFixed(2) : '—'}
            {r.lowConfidence ? ` · ${t('品質分數偏低')}` : ''}
          </dd>
        </div>
        <div>
          <dt>{t('成本')}</dt>
          <dd>{costText}</dd>
        </div>
        {r.promptVersion ? (
          <div>
            <dt>{t('Prompt')}</dt>
            <dd>v{r.promptVersion}</dd>
          </div>
        ) : null}
        {r.notes.length > 0 ? (
          <div>
            <dt>{t('備註')}</dt>
            <dd data-k="retro-note">
              {r.notes.map((n, i) => (
                <div key={`${n.at}-${i}`}>{n.note}</div>
              ))}
            </dd>
          </div>
        ) : null}
      </dl>
      {r.source || r.translation ? (
        <details className="retro-texts" open={defaultOpenTexts}>
          <summary>{t('原文／譯文對照')}</summary>
          <div className="retro-pair">
            <section>
              <h5>{t('原文')}</h5>
              <pre>{r.source || '—'}</pre>
            </section>
            <section>
              <h5>{t('譯文')}</h5>
              <pre>{r.translation || '—'}</pre>
            </section>
          </div>
        </details>
      ) : null}
    </article>
  )
}

export function MessageTranslateRecordSheet({
  messageId,
  open,
  onClose,
}: {
  messageId: string
  open: boolean
  onClose: () => void
}) {
  const t = useT()
  const bots = useStore((s) => s.bots)
  const [state, setState] = useState<
    { kind: 'loading' } | { kind: 'error'; message: string } | { kind: 'ok'; record: TranslateRecord | null }
  >({ kind: 'loading' })

  useEffect(() => {
    if (!open) return
    let alive = true
    fetchMessageTranslateRecord(messageId).then(
      (record) => alive && setState({ kind: 'ok', record }),
      (e: unknown) => alive && setState({ kind: 'error', message: e instanceof Error ? e.message : String(e) }),
    )
    return () => {
      alive = false
    }
  }, [open, messageId])

  const modal = (
    <Modal open={open} title={t('翻譯紀錄')} width={600} onClose={onClose}>
      <div className="retro-sheet-body">
        {state.kind === 'loading' ? <p className="hint">{t('讀取中…')}</p> : null}
        {state.kind === 'error' ? (
          <p className="hint" role="alert">
            {t('翻譯紀錄讀取失敗：{reason}', { reason: state.message })}
          </p>
        ) : null}
        {state.kind === 'ok' && state.record === null ? <p className="hint">{t('這則訊息沒有快取的翻譯紀錄。')}</p> : null}
        {state.kind === 'ok' && state.record !== null ? (
          <div className="retro-list" data-k="retro-list">
            <TranslateRecordCard r={state.record} bots={bots} defaultOpenTexts />
          </div>
        ) : null}
      </div>
    </Modal>
  )
  return typeof document !== 'undefined' ? createPortal(modal, document.body) : modal
}

export function TranslateRecordsPanel({ active = true }: { active?: boolean }) {
  const t = useT()
  const bots = useStore((s) => s.bots)
  const projects = useStore((s) => s.projects)
  const [projectId, setProjectId] = useState('')
  const [filter, setFilter] = useState<TranslateRecordFilter>('all')
  const [records, setRecords] = useState<TranslateRecord[]>([])
  const [nextBefore, setNextBefore] = useState<string | null>(null)
  const [loaded, setLoaded] = useState<{ key: string; error: string | null } | null>(null)
  const [moreBusy, setMoreBusy] = useState(false)
  const [reloadSeq, setReloadSeq] = useState(0)

  const queryKey = `${projectId}|${filter}|${reloadSeq}`
  const loading = active && loaded?.key !== queryKey
  const errorMsg = loaded?.key === queryKey ? loaded.error : null
  const reload = useCallback(() => setReloadSeq((n) => n + 1), [])

  useEffect(() => {
    if (!active) return
    let alive = true
    fetchTranslateRecords({ limit: PAGE, projectId: projectId || undefined, filter }).then(
      (p) => {
        if (!alive) return
        setRecords(p.records)
        setNextBefore(p.nextBefore)
        setLoaded({ key: queryKey, error: null })
      },
      (e: unknown) => alive && setLoaded({ key: queryKey, error: e instanceof Error ? e.message : String(e) }),
    )
    return () => {
      alive = false
    }
  }, [active, projectId, filter, queryKey])

  const loadMore = () => {
    if (!nextBefore || moreBusy) return
    setMoreBusy(true)
    fetchTranslateRecords({ limit: PAGE, projectId: projectId || undefined, filter, before: nextBefore }).then(
      (p) => {
        setRecords((prev) => [...prev, ...p.records])
        setNextBefore(p.nextBefore)
        setMoreBusy(false)
      },
      (e: unknown) => {
        setMoreBusy(false)
        useStore.getState().notify('error', t('翻譯紀錄讀取失敗：{reason}', { reason: e instanceof Error ? e.message : String(e) }))
      },
    )
  }

  return (
    <div className="retro-panel" data-k="translate-records-panel">
      <div className="retro-filters">
        <label className="field">
          <span>{t('專案')}</span>
          <select data-k="retro-filter-project" value={projectId} onChange={(e) => setProjectId(e.target.value)}>
            <option value="">{t('全部專案')}</option>
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.label || p.id}
              </option>
            ))}
          </select>
        </label>
        <label className="field">
          <span>{t('篩選')}</span>
          <select data-k="retro-filter-kind" value={filter} onChange={(e) => setFilter(e.target.value as TranslateRecordFilter)}>
            <option value="all">{t('全部')}</option>
            <option value="retried">{t('只看重翻過')}</option>
            <option value="failed">{t('只看檢查沒過')}</option>
            <option value="rejected">{t('只看被擋')}</option>
          </select>
        </label>
        <button type="button" className="btn retro-refresh" onClick={reload}>
          {t('重新整理')}
        </button>
      </div>
      {loading ? <p className="hint">{t('讀取中…')}</p> : null}
      {errorMsg ? (
        <p className="hint" role="alert">
          {t('翻譯紀錄讀取失敗：{reason}', { reason: errorMsg })}
        </p>
      ) : null}
      {!loading && !errorMsg && records.length === 0 ? <p className="hint">{t('還沒有翻譯紀錄。')}</p> : null}
      {!loading && !errorMsg && records.length > 0 ? (
        <div className="retro-list" data-k="retro-log-list">
          {records.map((r, i) => (
            <TranslateRecordCard key={`${r.messageId ?? 'old'}-${r.at}-${i}`} r={r} bots={bots} />
          ))}
          {nextBefore ? (
            <button type="button" className="btn retro-more" data-k="retro-log-more" disabled={moreBusy} onClick={loadMore}>
              {t('載入更多')}
            </button>
          ) : null}
        </div>
      ) : null}
    </div>
  )
}
