export function bumpEventRevs(s: { supervisorRev: number; schedulesRev: number; shareRev: Record<string, number>; bots: readonly { id: string }[] }) {
  const shareRev = { ...s.shareRev }
  for (const b of s.bots) shareRev[b.id] = (shareRev[b.id] ?? 0) + 1
  return { supervisorRev: s.supervisorRev + 1, schedulesRev: s.schedulesRev + 1, shareRev }
}
