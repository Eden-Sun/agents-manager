import { useT } from '../lib/i18n'
import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import type { OutboxFile } from '../api'
import { downloadFailure, lastSettledTurnKey, orderFiles, readOutbox, relayedOutboxRefs, splitOutboxName } from '../lib/outboxList'
import { isHtmlFile } from '../lib/htmlPreview'
import { clearOutboxPreviews } from '../lib/outboxPreviewCache'
import { useStore } from '../store/store'

export function useOutboxFiles() {
  const t = useT()
  const botId = useStore((s) => s.selectedBotId)
  const notify = useStore((s) => s.notify)
  const settledTurn = useStore((s) => (botId ? lastSettledTurnKey(s.turns[botId]) : ''))
  const relayKey = useStore((s) => (botId ? JSON.stringify(relayedOutboxRefs(s.messages[botId], botId)) : '{}'))
  const [files, setFiles] = useState<OutboxFile[]>([])
  const [reason, setReason] = useState<string | null>(null)
  const [dir, setDir] = useState('')
  const [kept, setKept] = useState(false)
  const [fetchedAt, setFetchedAt] = useState(0)
  const [loadedFor, setLoadedFor] = useState<string | null>(null)
  const [busy, setBusy] = useState<{ bot: string; name: string; mode: 'download' | 'preview' } | null>(null)
  const botRef = useRef(botId)
  useEffect(() => {
    botRef.current = botId
  }, [botId])
  const [now, setNow] = useState(() => Date.now())

  const [nonce, setNonce] = useState(0)

  const [lastBot, setLastBot] = useState(botId)
  if (lastBot !== botId) {
    setLastBot(botId)
    setFiles([])
    setReason(null)
    setDir('')
    setKept(false)
    setLoadedFor(null)
  }

  useEffect(() => () => clearOutboxPreviews(), [botId])

  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 30_000)
    return () => clearInterval(t)
  }, [])

  useEffect(() => {
    if (!botId) return
    let alive = true
    void (async () => {
      const out = await readOutbox(() => api.fetchOutbox(botId))
      const relayed = await relayedFiles(botId, JSON.parse(relayKey) as Record<string, string[]>)
      if (!alive) return
      setFiles(orderFiles([...out.files, ...relayed]))
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
  }, [botId, nonce, settledTurn, relayKey])

  const dropFile = (name: string, owner: string | undefined) =>
    setFiles((fs) => fs.filter((f) => f.name !== name || f.owner !== owner))

  const download = async (name: string, owner?: string) => {
    if (!botId) return
    const from = owner ?? botId
    const mine = { bot: from, name, mode: 'download' as const }
    const kept0 = kept
    setBusy(mine)
    try {
      const url = await api.outboxFileUrl(from, name)
      const a = document.createElement('a')
      a.href = url
      a.download = splitOutboxName(name).base
      document.body.appendChild(a)
      a.click()
      a.remove()
      setTimeout(() => URL.revokeObjectURL(url), 10_000)
    } catch (e) {
      const { text, gone } = downloadFailure(name, e, kept0)
      if (botRef.current === botId) {
        if (gone) {
          dropFile(name, owner)
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

  const openPreview = async (name: string, owner?: string) => {
    if (!botId) return
    const from = owner ?? botId
    const w = window.open('', '_blank')
    if (!w) {
      notify('error', t('瀏覽器擋了彈出視窗'))
      return
    }
    try {
      w.opener = null
    } catch {}

    const mine = { bot: from, name, mode: 'preview' as const }
    const kept0 = kept
    setBusy(mine)
    try {
      const url = isHtmlFile(name) ? await api.outboxHtmlUrl(from, name) : await api.outboxFileUrl(from, name, true)
      try {
        w.location.href = url
      } catch {
        w.close()
        throw new Error(t('無法導向預覽分頁'))
      }
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
          dropFile(name, owner)
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

async function relayedFiles(selfId: string, refs: Record<string, string[]>): Promise<OutboxFile[]> {
  const bots = useStore.getState().bots
  const lists = await Promise.all(
    Object.entries(refs).map(async ([owner, names]) => {
      const bot = bots.find((b) => b.id === owner)
      if (!bot || owner === selfId) return []
      try {
        const out = await api.fetchOutbox(owner)
        const want = new Set(names)
        return out.files.filter((f) => want.has(f.name)).map((f) => ({ ...f, owner, ownerName: bot.name }))
      } catch {
        return []
      }
    }),
  )
  return lists.flat()
}
