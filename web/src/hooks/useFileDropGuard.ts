import { useEffect } from 'react'

export function guardFileDrop(e: Pick<DragEvent, 'type' | 'dataTransfer' | 'defaultPrevented' | 'preventDefault'>): void {
  if (e.defaultPrevented) return
  if (!Array.from(e.dataTransfer?.types ?? []).includes('Files')) return
  e.preventDefault()
  if (e.type === 'dragover' && e.dataTransfer) e.dataTransfer.dropEffect = 'none'
}

export function useFileDropGuard(): void {
  useEffect(() => {
    window.addEventListener('dragover', guardFileDrop)
    window.addEventListener('drop', guardFileDrop)
    return () => {
      window.removeEventListener('dragover', guardFileDrop)
      window.removeEventListener('drop', guardFileDrop)
    }
  }, [])
}
