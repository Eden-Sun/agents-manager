export function scheduleReviewRefresh(opts: {
  rev: number
  state: 'none' | 'pending' | 'done' | 'failed' | undefined
  run: () => void
  delayMs?: number
}): () => void {
  if (opts.rev === 0 || opts.state === 'done') return () => {}
  const t = setTimeout(opts.run, opts.delayMs ?? 1000)
  return () => clearTimeout(t)
}
