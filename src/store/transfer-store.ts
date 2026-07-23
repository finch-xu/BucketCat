/**
 * Zustand store for the transfer panel: the frontend-side mirror of the
 * backend's transfer engine state.
 *
 * Why Zustand and not the existing `AppStore` React context: progress
 * arrives at ~6.7Hz per running task (one `transfer://progress` batch every
 * 150ms). The context in `src/store/app-store.tsx` rebuilds its whole value
 * object on every render and is read by every pane, so routing progress
 * through it would re-render the entire app -- including the virtualized
 * object list -- on every tick. Zustand's selector subscriptions let a
 * single transfer row subscribe to exactly its own task and nothing else.
 */
import { create } from "zustand";
import { useShallow } from "zustand/react/shallow";
import type { TransferProgress, TransferStatus, TransferTask } from "@/lib/api";

/** A `TransferTask` plus the two fields the backend recomputes every tick
 * instead of storing -- they only exist on this side of the wire. Cleared
 * back to their zero values whenever the task leaves `running` (see
 * `applyState`), so a stale reading never lingers next to a paused or
 * finished row. */
export interface LiveTransfer extends TransferTask {
  speed: number;
  eta_secs: number | null;
}

function toLiveTransfer(task: TransferTask): LiveTransfer {
  return { ...task, speed: 0, eta_secs: null };
}

export interface TransferSummary {
  activeCount: number;
  speed: number;
  pct: number;
}

/** Footer aggregate over `queued` + `running` tasks. Byte-weighted, not
 * task-averaged: a lone 4GB task beside three 1KB ones must not read as 75%
 * done just because 3 of 4 tasks are "finished". */
export function summarize(tasks: LiveTransfer[]): TransferSummary {
  const active = tasks.filter((t) => t.status === "queued" || t.status === "running");
  if (active.length === 0) return { activeCount: 0, speed: 0, pct: 0 };

  let transferred = 0;
  let total = 0;
  let speed = 0;
  for (const t of active) {
    transferred += t.transferred;
    total += t.total;
    speed += t.speed;
  }

  return {
    activeCount: active.length,
    speed,
    pct: total > 0 ? Math.round((transferred / total) * 100) : 0,
  };
}

interface TransferState {
  tasks: Record<string, LiveTransfer>;
  /** Task ids, newest first (highest `seq` first). */
  order: string[];
  panelOpen: boolean;

  /** Applies a full-task snapshot from `transfer://state` -- a new task is
   * inserted (newest-first) and an existing one is updated in place without
   * reordering. Clears `speed`/`eta_secs` whenever the incoming status is
   * not `running`, so e.g. a paused transfer never keeps showing its last
   * speed reading. */
  applyState: (task: TransferTask) => void;
  /** Merges a `transfer://progress` batch into the matching tasks. Tasks
   * absent from the batch are left untouched -- not cloned -- so unrelated
   * rows keep their object identity and don't re-render. Progress for a
   * task id the store doesn't know yet is dropped rather than synthesized:
   * a batch can race a tick ahead of the state event that introduces the
   * task, and a half-populated row is worse than a missing frame. */
  applyProgress: (batch: TransferProgress[]) => void;
  /** Rebuilds the whole map and order from a fresh `listTransfers()`
   * snapshot, discarding anything not in `tasks`. */
  replaceAll: (tasks: TransferTask[]) => void;
  /** Removes one task from both the map and the order. */
  drop: (id: string) => void;

  togglePanel: () => void;
  setPanelOpen: (open: boolean) => void;
}

function orderFor(tasks: Record<string, LiveTransfer>): string[] {
  return Object.values(tasks)
    .sort((a, b) => b.seq - a.seq)
    .map((t) => t.id);
}

function isRunning(status: TransferStatus): boolean {
  return status === "running";
}

export const useTransferStore = create<TransferState>((set) => ({
  tasks: {},
  order: [],
  panelOpen: false,

  applyState: (task) =>
    set((state) => {
      const existing = state.tasks[task.id];
      const next: LiveTransfer = isRunning(task.status)
        ? { ...task, speed: existing?.speed ?? 0, eta_secs: existing?.eta_secs ?? null }
        : toLiveTransfer(task);

      const tasks = { ...state.tasks, [task.id]: next };
      const order = existing ? state.order : [task.id, ...state.order];
      return { tasks, order };
    }),

  applyProgress: (batch) =>
    set((state) => {
      let tasks = state.tasks;
      let changed = false;
      for (const p of batch) {
        const existing = tasks[p.task_id];
        if (!existing) continue;
        if (!changed) {
          tasks = { ...tasks };
          changed = true;
        }
        tasks[p.task_id] = {
          ...existing,
          transferred: p.transferred,
          total: p.total,
          speed: p.speed,
          eta_secs: p.eta_secs,
        };
      }
      return changed ? { tasks } : state;
    }),

  replaceAll: (list) =>
    set(() => {
      const tasks: Record<string, LiveTransfer> = {};
      for (const t of list) tasks[t.id] = toLiveTransfer(t);
      return { tasks, order: orderFor(tasks) };
    }),

  drop: (id) =>
    set((state) => {
      if (!(id in state.tasks)) return state;
      const tasks = { ...state.tasks };
      delete tasks[id];
      return { tasks, order: state.order.filter((existingId) => existingId !== id) };
    }),

  togglePanel: () => set((state) => ({ panelOpen: !state.panelOpen })),
  setPanelOpen: (open) => set({ panelOpen: open }),
}));

/** Task ids, newest first. Subscribe here for the panel's list/rendering
 * order -- not for the tasks themselves, so adding/reordering the list
 * doesn't force every row to re-render. */
export function useTransferOrder(): string[] {
  return useTransferStore((state) => state.order);
}

/** One task by id. `undefined` if it isn't (or is no longer) known -- a row
 * component should unmount itself in that case rather than render stale
 * data. */
export function useTransferTask(id: string): LiveTransfer | undefined {
  return useTransferStore((state) => state.tasks[id]);
}

export function useTransferPanelOpen(): boolean {
  return useTransferStore((state) => state.panelOpen);
}

/** Footer aggregate. Uses `useShallow` because the selector below builds a
 * fresh object on every call -- without it, React 19's
 * `useSyncExternalStore` would see a new reference each render and loop with
 * "getSnapshot should be cached". */
export function useTransferSummary(): TransferSummary {
  return useTransferStore(useShallow((state) => summarize(Object.values(state.tasks))));
}
