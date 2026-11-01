import { useEffect, useState } from 'react'
import type { Bot, Project } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'

const MIN_AFTER_SEC = 60
const DEFAULT_AFTER_SEC = 180

/** 專案設定的「副管理者」：管理者回合中太久時，排隊給它的請求改送這位。只存草稿，按「儲存」才送 PATCH（User-only）。 */
export function ProjectDeputyEditor({ project, bots }: { project: Project; bots: Bot[] }) {
  const t = useT()
  const patchProject = useStore((s) => s.patchProject)
  const saved = project.deputy ?? null
  const [botId, setBotId] = useState(saved?.bot_id ?? '')
  const [afterSec, setAfterSec] = useState(String(saved?.after_sec ?? DEFAULT_AFTER_SEC))
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    setBotId(saved?.bot_id ?? '')
    setAfterSec(String(saved?.after_sec ?? DEFAULT_AFTER_SEC))
  }, [saved?.bot_id, saved?.after_sec, project.id])

  // 只有顧問、專家能當副管理者（管理者與執行者不行）。
  const candidates = bots.filter((b) => b.role === 'advisor' || b.role === 'specialist')
  const sec = Number(afterSec)
  const secOk = Number.isInteger(sec) && sec >= MIN_AFTER_SEC
  const dirty = botId !== (saved?.bot_id ?? '') || (botId !== '' && sec !== (saved?.after_sec ?? DEFAULT_AFTER_SEC))
  const canSave = dirty && !saving && (botId === '' || secOk)

  const save = () => {
    if (!canSave) return
    setSaving(true)
    const body = botId ? { bot_id: botId, after_sec: sec } : null
    void patchProject(project.id, { deputy: body }).finally(() => setSaving(false))
  }

  return (
    <div className="project-deputy-editor" aria-label={t('副管理者')}>
      <h4 className="rp-title">{t('副管理者')}</h4>
      <p className="hint">
        {t('管理者回合中、請求排隊超過門檻時，改送副管理者處理；管理者閒下來後會收到處理摘要。副管理者代班期間可派工與啟動／重啟成員，不能增聘。')}
      </p>
      <label>
        {t('副管理者')}{' '}
        <select value={botId} onChange={(e) => setBotId(e.target.value)} aria-label={t('副管理者')}>
          <option value="">{t('不設')}</option>
          {candidates.map((b) => (
            <option key={b.id} value={b.id}>
              {b.name}
            </option>
          ))}
        </select>
      </label>{' '}
      <label>
        {t('門檻（秒）')}{' '}
        <input
          type="number"
          min={MIN_AFTER_SEC}
          step={10}
          value={afterSec}
          disabled={!botId}
          aria-invalid={botId !== '' && !secOk}
          onChange={(e) => setAfterSec(e.target.value)}
          style={{ width: '6em' }}
        />
      </label>{' '}
      <button type="button" className="btn" disabled={!canSave} onClick={save}>
        {t('儲存')}
      </button>
      {botId !== '' && !secOk ? <p className="hint">{t('門檻最小 {min} 秒', { min: MIN_AFTER_SEC })}</p> : null}
      {candidates.length === 0 ? <p className="hint">{t('沒有可選的成員：先把一位成員設為顧問或專家。')}</p> : null}
    </div>
  )
}
