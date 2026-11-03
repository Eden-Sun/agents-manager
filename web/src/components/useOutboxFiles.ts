import { useT } from '../lib/i18n'
import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import type { OutboxFile } from '../api'
import { downloadFailure, lastSettledTurnKey, readOutbox, splitOutboxName } from '../lib/outboxList'
import { clearOutboxPreviews } from '../lib/outboxPreviewCache'
import { useStore } from '../store/store'

/**
 * 「bot 給你的檔案」的清單、重讀、下載與開新分頁預覽（原本寫在 `OutboxFiles` 裡）。
 * 桌面的托盤（`OutboxFiles`）與手機對話下方的收合列（`PhoneOutboxTray`，使用者 2026-11-03）共用這一份，
 * 不各寫一套。
 */
export function useOutboxFiles() {
  const t = useT()
  const botId = useStore((s) => s.selectedBotId)
  const notify = useStore((s) => s.notify)
  // 回合一結束就重讀：bot 說「放好了」的同時清單就該出現，不必切頁或按 ↻（字串比較，回合沒結束不會重跑）。
  const settledTurn = useStore((s) => (botId ? lastSettledTurnKey(s.turns[botId]) : ''))
  const [files, setFiles] = useState<OutboxFile[]>([])
  const [reason, setReason] = useState<string | null>(null)
  const [dir, setDir] = useState('')
  /** 分享用 bot 的 outbox：不會被清，不畫倒數。 */
  const [kept, setKept] = useState(false)
  /** 讀到清單的那一刻：剩餘秒數從這裡往下扣。 */
  const [fetchedAt, setFetchedAt] = useState(0)
  /** 已經讀完哪一顆的清單。用它而不是 `loading` 旗標：effect 裡同步 setState 會多跑一輪 render。 */
  const [loadedFor, setLoadedFor] = useState<string | null>(null)
  /** 正在處理的那一個：綁 bot、檔名與模式（download 或 preview），換了 bot 不會把狀態畫到另一顆的同名檔上（#1805）。 */
  const [busy, setBusy] = useState<{ bot: string; name: string; mode: 'download' | 'preview' } | null>(null)
  /** 目前畫面是哪顆 bot：下載回來時拿來判斷那一趟還屬不屬於現在這顆（#1805）。 */
  const botRef = useRef(botId)
  useEffect(() => {
    botRef.current = botId
  }, [botId])
  /** 倒數用的時鐘：分鐘級的字，30 秒走一次就夠。 */
  const [now, setNow] = useState(() => Date.now())

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
    setKept(false)
    setLoadedFor(null)
  }

  // 換 bot／離開時放掉抓過的預覽圖，不然 blob 一直佔記憶體。
  useEffect(() => () => clearOutboxPreviews(), [botId])

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
      setKept(out.kept === true)
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
    // 這一趟是哪顆 bot 的：回來時 bot 可能已經換了。舊的只能把檔案交給使用者，不能動現在這顆的清單與下載狀態。
    const mine = { bot: botId, name, mode: 'download' as const }
    const kept0 = kept
    setBusy(mine)
    try {
      // token 帶不進 `<a href>`，所以先抓成 blob 再按下去；抓完就釋放，不然大檔會一直佔記憶體。
      const url = await api.outboxFileUrl(botId, name)
      const a = document.createElement('a')
      a.href = url
      a.download = splitOutboxName(name).base // 子資料夾裡的檔：存檔名只取最後一段
      document.body.appendChild(a)
      a.click()
      a.remove()
      setTimeout(() => URL.revokeObjectURL(url), 10_000)
    } catch (e) {
      const { text, gone } = downloadFailure(name, e, kept0)
      if (botRef.current === botId) {
        // 過期被清掉的那一列留在畫面上沒有意義：當場拿掉並重讀一次清單（issue #547）。
        if (gone) {
          setFiles((fs) => fs.filter((f) => f.name !== name))
          setLoadedFor(null)
          setNonce((n) => n + 1)
        }
        notify('error', text)
      } else {
        // 已經換到別的 bot：清單不動，訊息寫明是哪顆 bot 的檔。
        const label = useStore.getState().bots.find((b) => b.id === botId)?.name ?? botId
        notify('error', t('{label} 的檔案：{text}', { label, text }))
      }
    } finally {
      // 只清自己設的那一格；別的下載（同一顆或切過去的）設的不碰。
      setBusy((cur) => (cur === mine ? null : cur))
    }
  }

  const openPreview = async (name: string) => {
    if (!botId) return
    // 點擊當下同步開空白頁，保留 transient activation；不帶 noopener 以便取得 Window handle
    const w = window.open('', '_blank')
    if (!w) {
      notify('error', t('瀏覽器擋了彈出視窗'))
      return
    }
    // 取得 handle 後立即切斷 opener，防止子頁面存取父頁面
    try {
      w.opener = null
    } catch {}

    const mine = { bot: botId, name, mode: 'preview' as const }
    const kept0 = kept
    setBusy(mine)
    try {
      // 預覽走帶 token 的 blobUrl（避免 URL 洩漏 token 或新分頁認證缺失）
      // 注意：預覽安全性靠後端與前端嚴格的 MIME 白名單，Blob URL 本身不繼承 HTTP 回應的 CSP
      const url = await api.outboxFileUrl(botId, name, true)
      try {
        w.location.href = url
      } catch {
        w.close()
        throw new Error(t('無法導向預覽分頁'))
      }
      // 不要固定 60 秒 revoke：每 30 秒檢查分頁是否關閉，關閉後才釋放 Blob URL
      const timer = setInterval(() => {
        try {
          if (w.closed) {
            clearInterval(timer)
            URL.revokeObjectURL(url)
          }
        } catch {
          clearInterval(timer)
          URL.revokeObjectURL(url)
        }
      }, 30_000)
    } catch (e) {
      try {
        w.close()
      } catch {}
      const { text, gone } = downloadFailure(name, e, kept0)
      if (botRef.current === botId) {
        if (gone) {
          setFiles((fs) => fs.filter((f) => f.name !== name))
          setLoadedFor(null)
          setNonce((n) => n + 1)
        }
        notify('error', text)
      } else {
        const label = useStore.getState().bots.find((b) => b.id === botId)?.name ?? botId
        notify('error', t('{label} 的檔案：{text}', { label, text }))
      }
    } finally {
      setBusy((cur) => (cur === mine ? null : cur))
    }
  }

  const refresh = () => {
    if (!botId) return
    setLoadedFor(null)
    setNonce((n) => n + 1)
  }

  return { botId, files, reason, dir, kept, fetchedAt, now, loadedFor, busy, refresh, download, openPreview }
}
