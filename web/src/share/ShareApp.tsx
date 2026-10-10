import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { createRequestId, settleCreateRequest } from '../lib/createRequestId'
import type { ShareClient } from './shareApi'
import { displayName, imagesByMessage, isImageName } from './shareImage'
import { ShareButton, ShareImageViewer, ShareThumb } from './ShareImages'
import { uploadErrorText, uploadPatiently, uploadRetryable } from './shareUpload'
import {
  fmtSize,
  mergeMessages,
  SHARE_TEXT_MAX,
  ShareHttpError,
  shareErrorText,
  stripShareMarks,
  type ShareFile,
  type ShareMessage,
  type SharePage,
  type ShareStatus,
} from './shareModel'

const ShareMarkdown = lazy(() => import('./ShareMarkdown'))

const NO_IMAGES: ShareFile[] = []

/** 推播斷了才輪詢；分頁在背景時放慢。 */
const POLL_MS = 4000

/** 選了的檔：一次只傳一個（`active`），其餘排著等；`id` 有了＝傳好了，`error`＝這張沒傳上去（`retry`＝可以再試一次）。 */
interface Pending {
  key: string
  name: string
  file: File
  id: string | null
  error: string | null
  retry: boolean
  active: boolean
}

/** 重送同一句話的冪等鍵保留多久（同主 UI 的 `SEND_RETRY_WINDOW_MS` 量級）：回應遺失時使用者照提示再按一次要拿到同一個鍵。 */
const SEND_RETRY_WINDOW_MS = 10 * 60_000

/** 送出後多久還沒等到 working／回覆，就放開送出鈕（SSE 與輪詢都漏掉時的最後出口，#922）。 */
const AWAIT_LIMIT_MS = 90_000

/** daemon 的 `client_request_id` 只收 `[A-Za-z0-9-_.:]{1,64}`；`createRequestId` 回 UUID，加前綴後仍在範圍內。 */
function sendRequestId(key: string): string {
  return `share-${createRequestId(key, { maxAgeMs: SEND_RETRY_WINDOW_MS })}`.replace(/[^A-Za-z0-9\-_.:]/g, '_').slice(0, 64)
}

function Bubble({ m, images, client, onOpen }: { m: ShareMessage; images: ShareFile[]; client: ShareClient; onOpen: (f: ShareFile) => void }) {
  const text = stripShareMarks(m.text)
  return (
    <div className={`sh-msg ${m.role}`}>
      <div className="sh-bubble">
        {m.role === 'assistant' ? (
          <Suspense fallback={<p className="sh-plain">{text}</p>}>
            <ShareMarkdown text={text} />
          </Suspense>
        ) : (
          <p className="sh-plain">{text}</p>
        )}
        {m.attachments.length > 0 ? (
          <ul className="sh-atts">
            {m.attachments.map((a, i) => (
              <li key={`${a.name}-${i}`}>📎 {a.name}</li>
            ))}
          </ul>
        ) : null}
        {images.length > 0 ? (
          <div className="sh-imgs">
            {images.map((f) => (
              <figure key={f.name} className="sh-img">
                <ShareThumb file={f} client={client} onOpen={onOpen} big />
                <ShareButton file={f} client={client} />
              </figure>
            ))}
          </div>
        ) : null}
      </div>
      {m.created_at ? <time className="sh-time">{new Date(m.created_at).toLocaleTimeString('zh-TW', { hour: '2-digit', minute: '2-digit', hour12: false })}</time> : null}
    </div>
  )
}

