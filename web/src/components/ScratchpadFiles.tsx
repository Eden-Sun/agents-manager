import { useEffect, useState } from 'react'
import * as api from '../api'
import type { ScratchpadFile } from '../api'
import { emptyReason, fileSize, modifiedAgo, orderFiles, splitMentioned } from '../lib/scratchpadList'
import { useStore } from '../store/store'
import './scratchpadFiles.css'

/**
 * 檔案暫存區裡的「這顆 bot 的 scratchpad」：bot 把整理好的東西寫成檔案之後（`scratchpad/tracking.tsv`
 * 那種），使用者在這裡點一下就下載得到——以前那些檔案只在 daemon 這台機器的暫存目錄裡，
 * 手機上根本拿不到（使用者 2026-09-16）。
 *
 * 只讀：這一段不刪檔、不改檔。上面那半（拖進來的暫存）是要**送進**對話的東西，這半是**從** bot 拿出來的。
 *
 * 預設只列對話裡提過檔名的；bot 自己的工作檔收在「其他 N 個」底下，要找再展開。
 * 資料庫與金鑰類 daemon 根本不列（見 `daemon/src/scratchpad.rs` 的 `withheld`）。
 */
export function ScratchpadFiles() {
  const botId = useStore((s) => s.selectedBotId)
  const notify = useStore((s) => s.notify)
  const [files, setFiles] = useState<ScratchpadFile[]>([])
  const [reason, setReason] = useState<string | null>(null)
  const [dir, setDir] = useState('')
  /** 已經讀完哪一顆的清單。用它而不是 `loading` 旗標：effect 裡同步 setState 會多跑一輪 render。 */
  const [loadedFor, setLoadedFor] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [showOthers, setShowOthers] = useState(false)

  /** 改它就重讀一次（同 HostShellPanel 的 `nonce`）。 */
  const [nonce, setNonce] = useState(0)

  // 換 bot 在 render 當下清畫面（同 HostShellPanel）：走 effect 的話會多跑一輪 render，
  // 中間那一格會先畫出上一顆 bot 的檔名。
  const [lastBot, setLastBot] = useState(botId)
  if (lastBot !== botId) {
    setLastBot(botId)
    setFiles([])
    setReason(null)
    setDir('')
    setLoadedFor(null)
    setShowOthers(false)
  }

  useEffect(() => {
    if (!botId) return
    // 換 bot 換得快時，慢回來的那一份不能蓋掉新的：離開就把自己標成過期。
    let alive = true
    void (async () => {
      try {
        const out = await api.fetchScratchpad(botId)
        if (!alive) return
        setFiles(orderFiles(out.files))
        setReason(out.reason)
        setDir(out.dir)
      } catch {
        // 讀不到不是紅字：這一段是附加資訊，不該讓整個暫存區看起來壞掉。
        if (!alive) return
        setFiles([])
        setReason(null)
        setDir('')
      } finally {
        if (alive) setLoadedFor(botId)
      }
    })()
    return () => {
      alive = false
    }
  }, [botId, nonce])

  const download = async (name: string) => {
    if (!botId) return
    setBusy(name)
    try {
      // token 帶不進 `<a href>`，所以先抓成 blob 再按下去；抓完就釋放，不然大檔會一直佔記憶體。
      const url = await api.scratchpadFileUrl(botId, name)
      const a = document.createElement('a')
      a.href = url
      a.download = name
      document.body.appendChild(a)
      a.click()
      a.remove()
      setTimeout(() => URL.revokeObjectURL(url), 10_000)
    } catch (e) {
      notify('error', e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  const { mentioned, others } = splitMentioned(files)

  const row = (f: ScratchpadFile) => (
    <li key={f.name}>
      <button
        type="button"
        className="sp-file"
        disabled={busy === f.name}
        title={`下載 ${f.name}`}
        onClick={() => void download(f.name)}
      >
        <span className="sp-name">{f.name}</span>
        <span className="sp-meta">
          {fileSize(f.size)} · {modifiedAgo(f.modified)}
          {busy === f.name ? ' · 下載中…' : ''}
        </span>
      </button>
    </li>
  )

  return (
    <section className="sp-files" aria-label="這顆 bot 的 scratchpad 檔案">
      <div className="sp-head">
        <span className="sp-title" title={dir || undefined}>
          bot 的 scratchpad
        </span>
        {mentioned.length > 0 ? <span className="sp-count">{mentioned.length}</span> : null}
        <span className="spacer" />
        <button
          type="button"
          className="icon-btn sp-refresh"
          disabled={!botId || loadedFor !== botId}
          aria-label="重新讀取 scratchpad 檔案"
          title="重新讀取（bot 剛寫完的檔案按這裡才會出現）"
          onClick={() => {
            if (!botId) return
            setLoadedFor(null)
            setNonce((n) => n + 1)
          }}
        >
          ↻
        </button>
      </div>
      <div className="sp-scroll">
        {mentioned.length === 0 ? (
          <p className="sp-empty">
            {botId && loadedFor !== botId ? '讀取中…' : emptyReason(reason, Boolean(botId), others.length)}
          </p>
        ) : (
          <ul className="sp-list" role="list">
            {mentioned.map(row)}
          </ul>
        )}
        {others.length > 0 ? (
          <>
            <button
              type="button"
              className="sp-others-toggle"
              aria-expanded={showOthers}
              title="bot 自己的工作檔（腳本、中間產物），對話裡沒提過檔名"
              onClick={() => setShowOthers((v) => !v)}
            >
              {showOthers ? '▾' : '▸'} 其他 {others.length} 個（對話沒提到）
            </button>
            {showOthers ? (
              <ul className="sp-list sp-others" role="list">
                {others.map(row)}
              </ul>
            ) : null}
          </>
        ) : null}
      </div>
    </section>
  )
}
