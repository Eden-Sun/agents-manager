/**
 * 右半面板的「預覽」分頁（issue #253）：頂層 bot 的專案起 vite dev server，iframe 內嵌顯示。
 * 狀態來自 store.previews（WS `preview_changed` 即時寫入）；進來與 daemon 重連時 GET 一次補齊 dir／error。
 */
import { useCallback, useEffect, useState } from 'react'
import { groupOthers, fetchPreview, type PreviewOther, type PreviewRelation, type StartPreviewOpts, previewApiMissing, PREVIEW_API_MISSING, previewUrl, startPreview, stopPreview, PREVIEW_OFF, type Preview } from '../api/preview'
import { isMock } from '../api'
import { ApiError } from '../api/types'
import { useStore } from '../store/store'
import './previewPanel.css'

/** 錯誤訊息＋試過的路徑（路徑可能很長，收進可展開的細節）。 */
interface PreviewErr {
  text: string
  tried: string[]
  /** 偵測不到 vite 設定檔：只是「AG Man 自己起不了」，不是死路——下面的 others 照樣能接。 */
  noConfig: boolean
}

function startError(e: unknown): PreviewErr {
  const plain = (text: string): PreviewErr => ({ text, tried: [], noConfig: false })
  if (previewApiMissing(e)) return plain(PREVIEW_API_MISSING)
  if (e instanceof ApiError) {
    if (e.body.reason === 'not_top_level' || e.body.error === 'not_top_level') return plain('只有頂層 bot 能開預覽。')
    if (e.body.reason === 'no_vite_config' || e.body.error === 'no_vite_config') {
      const tried = Array.isArray(e.body.tried) ? e.body.tried.map(String) : []
      return { text: '這顆 bot 的目錄裡找不到 vite 設定檔，AG Man 沒辦法自己起。', tried, noConfig: true }
    }
    return plain(e.message)
  }
  return plain(e instanceof Error ? e.message : String(e))
}

/** 次要說明：降一級的小字，試過的路徑收進 <details>。 */
function ErrNote({ err }: { err: PreviewErr }) {
  return (
    <div className={`preview-note${err.noConfig ? ' soft' : ' error'}`} role="alert">
      <span>{err.text}</span>
      {err.tried.length > 0 ? (
        <details className="preview-tried">
          <summary>試過的路徑（{err.tried.length}）</summary>
          <ul>
            {err.tried.map((t) => (
              <li key={t}>
                <code>{t}</code>
              </li>
            ))}
          </ul>
        </details>
      ) : null}
    </div>
  )
}

/** mock 模式沒有真的 vite：iframe 放一頁說明，截圖與手動驗才看得到「running」長相。 */
const MOCK_DOC = `<!doctype html><meta charset="utf-8"><body style="font:14px system-ui;margin:0;display:grid;place-items:center;height:100vh;background:#f6f8fa;color:#1b2430"><div style="text-align:center"><h2 style="margin:0 0 6px">Vite dev server</h2><p style="margin:0;color:#5e6a78">（mock 預覽畫面）</p></div>`

const RELATION_TITLE: Record<PreviewRelation, string> = {
  same_dir: '這顆 bot 自己的目錄',
  same_repo: '同一個 repo 的其他 checkout',
  other: '其他專案',
}

const RELATION_NOTE: Record<PreviewRelation, string> = {
  same_dir: '',
  same_repo: '另一份 checkout，看到的不是這顆 bot 工作樹裡的程式碼',
  other: '不是這顆 bot 的程式碼',
}

