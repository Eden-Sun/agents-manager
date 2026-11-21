/**
 * 譯文「複盤」記錄（使用者 2026-11-21）：只讀，給使用者自己看，不送管理者。
 * - `MessageRetroSheet`：這則訊息的複盤（訊息上「查看」打開）。
 * - `RetroRecordModal`：全部複盤，新到舊、可載入更多（環境設定 → Claude API → 翻譯 → 複盤記錄）。
 */
import { useEffect, useState } from 'react'
import { fetchMessageTranslateRetros, fetchTranslateRetros, type RetroAttempt, type TranslateRetro } from '../api/translateRetro'
import { useT } from '../lib/i18n'
import { botLabel } from '../lib/judgeStats'
import { msgTimeLabel } from '../lib/msgTime'
import { useStore } from '../store/store'
import { Modal } from './Modal'
import { translationModelName, usd } from './messageTranslationConstants'
import './translateRetro.css'

type T = ReturnType<typeof import('../lib/i18n').useT>

/** 清單一頁的筆數（daemon 上限 200）。 */
const PAGE = 30

/** 本地檢查旗標（true＝有問題）轉成標籤。 */
function failedChecks(t: T, c: TranslateRetro['localChecks']): string[] {
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

function attemptText(t: T, a: RetroAttempt): string {
  const parts = [translationModelName(a.model) + (a.effort ? ` · ${a.effort}` : '')]
  parts.push(a.reason ? t('未過：{reason}', { reason: reasonLabel(t, a.reason) }) : t('通過'))
  if (a.jevProb !== null) parts.push(`Jev ${a.jevProb.toFixed(2)}`)
  return parts.join('｜')
}

/** 一筆複盤的內容：時間、模型、重翻與每次嘗試、檢查、備註；原文／譯文對照點開才展開。 */
export function RetroDetail({ r, bots }: { r: TranslateRetro; bots: readonly { id: string; name: string }[] }) {
  const t = useT()
  const retried = r.attempts.length > 1
  const failed = failedChecks(t, r.localChecks)
  return (
    <article className="retro-item" data-k="retro-item">
      <header className="retro-head">
        <time dateTime={r.at} className="retro-time">{msgTimeLabel(r.at)}</time>
        <strong className="retro-model" data-k="retro-model" title={r.model}>{translationModelName(r.model)}</strong>
        {r.effort ? <span className="hint">{r.effort}</span> : null}
        <span className="hint retro-bot">{botLabel(r.botId, bots)}</span>
      </header>
      <dl className="retro-facts">
        <div>
          <dt>{t('重翻過')}</dt>
          <dd data-k="retro-retried">{r.attempts.length === 0 ? '—' : retried ? t('是（{n} 次嘗試）', { n: r.attempts.length }) : t('否')}</dd>
        </div>
        {r.attempts.length > 0 ? (
          <div>
            <dt>{t('嘗試')}</dt>
            <dd>
              <ol className="retro-attempts" data-k="retro-attempts">
                {r.attempts.map((a, i) => (
                  <li key={i} title={a.model}>{attemptText(t, a)}</li>
                ))}
              </ol>
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
          <dd data-k="retro-checks">{failed.length === 0 ? t('全部通過') : t('未過：{items}', { items: failed.join('、') })}</dd>
        </div>
        <div>
          <dt>{t('Jev 機率')}</dt>
          <dd>{r.jevProb !== null ? r.jevProb.toFixed(2) : '—'}{r.lowConfidence ? ` · ${t('品質分數偏低')}` : ''}</dd>
        </div>
        <div>
          <dt>{t('成本')}</dt>
          <dd>{r.costUsd !== null ? usd(r.costUsd) : '—'}</dd>
        </div>
        {r.note ? (
          <div>
            <dt>{t('備註')}</dt>
            <dd data-k="retro-note">{r.note}</dd>
          </div>
        ) : null}
      </dl>
      <details className="retro-texts">
        <summary>{t('原文／譯文對照')}</summary>
        <div className="retro-pair">
          <section>
            <h5>{t('原文')}</h5>
            <pre>{r.source}</pre>
          </section>
          <section>
            <h5>{t('譯文')}</h5>
            <pre>{r.translation}</pre>
          </section>
        </div>
      </details>
    </article>
  )
}

/** 這則訊息的複盤（新到舊）。 */
export function MessageRetroSheet({ messageId, open, onClose }: { messageId: string; open: boolean; onClose: () => void }) {
  const t = useT()
  const bots = useStore((s) => s.bots)
  const [state, setState] = useState<{ kind: 'loading' } | { kind: 'error'; message: string } | { kind: 'ok'; retros: TranslateRetro[] }>({ kind: 'loading' })

  // 呼叫端只在打開時才掛載（每次打開都是全新的 loading 狀態）。
  useEffect(() => {
    if (!open) return
    let alive = true
    fetchMessageTranslateRetros(messageId).then(
      (retros) => alive && setState({ kind: 'ok', retros }),
      (e: unknown) => alive && setState({ kind: 'error', message: e instanceof Error ? e.message : String(e) }),
    )
    return () => {
      alive = false
    }
  }, [open, messageId])

  return (
    <Modal open={open} title={t('複盤記錄')} subtitle={t('只給你自己看，不送管理者。')} width={560} onClose={onClose}>
      <div className="retro-body">
      {state.kind === 'loading' ? <p className="hint">{t('讀取中…')}</p> : null}
      {state.kind === 'error' ? <p className="hint" role="alert">{t('複盤記錄讀取失敗：{reason}', { reason: state.message })}</p> : null}
      {state.kind === 'ok' && state.retros.length === 0 ? <p className="hint">{t('還沒有複盤記錄。')}</p> : null}
      {state.kind === 'ok' ? (
        <div className="retro-list" data-k="retro-list">
          {[...state.retros].reverse().map((r, i) => (
            <RetroDetail key={`${r.at}-${i}`} r={r} bots={bots} />
          ))}
        </div>
      ) : null}
      </div>
    </Modal>
  )
}

/** 全部複盤（新到舊，可載入更多）。 */
export function RetroRecordModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT()
  const bots = useStore((s) => s.bots)
  const [retros, setRetros] = useState<TranslateRetro[]>([])
  const [nextBefore, setNextBefore] = useState<string | null>(null)
  const [status, setStatus] = useState<{ kind: 'loading' } | { kind: 'error'; message: string } | { kind: 'ok' }>({ kind: 'loading' })
  const [moreBusy, setMoreBusy] = useState(false)

  // 呼叫端只在打開時才掛載（每次打開都是全新的 loading 狀態）。
  useEffect(() => {
    if (!open) return
    let alive = true
    fetchTranslateRetros({ limit: PAGE }).then(
      (p) => {
        if (!alive) return
        setRetros(p.retros)
        setNextBefore(p.nextBefore)
        setStatus({ kind: 'ok' })
      },
      (e: unknown) => alive && setStatus({ kind: 'error', message: e instanceof Error ? e.message : String(e) }),
    )
    return () => {
      alive = false
    }
  }, [open])

  const loadMore = () => {
    if (!nextBefore || moreBusy) return
    setMoreBusy(true)
    fetchTranslateRetros({ limit: PAGE, before: nextBefore }).then(
      (p) => {
        setRetros((prev) => [...prev, ...p.retros])
        setNextBefore(p.nextBefore)
        setMoreBusy(false)
      },
      (e: unknown) => {
        setMoreBusy(false)
        useStore.getState().notify('error', t('複盤記錄讀取失敗：{reason}', { reason: e instanceof Error ? e.message : String(e) }))
      },
    )
  }

  return (
    <Modal open={open} title={t('複盤記錄')} subtitle={t('只給你自己看，不送管理者。')} width={600} onClose={onClose}>
      <div className="retro-body">
      {status.kind === 'loading' ? <p className="hint">{t('讀取中…')}</p> : null}
      {status.kind === 'error' ? <p className="hint" role="alert">{t('複盤記錄讀取失敗：{reason}', { reason: status.message })}</p> : null}
      {status.kind === 'ok' && retros.length === 0 ? <p className="hint">{t('還沒有複盤記錄。')}</p> : null}
      {status.kind === 'ok' && retros.length > 0 ? (
        <div className="retro-list" data-k="retro-log-list">
          {retros.map((r, i) => (
            <RetroDetail key={`${r.messageId}-${r.at}-${i}`} r={r} bots={bots} />
          ))}
          {nextBefore ? (
            <button type="button" className="btn retro-more" data-k="retro-log-more" disabled={moreBusy} onClick={loadMore}>
              {t('載入更多')}
            </button>
          ) : null}
        </div>
      ) : null}
      </div>
    </Modal>
  )
}
