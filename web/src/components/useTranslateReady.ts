import { useEffect, useState } from 'react'
import { fetchClaudeApiSettings, type ClaudeApiSettings } from '../api/claudeApi'
import { msg } from '../lib/i18n'

export type TranslateReady = { ok: true; auto: boolean } | { ok: false; reason: string } | null

export function translateReadyOf(s: ClaudeApiSettings | null): TranslateReady {
  if (!s) return null
  if (!s.enabled) return { ok: false, reason: msg('Claude API 總開關沒開：到「環境設定 → Claude API」打開') }
  if (!s.key_present) return { ok: false, reason: msg('Claude API key 還沒設定：到「環境設定 → Claude API」設定') }
  const use = s.uses.find((u) => u.id === 'message_translate')
  if (use && !use.enabled) return { ok: false, reason: msg('「對話訊息翻譯」被關掉了：到「環境設定 → Claude API」打開') }
  return { ok: true, auto: use?.auto === true }
}

let cache: { at: number; p: Promise<TranslateReady> } | null = null
let lastReady: TranslateReady = null
let generation = 0
const TTL_MS = 60_000

function load(): Promise<TranslateReady> {
  if (!cache || Date.now() - cache.at > TTL_MS) {
    const at = Date.now()
    const gen = generation
    cache = {
      at,
      p: fetchClaudeApiSettings().then(
        (s) => {
          const r = translateReadyOf(s)
          if (gen === generation) lastReady = r
          return r
        },
        () => null,
      ),
    }
  }
  return cache.p
}

export function resetTranslateReady() {
  generation++
  cache = null
  lastReady = null
}

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
