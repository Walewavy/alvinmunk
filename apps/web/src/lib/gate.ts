/**
 * Gate client — reputation as a capability. Lists access gates, checks/records unlocks.
 * The composable bit: `check(addr, id)` is a pure on-chain read any app can call.
 */
import { invokeAndWait, invokeAndWaitHash, readPublic, args, gateId } from './contracts';
import { readClient } from './sdk';
import type { Wallet } from './wallet';

export const TRACK = { SOCIAL: 0, EARNED: 1 } as const;

export interface Gate {
  id: number;
  track: number; // 0 = Social, 1 = Earned
  min: number;
  label: string;
  active: boolean;
}

/** Per-gate status returned by the batch gate read. */
export interface GateStatus {
  gate: Gate;
  /** Does the address meet the gate's reputation threshold? */
  passes: boolean;
  /** Has the address recorded the unlock on-chain? */
  unlocked: boolean;
}

function normalizeGate(g: {
  id: number;
  track: number;
  min: bigint;
  label: string;
  active: boolean;
}): Gate {
  return {
    id: Number(g.id),
    track: Number(g.track),
    min: Number(g.min),
    label: String(g.label),
    active: Boolean(g.active),
  };
}

export async function getGates(): Promise<Gate[]> {
  if (!gateId()) return [];
  return readGates().catch(() => []);
}

/** Every gate, active or not. Throws on RPC failure (the admin table must not read an
 * outage as "no gates"); `getGates` is the forgiving variant for player views. */
export async function readGates(): Promise<Gate[]> {
  const raw = await readPublic<
    Array<{ id: number; track: number; min: bigint; label: string; active: boolean }>
  >(gateId(), 'get_gates', []);
  return (raw ?? []).map(normalizeGate);
}

/** Composable read — does `address` pass gate `id`? (cross-reads reputation on-chain). */
export async function checkGate(address: string, id: number): Promise<boolean> {
  if (!gateId()) return false;
  return readClient()
    .checkGate(address, id)
    .catch(() => false);
}

/** Batch check — does `address` pass each of `ids`? Reputation is read once on-chain. */
export async function checkMany(address: string, ids: number[]): Promise<boolean[]> {
  if (!gateId()) return ids.map(() => false);
  const raw = await readPublic<boolean[]>(gateId(), 'check_many', [
    args.addr(address),
    args.u32List(ids),
  ]);
  return (raw ?? []).map(Boolean);
}

/** Every gate with its pass/unlocked status, from a single on-chain read. */
export async function getStatus(address: string): Promise<GateStatus[]> {
  if (!gateId()) return [];
  const raw = await readPublic<
    Array<{ gate: { id: number; track: number; min: bigint; label: string; active: boolean }; passes: boolean; unlocked: boolean }>
  >(gateId(), 'get_status', [args.addr(address)]);
  return (raw ?? []).map((s) => ({
    gate: normalizeGate(s.gate),
    passes: Boolean(s.passes),
    unlocked: Boolean(s.unlocked),
  }));
}

export async function isUnlocked(address: string, id: number): Promise<boolean> {
  if (!gateId()) return false;
  return (
    (await readPublic<boolean>(gateId(), 'is_unlocked', [args.addr(address), args.u32(id)]).catch(
      () => false,
    )) ?? false
  );
}

export async function unlockGate(wallet: Wallet, id: number): Promise<void> {
  await invokeAndWait(gateId(), 'unlock', [args.addr(wallet.address), args.u32(id)], wallet);
}

// --- Admin content management. Every write is `admin.require_auth()`-gated on-chain. ---

/** Define or replace gate `id` (always saved ACTIVE). Resolves the confirmed tx hash. */
export async function createGate(
  wallet: Wallet,
  id: number,
  track: number,
  min: bigint,
  label: string,
): Promise<string> {
  return invokeAndWaitHash(
    gateId(),
    'create_gate',
    [args.u32(id), args.u32(track), args.u64(min), args.str(label)],
    wallet,
  );
}

export async function setGateActive(wallet: Wallet, id: number, active: boolean): Promise<string> {
  return invokeAndWaitHash(gateId(), 'set_gate_active', [args.u32(id), args.bool(active)], wallet);
}
