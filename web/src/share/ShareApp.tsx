import { lazy, Suspense, useCallback, useEffect, useRef, useState } from 'react'
import type { ShareClient } from './shareApi'
import {
  fmtSize,
  mergeMessages,
  SHARE_FILE_MAX,
  SHARE_TEXT_MAX,
  ShareHttpError,
  shareErrorText,
  type ShareFile,
  type ShareMessage,
  type SharePage,
  type ShareStatus,
} from './shareModel'

const ShareMarkdown = lazy(() => import('./ShareMarkdown'))

/** 推播斷了才輪詢；分頁在背景時放慢。 */
const POLL_MS = 4000

interface Pending {
  key: string
  name: string
  id: string | null
  error: string | null
}

function newRequestId(): string {
  const b = new Uint8Array(12)
  crypto.getRandomValues(b)
  return `share-${Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')}`
}

function Bubble({ m }: { m: ShareMessage }) {
  return (
    <div className={`sh-msg ${m.role}`}>
      <div className="sh-bubble">
        {m.role === 'assistant' ? (
          <Suspense fallback={<p className="sh-plain">{m.text}</p>}>
            <ShareMarkdown text={m.text} />
          </Suspense>
        ) : (
          <p className="sh-plain">{m.text}</p>
        )}
        {m.attachments.length > 0 ? (
          <ul className="sh-atts">
            {m.attachments.map((a, i) => (
              <li key={`${a.name}-${i}`}>📎 {a.name}</li>
            ))}
          </ul>
        ) : null}
      </div>
      {m.created_at ? <time className="sh-time">{new Date(m.created_at).toLocaleTimeString('zh-TW', { hour: '2-digit', minute: '2-digit', hour12: false })}</time> : null}
    </div>
  )
}

