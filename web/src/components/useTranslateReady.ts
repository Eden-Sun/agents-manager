import { useEffect, useState } from 'react'
import { fetchClaudeApiSettings, type ClaudeApiSettings } from '../api/claudeApi'
import { msg } from '../lib/i18n'

/** 翻譯能不能用；`null`＝還沒讀到（或讀不到），照常顯示按鈕、按了由 daemon 判斷。 */
export type TranslateReady = { ok: true } | { ok: false; reason: string } | null

export function translateReadyOf(s: ClaudeApiSettings | null): TranslateReady {
  if (!s) return null
  if (!s.enabled) return { ok: false, reason: msg('Claude API 總開關沒開：到「環境設定 → Claude API」打開') }
  if (!s.key_present) return { ok: false, reason: msg('Claude API key 還沒設定：到「環境設定 → Claude API」設定') }
  const use = s.uses.find((u) => u.id === 'message_translate')
  if (use && !use.enabled) return { ok: false, reason: msg('「對話訊息翻譯」被關掉了：到「環境設定 → Claude API」打開') }
  return { ok: true }
}

// 整頁共用一次讀取：每則英文訊息都掛這個 hook，不能各自打 API。60 秒後再用到才重讀（設定頁改了開關能跟上）。
let cache: { at: number; p: Promise<TranslateReady> } | null = null
const TTL_MS = 60_000

function load(): Promise<TranslateReady> {
  if (!cache || Date.now() - cache.at > TTL_MS) {
    cache = { at: Date.now(), p: fetchClaudeApiSettings().then(translateReadyOf, () => null) }
  }
  return cache.p
}

/** 給測試：清掉共用快取。 */
export function resetTranslateReady() {
  cache = null
}

/** 翻譯按鈕事先知道「未啟用」：沒開時按鈕標「未啟用」並在 tooltip 說去哪裡開（使用者 2026-11-04）。 */
export function useTranslateReady(active: boolean): TranslateReady {
  const [ready, setReady] = useState<TranslateReady>(null)
  useEffect(() => {
    if (!active) return
    let alive = true
    void load().then((r) => alive && setReady(r))
    return () => {
      alive = false
    }
  }, [active])
  return ready
}
