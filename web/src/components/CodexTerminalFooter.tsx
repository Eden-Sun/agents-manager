import type { CodexTerminalFooter } from "../lib/codexTerminalFooter"
import { useT } from "../lib/i18n"

export function CodexTerminalFooterItems({ footer }: { footer: CodexTerminalFooter | null }) {
  const t = useT()
  if (!footer) return null
  return <>
    {footer.warnings !== null ? <span className="sl-item sl-warn" title={t("Codex 警告數；在終端按 F2 查看內容")}>⚠ {footer.warnings} {footer.warnings === 1 ? "warning" : "warnings"}</span> : null}
  </>
}
