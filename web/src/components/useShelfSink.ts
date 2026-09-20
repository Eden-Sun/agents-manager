import { useEffect } from 'react'
import { useShelf } from '../store/shelf'

/** Register the on-screen conversation as the shelf's hand-off target (called by chat panels). */
export function useShelfSink(add: (files: File[]) => void, label: string | null) {
  useEffect(() => {
    if (!label) return
    const sink = { add, label }
    useShelf.getState().setSink(sink)
    return () => {
      // Only clear our own: on a panel swap the next panel's effect may already have run.
      if (useShelf.getState().sink === sink) useShelf.getState().setSink(null)
    }
  }, [add, label])
}
