import { useEffect, useState } from 'react'
import {
  fetchChangelog,
  fetchTranslation,
  postTranslation,
  TRANSLATION_POLL_MS,
  translationPollAction,
  type ChangelogReply,
  type TranslationView,
} from '../api/changelog'
import './updateChangelog.css'

/**
 * 「有更新 · 重啟套用」確認框裡的 changelog（2026-09-10 使用者需求：先看改了什麼才重啟）。
 * 抓不到不擋重啟——那是使用者的決定，這裡只講清楚原因。
 * 翻譯是另外一層（#1870）：沒開、或還在查狀態時，不畫翻譯按鈕、也不畫錯誤。
 * 按鈕文字仍是「翻譯成繁中」；目標語言由 daemon 的 lang 設定決定。
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
        正在取得新版 changelog…
      </div>
    )
  }

  const head =
    reply.installedVersion && reply.fromVersion && reply.installedVersion !== reply.fromVersion
      ? `${reply.fromVersion} → ${reply.installedVersion}`
      : reply.installedVersion
        ? `新版 ${reply.installedVersion}`
        : null

  if (!reply.found) {
    return (
      <div className="update-changelog missing" role="status">
        <strong>找不到 changelog</strong>
        {head ? <span className="update-changelog-ver">{head}</span> : null}
        <div className="update-changelog-why">{reply.error ?? '沒有拿到任何段落'}</div>
        <a href={reply.sourceUrl} target="_blank" rel="noreferrer">
          自己到 GitHub 看 CHANGELOG ↗
        </a>
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
        <strong>這次更新改了什麼</strong>
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
        <p className="update-changelog-tr-note">只翻了送出的部分，其餘仍看原文。</p>
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
        完整 CHANGELOG ↗
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

function retryReady(retryAfter: string | null, now: number): boolean {
  if (retryAfter == null || retryAfter === '') return true
  const at = Date.parse(retryAfter)
  if (Number.isNaN(at)) return false
  return at <= now
}

const ERROR_LABEL: Record<string, string> = {
  timeout: '逾時',
  rate_limited: '被限流',
  http: '連線失敗',
  missing_usage: '沒有用量',
  schema: '格式不符',
  sections: '段落對不上',
  summary: '摘要不符',
  max_tokens: '輸出被截斷',
  refusal: '被拒絕',
  stop: '沒有正常結束',
  ledger: '紀錄讀不到',
  restart: '翻譯過程中斷',
  not_sent: '還沒送出',
  in_flight: '正在翻其他版本',
  hourly: '這一小時的次數已滿',
  attempts: '已達重試上限',
}

function shortReason(code: string | null): string {
  if (!code || code.length > 32) return '沒有完成'
  return ERROR_LABEL[code] ?? '沒有完成'
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
  if (!view || view.status === 'unconfigured' || view.status === 'skipped') return null
  if (view.status === 'absent') {
    return (
      <div className="update-changelog-tr">
        <button type="button" onClick={onTranslate}>翻譯成繁中</button>
      </div>
    )
  }
  if (view.status === 'unavailable') {
    return <p className="update-changelog-tr-note">翻譯狀態暫時讀不到</p>
  }
  if (view.status === 'completed') {
    return (
      <div className="update-changelog-tr">
        <button type="button" onClick={onToggle}>{showOriginal ? '看翻譯' : '看原文'}</button>
      </div>
    )
  }
  if (view.status === 'in_flight' || view.status === 'reserved') {
    return (
      <div className="update-changelog-tr">
        <p className="update-changelog-tr-note">{pollStopped ? '翻譯還在進行' : '正在翻譯…'}</p>
        {pollStopped ? <button type="button" onClick={onRecheck}>再查一次</button> : null}
      </div>
    )
  }
  const ready = retryReady(view.retryAfter, now)
  const lead = view.status === 'interrupted' ? '翻譯中斷' : '翻譯沒有完成'
  return (
    <div className="update-changelog-tr">
      <p className="update-changelog-tr-note">{lead}：{shortReason(view.error)}</p>
      <button type="button" onClick={onRetry} disabled={!ready}>再試一次</button>
    </div>
  )
}
