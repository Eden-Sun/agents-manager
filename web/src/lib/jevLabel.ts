import { msg } from './i18n'

export const JEV_FULL_NAME = msg('Jev（TypeSafe 型別判斷）')
export const JEV_ADVISORY_NOTE = msg('只是建議，不會自動動作')

export function jevProbText(prob: number | null | undefined): string | null {
  if (typeof prob !== 'number' || !Number.isFinite(prob) || prob < 0 || prob > 1) return null
  return `${Math.round(prob * 100)}%`
}
