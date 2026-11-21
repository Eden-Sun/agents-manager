import { useState } from 'react'
import * as api from '../api'
import { reviewErrText } from '../lib/claudeReviewErr'
import { useStore } from '../store/store'
import { t } from '../lib/i18n'

export function useReviewAsk(
  kind: string,
  host: string,
  from: string | null,
  to: string | null,
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
    dialog: { secondaryLabel: asking ? t('派工中…') : t('分析影響'), secondaryDisabled: asking || (requireTo && !to), onSecondary: ask },
  }
}
