import { useEffect, useState } from 'react'
import { fetchClaudeApiSettings, type ClaudeApiSettings } from '../api/claudeApi'
import { msg } from '../lib/i18n'

/** 翻譯能不能用；`null`＝還沒讀到（或讀不到），照常顯示按鈕、按了由 daemon 判斷。 */
export type TranslateReady = { ok: true; auto: boolean } | { ok: false; reason: string } | null

export function translateReadyOf(s: ClaudeApiSettings | null): TranslateReady {
  if (!s) return null
  if (!s.enabled) return { ok: false, reason: msg('Claude API 總開關沒開：到「環境設定 → Claude API」打開') }
  if (!s.key_present) return { ok: false, reason: msg('Claude API key 還沒設定：到「環境設定 → Claude API」設定') }
  const use = s.uses.find((u) => u.id === 'message_translate')
  if (use && !use.enabled) return { ok: false, reason: msg('「對話訊息翻譯」被關掉了：到「環境設定 → Claude API」打開') }
  // 沒帶 auto 欄位（舊 daemon）＝不自動：自動翻譯會花錢，不確定就別動。
  return { ok: true, auto: use?.auto === true }
}

// 整頁共用一次讀取：每則英文訊息都掛這個 hook，不能各自打 API。60 秒後再用到才重讀（設定頁改了開關能跟上）。
let cache: { at: number; p: Promise<TranslateReady> } | null = null
/** 最近讀到的結果：元件重掛時同步拿到，不必等下一輪非同步讀取（重掛太頻繁時會一直停在 null）。 */
let lastReady: TranslateReady = null
const TTL_MS = 60_000

function load(): Promise<TranslateReady> {
  if (!cache || Date.now() - cache.at > TTL_MS) {
    const at = Date.now()
    cache = {
      at,
      p: fetchClaudeApiSettings().then(
        (s) => (lastReady = translateReadyOf(s)),
        () => null,
      ),
    }
  }
  return cache.p
}

/** 給測試：清掉共用快取。 */
export function resetTranslateReady() {
  cache = null
  lastReady = null
}

/** 翻譯按鈕事先知道「未啟用」：沒開時按鈕標「未啟用」並在 tooltip 說去哪裡開（使用者 2026-11-04）。 */
export function useTranslateReady(active: boolean): TranslateReady {
  const [ready, setReady] = useState<TranslateReady>(() => lastReady)
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
