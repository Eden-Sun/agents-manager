export type FrameGate = { skip: boolean; seen: number }

export function gateFrame(seen: number, type: string, seq: number | undefined): FrameGate {
  if (typeof seq !== 'number' || type === 'turn_progress' || type === 'resync') return { skip: false, seen }
  if (seq <= seen) return { skip: true, seen }
  return { skip: false, seen: seq }
}