function FilesPanel({ files, client, onRefresh, onClose }: { files: ShareFile[]; client: ShareClient; onRefresh: () => void; onClose?: () => void }) {
  return (
    <section className="sh-files" aria-label="bot 給你的檔案">
      <div className="sh-files-head">
        <strong>bot 給你的檔案</strong>
        <button type="button" className="sh-link" onClick={onRefresh}>
          重新整理
        </button>
        {onClose ? (
          <button type="button" className="sh-link" onClick={onClose} aria-label="關閉檔案清單">
            ✕
          </button>
        ) : null}
      </div>
      {files.length === 0 ? (
        <p className="sh-empty">還沒有檔案。bot 產生檔案給你時會出現在這裡。</p>
      ) : (
        <ul className="sh-file-list">
          {files.map((f) => (
            <li key={f.name}>
              <a href={client.fileUrl(f.name)} download={f.name} rel="noreferrer">
                <span className="sh-file-name">{f.name}</span>
                <span className="sh-file-size">{fmtSize(f.size)}</span>
              </a>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

export function ShareApp({ client }: { client: ShareClient }) {
  const [botName, setBotName] = useState('')
  const [messages, setMessages] = useState<ShareMessage[]>([])
  const [status, setStatus] = useState<ShareStatus>('idle')
  const [files, setFiles] = useState<ShareFile[]>([])
  const [state, setState] = useState<'loading' | 'ready' | 'gone'>('loading')
  const [netError, setNetError] = useState<string | null>(null)
  const [text, setText] = useState('')
  const [pending, setPending] = useState<Pending[]>([])
  const [sending, setSending] = useState(false)
  const [sendError, setSendError] = useState<string | null>(null)
  const [filesOpen, setFilesOpen] = useState(false)
  const [poll, setPoll] = useState(false)
  const [hasMore, setHasMore] = useState(false)
  const [loadingOlder, setLoadingOlder] = useState(false)
  const [olderError, setOlderError] = useState<string | null>(null)
  // 送出後到 daemon 說「思考中」之前也算在忙，不然按完送出畫面一片靜。
  // 世代號：SSE 若在 POST resolve 之前先到，不能再被後到的 resolve 設回 true。
  const [awaiting, setAwaiting] = useState(false)
  const awaitGen = useRef(0)
  const sendAt = useRef<string | null>(null)
  const listRef = useRef<HTMLDivElement>(null)
  const fileInput = useRef<HTMLInputElement>(null)
  const stick = useRef(true)
  const stopLive = useRef<() => void>(() => {})
  const sawOlder = useRef(false)

  const fail = useCallback((e: unknown) => {
    if (e instanceof ShareHttpError && e.status === 404) {
      stopLive.current()
      setState('gone')
    } else setNetError(shareErrorText(e, 'load'))
  }, [])

  const applyPage = useCallback((page: SharePage) => {
    setBotName(page.bot_name)
    setStatus(page.status)
    setMessages((cur) => mergeMessages(cur, page.messages))
    if (!sawOlder.current) setHasMore(page.has_more)
    const at = sendAt.current
    // 還沒看到這一輪的 working 或新 assistant 之前，idle 的舊頁不能把「思考中」清掉。
    if (at && (page.status === 'working' || page.messages.some((m) => m.role === 'assistant' && m.created_at >= at))) {
      setAwaiting(false)
    }
    setNetError(null)
    setState('ready')
  }, [])

  const load = useCallback(() => client.messages().then(applyPage, fail), [client, applyPage, fail])

  const loadFiles = useCallback(
    () =>
      client.files().then(
        (f) => setFiles(f),
        () => {
          /* 清單抓不到就留著上一份；連結失效由 load() 判斷 */
        },
      ),
    [client],
  )

  useEffect(() => {
    client.messages().then(applyPage, fail)
    client.files().then(
      (f) => setFiles(f),
      () => {},
    )
    const stop = client.subscribe({
      onMessage: (m) => {
        setMessages((cur) => mergeMessages(cur, [m]))
        if (m.role === 'assistant') {
          setAwaiting(false)
          void loadFiles()
        }
      },
      onStatus: (s) => {
        setStatus(s)
        // 只在這一輪送出之後的 working／idle 清掉。POST resolve 不再把 awaiting 設回 true。
        if (sendAt.current) setAwaiting(false)
      },
      onDown: () => setPoll(true),
      onUp: () => setPoll(false),
    })
    stopLive.current = stop
    return () => {
      stop()
      stopLive.current = () => {}
    }
  }, [client, applyPage, fail, loadFiles])

  useEffect(() => {
    if (!poll || state === 'gone') return
    const id = setInterval(() => {
      if (document.visibilityState === 'visible') void load()
    }, POLL_MS)
    return () => clearInterval(id)
  }, [poll, state, load])

  const busy = status === 'working' || awaiting

  useEffect(() => {
    const el = listRef.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [messages, busy])

  const pick = (list: FileList | null) => {
    for (const f of Array.from(list ?? [])) {
      const key = `${f.name}-${f.size}-${Math.random()}`
      if (f.size > SHARE_FILE_MAX) {
        setPending((p) => [...p, { key, name: f.name, id: null, error: '超過 25 MB' }])
        continue
      }
      setPending((p) => [...p, { key, name: f.name, id: null, error: null }])
      client.upload(f).then(
        (r) => setPending((p) => p.map((x) => (x.key === key ? { ...x, id: r.id, name: r.name } : x))),
        (e) => setPending((p) => p.map((x) => (x.key === key ? { ...x, error: shareErrorText(e, 'upload') } : x))),
      )
    }
    if (fileInput.current) fileInput.current.value = ''
  }

  const uploading = pending.some((p) => !p.id && !p.error)
  const ready = pending.filter((p) => p.id)
  const tooLong = text.length > SHARE_TEXT_MAX
  const canSend = !sending && !uploading && !tooLong && (text.trim().length > 0 || ready.length > 0)

  const loadOlder = async () => {
    const oldest = messages[0]?.id
    if (!oldest || loadingOlder) return
    stick.current = false
    setLoadingOlder(true)
    setOlderError(null)
    try {
      const page = await client.messages(oldest)
      sawOlder.current = true
      setMessages((cur) => mergeMessages(cur, page.messages))
      setHasMore(page.has_more)
      setState('ready')
    } catch (e) {
      if (e instanceof ShareHttpError && e.status === 404) {
        stopLive.current()
        setState('gone')
      } else setOlderError(shareErrorText(e, 'load'))
    } finally {
      setLoadingOlder(false)
    }
  }

  const submit = async () => {
    if (!canSend) return
    const gen = ++awaitGen.current
    sendAt.current = new Date().toISOString()
    setAwaiting(true)
    setSending(true)
    setSendError(null)
    try {
      await client.send(text.trim(), newRequestId(), ready.map((p) => p.id!))
      setText('')
      setPending((p) => p.filter((x) => !x.id))
      stick.current = true
      // 不在這裡 setAwaiting(true)：SSE 可能已經先把這一世代清掉。
      if (awaitGen.current === gen) void load()
    } catch (e) {
      if (awaitGen.current === gen) setAwaiting(false)
      if (e instanceof ShareHttpError && e.status === 404) {
        stopLive.current()
        setState('gone')
      }
      setSendError(shareErrorText(e, 'send'))
    } finally {
      setSending(false)
    }
  }

  if (state === 'gone') {
    return (
      <main className="sh-gone">
        <div className="sh-gone-card">
          <div className="sh-gone-icon" aria-hidden="true">
            🔗
          </div>
          <h1>這個分享連結已失效</h1>
          <p>連結可能被關閉或換新了。請向分享給你的人要一條新的連結。</p>
        </div>
      </main>
    )
  }

  return (
    <div className="sh-app">
      <header className="sh-head">
        <div className="sh-title">
          <span className={`sh-dot ${busy ? 'busy' : 'idle'}`} aria-hidden="true" />
          <h1>{botName || '…'}</h1>
          <span className="sh-state">{busy ? '思考中…' : '在線'}</span>
        </div>
        <button type="button" className="sh-files-btn" aria-expanded={filesOpen} onClick={() => setFilesOpen((v) => !v)}>
          檔案{files.length ? `（${files.length}）` : ''}
        </button>
      </header>
      {netError ? (
        <div className="sh-net" role="status">
          {netError}
        </div>
      ) : null}
      <div className="sh-body">
        <div
          className="sh-list"
          ref={listRef}
          onScroll={(e) => {
            const el = e.currentTarget
            stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80
          }}
        >
          {hasMore ? (
            <button type="button" className="sh-link sh-older" disabled={loadingOlder} onClick={() => void loadOlder()}>
              {loadingOlder ? '載入中…' : '載入較早訊息'}
            </button>
          ) : null}
          {olderError ? (
            <p className="sh-send-err" role="alert">
              {olderError}{' '}
              <button type="button" className="sh-link" onClick={() => void loadOlder()}>
                重試
              </button>
            </p>
          ) : null}
          {state === 'loading' ? <p className="sh-empty">載入中…</p> : null}
          {state === 'ready' && messages.length === 0 ? <p className="sh-empty">打個招呼開始對話吧。</p> : null}
          {messages.map((m) => (
            <Bubble key={m.id} m={m} />
          ))}
          {busy ? (
            <div className="sh-msg assistant">
              <div className="sh-bubble sh-thinking" role="status">
                <span className="sh-dots" aria-hidden="true">
                  <i />
                  <i />
                  <i />
                </span>
                思考中…
              </div>
            </div>
          ) : null}
        </div>
        <aside className={`sh-side${filesOpen ? ' open' : ''}`}>
          <FilesPanel files={files} client={client} onRefresh={() => void loadFiles()} onClose={() => setFilesOpen(false)} />
        </aside>
      </div>
      <form
        className="sh-composer"
        onSubmit={(e) => {
          e.preventDefault()
          void submit()
        }}
      >
        {pending.length > 0 ? (
          <ul className="sh-pending">
            {pending.map((p) => (
              <li key={p.key} className={p.error ? 'err' : p.id ? 'ok' : 'up'}>
                <span>📎 {p.name}</span>
                <span className="sh-pending-st">{p.error ?? (p.id ? '' : '上傳中…')}</span>
                <button type="button" aria-label={`移除 ${p.name}`} onClick={() => setPending((x) => x.filter((y) => y.key !== p.key))}>
                  ✕
                </button>
              </li>
            ))}
          </ul>
        ) : null}
        {sendError || tooLong ? (
          <p className="sh-send-err" role="alert">
            {tooLong ? `太長了（${text.length}／${SHARE_TEXT_MAX} 字），請分成幾段送。` : sendError}
          </p>
        ) : null}
        <div className="sh-input-row">
          <input ref={fileInput} type="file" multiple hidden onChange={(e) => pick(e.target.files)} />
          <button type="button" className="sh-attach" aria-label="附加檔案" title="附加檔案（單檔 25 MB 以內）" onClick={() => fileInput.current?.click()}>
            📎
          </button>
          <textarea
            value={text}
            rows={1}
            placeholder={busy ? '對方思考中，可以先打下一則…' : '輸入訊息…'}
            aria-label="訊息"
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              // 手機上 Enter 是換行；桌機 Enter 送出、Shift+Enter 換行。輸入法選字中不送。
              if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing && !window.matchMedia('(pointer: coarse)').matches) {
                e.preventDefault()
                void submit()
              }
            }}
          />
          <button type="submit" className="sh-send" disabled={!canSend}>
            {sending ? '送出中…' : '送出'}
          </button>
        </div>
      </form>
    </div>
  )
}
