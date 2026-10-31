import { useEffect, useMemo, useState } from 'react'
import type { Project, RuntimeSettings } from '../api/types'
import { useT } from '../lib/i18n'
import { ROLE_SUGGESTIONS, RUNTIME_ROLES, type RuntimeRole, cleanSettings, firstRangeError, sameSettings } from '../lib/runtimePolicy'
import { useStore } from '../store/store'
import { RuntimeSettingsFields } from './RuntimeSettingsFields'

/** 專案設定的「角色設定」：每個角色一張卡（context、保溫、保溫分鐘、進階）。只存草稿，按「儲存」才送 PATCH（User-only）。 */
export function RolePolicyEditor({ project }: { project: Project }) {
  const t = useT()
  const patchProject = useStore((s) => s.patchProject)
  const saved = useMemo<Record<string, RuntimeSettings>>(() => project.role_policy ?? {}, [project.role_policy])
  const savedKey = JSON.stringify(saved)
  const [draft, setDraft] = useState<Record<string, RuntimeSettings>>(saved)
  const [saving, setSaving] = useState(false)

  // 專案在別處被改了（或換了專案）：草稿跟著存的值走。
  useEffect(() => {
    setDraft(JSON.parse(savedKey) as Record<string, RuntimeSettings>)
  }, [savedKey, project.id])

  const roleLabel: Record<RuntimeRole, string> = {
    manager: t('管理者'),
    specialist: t('專家'),
    advisor: t('顧問'),
    executor: t('執行者'),
  }

  const dirtyRoles = RUNTIME_ROLES.filter((r) => !sameSettings(draft[r] ?? {}, saved[r] ?? {}))
  const rangeErrors = RUNTIME_ROLES.map((r) => ({ role: r, field: firstRangeError(draft[r] ?? {}) })).filter((x) => x.field)
  const canSave = dirtyRoles.length > 0 && rangeErrors.length === 0 && !saving

  const save = () => {
    if (!canSave) return
    const body: Record<string, RuntimeSettings> = {}
    for (const r of RUNTIME_ROLES) {
      const c = cleanSettings(draft[r] ?? {})
      if (Object.keys(c).length > 0) body[r] = c
    }
    setSaving(true)
    void patchProject(project.id, { role_policy: body }).finally(() => setSaving(false))
  }

  return (
    <div className="role-policy-editor" aria-label={t('角色設定')}>
      <h4 className="rp-title">{t('角色設定')}</h4>
      <p className="hint">{t('設定角色的 context 上限與保溫。不設＝跟預設（現有行為）；「加入建議」只帶入草稿，要按儲存才會生效（改動後的 Bot 需重啟）。保溫只影響 claude，codex 永快取不需保溫。')}</p>
      <div className="rp-cards">
        {RUNTIME_ROLES.map((r) => {
          const value = draft[r] ?? {}
          const err = firstRangeError(value)
          const isDirty = dirtyRoles.includes(r)
          return (
            <section key={r} className={`rp-card${isDirty ? ' dirty' : ''}`} aria-label={roleLabel[r]}>
              <header className="rp-card-head">
                <strong>{roleLabel[r]}</strong>
                <span className="rp-cache" title={t('快取命中率需要 daemon 提供，目前沒有資料')}>
                  {t('快取命中')} —
                </span>
              </header>
              <RuntimeSettingsFields
                idPrefix={`rp-${r}`}
                value={value}
                inheritLabel={t('跟預設')}
                disabled={saving}
                onChange={(next) => setDraft((d) => ({ ...d, [r]: next }))}
              />
              {err ? <span className="hint rp-err">{t('數值超出範圍：{field}', { field: err })}</span> : null}
              <div className="rp-card-actions">
                <button type="button" className="btn" disabled={saving} onClick={() => setDraft((d) => ({ ...d, [r]: { ...ROLE_SUGGESTIONS[r] } }))}>
                  {t('加入建議')}
                </button>
              </div>
            </section>
          )
        })}
      </div>
      <div className="rp-savebar">
        <span className="hint">{dirtyRoles.length > 0 ? t('已變更：{roles}', { roles: dirtyRoles.map((r) => roleLabel[r]).join('、') }) : t('沒有變更')}</span>
        <button type="button" className="btn" disabled={dirtyRoles.length === 0 || saving} onClick={() => setDraft(saved)}>
          {t('還原')}
        </button>
        <button type="button" className="btn primary" disabled={!canSave} onClick={save}>
          {t('儲存角色設定')}
        </button>
      </div>
    </div>
  )
}
