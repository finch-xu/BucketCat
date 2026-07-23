import { beforeEach, describe, expect, it } from "vitest";
import type { TransferTask } from "@/lib/api";
import { summarize, useTransferStore } from "./transfer-store";

function task(over: Partial<TransferTask> = {}): TransferTask {
  return {
    id: "t1",
    seq: 1,
    direction: "upload",
    connection_id: "c1",
    bucket: "b",
    key: "a.bin",
    local_path: "/tmp/a.bin",
    file_name: "a.bin",
    total: 1000,
    transferred: 0,
    status: "queued",
    error_code: null,
    ...over,
  };
}

beforeEach(() => {
  useTransferStore.getState().replaceAll([]);
});

describe("transfer store", () => {
  it("orders tasks newest first", () => {
    const s = useTransferStore.getState();
    s.applyState(task({ id: "a", seq: 1 }));
    s.applyState(task({ id: "b", seq: 2 }));
    s.applyState(task({ id: "c", seq: 3 }));
    expect(useTransferStore.getState().order).toEqual(["c", "b", "a"]);
  });

  it("updates a task in place without reordering", () => {
    const s = useTransferStore.getState();
    s.applyState(task({ id: "a", seq: 1 }));
    s.applyState(task({ id: "b", seq: 2 }));
    s.applyState(task({ id: "a", seq: 1, status: "running" }));
    expect(useTransferStore.getState().order).toEqual(["b", "a"]);
    expect(useTransferStore.getState().tasks.a.status).toBe("running");
  });

  it("keeps the object identity of untouched tasks", () => {
    const s = useTransferStore.getState();
    s.applyState(task({ id: "a", seq: 1 }));
    s.applyState(task({ id: "b", seq: 2 }));
    const bBefore = useTransferStore.getState().tasks.b;
    s.applyProgress([
      { task_id: "a", transferred: 100, total: 1000, speed: 50, eta_secs: 18 },
    ]);
    expect(useTransferStore.getState().tasks.b).toBe(bBefore);
  });

  it("merges progress into the matching task", () => {
    const s = useTransferStore.getState();
    s.applyState(task({ id: "a", seq: 1, status: "running" }));
    s.applyProgress([
      { task_id: "a", transferred: 250, total: 1000, speed: 500, eta_secs: 2 },
    ]);
    const a = useTransferStore.getState().tasks.a;
    expect(a.transferred).toBe(250);
    expect(a.speed).toBe(500);
    expect(a.eta_secs).toBe(2);
    expect(a.status).toBe("running");
  });

  it("ignores progress for an unknown task", () => {
    const s = useTransferStore.getState();
    s.applyProgress([
      { task_id: "ghost", transferred: 1, total: 2, speed: 1, eta_secs: 1 },
    ]);
    expect(useTransferStore.getState().order).toEqual([]);
  });

  it("clears speed and eta when a task leaves the running state", () => {
    const s = useTransferStore.getState();
    s.applyState(task({ id: "a", seq: 1, status: "running" }));
    s.applyProgress([
      { task_id: "a", transferred: 900, total: 1000, speed: 900, eta_secs: 1 },
    ]);
    s.applyState(task({ id: "a", seq: 1, status: "paused", transferred: 900 }));
    const a = useTransferStore.getState().tasks.a;
    expect(a.speed).toBe(0);
    expect(a.eta_secs).toBeNull();
  });

  it("replaceAll rebuilds both the map and the order", () => {
    const s = useTransferStore.getState();
    s.applyState(task({ id: "old", seq: 9 }));
    s.replaceAll([task({ id: "x", seq: 1 }), task({ id: "y", seq: 2 })]);
    expect(useTransferStore.getState().order).toEqual(["y", "x"]);
    expect(useTransferStore.getState().tasks.old).toBeUndefined();
  });

  it("drop removes a task from both the map and the order", () => {
    const s = useTransferStore.getState();
    s.applyState(task({ id: "a", seq: 1 }));
    s.applyState(task({ id: "b", seq: 2 }));
    s.drop("a");
    expect(useTransferStore.getState().order).toEqual(["b"]);
    expect(useTransferStore.getState().tasks.a).toBeUndefined();
  });
});

describe("summarize", () => {
  it("reports nothing for an empty list", () => {
    expect(summarize([])).toEqual({ activeCount: 0, speed: 0, pct: 0 });
  });

  it("counts queued and running as active and sums their speed", () => {
    const s = summarize([
      { ...task({ id: "a", status: "running", transferred: 500 }), speed: 100, eta_secs: 5 },
      { ...task({ id: "b", status: "queued" }), speed: 0, eta_secs: null },
      { ...task({ id: "c", status: "completed", transferred: 1000 }), speed: 0, eta_secs: null },
    ]);
    expect(s.activeCount).toBe(2);
    expect(s.speed).toBe(100);
    expect(s.pct).toBe(25);
  });

  it("does not divide by zero when active tasks have no known size", () => {
    const s = summarize([
      { ...task({ id: "a", status: "queued", total: 0 }), speed: 0, eta_secs: null },
    ]);
    expect(s.pct).toBe(0);
  });
});
