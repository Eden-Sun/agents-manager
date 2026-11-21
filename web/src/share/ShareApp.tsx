import { lazy, Suspense, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { createRequestId, settleCreateRequest } from '../lib/createRequestId'
import type { ShareClient } from './shareApi'
import { displayName, imagesByMessage, isImageName } from './shareImage'
import { ShareButton, ShareImageViewer, ShareThumb } from './ShareImages'
import { useT } from './shareI18n'
import { pruneShareImages } from './shareImageCache'
import { decideMessageResponse } from './messageResponse'
import { uploadErrorText, uploadPatiently, uploadRetryable } from './shareUpload'
import {
  fmtSize,
  mergeMessages,
  SHARE_TEXT_MAX,
  shareTextChars,
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

const POLL_MS = 4000
const FILE_POLL_AFTER_REPLY_MS = 60_000

interface Pending {
  key: string
  name: string
  file: File
  id: string | null
  error: string | null
  retry: boolean
  active: boolean
}

const SEND_RETRY_WINDOW_MS = 10 * 60_000

const AWAIT_LIMIT_MS = 90_000

const ATTACH_MAX = 20

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
  const t = useT()
  return (
    <section className="sh-files" aria-label={t('bot 給你的檔案')}>
      <div className="sh-files-head">
        <strong>{t('bot 給你的檔案')}</strong>
        <button type="button" className="sh-link" onClick={onRefresh}>
          {t('重新整理')}
        </button>
        {onClose ? (
          <button type="button" className="sh-link" onClick={onClose} aria-label={t('關閉檔案清單')}>
            ✕
          </button>
        ) : null}
      </div>
      {files.length === 0 ? (
        <p className="sh-empty">{t('還沒有檔案。bot 產生檔案給你時會出現在這裡。')}</p>
      ) : (
        <ul className="sh-file-list">
          {files.map((f) => (
            <li key={f.name} className={isImageName(f.name) ? 'img' : undefined}>
              {isImageName(f.name) ? <ShareThumb file={f} client={client} onOpen={onOpen} /> : null}
              {isImageName(f.name) ? (
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
  const t = useT()
  const [botName, setBotName] = useState('')
  const [messages, setMessages] = useState<ShareMessage[]>([])
  const [status, setStatus] = useState<ShareStatus>('idle')
  const [files, setFiles] = useState<ShareFile[]>([])
  useEffect(() => pruneShareImages(client, files), [client, files])
  const [state, setState] = useState<'loading' | 'ready' | 'gone'>('loading')
  const [goneBlocked, setGoneBlocked] = useState(false)
  const [netError, setNetError] = useState<string | null>(null)
  const [text, setText] = useState('')
  const [pending, setPending] = useState<Pending[]>([])
  const [sending, setSending] = useState(false)
  const [sendError, setSendError] = useState<string | null>(null)
  const [filesOpen, setFilesOpen] = useState(false)
  const [viewing, setViewing] = useState<ShareFile | null>(null)
  const closeViewer = useCallback(() => setViewing(null), [])
  const viewingFile = useMemo(() => (viewing ? (files.find((f) => f.name === viewing.name) ?? viewing) : null), [viewing, files])
  const [poll, setPoll] = useState(false)
  const [filePollUntil, setFilePollUntil] = useState(0)
  const [hasMore, setHasMore] = useState(false)
  const [loadingOlder, setLoadingOlder] = useState(false)
  const [olderError, setOlderError] = useState<string | null>(null)
  const [awaiting, setAwaiting] = useState(false)
  const awaitGen = useRef(0)
  const statusEvents = useRef(0)
  const anchor = useRef<{ at: string; id: string | null } | null>(null)
  const listRef = useRef<HTMLDivElement>(null)
  const fileInput = useRef<HTMLInputElement>(null)
  const uploadJob = useRef<{ key: string; ctl: AbortController } | null>(null)
  const stick = useRef(true)
  const restore = useRef<{ height: number; top: number } | null>(null)
  const stopLive = useRef<() => void>(() => {})
  const sawOlder = useRef(false)
  const loadedOlderMessages = useRef<ShareMessage[]>([])
  const [loadFails, setLoadFails] = useState(0)
  const messageLoadGen = useRef(0)
  const appliedMessageGen = useRef(0)
  const settledMessageGen = useRef(0)
  const linkGone = useRef(false)
  const messageResyncGen = useRef(0)
  const appliedResyncGen = useRef(0)
  const resyncDelta = useRef<{ gen: number; msg: ShareMessage }[]>([])
  const wasDown = useRef(false)
  const lastAssistant = useRef<string | null>(null)
  const pageSeen = useRef(false)
  const fileLoadGen = useRef(0)
  const appliedFileGen = useRef(0)

  const fail = useCallback((e: unknown) => {
    if (e instanceof ShareHttpError && (e.status === 401 || e.status === 403)) {
      linkGone.current = true
      stopLive.current()
      setGoneBlocked(true)
      setState('gone')
    } else if (e instanceof ShareHttpError && e.status === 404) {
      linkGone.current = true
      stopLive.current()
      setState('gone')
    } else {
      setNetError(shareErrorText(e, 'load'))
      setLoadFails((n) => n + 1)
    }
  }, [])

  const alive = useRef(true)
  useEffect(() => () => {
    alive.current = false
  }, [])
  const loadFiles = useCallback(
    () => {
      const gen = ++fileLoadGen.current
      return client.files().then(
        (f) => {
          if (!alive.current) return
          const decision = decideMessageResponse({
            requestGen: gen,
            latestIssuedGen: fileLoadGen.current,
            kind: 'success',
            appliedGen: appliedFileGen.current,
            settledGen: appliedFileGen.current,
            state: linkGone.current ? 'gone' : 'active',
          })
          if (decision !== 'apply') return
          appliedFileGen.current = gen
          setFiles(f)
        },
        () => {
        },
      )
    },
    [client],
  )

  const applyPage = useCallback((page: SharePage, replace: boolean, statusAt: number, resyncGen: number) => {
    const statusFresh = statusAt === statusEvents.current
    setBotName(page.bot_name)
    if (statusFresh) setStatus(page.status)
    if (replace) {
      const replay = resyncDelta.current.filter((d) => d.gen >= resyncGen)
      resyncDelta.current = resyncDelta.current.filter((d) => d.gen > resyncGen)
      const snapshotTail = mergeMessages([], page.messages).at(-1)
      const retainedOlder = snapshotTail
        ? loadedOlderMessages.current.filter((m) => m.created_at < snapshotTail.created_at || (m.created_at === snapshotTail.created_at && m.id.localeCompare(snapshotTail.id) <= 0))
        : []
      loadedOlderMessages.current = retainedOlder
      setMessages(mergeMessages(retainedOlder, mergeMessages(page.messages, replay.map((d) => d.msg))))
    } else setMessages((cur) => mergeMessages(cur, page.messages))
    const newest = page.messages.findLast((m) => m.role === 'assistant')?.id ?? null
    const first = !pageSeen.current
    pageSeen.current = true
    if (newest !== lastAssistant.current) {
      lastAssistant.current = newest
      if (!first && newest) {
        setFilePollUntil(Date.now() + FILE_POLL_AFTER_REPLY_MS)
        void loadFiles()
      }
    }
    if (!sawOlder.current) setHasMore(page.has_more)
    const a = anchor.current
    if (a) {
      const at = a.id ? page.messages.findIndex((m) => m.id === a.id) : -1
      const answered = a.id
        ? at >= 0 && page.messages.slice(at + 1).some((m) => m.role === 'assistant')
        : page.messages.some((m) => m.role === 'assistant' && m.created_at >= a.at)
      if ((statusFresh && page.status === 'working') || answered) setAwaiting(false)
    }
    setNetError(null)
    setLoadFails(0)
    setState('ready')
  }, [loadFiles])

  const load = useCallback(() => {
    const gen = ++messageLoadGen.current
    const resyncGen = messageResyncGen.current
    const statusAt = statusEvents.current
    return client.messages().then(
      (page) => {
        const decision = decideMessageResponse({
          requestGen: gen,
          latestIssuedGen: messageLoadGen.current,
          kind: 'success',
          appliedGen: appliedMessageGen.current,
          settledGen: settledMessageGen.current,
          state: linkGone.current ? 'gone' : 'active',
        })
        if (decision !== 'apply') return
        appliedMessageGen.current = gen
        settledMessageGen.current = Math.max(settledMessageGen.current, gen)
        const replace = resyncGen > appliedResyncGen.current
        if (replace) appliedResyncGen.current = resyncGen
        applyPage(page, replace, statusAt, resyncGen)
      },
      (e) => {
        const kind = e instanceof ShareHttpError && e.status === 404 ? 'not_found' : 'failure'
        const decision = decideMessageResponse({
          requestGen: gen,
          latestIssuedGen: messageLoadGen.current,
          kind,
          appliedGen: appliedMessageGen.current,
          settledGen: settledMessageGen.current,
          state: linkGone.current ? 'gone' : 'active',
        })
        if (decision === 'discard') return
        if (decision === 'expire') {
          linkGone.current = true
          fail(e)
          return
        }
        settledMessageGen.current = Math.max(settledMessageGen.current, gen)
        fail(e)
      },
    )
  }, [client, applyPage, fail])

  useEffect(() => {
    void load()
    void loadFiles()
    const stop = client.subscribe({
      onMessage: (m) => {
        setMessages((cur) => mergeMessages(cur, [m]))
        if (messageResyncGen.current > appliedResyncGen.current) resyncDelta.current.push({ gen: messageResyncGen.current, msg: m })
        if (m.role === 'assistant') {
          lastAssistant.current = m.id
          setAwaiting(false)
          setFilePollUntil(Date.now() + FILE_POLL_AFTER_REPLY_MS)
          void loadFiles()
        }
      },
      onStatus: (s) => {
        statusEvents.current++
        setStatus(s)
        if (anchor.current) setAwaiting(false)
      },
      onResync: () => {
        messageResyncGen.current++
        void load().then(() => loadFiles())
      },
      onDown: () => {
        wasDown.current = true
        if (messageResyncGen.current <= appliedResyncGen.current) messageResyncGen.current++
        setPoll(true)
      },
      onUp: () => {
        setPoll(false)
        if (wasDown.current) {
          wasDown.current = false
          if (messageResyncGen.current <= appliedResyncGen.current) messageResyncGen.current++
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
      if (document.visibilityState === 'visible') {
        if (messageResyncGen.current <= appliedResyncGen.current) messageResyncGen.current++
        void load()
      }
    }, POLL_MS)
    return () => clearInterval(id)
  }, [poll, state, load])

  useEffect(() => {
    if (state !== 'ready' || !filePollUntil) return
    const id = setInterval(() => {
      if (Date.now() >= filePollUntil) {
        clearInterval(id)
        return
      }
      if (document.visibilityState === 'visible') void loadFiles()
    }, POLL_MS)
    return () => clearInterval(id)
  }, [state, filePollUntil, loadFiles])

  useEffect(() => {
    if (loadFails === 0 || poll || state === 'gone') return
    const t = setTimeout(() => void load(), POLL_MS)
    return () => clearTimeout(t)
  }, [loadFails, poll, state, load])

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

  useEffect(() => {
    if (!awaiting) return
    const timer = setTimeout(() => {
      setAwaiting(false)
      setSendError(t('沒等到回應，可以再送一次。'))
    }, AWAIT_LIMIT_MS)
    return () => clearTimeout(timer)
  }, [awaiting, t])

  const busy = status === 'working' || awaiting
  const imagesOf = useMemo(() => imagesByMessage(messages, files), [messages, files])

  useEffect(() => {
    const el = listRef.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [messages, busy])

  useLayoutEffect(() => {
    const r = restore.current
    const el = listRef.current
    if (!r || !el) return
    restore.current = null
    el.scrollTop = r.top + (el.scrollHeight - r.height)
  }, [messages])

  const pick = (list: FileList | null) => {
    const picked = Array.from(list ?? []).map((f) => ({ key: `${f.name}-${f.size}-${Math.random()}`, name: f.name, file: f, id: null, error: null, retry: false, active: false }))
    if (picked.length) setPending((p) => [...p, ...picked])
    if (fileInput.current) fileInput.current.value = ''
  }

  const removePending = (key: string) => {
    if (uploadJob.current?.key === key) {
      uploadJob.current.ctl.abort()
      uploadJob.current = null
    }
    setPending((x) => x.filter((y) => y.key !== key))
  }

  useEffect(
    () => () => {
      uploadJob.current?.ctl.abort()
      uploadJob.current = null
    },
    [],
  )

  useEffect(() => {
    if (uploadJob.current) return
    const next = pending.find((p) => !p.id && !p.error && !p.active)
    if (!next) return
    const job = { key: next.key, ctl: new AbortController() }
    uploadJob.current = job
    const patch = (fields: Partial<Pending>) => setPending((p) => p.map((x) => (x.key === next.key ? { ...x, ...fields } : x)))
    patch({ active: true })
    uploadPatiently(client, next.file, { signal: job.ctl.signal }).then(
      (r) => {
        if (uploadJob.current !== job) return
        uploadJob.current = null
        patch({ id: r.id, name: r.name, active: false })
      },
      (e) => {
        if (uploadJob.current !== job) return
        uploadJob.current = null
        if (e instanceof ShareHttpError && e.status === 404) {
          linkGone.current = true
          stopLive.current()
          setState('gone')
        }
        patch({ error: uploadErrorText(e), retry: uploadRetryable(e), active: false })
      },
    )
  }, [pending, client])

  const uploading = pending.some((p) => !p.id && !p.error)
  const pendingStatus = (p: Pending): string => {
    if (p.error) return p.error
    if (p.id) return ''
    if (!p.active) return t('等待中')
    return pending.length > 1
      ? t('上傳中（第 {idx} 張／共 {total} 張）', { idx: pending.indexOf(p) + 1, total: pending.length })
      : t('上傳中…')
  }
  const ready = pending.filter((p) => p.id)
  const textChars = shareTextChars(text)
  const tooLong = textChars > SHARE_TEXT_MAX
  const tooMany = ready.length > ATTACH_MAX
  const failedAttach = pending.some((p) => p.error !== null)
  const canSend = !busy && !sending && !uploading && !failedAttach && !tooLong && !tooMany && (text.trim().length > 0 || ready.length > 0)

  const loadOlder = async () => {
    const oldest = messages[0]?.id
    if (!oldest || loadingOlder) return
    stick.current = false
    setLoadingOlder(true)
    setOlderError(null)
    const resyncGen = messageResyncGen.current
    try {
      const page = await client.messages(oldest)
      if (linkGone.current) return
      if (messageResyncGen.current !== resyncGen) return
      const el = listRef.current
      restore.current = el ? { height: el.scrollHeight, top: el.scrollTop } : null
      sawOlder.current = true
      loadedOlderMessages.current = mergeMessages(loadedOlderMessages.current, page.messages)
      setMessages((cur) => mergeMessages(cur, page.messages))
      setHasMore(page.has_more)
      setState('ready')
    } catch (e) {
      if (e instanceof ShareHttpError && e.status === 404) {
        linkGone.current = true
        stopLive.current()
        setState('gone')
      } else if (e instanceof ShareHttpError && e.status === 400 && e.reason === 'bad_cursor') {
        messageResyncGen.current++
        void load()
      } else setOlderError(shareErrorText(e, 'older'))
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
    const reqKey = `share-send:${body}\u0000${ids.join(',')}`
    try {
      const res = await client.send(body, sendRequestId(reqKey), ids)
      settleCreateRequest(reqKey)
      if (res?.delivery === 'failed') {
        anchor.current = null
        if (awaitGen.current === gen) setAwaiting(false)
        setSendError(t('bot 沒收到這一則，請再送一次；你打的字還在。'))
        void load()
        return
      }
      if (res?.messageId && anchor.current && awaitGen.current === gen) anchor.current = { ...anchor.current, id: res.messageId }
      setText((cur) => (cur.trim() === body ? '' : cur))
      setPending((p) => p.filter((x) => !(x.id && ids.includes(x.id))))
      stick.current = true
      if (res?.delivery === 'unknown') setSendError(t('送出了，但還不確定 bot 有沒有收到，稍等一下。'))
      if (awaitGen.current === gen) void load()
    } catch (e) {
      if (e instanceof ShareHttpError) settleCreateRequest(reqKey)
      if (awaitGen.current === gen) setAwaiting(false)
      if (e instanceof ShareHttpError && e.status === 404) {
        linkGone.current = true
        stopLive.current()
        setState('gone')
      }
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
          {goneBlocked ? (
            <>
              <h1>{t('這個頁面打不開')}</h1>
              <p>{t('請用分享給你的人提供的網址重新開啟。')}</p>
            </>
          ) : (
            <>
              <h1>{t('這個分享連結已失效')}</h1>
              <p>{t('連結可能被關閉或換新了。請向分享給你的人要一條新的連結。')}</p>
            </>
          )}
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
          <span className="sh-state">{busy ? t('思考中…') : t('在線')}</span>
        </div>
        <button type="button" className="sh-files-btn" aria-expanded={filesOpen} onClick={() => setFilesOpen((v) => !v)}>
          {files.length ? t('檔案（{count}）', { count: files.length }) : t('檔案')}
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
              {loadingOlder ? t('載入中…') : t('載入較早訊息')}
            </button>
          ) : null}
          {olderError ? (
            <p className="sh-send-err" role="alert">
              {olderError}{' '}
              <button type="button" className="sh-link" onClick={() => void loadOlder()}>
                {t('重試')}
              </button>
            </p>
          ) : null}
          {state === 'loading' ? <p className="sh-empty">{t('載入中…')}</p> : null}
          {state === 'ready' && messages.length === 0 ? <p className="sh-empty">{t('打個招呼開始對話吧。')}</p> : null}
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
                {t('思考中…')}
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
                <span className="sh-pending-st" role={p.error ? 'alert' : p.id ? undefined : 'status'}>{pendingStatus(p)}</span>
                {p.error && p.retry ? (
                  <button type="button" className="sh-retry" onClick={() => setPending((x) => x.map((y) => (y.key === p.key ? { ...y, error: null, retry: false } : y)))}>
                    {t('再試一次')}
                  </button>
                ) : null}
                <button type="button" aria-label={t('移除 {name}', { name: p.name })} onClick={() => removePending(p.key)}>
                  ✕
                </button>
              </li>
            ))}
          </ul>
        ) : null}
        {sendError || tooLong || tooMany || failedAttach ? (
          <p className="sh-send-err" role="alert">
            {tooMany
              ? t('一次最多送 {max} 個檔案，請先移除 {count} 個，剩下的下一則再送。', { max: ATTACH_MAX, count: ready.length - ATTACH_MAX })
              : tooLong
                ? t('太長了（{chars}／{max} 字），請分成幾段送。', { chars: textChars, max: SHARE_TEXT_MAX })
                : failedAttach
                  ? t('有附件沒傳上去，請移除它或再試一次，再送出。')
                  : sendError}
          </p>
        ) : null}
        <div className="sh-input-row">
          <input ref={fileInput} type="file" multiple hidden onChange={(e) => pick(e.target.files)} />
          <button type="button" className="sh-attach" aria-label={t('附加檔案')} title={t('附加檔案（單檔 25 MB 以內）')} onClick={() => fileInput.current?.click()}>
            📎
          </button>
          <textarea
            value={text}
            rows={1}
            placeholder={busy ? t('AI 思考中，可以先打下一則…') : t('輸入訊息…')}
            aria-label={t('訊息')}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing && e.nativeEvent.keyCode !== 229 && !window.matchMedia('(pointer: coarse)').matches) {
                e.preventDefault()
                void submit()
              }
            }}
          />
          <button type="submit" className="sh-send" disabled={!canSend} title={busy ? t('等 bot 回完再送') : undefined}>
            {sending ? t('送出中…') : t('送出')}
          </button>
        </div>
        {busy && !sending ? (
          <p className="sh-wait" role="status">
            {t('等 bot 回完再送（可以先打好）')}
          </p>
        ) : null}
      </form>
      {viewingFile ? <ShareImageViewer file={viewingFile} client={client} onClose={closeViewer} /> : null}
    </div>
  )
}
