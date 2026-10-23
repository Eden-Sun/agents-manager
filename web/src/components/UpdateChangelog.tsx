import { useEffect, useState } from 'react'
import {
  fetchChangelog,
  hasOwnSource,
  fetchTranslation,
  postTranslation,
  TRANSLATION_POLL_MS,
  translationPollAction,
  type ChangelogReply,
  type TranslationView,
} from '../api/changelog'
import { msg, t, tr, useT } from '../lib/i18n'
import './updateChangelog.css'

/**
 * 「有更新 · 重啟套用」確認框裡的 changelog（2026-09-10 使用者需求：先看改了什麼才重啟）。
 * 抓不到不擋重啟——那是使用者的決定，這裡只講清楚原因。
 * 翻譯是另外一層（#1870）：沒開、或還在查狀態時，不畫翻譯按鈕、也不畫錯誤。
 * 翻譯鈕跟回應的 `lang`：沒有或不認得就不寫目標語言。
 */
export function UpdateChangelog({
  kind,
  host,
  from,
  to = null,
}: {
  kind: string
  host: string
  from: string | null
  /** 已知的目標版本；codex 的更新是 TUI 當場問的、磁碟上還是舊版，只能從畫面讀。 */
  to?: string | null
}) {
  const t = useT()
  // 以「這次請求的 key」配對結果：key 換了就等於重新載入，不必在 effect 裡同步清狀態。
  const key = `${kind}|${host}|${from ?? ''}|${to ?? ''}`
  const [state, setState] = useState<{ key: string; reply: ChangelogReply } | null>(null)
  const [tr, setTr] = useState<{ key: string; view: TranslationView } | null>(null)
  const [originalPref, setOriginalPref] = useState<{ key: string; on: boolean } | null>(null)
  const [stopped, setStopped] = useState<{ key: string; epoch: number } | null>(null)
  const [pollEpoch, setPollEpoch] = useState(0)
  const [now, setNow] = useState(() => Date.now())

  useEffect(() => {
    let alive = true
    void fetchChangelog(kind, host, from, to).then((r) => {
      if (alive) setState({ key, reply: r })
    })
    return () => {
      alive = false
    }
  }, [kind, host, from, to, key])

  useEffect(() => {
    let alive = true
    void fetchTranslation(kind, host, from, to).then((view) => {
      if (alive) setTr({ key, view })
    })
    return () => {
      alive = false
    }
  }, [kind, host, from, to, key])

  const view = tr?.key === key ? tr.view : null
  const polling = view?.status === 'in_flight' || view?.status === 'reserved'
  const retryAfter = view?.retryAfter ?? null
  const pollStopped = stopped?.key === key && stopped.epoch === pollEpoch

  useEffect(() => {
    if (!polling) return
    let tick = 0
    let alive = true
    const id = setInterval(() => {
      tick += 1
      if (translationPollAction(tick) === 'stop') {
        clearInterval(id)
        if (alive) setStopped({ key, epoch: pollEpoch })
        return
      }
      void fetchTranslation(kind, host, from, to).then((next) => {
        if (alive) setTr({ key, view: next })
      })
    }, TRANSLATION_POLL_MS)
    return () => {
      alive = false
      clearInterval(id)
    }
  }, [polling, pollEpoch, key, kind, host, from, to])

  useEffect(() => {
    if (!retryAfter) return
    const id = setInterval(() => setNow(Date.now()), 500)
    return () => clearInterval(id)
  }, [retryAfter])

  const reply = state?.key === key ? state.reply : null
  if (!reply) {
    return (
      <div className="update-changelog loading" role="status">
        {t('正在取得新版 changelog…')}
      </div>
    )
  }

  const head =
    reply.installedVersion && reply.fromVersion && reply.installedVersion !== reply.fromVersion
      ? `${reply.fromVersion} → ${reply.installedVersion}`
      : reply.installedVersion
        ? t('新版 {version}', { version: reply.installedVersion })
        : null

  if (!reply.found) {
    const official = hasOwnSource(kind)
    return (
      <div className="update-changelog missing" role="status">
        <strong>{official ? t('找不到這一版的更新說明') : t('找不到 changelog')}</strong>
        {head ? <span className="update-changelog-ver">{head}</span> : null}
        <div className="update-changelog-why">{reply.error ?? t('沒有拿到任何段落')}</div>
        <a href={reply.sourceUrl} target="_blank" rel="noreferrer">
          {official ? t('看官方更新說明 ↗') : t('自己到 GitHub 看 CHANGELOG ↗')}
        </a>
        {reply.altUrl ? (
          <>
            {' '}
            <a href={reply.altUrl} target="_blank" rel="noreferrer">
              {t('備援：官方 releases ↗')}
            </a>
          </>
        ) : null}
      </div>
    )
  }

  const showOriginal = originalPref?.key === key && originalPref.on
  const translated = view?.status === 'completed' && !showOriginal ? view : null
  const sections = translated ? translated.sections : reply.sections
  const store = (next: TranslationView) => setTr({ key, view: next })

  return (
    <div className="update-changelog">
      <div className="update-changelog-head">
        <strong>{t('這次更新改了什麼')}</strong>
        {head ? <span className="update-changelog-ver">{head}</span> : null}
      </div>
      {translated && translated.summary.length > 0 ? (
        <ul className="update-changelog-summary">
          {translated.summary.map((line) => (
            <li key={line}>{line}</li>
          ))}
        </ul>
      ) : null}
      {translated?.coverage === 'partial' ? (
        <p className="update-changelog-tr-note">{t('只翻了送出的部分，其餘仍看原文。')}</p>
      ) : null}
      <div className="update-changelog-body">
        {sections.map((s) => (
          <section key={s.version}>
            <h4>{s.version}</h4>
            <pre>{s.body}</pre>
          </section>
        ))}
      </div>
      <a className="update-changelog-src" href={reply.sourceUrl} target="_blank" rel="noreferrer">
        {t('完整 CHANGELOG ↗')}
      </a>
      <TranslationBar
        view={view}
        showOriginal={showOriginal}
        now={now}
        pollStopped={pollStopped}
        onToggle={() => setOriginalPref({ key, on: !showOriginal })}
        onTranslate={() => {
          void postTranslation(kind, host, from, to).then(store)
        }}
        onRetry={() => {
          if (!retryReady(view?.retryAfter ?? null, Date.now())) return
          void postTranslation(kind, host, from, to).then(store)
        }}
        onRecheck={() => {
          setPollEpoch((n) => n + 1)
          void fetchTranslation(kind, host, from, to).then(store)
        }}
      />
    </div>
  )
}

