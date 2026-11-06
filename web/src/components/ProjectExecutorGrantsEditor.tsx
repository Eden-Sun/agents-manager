import { useState } from 'react'
import type { Bot, ExecutorGrantInput, Project } from '../api/types'
import { useT } from '../lib/i18n'
import { dateTimeLabel, msgTimeTitle } from '../lib/msgTime'
import { useStore } from '../store/store'
import { useSavePart } from './projectSaveGroup'
import './runtimeSettings.css'

/** executor id → specialist id；一顆執行者最多授權給一位專家（排他），所以用執行者當鍵。 */
type GrantMap = Record<string, string>

const toMap = (grants: { specialist: string; executor: string }[]): GrantMap => {
  const m: GrantMap = {}
  for (const g of grants) m[g.executor] = g.specialist
  return m
}

const sameMap = (a: GrantMap, b: GrantMap) => {
  const ka = Object.keys(a)
  return ka.length === Object.keys(b).length && ka.every((k) => a[k] === b[k])
}

/**
 * 專案設定的「執行者授權」：專家可直接派工給被授權的執行者並驗收結果（使用者 2026-11-06 方案 c）。
 * 每位專家一列、可勾選執行者；已授權給別位專家的執行者不可選。存檔走 `PUT /projects/:id/executor-grants`（整組取代），
 * 在儲存群組裡就跟著底部那顆「儲存」在 PATCH 之後送出。
 */
export function ProjectExecutorGrantsEditor({ project, bots }: { project: Project; bots: Bot[] }) {
  const t = useT()
  const setExecutorGrants = useStore((s) => s.setExecutorGrants)
  // undefined＝舊 daemon 沒有這個欄位（也不會有 PUT），只顯示提示。
  const supported = project.executor_grants !== undefined
  const savedGrants = project.executor_grants ?? []
  const saved = toMap(savedGrants)
  const savedKey = JSON.stringify(saved)
  // 草稿綁著當時的已存快照（專案＋grants）：daemon 那邊變了，草稿自動作廢，不必在 effect 裡清。
  const [draftState, setDraftState] = useState<{ base: string; map: GrantMap } | null>(null)
  const base = `${project.id}\u0000${savedKey}`
  const draft = draftState?.base === base ? draftState.map : null
  const setDraft = (map: GrantMap | null) => setDraftState(map ? { base, map } : null)
  const [saving, setSaving] = useState(false)

  const specialists = bots.filter((b) => b.role === 'specialist')
  const executors = bots.filter((b) => b.role === 'executor')
  const current = draft ?? saved
  const dirty = supported && !sameMap(current, saved)
  const body = (): ExecutorGrantInput[] => Object.entries(current).map(([executor, specialist]) => ({ specialist, executor }))
  // 帶著畫面上那份授權的版本：別人先改過，daemon 回 409、草稿留著（提示「已被他人修改，請重新整理」）。
  const commit = () => setExecutorGrants(project.id, body(), project.grants_revision)
  const group = useSavePart('executor_grants', { label: t('執行者授權'), dirty, valid: true }, { commit, reset: () => setDraft(null) })

  const nameOf = (id: string) => bots.find((b) => b.id === id)?.name ?? id
  const toggle = (specialist: string, executor: string, on: boolean) => {
    const next = { ...current }
    if (on) next[executor] = specialist
    else delete next[executor]
    setDraft(next)
  }
  const save = () => {
    if (!dirty || saving) return
    setSaving(true)
    void commit().then((ok) => {
      if (ok) setDraft(null)
    }).finally(() => setSaving(false))
  }

  return (
    <div className="project-grants-editor" aria-label={t('執行者授權')}>
      <h4 className="rp-title">{t('執行者授權')}</h4>
      <p className="hint">{t('把執行者授權給專家：專家可直接派工給它並驗收結果，不必繞回管理者。一顆執行者同時只能授權給一位專家；授權只涵蓋派工，不含啟動、重啟、增聘或改角色。')}</p>
      {!supported ? (
        <p className="hint">{t('daemon 版本太舊，還不支援執行者授權。')}</p>
      ) : specialists.length === 0 || executors.length === 0 ? (
        <p className="hint">{t('需要至少一位專家與一位執行者才能授權。')}</p>
      ) : (
        <div className="grants-list">
          {specialists.map((s) => (
            <fieldset key={s.id} className="grants-row" data-specialist={s.id}>
              <legend>
                {s.name}
                {s.track ? <span className="grants-track">{t(' · {track}', { track: s.track })}</span> : null}
              </legend>
              {executors.map((e) => {
                const holder = current[e.id]
                const takenByOther = holder !== undefined && holder !== s.id
                const checked = holder === s.id
                const g = checked ? savedGrants.find((x) => x.executor === e.id && x.specialist === s.id) : undefined
                const by = g ? (g.granted_by === 'user' || !g.granted_by ? t('使用者') : nameOf(g.granted_by)) : ''
                return (
                  <label key={e.id} className={`grants-exec${takenByOther ? ' taken' : ''}`} data-executor={e.id}>
                    <input type="checkbox" checked={checked} disabled={takenByOther || saving || group.saving} onChange={(ev) => toggle(s.id, e.id, ev.target.checked)} />{' '}
                    {e.name}
                    {takenByOther ? <span className="hint grants-taken">{t('（已授權給 {name}）', { name: nameOf(holder) })}</span> : null}
                    {g ? (
                      <span className="hint grants-meta" title={msgTimeTitle(g.granted_at)}>
                        {t('{by} 授權 · {time}', { by, time: dateTimeLabel(g.granted_at) })}
                      </span>
                    ) : null}
                  </label>
                )
              })}
            </fieldset>
          ))}
        </div>
      )}
      {group.grouped || !supported ? null : (
        <button type="button" className="btn" disabled={!dirty || saving} onClick={save}>
          {t('儲存')}
        </button>
      )}
    </div>
  )
}
