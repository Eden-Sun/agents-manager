import { useCallback, useEffect, useRef } from 'react'
import { fetchShare, type ShareState } from '../api/share'
import { useStore } from '../store/store'

type ShareRead = { rev: number; seq: number; promise: Promise<ShareState> }
export type SharePost = { rev: number; seq: number }

/** Share GET/POST ordering shared by the settings panel and chat button. */
export function useShareRequests(
  botId: string,
  onShare: (share: ShareState, rev: number) => void,
  onError?: (error: unknown) => void,
) {
  const callbacks = useRef({ onShare, onError })
  useEffect(() => {
    callbacks.current = { onShare, onError }
  }, [onShare, onError])
  const seq = useRef(0)
  const latestPost = useRef(0)
  const latestApplied = useRef(0)
  const pending = useRef<ShareRead | null>(null)
  const currentRev = useCallback(() => useStore.getState().shareRev[botId] ?? 0, [botId])

  const isCurrentRead = useCallback((request: ShareRead) => {
    const newerRead = pending.current && pending.current.seq > request.seq
    return currentRev() === request.rev &&
      request.seq > latestPost.current &&
      request.seq >= latestApplied.current &&
      !newerRead
  }, [currentRev])

  const readAt = useCallback((rev = currentRev(), afterSeq = 0): ShareRead => {
    const active = pending.current
    if (active?.rev === rev && active.seq > afterSeq) return active

    const request: ShareRead = { rev, seq: ++seq.current, promise: fetchShare(botId) }
    pending.current = request
    void request.promise.then(
      (share) => {
        if (
          pending.current === request &&
          request.seq > latestPost.current &&
          request.seq > latestApplied.current &&
          currentRev() === rev
        ) {
          latestApplied.current = request.seq
          callbacks.current.onShare(share, rev)
        }
        if (pending.current === request) pending.current = null
      },
      (error) => {
        if (pending.current !== request) return
        pending.current = null
        if (request.seq > latestPost.current && currentRev() === rev) callbacks.current.onError?.(error)
      },
    )
    return request
  }, [botId, currentRev])

  const readCurrent = useCallback(async (afterSeq: number): Promise<ShareState> => {
    while (true) {
      const rev = currentRev()
      const request = readAt(rev, afterSeq)
      try {
        const share = await request.promise
        if (isCurrentRead(request)) return share
      } catch (error) {
        if (currentRev() === rev) throw error
      }
    }
  }, [currentRev, isCurrentRead, readAt])

  const beginPost = useCallback((): SharePost => {
    const post = { rev: currentRev(), seq: ++seq.current }
    latestPost.current = post.seq
    if (pending.current && pending.current.seq < post.seq) pending.current = null
    return post
  }, [currentRev])

  const applyPost = useCallback(async (post: SharePost, share: ShareState): Promise<ShareState> => {
    if (currentRev() !== post.rev || seq.current > post.seq) return readCurrent(post.seq)
    latestApplied.current = post.seq
    callbacks.current.onShare(share, post.rev)
    return share
  }, [currentRev, readCurrent])

  const isCurrentPost = useCallback((post: SharePost) => currentRev() === post.rev && seq.current === post.seq, [currentRev])

  return { currentRev, readAt, readCurrent, beginPost, applyPost, isCurrentPost, isCurrentRead }
}
