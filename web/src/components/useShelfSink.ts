import { useEffect } from 'react'
import { useShelf } from '../store/shelf'

export function useShelfSink(add: (files: File[]) => void, label: string | null) {
  useEffect(() => {
    if (!label) return
    const sink = { add, label }
    useShelf.getState().setSink(sink)
    return () => {
      if (useShelf.getState().sink === sink) useShelf.getState().setSink(null)
    }
  }, [add, label])
}
