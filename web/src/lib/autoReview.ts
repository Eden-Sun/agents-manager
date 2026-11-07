import type * as api from '../api'

/** 開框先 GET 資格，再以明確 POST 補分析；daemon 依指紋去重，failed 不自動重試。 */
export async function loadReviewOnOpen(
  input: { kind: string; host: string; from: string | null; to: string | null },
  client: Pick<typeof api, 'fetchUpdateReview' | 'requestUpdateReview'>,
) {
  const current = await client.fetchUpdateReview(input)
  if (!current.auto || current.review.state === 'pending') return current
  try {
    await client.requestUpdateReview({ ...input, to: input.to || current.version, auto: true })
    return await client.fetchUpdateReview(input)
  } catch {
    return current
  }
}
