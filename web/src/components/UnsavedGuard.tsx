import { useUnsavedGuard } from '../hooks/useUnsavedGuard'

/**
 * `useUnsavedGuard` 的元件版：面板在提早 return 之後才算得出「有沒有未儲存」，hook 不能擺在 return 後面，
 * 所以把它放進一個不畫東西的子元件，擺在 JSX 裡就好。
 */
export function UnsavedGuard({ dirty }: { dirty: boolean }) {
  useUnsavedGuard(dirty)
  return null
}