const TRANSLATE_TARGET: Record<string, string> = {
  'zh-Hant': msg('繁中'),
  'zh-Hans': msg('簡中'),
  en: msg('英文'),
  ja: msg('日文'),
  ko: msg('韓文'),
}

/** 只採用回應裡的 lang。沒有或不認得就寫「翻譯」，不猜成繁中。 */
function translateButtonLabel(lang: string | null): string {
  if (!lang) return t('翻譯')
  const name = TRANSLATE_TARGET[lang]
  return name ? t('翻譯成{name}', { name: tr(name) }) : t('翻譯')
}

function retryReady(retryAfter: string | null, now: number): boolean {
  if (retryAfter == null || retryAfter === '') return true
  const at = Date.parse(retryAfter)
  if (Number.isNaN(at)) return false
  return at <= now
}

const ERROR_LABEL: Record<string, string> = {
  timeout: msg('逾時'),
  rate_limited: msg('被限流'),
  http: msg('連線失敗'),
  missing_usage: msg('沒有用量'),
  schema: msg('格式不符'),
  sections: msg('段落對不上'),
  summary: msg('摘要不符'),
  lang: msg('語言不在允許表'),
  max_tokens: msg('輸出被截斷'),
  refusal: msg('被拒絕'),
  stop: msg('沒有正常結束'),
  ledger: msg('紀錄讀不到'),
  restart: msg('翻譯過程中斷'),
  not_sent: msg('還沒送出'),
  in_flight: msg('正在翻其他版本'),
  hourly: msg('這一小時的次數已滿'),
  attempts: msg('已達重試上限'),
}

function shortReason(code: string | null): string {
  if (!code || code.length > 32 || !ERROR_LABEL[code]) return t('沒有完成')
  return tr(ERROR_LABEL[code])
}

function TranslationBar({
  view,
  showOriginal,
  now,
  pollStopped,
  onToggle,
  onTranslate,
  onRetry,
  onRecheck,
}: {
  view: TranslationView | null
  showOriginal: boolean
  now: number
  pollStopped: boolean
  onToggle: () => void
  onTranslate: () => void
  onRetry: () => void
  onRecheck: () => void
}) {
  const t = useT()
  if (!view || view.status === 'unconfigured' || view.status === 'skipped') return null
  if (view.status === 'absent') {
    return (
      <div className="update-changelog-tr">
        <button type="button" onClick={onTranslate}>{translateButtonLabel(view.lang)}</button>
      </div>
    )
  }
  if (view.status === 'unavailable') {
    return <p className="update-changelog-tr-note">{t('翻譯狀態暫時讀不到')}</p>
  }
  if (view.status === 'completed') {
    return (
      <div className="update-changelog-tr">
        <button type="button" onClick={onToggle}>{showOriginal ? t('看翻譯') : t('看原文')}</button>
      </div>
    )
  }
  if (view.status === 'in_flight' || view.status === 'reserved') {
    return (
      <div className="update-changelog-tr">
        <p className="update-changelog-tr-note">{pollStopped ? t('翻譯還在進行') : t('正在翻譯…')}</p>
        {pollStopped ? <button type="button" onClick={onRecheck}>{t('再查一次')}</button> : null}
      </div>
    )
  }
  const ready = retryReady(view.retryAfter, now)
  const lead = view.status === 'interrupted' ? t('翻譯中斷') : t('翻譯沒有完成')
  return (
    <div className="update-changelog-tr">
      <p className="update-changelog-tr-note">{t('{lead}：{reason}', { lead, reason: shortReason(view.error) })}</p>
      <button type="button" onClick={onRetry} disabled={!ready}>{t('再試一次')}</button>
    </div>
  )
}
