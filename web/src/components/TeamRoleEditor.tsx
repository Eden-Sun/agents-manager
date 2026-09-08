import { useEffect, useState } from 'react'
import type { BotKind, TeamRoleKey, TeamRolePatch, TeamWorkerSpec } from '../api/types'
import { BOT_KINDS, LOCAL_HOST, TEAM_ROLE_KEYS, TEAM_ROLE_KEY_LABEL } from '../api/types'
import { toolsOfHost, useStore } from '../store/store'
import { IdentityOptions } from './BotSettingsPanel'
import { KindTag } from './KindTag'
import { Modal } from './Modal'
import { ApiModelFields } from './ModelPicker'

/**
 * SPEC-team §10.5 — 改 team 三個角色（PM / 執行者 / Reviewer）的 kind、身分、模型。
 *
 * 為什麼不是走 bot 設定：team 成員是從 `roles_json` 建出來的，下一批執行者、reopen 之後
 * 重建的成員都照那份 spec。從 bot 設定改一個成員只會改到那一列，`roles_json` 不動——
 * 下一批立刻又跑回舊設定，兩邊永遠對不起來。所以 team 成員一律從這裡改。
 *
 * 為什麼 kind 要特別講：成員跑哪個 CLI 是開 pane 時決定的，改 kind = daemon 換一個 bot
 * （§7.6：同名、同 cwd、未做完的 task 跟著搬，舊 bot 的訊息留著）。該成員正在跑一個 turn
 * 時 daemon 回 409 `member busy`，要先暫停。
 */

const ROLE_HINT: Record<TeamRoleKey, string> = {
  pm: '拆 task、派工、收回報。全程同一個 bot，換 kind 會換掉它（對話另起）。',
  workers: '實際寫程式的那幾個。改設定預設只對下一批生效，換 kind 則是立刻換人。',
  reviewer: '唯讀審查。全程同一個 bot，換 kind 會換掉它（對話另起）。',
}

function specLabel(spec: TeamWorkerSpec): string {
  const bits = [spec.model ?? '預設模型']
  if (spec.effort) bits.push(spec.effort)
  if (spec.fast) bits.push('fast')
  if (spec.identity) bits.push(spec.identity)
  return bits.join(' · ')
}

/** 一個角色一列：現況一行字，右邊一顆「修改」。 */
function RoleRow({ teamId, role, onEdit }: { teamId: string; role: TeamRoleKey; onEdit: () => void }) {
  const spec = useStore((s) => s.teamDetail[teamId]?.roles[role] ?? null)
  if (!spec) return null
  return (
    <div className="team-role-row">
      <span className="team-role-name">{TEAM_ROLE_KEY_LABEL[role]}</span>
      <KindTag kind={spec.kind} />
      <span className="team-role-spec mono" title={ROLE_HINT[role]}>
        {specLabel(spec)}
      </span>
      <button type="button" className="mini-btn" onClick={onEdit} title={`修改${TEAM_ROLE_KEY_LABEL[role]}的 kind / 身分 / 模型`}>
        修改
      </button>
    </div>
  )
}