/** 本機所有在跑的 vite，依 relation 分組；選了就 attach（斷開時不會關掉對方）。 */
function OthersList({ others, busy, onAttach }: { others: PreviewOther[]; busy: boolean; onAttach: (port: number) => void }) {
  if (others.length === 0) return null
  return (
    <div className="preview-others">
      <p className="preview-body">本機已開的 vite，選一個直接接上：</p>
      {groupOthers(others).map((g) => (
        <section key={g.relation} className={`preview-group ${g.relation}`}>
          <h3 className="preview-group-title">
            {RELATION_TITLE[g.relation]}
            {RELATION_NOTE[g.relation] ? <span className="preview-group-note">{RELATION_NOTE[g.relation]}</span> : null}
          </h3>
          <ul>
            {g.items.map((o) => (
              <li key={o.port}>
                <span className="preview-other-dir">
                  :{o.port} · <code>{o.dir}</code>
                  {o.repo ? <span className="preview-other-repo"> ({o.repo})</span> : null}
                </span>
                <button type="button" className="btn preview-btn" disabled={busy} onClick={() => onAttach(o.port)}>
                  {g.relation === 'same_dir' ? '接這個' : '還是接這個'}
                </button>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </div>
  )
}

export function PreviewPanel({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const stored = useStore((s) => s.previews[botId])
  const setPreview = useStore((s) => s.setPreview)
  const connected = useStore((s) => s.connected)
  const notify = useStore((s) => s.notify)
  const [pending, setPending] = useState<'start' | 'stop' | null>(null)
  const [err, setErr] = useState<PreviewErr | null>(null)
  // GET 失敗：不能拿 state 帶的 starting 當真，否則分頁會永遠卡在「啟動中」。
  const [loadFailed, setLoadFailed] = useState(false)
  // 「重新整理」換 key 強制重載 iframe；跨 origin 拿不到 contentWindow.location.reload。
  const [nonce, setNonce] = useState(0)

  // 首屏先用 /api/state 帶的簡版，GET 回來再蓋掉。
  const fromState = bot?.preview
  const p0: Preview =
    stored ?? (fromState ? { ...PREVIEW_OFF, status: fromState.status, port: fromState.port } : PREVIEW_OFF)
  const p: Preview = loadFailed && p0.status === 'starting' ? PREVIEW_OFF : p0

  const seenStatus = stored?.status
  useEffect(() => {
    if (!connected) return
    let alive = true
    fetchPreview(botId)
      .then((r) => {
        if (!alive) return
        setLoadFailed(false)
        setPreview(botId, r)
      })
      .catch((e) => {
        if (!alive) return
        setLoadFailed(true)
        setErr(startError(e))
      })
    return () => {
      alive = false
    }
  }, [botId, connected, setPreview, seenStatus])

  const start = useCallback(async (opts?: StartPreviewOpts) => {
    setPending('start')
    setErr(null)
    try {
      setPreview(botId, await startPreview(botId, opts))
    } catch (e) {
      setErr(startError(e))
    } finally {
      setPending(null)
    }
  }, [botId, setPreview])

  const stop = useCallback(async () => {
    setPending('stop')
    setErr(null)
    try {
      setPreview(botId, await stopPreview(botId))
      notify('info', '預覽已停止')
    } catch (e) {
      setErr(startError(e))
    } finally {
      setPending(null)
    }
  }, [botId, notify, setPreview])

  // 多個候選目錄時讓使用者挑；沒挑＝不帶 dir，由 daemon 用第一個。
  const [pickDir, setPickDir] = useState('')
  const attached = p.source === 'attached'
  const busy = pending !== null
  const url = p.port ? previewUrl(p.port) : null

  const startBlock = (
    <>
      {p.candidates.length > 1 ? (
        <label className="preview-pick">
          目錄
          <select value={pickDir} onChange={(e) => setPickDir(e.target.value)} disabled={busy}>
            {p.candidates.map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
          </select>
        </label>
      ) : null}
      <div className="preview-actions">
        <button
          type="button"
          className={`btn${p.others.length > 0 ? '' : ' primary'}`}
          disabled={busy || !connected}
          onClick={() => void start(p.candidates.length > 1 ? { dir: pickDir || p.candidates[0] } : undefined)}
        >
          {pending === 'start' ? '啟動中…' : '啟動預覽'}
        </button>
      </div>
    </>
  )

  if (p.status === 'running' && url) {
    return (
      <div className="preview-pane">
        <div className="preview-bar">
          <span className="preview-url" title={url}>
            {url}
          </span>
          <span className="preview-src" title={p.dir ?? undefined}>
            {attached ? `已接上既有的 vite（port ${p.port}）` : p.source === 'spawned' ? '由 AG Man 啟動' : ''}
          </span>
          <span className="spacer" />
          <button type="button" className="btn preview-btn" onClick={() => setNonce((n) => n + 1)}>
            重新整理
          </button>
          <a className="btn preview-btn" href={url} target="_blank" rel="noreferrer">
            在新分頁開
          </a>
          <button
            type="button"
            className="btn preview-btn"
            disabled={busy}
            title={attached ? '只斷開連結，不會關掉對方的 vite server' : undefined}
            onClick={() => void stop()}
          >
            {pending === 'stop' ? (attached ? '中斷中…' : '停止中…') : attached ? '中斷連接' : '停止'}
          </button>
        </div>
        {err ? <ErrNote err={err} /> : null}
        <iframe
          key={nonce}
          className="preview-frame"
          title={`${bot?.name ?? ''} 預覽`}
          {...(isMock ? { srcDoc: MOCK_DOC } : { src: url })}
        />
      </div>
    )
  }

  return (
    <div className="preview-pane">
      <div className="preview-empty" role="status">
        {p.status === 'starting' ? (
          <>
            <h2 className="preview-title">正在啟動預覽…</h2>
            <p className="preview-body">
              {p.dir ? (
                <>
                  在 <code>{p.dir}</code> 起 vite（最多 60 秒）。
                </>
              ) : (
                '起 vite dev server 中（最多 60 秒）。'
              )}
            </p>
            <progress className="preview-progress" aria-label="啟動中" />
            <div className="preview-actions">
              <button type="button" className="btn" disabled={busy} onClick={() => void stop()}>
                取消
              </button>
            </div>
          </>
        ) : p.status === 'failed' ? (
          <>
            <h2 className="preview-title">預覽啟動失敗</h2>
            {p.dir ? (
              <p className="preview-body">
                目錄：<code>{p.dir}</code>
              </p>
            ) : null}
            {p.error ? <pre className="preview-log">{p.error}</pre> : null}
            <div className="preview-actions">
              <button type="button" className="btn primary" disabled={busy} onClick={() => void start()}>
                {pending === 'start' ? '重試中…' : '重試'}
              </button>
              <button type="button" className="btn" disabled={busy} onClick={() => void stop()}>
                關閉
              </button>
            </div>
            <OthersList others={p.others} busy={busy || !connected} onAttach={(port) => void start({ mode: 'attach', port })} />
          </>
        ) : (
          <>
            <h2 className="preview-title">預覽</h2>
            {p.others.length > 0 ? (
              // 本機已有 vite 在跑：直接接上是主角，自己起放次要（偵測不到設定檔時只有這條路）。
              <>
                <OthersList
                  others={p.others}
                  busy={busy || !connected}
                  onAttach={(port) => void start({ mode: 'attach', port })}
                />
                {err ? <ErrNote err={err} /> : null}
                <details className="preview-spawn">
                  <summary>或由 AG Man 替 {bot ? bot.name : '這顆 bot'} 另起一顆</summary>
                  {startBlock}
                </details>
              </>
            ) : (
              <>
                <p className="preview-body">
                  替 {bot ? <b>{bot.name}</b> : '這顆 bot'} 的專案起 vite dev server，畫面直接顯示在這裡。會在{' '}
                  <code>{bot?.cwd ?? '專案目錄'}</code> 或其 <code>web/</code>、<code>apps/*</code> 下找 <code>vite.config.*</code>。
                  本機目前沒有偵測到在跑的 vite。
                </p>
                {startBlock}
              </>
            )}
          </>
        )}
        {err && !(p.status === 'off' && p.others.length > 0) ? <ErrNote err={err} /> : null}
      </div>
    </div>
  )
}
