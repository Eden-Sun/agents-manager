import { useMemo } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { useStore } from '../store/store'
import { onboardingModel, type OnbModel } from './onboarding'

/** 本機的環境準備現況；選取的欄位都是穩定引用，結果用 `useMemo` 算（React #185）。 */
export function useOnboarding(): OnbModel {
  const { tools, identities, herdrConnected, projectCount } = useStore(
    useShallow((s) => ({ tools: s.localTools, identities: s.localIdentityStatus, herdrConnected: s.connected, projectCount: s.projects.length })),
  )
  return useMemo(() => onboardingModel({ tools, identities, herdrConnected, projectCount }), [tools, identities, herdrConnected, projectCount])
}
