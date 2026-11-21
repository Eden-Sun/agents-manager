import type { TerminalSnapshot } from '../api/types'

export interface RunPin {
  read: () => Promise<string | null>
  id: () => string | null
  changed: () => boolean
}

export function runPin(
  readTerminal: () => Promise<TerminalSnapshot>,
  currentRunId: () => string | null | undefined,
): RunPin {
  let pinned: string | null = null
  let changed = false
  return {
    read: async () => {
      let snap: TerminalSnapshot
      try {
        snap = await readTerminal()
      } catch {
        return null
      }
      const stored = currentRunId() ?? null
      const seen = snap.run_id ?? stored
      if (!seen) return null
      if (pinned === null) pinned = seen
      if (seen !== pinned || (stored !== null && stored !== pinned)) {
        changed = true
        return null
      }
      return snap.text
    },
    id: () => pinned,
    changed: () => changed,
  }
}
