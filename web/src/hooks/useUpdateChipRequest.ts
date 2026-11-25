import { useEffect, useRef } from 'react'
import { useStore } from '../store/store'

export function useUpdateChipRequest(accepts: (kind: string) => boolean, open: () => void) {
  const req = useStore((s) => s.updateChipRequest)
  const seen = useRef(req?.seq ?? 0)
  useEffect(() => {
    if (!req || req.seq === seen.current) return
    seen.current = req.seq
    if (accepts(req.kind)) open()
  }, [req])
}
