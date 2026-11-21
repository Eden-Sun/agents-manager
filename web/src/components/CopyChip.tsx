import { copyText } from "../lib/copyText"
import { useState } from "react"
import { useT } from "../lib/i18n"
import "./copyChip.css"

export function CopyChip({
  label,
  value,
  title,
  className,
}: {
  label: string
  value: string
  title: string
  className?: string
}) {
  const t = useT()
  const [done, setDone] = useState<"ok" | "fail" | null>(null)
  if (!value) return null
  if (done === "fail") {
    return (
      <span className={`copy-chip copy-chip-selectable${className ? " " + className : ""}`} title={title}>
        <span className="copy-chip-label">{label}</span>
        <span className="copy-chip-value mono">{value}</span>
        <span className="copy-chip-hint">{t("複製失敗，長按選取")}</span>
      </span>
    )
  }
  return (
    <button
      type="button"
      className={`copy-chip${done === "ok" ? " done" : ""}${className ? " " + className : ""}`}
      title={`${title}\n${value}${t("（點擊複製）")}`}
      onClick={() => {
        void copyText(value).then((ok) => {
          if (!ok) return setDone("fail")
          setDone("ok")
          setTimeout(() => setDone(null), 1200)
        })
      }}
    >
      <span className="copy-chip-label">{label}</span>
      <span className="copy-chip-value mono">{value}</span>
      {done === "ok" ? <span className="copy-chip-ok">{t("已複製")}</span> : null}
    </button>
  )
}
