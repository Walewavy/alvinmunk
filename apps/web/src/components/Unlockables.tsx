'use client';

import React, { useCallback, useEffect, useState } from 'react';
import { Lock, Check } from 'lucide-react';
import { getWallet } from '@/lib/wallet';
import { getStatus, unlockGate, TRACK, type GateStatus } from '@/lib/gate';
import { Frame } from '@/components/fx/frame';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import { useTranslations } from '@/lib/i18n';

type Row = GateStatus;

/**
 * Unlockables — reputation as a CAPABILITY. Each gate is a perk that your Social/Earned
 * XP opens (read on-chain). Shows locked / unlockable / unlocked; the gate is composable
 * (any app can `check` it). Hides itself when no gates are configured.
 */
export function Unlockables({ address }: { address: string }) {
  const t = useTranslations();
  const [rows, setRows] = useState<Row[] | null>(null);
  const [busy, setBusy] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    const statuses = await getStatus(address).catch(() => [] as GateStatus[]);
    setRows(statuses.filter((s) => s.gate.active));
  }, [address]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function onUnlock(id: number) {
    setBusy(id);
    setError(null);
    try {
      const w = await getWallet();
      await unlockGate(w, id);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : t('unlockables.error'));
    } finally {
      setBusy(null);
    }
  }

  if (rows !== null && rows.length === 0) return null;

  const trackLabel = (track: number) =>
    track === TRACK.EARNED ? t('unlockables.earnedXp') : t('unlockables.socialXp');

  return (
    <Frame label={t('unlockables.frame')} index="ACCESS" accent="tertiary">
      <div className="border-b border-border/60 px-4 py-2.5">
        <p className="text-xs text-muted-foreground">{t('unlockables.intro')}</p>
      </div>
      <ul className="divide-y divide-border/50">
        {(rows ?? []).map((g) => {
          const { gate, passes, unlocked } = g;
          return (
            <li key={gate.id} className="flex items-center gap-3 p-4">
              <div
                className={cn(
                  'grid size-9 shrink-0 place-items-center border',
                  unlocked
                    ? 'border-secondary text-secondary'
                    : passes
                      ? 'border-tertiary text-tertiary'
                      : 'border-border text-muted-foreground',
                )}
              >
                {unlocked ? <Check className="size-4" /> : <Lock className="size-4" />}
              </div>
              <div className="min-w-0 flex-1">
                <p className="truncate text-sm font-medium">{gate.label}</p>
                <p className="font-mono text-[10px] uppercase tracking-wider text-muted-foreground">
                  {t('unlockables.needs', { min: String(gate.min), track: trackLabel(gate.track), cur: String(gate.min) })}
                </p>
              </div>
              {unlocked ? (
                <span className="font-mono text-[10px] uppercase tracking-[0.15em] text-secondary">
                  {t('unlockables.unlocked')}
                </span>
              ) : (
                <Button
                  size="sm"
                  variant={passes ? 'flow' : 'secondary'}
                  disabled={!passes || busy !== null}
                  onClick={() => onUnlock(gate.id)}
                >
                  {busy === gate.id ? t('unlockables.unlocking') : passes ? t('unlockables.unlock') : t('unlockables.locked')}
                </Button>
              )}
            </li>
          );
        })}
      </ul>
      {error && <p className="px-4 py-2 text-sm text-destructive">{error}</p>}
    </Frame>
  );
}
