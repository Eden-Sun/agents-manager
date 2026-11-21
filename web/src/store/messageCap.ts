import { MESSAGE_CAP } from './lists'

export type CapFloors = Record<string, number>

export function capFor(floors: CapFloors, key: string): number {
  const floor = floors[key]
  return typeof floor === 'number' && floor > MESSAGE_CAP ? floor : MESSAGE_CAP
}

export function raiseFloor(floors: CapFloors, key: string, added: number, loaded: number): CapFloors {
  if (added <= 0) return floors
  const next = loaded + MESSAGE_CAP
  if (next <= capFor(floors, key)) return floors
  return { ...floors, [key]: next }
}

export function clearFloor(floors: CapFloors, key: string): CapFloors {
  if (floors[key] === undefined) return floors
  const out = { ...floors }
  delete out[key]
  return out
}