function FilesPanel({
  files,
  client,
  onRefresh,
  onClose,
  onOpen,
}: {
  files: ShareFile[]
  client: ShareClient
  onRefresh: () => void
  onClose?: () => void
  onOpen: (f: ShareFile) => void
}) {
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
            <li key={f.name} className={isImageName(f.name) ? 'img' : undefined}>
              {isImageName(f.name) ? <ShareThumb file={f} client={client} onOpen={onOpen} /> : null}
              {isImageName(f.name) ? (
                // 圖對 end user 就是「一張圖」：不顯示副檔名、不給原檔，動作只有「分享／存到手機」。
                <div className="sh-file-png">
                  <button type="button" className="sh-file-name sh-file-open" onClick={() => onOpen(f)}>
                    {displayName(f.name)}
                  </button>
                  <ShareButton file={f} client={client} />
                </div>
              ) : (
                <a href={client.fileUrl(f.name)} download={f.name} rel="noreferrer">
                  <span className="sh-file-name">{f.name}</span>
                  <span className="sh-file-size">{fmtSize(f.size)}</span>
                </a>
              )}
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
  const [viewing, setViewing] = useState<ShareFile | null>(null)
  const closeViewer = useCallback(() => setViewing(null), [])
  const [poll, setPoll] = useState(false)
  const [hasMore, setHasMore] = useState(false)
  const [loadingOlder, setLoadingOlder] = useState(false)
  const [olderError, setOlderError] = useState<string | null>(null)
  // 送出後到 daemon 說「思考中」之前也算在忙，不然按完送出畫面一片靜。
  // 世代號：SSE 若在 POST resolve 之前先到，不能再被後到的 resolve 設回 true。
  const [awaiting, setAwaiting] = useState(false)
  const awaitGen = useRef(0)
  /** 這一輪送出的錨點：`at` 是瀏覽器時鐘（只在 POST 還沒回 `message_id` 之前當備用），`id` 是 daemon 回的那則 user 訊息。 */
  const anchor = useRef<{ at: string; id: string | null } | null>(null)
  const listRef = useRef<HTMLDivElement>(null)
  const fileInput = useRef<HTMLInputElement>(null)
  const uploadBusy = useRef(false)
  const stick = useRef(true)
  const stopLive = useRef<() => void>(() => {})
  const sawOlder = useRef(false)
  /** `load` 連續失敗幾次（非 404）；> 0 而且沒在輪詢時，自己排下一次重試。 */
  const [loadFails, setLoadFails] = useState(0)
  /** resync 要求「下一份成功的頁面取代手上的清單」；重抓失敗時留著，重試成功照樣取代。 */
  const replaceNext = useRef(false)
  /** SSE 斷過：重新連上時要補抓一次（斷線期間的事件不會重播）。 */
  const wasDown = useRef(false)
  /** 手上看過的最新一則 bot 回覆；頁面帶來更新的一則＝bot 剛回完，檔案清單要重抓（輪詢模式沒有 SSE 的 message 事件）。 */
  const lastAssistant = useRef<string | null>(null)
  const pageSeen = useRef(false)

  const fail = useCallback((e: unknown) => {
    if (e instanceof ShareHttpError && e.status === 404) {
      stopLive.current()
      setState('gone')
    } else {
      setNetError(shareErrorText(e, 'load'))
      setLoadFails((n) => n + 1)
    }
  }, [])

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

  const applyPage = useCallback((page: SharePage) => {
    setBotName(page.bot_name)
    setStatus(page.status)
    if (replaceNext.current) {
      replaceNext.current = false
      sawOlder.current = false
      setMessages(mergeMessages([], page.messages))
    } else setMessages((cur) => mergeMessages(cur, page.messages))
    const newest = page.messages.findLast((m) => m.role === 'assistant')?.id ?? null
    const first = !pageSeen.current
    pageSeen.current = true
    if (newest !== lastAssistant.current) {
      lastAssistant.current = newest
      // 第一頁不抓：開頁本來就抓了一次。
      if (!first && newest) void loadFiles()
    }
    if (!sawOlder.current) setHasMore(page.has_more)
    const a = anchor.current
    // 還沒看到這一輪的 working 或新 assistant 之前，idle 的舊頁不能把「思考中」清掉。
    // 有 `message_id` 就看「那則 user 訊息之後有沒有 assistant」，不比時鐘（手機時鐘快過 daemon 時 created_at 永遠對不上）。
    if (a) {
      const at = a.id ? page.messages.findIndex((m) => m.id === a.id) : -1
      const answered = a.id
        ? at >= 0 && page.messages.slice(at + 1).some((m) => m.role === 'assistant')
        : page.messages.some((m) => m.role === 'assistant' && m.created_at >= a.at)
      if (page.status === 'working' || answered) setAwaiting(false)
    }
    setNetError(null)
    setLoadFails(0)
    setState('ready')
  }, [loadFiles])

  const load = useCallback(() => client.messages().then(applyPage, fail), [client, applyPage, fail])

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
          lastAssistant.current = m.id
          setAwaiting(false)
          void loadFiles()
        }
      },
      onStatus: (s) => {
        setStatus(s)
        // 只在這一輪送出之後的 working／idle 清掉。POST resolve 不再把 awaiting 設回 true。
        if (anchor.current) setAwaiting(false)
      },
      // 漏了事件或對話被倒回：整頁重抓，以重抓結果取代手上的清單（倒回的那幾則要消失，合併不會刪）。
      onResync: () => {
        replaceNext.current = true
        void load().then(() => loadFiles())
      },
      onDown: () => {
        wasDown.current = true
        setPoll(true)
      },
      onUp: () => {
        setPoll(false)
        if (wasDown.current) {
          wasDown.current = false
          void load()
          void loadFiles()
        }
      },
    })
    stopLive.current = stop
    return () => {
      stop()
      stopLive.current = () => {}
    }
  }, [client, applyPage, fail, loadFiles, load])

  useEffect(() => {
    if (!poll || state === 'gone') return
    const id = setInterval(() => {
      if (document.visibilityState === 'visible') void load()
    }, POLL_MS)
    return () => clearInterval(id)
  }, [poll, state, load])

  // SSE 連著時 messages 抓失敗：輪詢沒開，沒有人會再抓。自己隔 POLL_MS 再試，成功（applyPage）就歸零。
  useEffect(() => {
    if (loadFails === 0 || poll || state === 'gone') return
    const t = setTimeout(() => void load(), POLL_MS)
    return () => clearTimeout(t)
  }, [loadFails, poll, state, load])

  // 背景時不輪詢、SSE 也可能已經死了還沒報錯：回到前景先補抓一次。
  useEffect(() => {
    if (state === 'gone') return
    const onVisible = () => {
      if (document.visibilityState !== 'visible') return
      void load()
      void loadFiles()
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => document.removeEventListener('visibilitychange', onVisible)
  }, [state, load, loadFiles])

  // 90 秒上限：送出後一直沒等到 working／回覆（SSE 與輪詢都漏掉、或 bot 根本沒收到），不能讓送出鈕永遠灰著。
  useEffect(() => {
    if (!awaiting) return
    const t = setTimeout(() => {
      setAwaiting(false)
      setSendError('沒等到回應，可以再送一次。')
    }, AWAIT_LIMIT_MS)
    return () => clearTimeout(t)
  }, [awaiting])

  const busy = status === 'working' || awaiting
  // bot 這一回合做的圖（與回覆裡提到的）直接畫在那則回覆下面。
  const imagesOf = useMemo(() => imagesByMessage(messages, files), [messages, files])

  useEffect(() => {
    const el = listRef.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [messages, busy])

  // 一次選好幾張照片：一張接一張傳（客訴 2026-10-04：同時送出時第三張起就「傳得太快了」）。429 在 `uploadPatiently` 裡自己等著重試。
  const pick = (list: FileList | null) => {
    const picked = Array.from(list ?? []).map((f) => ({ key: `${f.name}-${f.size}-${Math.random()}`, name: f.name, file: f, id: null, error: null, retry: false, active: false }))
    if (picked.length) setPending((p) => [...p, ...picked])
    if (fileInput.current) fileInput.current.value = ''
  }

  useEffect(() => {
    if (uploadBusy.current) return
    const next = pending.find((p) => !p.id && !p.error && !p.active)
    if (!next) return
    uploadBusy.current = true
    const patch = (fields: Partial<Pending>) => setPending((p) => p.map((x) => (x.key === next.key ? { ...x, ...fields } : x)))
    patch({ active: true })
    uploadPatiently(client, next.file).then(
      (r) => {
        uploadBusy.current = false
        patch({ id: r.id, name: r.name, active: false })
      },
      (e) => {
        uploadBusy.current = false
        if (e instanceof ShareHttpError && e.status === 404) {
          stopLive.current()
          setState('gone')
        }
        patch({ error: uploadErrorText(e), retry: uploadRetryable(e), active: false })
      },
    )
  }, [pending, client])

  const uploading = pending.some((p) => !p.id && !p.error)
  // 「第幾張／共幾張」：這一批還沒送出的檔（傳好的、排著的、失敗的都算）。
  const pendingStatus = (p: Pending): string => {
    if (p.error) return p.error
    if (p.id) return ''
    if (!p.active) return '等待中'
    return pending.length > 1 ? `上傳中（第 ${pending.indexOf(p) + 1} 張／共 ${pending.length} 張）` : '上傳中…'
  }
  const ready = pending.filter((p) => p.id)
  const tooLong = text.length > SHARE_TEXT_MAX
  // bot 還沒回完不給送：daemon 一段對話同時只排一則，再送會 409（API.md §5.6）。
  const canSend = !busy && !sending && !uploading && !tooLong && (text.trim().length > 0 || ready.length > 0)

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
    anchor.current = { at: new Date().toISOString(), id: null }
    setAwaiting(true)
    setSending(true)
    setSendError(null)
    const body = text.trim()
    const ids = ready.map((p) => p.id!)
    // 冪等鍵跟著「這句話＋這些附件」走（#921）：回應遺失（網路斷在 daemon 收下之後）時照提示再按一次，要拿到同一個鍵，
    // daemon 才認得是重送。只有 daemon 明確回了（`ShareHttpError`）才作廢；改了字或附件就是新的一則、新的鍵。
    const reqKey = `share-send:${body}\u0000${ids.join(',')}`
    try {
      const res = await client.send(body, sendRequestId(reqKey), ids)
      settleCreateRequest(reqKey)
      if (res?.delivery === 'failed') {
        // daemon 收下了、但 bot 沒收到（pane 打不進字、附件綁定失敗…）：不會有任何回覆事件，字與附件都留著讓人再送。
        anchor.current = null
        if (awaitGen.current === gen) setAwaiting(false)
        setSendError('bot 沒收到這一則，請再送一次；你打的字還在。')
        void load()
        return
      }
      if (res?.messageId && anchor.current && awaitGen.current === gen) anchor.current = { ...anchor.current, id: res.messageId }
      // 送出期間又打了字：留著（輸入框送出中沒有停用）。
      setText((cur) => (cur.trim() === body ? '' : cur))
      // 只拿掉這一則真的帶走的附件；送出期間才傳好的留著給下一則。
      setPending((p) => p.filter((x) => !(x.id && ids.includes(x.id))))
      stick.current = true
      if (res?.delivery === 'unknown') setSendError('送出了，但還不確定 bot 有沒有收到，稍等一下。')
      // 不在這裡 setAwaiting(true)：SSE 可能已經先把這一世代清掉。
      if (awaitGen.current === gen) void load()
    } catch (e) {
      if (e instanceof ShareHttpError) settleCreateRequest(reqKey)
      if (awaitGen.current === gen) setAwaiting(false)
      if (e instanceof ShareHttpError && e.status === 404) {
        stopLive.current()
        setState('gone')
      }
      // 409：上一則還在排，字與附件都留著（setText／清附件只在成功時做），重抓一次讓「思考中」對上 daemon 的狀態。
      if (e instanceof ShareHttpError && e.status === 409) void load()
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
            <Bubble key={m.id} m={m} images={imagesOf.get(m.id) ?? NO_IMAGES} client={client} onOpen={setViewing} />
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
          <FilesPanel files={files} client={client} onRefresh={() => void loadFiles()} onClose={() => setFilesOpen(false)} onOpen={setViewing} />
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
                <span className="sh-pending-st">{pendingStatus(p)}</span>
                {p.error && p.retry ? (
                  <button type="button" className="sh-retry" onClick={() => setPending((x) => x.map((y) => (y.key === p.key ? { ...y, error: null, retry: false } : y)))}>
                    再試一次
                  </button>
                ) : null}
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
            placeholder={busy ? 'AI 思考中，可以先打下一則…' : '輸入訊息…'}
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
          <button type="submit" className="sh-send" disabled={!canSend} title={busy ? '等 bot 回完再送' : undefined}>
            {sending ? '送出中…' : '送出'}
          </button>
        </div>
        {busy && !sending ? (
          <p className="sh-wait" role="status">
            等 bot 回完再送（可以先打好）
          </p>
        ) : null}
      </form>
      {viewing ? <ShareImageViewer file={viewing} client={client} onClose={closeViewer} /> : null}
    </div>
  )
}
