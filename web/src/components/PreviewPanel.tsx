/**
 * 右半面板的「預覽」分頁（issue #253）：頂層 bot 的專案起 vite dev server，iframe 內嵌顯示。
 * 狀態來自 store.previews（WS `preview_changed` 即時寫入）；進來與 daemon 重連時 GET 一次補齊 dir／error。
 */
import { useCallback, useEffect, useState } from 'react'
import { fetchPreview, type StartPreviewOpts, previewApiMissing, PREVIEW_API_MISSING, previewUrl, startPreview, stopPreview, PREVIEW_OFF, type Preview } from '../api/preview'
import { isMock } from '../api'
import { ApiError } from '../api/types'
import { useStore } from '../store/store'
import './previewPanel.css'

/** 起 vite 失敗的 409：`no_vite_config` 的 body 帶試過哪些路徑。 */
function startError(e: unknown): string {
  if (previewApiMissing(e)) return PREVIEW_API_MISSING
  if (e instanceof ApiError) {
    if (e.body.reason === 'not_top_level' || e.body.error === 'not_top_level') return '只有頂層 bot 能開預覽。'
    const tried = e.body.tried
    if (e.body.reason === 'no_vite_config' || e.body.error === 'no_vite_config') {
      const list = Array.isArray(tried) ? tried.map(String).join('、') : ''
      return `找不到 vite 設定檔${list ? `（試過：${list}）` : ''}。`
    }
    return e.message
  }
  return e instanceof Error ? e.message : String(e)
}

/** mock 模式沒有真的 vite：iframe 放一頁說明，截圖與手動驗才看得到「running」長相。 */
const MOCK_DOC = `<!doctype html><meta charset="utf-8"><body style="font:14px system-ui;margin:0;display:grid;place-items:center;height:100vh;background:#f6f8fa;color:#1b2430"><div style="text-align:center"><h2 style="margin:0 0 6px">Vite dev server</h2><p style="margin:0;color:#5e6a78">（mock 預覽畫面）</p></div>`

export function PreviewPanel({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const stored = useStore((s) => s.previews[botId])
  const setPreview = useStore((s) => s.setPreview)
  const connected = useStore((s) => s.connected)
  const notify = useStore((s) => s.notify)
  const [pending, setPending] = useState<'start' | 'stop' | null>(null)
  const [err, setErr] = useState<string | null>(null)
  // GET 失敗：不能拿 state 帶的 starting 當真，否則分頁會永遠卡在「啟動中」。
  const [loadFailed, setLoadFailed] = useState(false)
  // 「重新整理」換 key 強制重載 iframe；跨 origin 拿不到 contentWindow.location.reload。
  const [nonce, setNonce] = useState(0)

  // 首屏先用 /api/state 帶的簡版，GET 回來再蓋掉。
  const fromState = bot?.preview
  const p0: Preview =
    stored ?? (fromState ? { ...PREVIEW_OFF, status: fromState.status, port: fromState.port } : PREVIEW_OFF)
  const p: Preview = loadFailed && p0.status === 'starting' ? PREVIEW_OFF : p0

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
  }, [botId, connected, setPreview])

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
        {err ? (
          <div className="preview-note error" role="alert">
            {err}
          </div>
        ) : null}
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
          </>
        ) : (
          <>
            <h2 className="preview-title">預覽</h2>
            <p className="preview-body">
              替 {bot ? <b>{bot.name}</b> : '這顆 bot'} 的專案起 vite dev server，畫面直接顯示在這裡。會在 <code>{bot?.cwd ?? '專案目錄'}</code> 或其{' '}
              <code>web/</code> 下找 <code>vite.config.*</code>。
            </p>
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
                className="btn primary"
                disabled={busy || !connected}
                onClick={() => void start(p.candidates.length > 1 ? { dir: pickDir || p.candidates[0] } : undefined)}
              >
                {pending === 'start' ? '啟動中…' : '啟動預覽'}
              </button>
            </div>
            {p.others.length > 0 ? (
              <div className="preview-others">
                <p className="preview-body">
                  本機還有別的 vite 在跑（另一份 checkout，看到的不是這顆 bot 的程式碼）：
                </p>
                <ul>
                  {p.others.map((o) => (
                    <li key={o.port}>
                      <span className="preview-other-dir">
                        :{o.port} · <code>{o.dir}</code>
                      </span>
                      <button
                        type="button"
                        className="btn preview-btn"
                        disabled={busy || !connected}
                        onClick={() => void start({ mode: 'attach', port: o.port })}
                      >
                        還是接這個
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
          </>
        )}
        {err ? (
          <div className="preview-note error" role="alert">
            {err}
          </div>
        ) : null}
      </div>
    </div>
  )
}
