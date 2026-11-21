import { useT } from '../lib/i18n'

export function AgyUpdateBody({ host, current, target }: { host: string; current: string | null; target: string }) {
  const t = useT()
  return (
    <>
      {t.x('在 {host} 把 agy 從 {from} 更新到官方最新版（約 {target}）：從官方下載（約 50 MB）、驗 sha512 後換掉 {path}；不跑官方 install.sh、{cmd1} 或 {cmd2}。跑著的 agy bot 不會被打斷，重啟後才換版。', {
        host: <strong>{host}</strong>,
        from: current ?? '?',
        target,
        path: <code>~/.local/bin/agy</code>,
        cmd1: <code>agy install</code>,
        cmd2: <code>agy update</code>,
      })}
    </>
  )
}
