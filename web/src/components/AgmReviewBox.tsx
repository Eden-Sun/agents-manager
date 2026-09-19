/**
 * 更新框裡的「AGM 解析」區塊（使用者 2026-09-19：「解析結果直接在更新視窗 show 出」）。
 *
 * 三種狀態：還沒派（顯示怎麼做）、派了還沒結論（顯示等誰）、有結論（**直接把結論印在框裡**）。
 * 以前只有一顆按鈕加一句「結論會回到這裡」，使用者還要自己去別的對話翻。
 */
import { useEffect, useState } from 'react'
import * as api from '../api'
import type { ClaudeReview } from '../api/types'
import './agmReviewBox.css'

export function AgmReviewBox({ host, from, refreshKey }: { host: string; from: string | null; refreshKey: number }) {
  const [review, setReview] = useState<ClaudeReview | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    api
      .fetchClaudeUpdateReview({ host, from })
      .then((r) => alive && setReview(r))
      .catch((e: unknown) => alive && setError(e instanceof Error ? e.message : String(e)))
    return () => {
      alive = false
    }
  }, [host, from, refreshKey])

  // 這顆 daemon 還沒有那支 API（二進位比前端舊）或讀不到：不要在更新框裡吵，按鈕自己會說。
  if (error || !review || review.state === 'none') return null

  return (
    <section className="agm-review">
      <div className="agm-review-head">
        <strong>AGM 解析</strong>
        <span className="agm-review-who">
          {review.state === 'done'
            ? `${review.target_bot_name || 'AGM'} · ${review.answered_at ? new Date(review.answered_at).toLocaleString() : ''}`
            : `${review.target_bot_name || 'AGM'} 解析中…`}
        </span>
      </div>
      {review.state === 'done' ? <pre className="agm-review-body">{review.result}</pre> : <p className="hint">派出去了，結論回來就會出現在這裡。</p>}
    </section>
  )
}
