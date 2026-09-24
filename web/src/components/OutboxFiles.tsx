import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import type { OutboxFile } from '../api'
import { downloadFailure, emptyReason, fileSize, isPreviewableImage, lastSettledTurnKey, LOAD_FAILED, readOutbox, remainingLabel, remainingNow } from '../lib/outboxList'
import { clearOutboxPreviews } from '../lib/outboxPreviewCache'
import { OutboxImagePreview } from './OutboxImagePreview'
import { useStore } from '../store/store'
import './outboxFiles.css'

/**
 * 檔案暫存區裡的「bot 給你的檔案」：bot 把要交給使用者的東西放進 `$AM_OUTBOX`（SPEC §6.5f），
 * 使用者在這裡點一下就下載得到——那些檔案在 daemon 這台機器上，手機上本來拿不到。
 *
 * **只讀 outbox，不讀 scratchpad**（使用者 2026-09-16 裁示：scratchpad 暴露過私鑰與正式 DB 複本）。
 * 檔案放進去 1 小時後由 AGM 清掉，所以每個檔案標出還剩多久。
 *
 * 只讀：這一段不刪檔、不改檔。上面那半（拖進來的暫存）是要**送進**對話的東西，這半是**從** bot 拿出來的。
 */
export function OutboxFiles() {
  const botId = useStore((s) => s.selectedBotId)
  const notify = useStore((s) => s.notify)
  // 回合一結束就重讀：bot 說「放好了」的同時清單就該出現，不必切頁或按 ↻（字串比較，回合沒結束不會重跑）。
  const settledTurn = useStore((s) => (botId ? lastSettledTurnKey(s.turns[botId]) : ''))
  const [files, setFiles] = useState<OutboxFile[]>([])
  const [reason, setReason] = useState<string | null>(null)
  const [dir, setDir] = useState('')
  /** 讀到清單的那一刻：剩餘秒數從這裡往下扣。 */
  const [fetchedAt, setFetchedAt] = useState(0)
  /** 已經讀完哪一顆的清單。用它而不是 `loading` 旗標：effect 裡同步 setState 會多跑一輪 render。 */
  const [loadedFor, setLoadedFor] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  /** 倒數用的時鐘：分鐘級的字，30 秒走一次就夠。 */
  const [now, setNow] = useState(() => Date.now())

  /** 改它就重讀一次（同 HostShellPanel 的 `nonce`）。 */
  const [nonce, setNonce] = useState(0)

  /** 滑過（或鍵盤聚焦）的圖檔：浮出預覽。 */
  const [peek, setPeek] = useState<{ name: string; modified: number; anchor: DOMRect } | null>(null)
  const peekTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const showPeek = (f: OutboxFile, el: HTMLElement) => {
    if (!isPreviewableImage(f.name)) return
    if (peekTimer.current) clearTimeout(peekTimer.current)
    // 滑鼠只是劃過清單不該每張都抓一次：停 150ms 才算要看。
    peekTimer.current = setTimeout(() => setPeek({ name: f.name, modified: f.modified, anchor: el.getBoundingClientRect() }), 150)
  }
  const hidePeek = () => {
    if (peekTimer.current) clearTimeout(peekTimer.current)
    peekTimer.current = null
    setPeek(null)
  }

  // 換 bot 在 render 當下清畫面（同 HostShellPanel）：走 effect 的話會多跑一輪 render，
  // 中間那一格會先畫出上一顆 bot 的檔名。
  const [lastBot, setLastBot] = useState(botId)
  if (lastBot !== botId) {
    setLastBot(botId)
    setFiles([])
    setReason(null)
    setDir('')
    setLoadedFor(null)
    setPeek(null)
  }

  // 換 bot／離開時放掉抓過的預覽圖，不然 blob 一直佔記憶體。
  useEffect(
    () => () => {
      if (peekTimer.current) clearTimeout(peekTimer.current)
      clearOutboxPreviews()
    },
    [botId],
  )

  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 30_000)
    return () => clearInterval(t)
  }, [])

  useEffect(() => {
    if (!botId) return
    // 換 bot 換得快時，慢回來的那一份不能蓋掉新的：離開就把自己標成過期。
    let alive = true
    void (async () => {
      // 讀不到不跳紅字（這一段是附加資訊，每個回合結束都重讀），但也不能說成「還沒有檔案」：`readOutbox` 回 `load_failed`（#234）。
      const out = await readOutbox(() => api.fetchOutbox(botId))
      if (!alive) return
      setFiles(out.files)
      setReason(out.reason)
      setDir(out.dir)
      setFetchedAt(Date.now())
      setNow(Date.now())
      setLoadedFor(botId)
    })()
    return () => {
      alive = false
    }
  }, [botId, nonce, settledTurn])

  const download = async (name: string) => {
    if (!botId) return
    setBusy(name)
    try {
      // token 帶不進 `<a href>`，所以先抓成 blob 再按下去；抓完就釋放，不然大檔會一直佔記憶體。
      const url = await api.outboxFileUrl(botId, name)
      const a = document.createElement('a')
      a.href = url
      a.download = name
      document.body.appendChild(a)
      a.click()
      a.remove()
      setTimeout(() => URL.revokeObjectURL(url), 10_000)
    } catch (e) {
      // 過期被清掉的那一列留在畫面上沒有意義：當場拿掉並重讀一次清單（issue #547）。
      const { text, gone } = downloadFailure(name, e)
      if (gone) {
        setFiles((fs) => fs.filter((f) => f.name !== name))
        setLoadedFor(null)
        setNonce((n) => n + 1)
      }
      notify('error', text)
    } finally {
      setBusy(null)
    }
  }

  return (
    <section className="outbox-files" aria-label="這顆 bot 交給你的檔案">
      <div className="outbox-head">
        <span className="outbox-title" title={dir || undefined}>
          bot 給你的檔案
        </span>
        {files.length > 0 ? <span className="outbox-count">{files.length}</span> : null}
        <span className="spacer" />
        <button
          type="button"
          className="icon-btn outbox-refresh"
          disabled={!botId || loadedFor !== botId}
          aria-label="重新讀取 bot 給你的檔案"
          title="重新讀取（bot 剛放進去的檔案按這裡才會出現）"
          onClick={() => {
            if (!botId) return
            setLoadedFor(null)
            setNonce((n) => n + 1)
          }}
        >
          ↻
        </button>
      </div>
      {files.length === 0 ? (
        <p className={`outbox-empty${reason === LOAD_FAILED ? ' failed' : ''}`} role={reason === LOAD_FAILED ? 'alert' : undefined}>
          {botId && loadedFor !== botId ? '讀取中…' : emptyReason(reason, Boolean(botId))}
        </p>
      ) : (
        <ul className="outbox-list" role="list">
          {files.map((f) => {
            const left = remainingNow(f, fetchedAt, now)
            return (
              <li key={f.name}>
                <button
                  type="button"
                  className="outbox-file"
                  disabled={busy === f.name}
                  title={`下載 ${f.name}`}
                  onClick={() => void download(f.name)}
                  onMouseEnter={(e) => showPeek(f, e.currentTarget)}
                  onMouseLeave={hidePeek}
                  onFocus={(e) => showPeek(f, e.currentTarget)}
                  onBlur={hidePeek}
                >
                  <span className="outbox-name">{f.name}</span>
                  <span className={`outbox-meta${left <= 600 ? ' soon' : ''}`}>
                    {fileSize(f.size)} · {remainingLabel(left)}
                    {busy === f.name ? ' · 下載中…' : ''}
                  </span>
                </button>
              </li>
            )
          })}
        </ul>
      )}
      {peek && botId ? <OutboxImagePreview botId={botId} name={peek.name} modified={peek.modified} anchor={peek.anchor} /> : null}
    </section>
  )
}