function RoleForm({
  teamId,
  role,
  host,
  onClose,
}: {
  teamId: string
  role: TeamRoleKey
  host: string
  onClose: () => void
}) {
  const spec = useStore((s) => s.teamDetail[teamId]?.roles[role] ?? null)
  const patchTeam = useStore((s) => s.patchTeam)
  const busy = useStore((s) => Boolean(s.busy[`team:${teamId}:patch`]))
  const tools = useStore((s) => toolsOfHost(s, host))
  const [kind, setKind] = useState<BotKind>(spec?.kind ?? 'claude')
  const [model, setModel] = useState<string | null>(spec?.model ?? null)
  const [effort, setEffort] = useState<string | null>(spec?.effort ?? null)
  const [fast, setFast] = useState(spec?.fast ?? false)
  const [identity, setIdentity] = useState(spec?.identity ?? '')
  const [apply, setApply] = useState<'next' | 'now'>('next')

  // 換一個角色（側欄點了另一個成員的齒輪）就整組欄位重讀，不要留著上一個角色的值。
  useEffect(() => {
    if (!spec) return
    setKind(spec.kind)
    setModel(spec.model)
    setEffort(spec.effort)
    setFast(spec.fast)
    setIdentity(spec.identity ?? '')
    setApply('next')
    // 只在切換角色 / team 時重置：spec 每次 loadTeam 都是新物件，跟著它跑會把使用者打到一半的值蓋掉。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [teamId, role])

  if (!spec) return null
  const swapping = kind !== spec.kind
  const save = async () => {
    const body: TeamRolePatch = { model, effort, fast, identity: identity || null, apply }
    if (swapping) body.kind = kind
    const ok = await patchTeam(teamId, { [role]: body })
    if (ok) onClose()
  }

  return (
    <Modal
      open
      title={`${TEAM_ROLE_KEY_LABEL[role]}的設定`}
      subtitle={host === LOCAL_HOST ? '本機' : host}
      onClose={onClose}
    >
      <form
        className="sheet-form"
        onSubmit={(e) => {
          e.preventDefault()
          void save()
        }}
      >
        <p className="team-card-hint">{ROLE_HINT[role]}</p>
        <div className="field">
          <span>kind</span>
          <div className="opt-group kinds" role="radiogroup" aria-label={`${TEAM_ROLE_KEY_LABEL[role]} kind`}>
            {BOT_KINDS.map((k) => {
              const missing = !tools[k].installed
              return (
                <button
                  key={k}
                  type="button"
                  role="radio"
                  aria-checked={kind === k}
                  className={`opt${kind === k ? ' on' : ''}`}
                  disabled={missing}
                  title={missing ? `${host === LOCAL_HOST ? '本機' : host} 尚未安裝 ${k}` : k}
                  onClick={() => {
                    // 模型與身分都是綁 kind 的，換 kind 就一起清掉——留著上一個 CLI 的
                    // 模型名字送出去只會被 daemon 打回來。
                    setKind(k)
                    setModel(null)
                    setEffort(null)
                    setFast(false)
                    setIdentity('')
                  }}
                >
                  <KindTag kind={k} />
                  <span className="opt-label">{k}</span>
                  {missing ? <span className="kind-missing-reason">未安裝</span> : null}
                </button>
              )
            })}
          </div>
          {swapping ? (
            <span className="hint warn">
              換 kind = 換一個 bot：同名、同工作目錄，未做完的 task 跟著搬，舊 bot 的訊息留著。
              該成員正在跑一個 turn 時會被擋下來（請先暫停 Team）。
            </span>
          ) : null}
        </div>
        <ApiModelFields
          kind={kind}
          host={host}
          identity={identity || null}
          model={model}
          onModel={setModel}
          effort={effort}
          onEffort={setEffort}
          fast={fast}
          onFast={setFast}
        />
        <IdentityOptions kind={kind} host={host} value={identity} onChange={setIdentity} recheck={false} />
        {swapping ? null : (
          <fieldset className="field">
            <span>何時生效</span>
            <div className="opt-group" role="radiogroup" aria-label="何時生效">
              <button
                type="button"
                className={`opt${apply === 'next' ? ' on' : ''}`}
                role="radio"
                aria-checked={apply === 'next'}
                onClick={() => setApply('next')}
              >
                下一批
              </button>
              <button
                type="button"
                className={`opt${apply === 'now' ? ' on' : ''}`}
                role="radio"
                aria-checked={apply === 'now'}
                onClick={() => setApply('now')}
              >
                立即重啟
              </button>
            </div>
            <span className="hint">
              {apply === 'now'
                ? '這個角色有 run 的成員會馬上停掉再啟動，進行中的工作會斷。'
                : '現有成員照舊跑到換批或重啟；spec 先寫回去，之後建立的成員照新的。'}
            </span>
          </fieldset>
        )}
        <div className="form-actions">
          <button type="button" className="btn" onClick={onClose}>
            取消
          </button>
          <button type="submit" className="btn primary" disabled={busy}>
            {swapping ? '換人' : apply === 'now' ? '儲存並重啟' : '儲存'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/**
 * TeamPanel 副標題列那一塊：三個角色各一列，點「修改」開表單。
 *
 * 側欄成員列的齒輪也開這裡（`openTeamRole`），所以彈窗開在哪個角色由 store 決定，
 * 兩個入口共用同一份表單。
 */
export function TeamRoleEditor({ teamId, host, disabled }: { teamId: string; host: string; disabled: boolean }) {
  const roles = useStore((s) => s.teamDetail[teamId]?.roles ?? null)
  const editing = useStore((s) => (s.teamRoleEdit?.teamId === teamId ? s.teamRoleEdit.role : null))
  const openTeamRole = useStore((s) => s.openTeamRole)
  const closeTeamRole = useStore((s) => s.closeTeamRole)
  if (!roles) return null
  return (
    <div className="team-roles">
      {TEAM_ROLE_KEYS.filter((r) => roles[r] !== null).map((r) => (
        <RoleRow key={r} teamId={teamId} role={r} onEdit={() => openTeamRole(teamId, r)} />
      ))}
      {editing && !disabled ? <RoleForm teamId={teamId} role={editing} host={host} onClose={closeTeamRole} /> : null}
    </div>
  )
}
