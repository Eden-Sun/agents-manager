import { useStore, projectHostName } from '../store/store'
import { hostLabel } from '../store/identityRows'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'

/**
 * 額度 popover 裡 agy 那格的「登出」（使用者 2026-10-05）。agy 沒有 `logout` 子命令，daemon 直接刪該主機的 OAuth 憑證檔
 * （`POST /api/hosts/{name}/agy/logout`），所以不開 pane。確認框放在 `QuotaStrip` 外層、由它持有開關：
 * popover 的「點外面就關」會把 popover 內的元件卸掉，框掛在裡面就會跟著消失。
 */
export function AgyLogoutButton({ host, onOpen, busy }: { host: string; onOpen: () => void; busy: boolean }) {
  const t = useT()
  return (
    <button
      type="button"
      className="mini-btn quota-agy-logout"
      disabled={busy}
      title={t('清掉{host}的 agy 登入憑證', { host: hostLabel(host) })}
      onClick={onOpen}
    >
      {busy ? t('登出中…') : t('登出 agy')}
    </button>
  )
}

/** 這台主機上正在跑的 agy bot 數：憑證清掉後它們會失去授權。 */
function useRunningAgyBots(host: string): number {
  return useStore((s) =>
    s.bots.filter((b) => {
      if (b.kind !== 'agy' || projectHostName(s, b.project_id) !== (host || 'local')) return false
      const r = s.runs[b.id]
      return r ? r.state !== 'stopped' && r.state !== 'exited' : false
    }).length,
  )
}

export function AgyLogoutDialog({ host, open, onClose }: { host: string; open: boolean; onClose: () => void }) {
  const t = useT()
  const logoutAgy = useStore((s) => s.logoutAgy)
  const running = useRunningAgyBots(host)
  return (
    <ConfirmDialog
      open={open}
      title={t('登出 agy')}
      body={
        <>
          {t.x('會刪掉{host}上 agy 的登入憑證，之後要在 agy 裡重新登入才能用。', { host: hostLabel(host) })}
          {running > 0 ? (
            <>
              <br />
              <strong>{t('目前有 {n} 顆 agy bot 在跑', { n: running })}</strong>
              {t('：不會被停掉，但憑證清掉後它們會失去授權。')}
            </>
          ) : null}
        </>
      }
      confirmLabel={t('登出')}
      danger
      onCancel={onClose}
      onConfirm={() => {
        onClose()
        void logoutAgy(host)
      }}
    />
  )
}
