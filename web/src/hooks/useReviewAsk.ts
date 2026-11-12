import { useState } from 'react'
import * as api from '../api'
import { reviewErrText } from '../lib/claudeReviewErr'
import { useStore } from '../store/store'
import { t } from '../lib/i18n'

/**
 * 更新確認框的「分析影響」（同 `ClaudeInstallChip`／`HerdrUpdateChip` 的寫法，agy／grok 共用，#1928）。
 * 只有使用者按下去才會 `POST /api/claude-update/review`（daemon 開了 Claude API 就走 API 快速解析，否則派 bot）；
 * 開框只讀（`AgmReviewBox` 的 GET），不會花 API。派出去之後 `reviewKey` +1，分析區重讀、結論回到框裡。
 * `to` 是 `null`（plan 還沒有）時按鈕停用；hook 一律呼叫，呼叫端不用為了 hooks 規則改寫提早 return。
 */
export function useReviewAsk(
  kind: string,
  host: string,
  from: string | null,
  to: string | null,
  /** 預設 `to` 為空就停用；codex 等「版本範圍可能沒有終點」的呼叫端傳 `false`，照舊送出（後端自己解析最新版）。 */
  requireTo = true,
) {
  const notify = useStore((s) => s.notify)
  const [asking, setAsking] = useState(false)
  const [reviewKey, setReviewKey] = useState(0)
  const ask = () => {
    if (requireTo && !to) return
    setAsking(true)
    void api
      .requestUpdateReview({ kind, host, from, to })
      .then((r) => {
        setReviewKey((n) => n + 1)
        notify(
          'info',
          r.duplicate
            ? t('{kind} {version} 已經派給 {bot} 解析過了，結論會回到這裡', {
                kind,
                version: r.version,
                bot: r.source_name || r.target_bot_name || 'AGM',
              })
            : t('已請 {bot} 解析 {kind} {version} 的 changelog，結論會回到這裡', {
                bot: r.source_name || r.target_bot_name || 'AGM',
                kind,
                version: r.version,
              }),
        )
      })
      .catch((e: unknown) => notify('error', t('派不出去：{reason}', { reason: reviewErrText(e) })))
      .finally(() => setAsking(false))
  }
  return {
    reviewKey,
    asking,
    ask,
    /** 直接展開給 `ConfirmDialog`。 */
    dialog: { secondaryLabel: asking ? t('派工中…') : t('分析影響'), secondaryDisabled: asking || (requireTo && !to), onSecondary: ask },
  }
}
