import { useUnsavedGuard } from '../hooks/useUnsavedGuard'

export function UnsavedGuard({ dirty }: { dirty: boolean }) {
  useUnsavedGuard(dirty)
  return null
}
