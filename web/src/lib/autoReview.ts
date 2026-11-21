import type * as api from '../api'

export async function loadReviewOnOpen(
  input: { kind: string; host: string; from: string | null; to: string | null },
  client: Pick<typeof api, 'fetchUpdateReview' | 'requestUpdateReview'>,
) {
  const current = await client.fetchUpdateReview(input)
  if (input.kind !== 'herdr') return current
  if (!current.auto || current.review.state === 'pending') return current
  try {
    await client.requestUpdateReview({ ...input, to: input.to || current.version, auto: true })
    return await client.fetchUpdateReview(input)
  } catch {
    return current
  }
}
