import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react'
import type { PatchProjectInput } from '../api/types'

/**
 * 專案設定的「一顆儲存」（2026-11-01 使用者：執行者池、副管理者、角色設定三顆儲存合成一顆）。
 * 各區塊把草稿登記進來（dirty／valid＋取 patch 與清草稿的函式），底部儲存列合併成一次 PATCH。
 * 不在群組裡（單獨掛載、測試）時，各區塊照舊用自己的儲存鈕。
 */
export interface SavePart {
  label: string
  dirty: boolean
  valid: boolean
}
interface SaveFns {
  patch: () => PatchProjectInput
  reset: () => void
}
interface Group {
  setPart: (key: string, part: SavePart | null) => void
  fns: React.MutableRefObject<Record<string, SaveFns>>
  saving: boolean
}

const Ctx = createContext<Group | null>(null)
/** 包住要合併儲存的區塊；value 用 `useProjectSaveGroup().value`。 */
export const SaveGroupProvider = Ctx.Provider

/** 區塊用：在群組裡就登記草稿並回傳 true（區塊收掉自己的儲存鈕）。 */
export function useSavePart(key: string, part: SavePart, fns: SaveFns): { grouped: boolean; saving: boolean } {
  const g = useContext(Ctx)
  if (g) g.fns.current[key] = fns
  const { label, dirty, valid } = part
  useEffect(() => {
    if (!g) return
    g.setPart(key, { label, dirty, valid })
  }, [g, key, label, dirty, valid])
  useEffect(() => () => {
    if (!g) return
    g.setPart(key, null)
    delete g.fns.current[key]
  }, [g, key])
  return { grouped: g !== null, saving: g?.saving ?? false }
}

export function useProjectSaveGroup(onSave: (patch: PatchProjectInput) => Promise<boolean>) {
  const [parts, setParts] = useState<Record<string, SavePart>>({})
  const [saving, setSaving] = useState(false)
  const fns = useRef<Record<string, SaveFns>>({})
  const setPart = useCallback((key: string, part: SavePart | null) => {
    setParts((p) => {
      const next = { ...p }
      if (part) next[key] = part
      else delete next[key]
      return next
    })
  }, [])
  const dirty = Object.entries(parts).filter(([, p]) => p.dirty)
  const canSave = dirty.length > 0 && dirty.every(([, p]) => p.valid) && !saving
  const save = async () => {
    if (!canSave) return
    const patch: PatchProjectInput = {}
    for (const [k] of dirty) Object.assign(patch, fns.current[k]?.patch())
    setSaving(true)
    const ok = await onSave(patch).finally(() => setSaving(false))
    if (ok) for (const [k] of dirty) fns.current[k]?.reset()
  }
  const revert = () => {
    for (const [k] of dirty) fns.current[k]?.reset()
  }
  const value = useMemo<Group>(() => ({ setPart, fns, saving }), [setPart, saving])
  return { value, dirtyLabels: dirty.map(([, p]) => p.label), invalid: dirty.some(([, p]) => !p.valid), canSave, saving, save, revert }
}
