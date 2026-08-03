import { describe, expect, it } from "vitest";
import {
  colorFor,
  hasRetryNotice,
  isThrottled,
  provenanceTitle,
  subtitleTone,
  type TaskStatusInfo,
} from "./transfer-row";

function task(over: Partial<TaskStatusInfo> = {}): TaskStatusInfo {
  return {
    status: "running",
    error_code: null,
    notice: null,
    ...over,
  };
}

describe("isThrottled", () => {
  it("is true for a failed task whose error is a rate limit", () => {
    expect(isThrottled(task({ status: "failed", error_code: "network/throttled" }))).toBe(true);
  });

  it("is false for a failed task with any other error", () => {
    expect(isThrottled(task({ status: "failed", error_code: "network/timeout" }))).toBe(false);
  });

  it("is false for a non-failed task even with the throttled code set", () => {
    expect(isThrottled(task({ status: "running", error_code: "network/throttled" }))).toBe(false);
  });
});

describe("hasRetryNotice", () => {
  it("is true when a running task carries a notice", () => {
    expect(
      hasRetryNotice(task({ status: "running", notice: { code: "network/throttled", attempt: 2, max: 5 } })),
    ).toBe(true);
  });

  it("is false when running without a notice", () => {
    expect(hasRetryNotice(task({ status: "running", notice: null }))).toBe(false);
  });

  it("is false for a non-running status even if a notice were present", () => {
    expect(
      hasRetryNotice(task({ status: "paused", notice: { code: "network/throttled", attempt: 1, max: 3 } })),
    ).toBe(false);
  });
});

describe("subtitleTone", () => {
  it("is warning for a throttled failure", () => {
    expect(subtitleTone(task({ status: "failed", error_code: "network/throttled" }))).toBe("warning");
  });

  it("is warning for a running task with a retry notice", () => {
    expect(
      subtitleTone(task({ status: "running", notice: { code: "network/timeout", attempt: 1, max: 3 } })),
    ).toBe("warning");
  });

  it("is destructive for a plain failure", () => {
    expect(subtitleTone(task({ status: "failed", error_code: "network/timeout" }))).toBe("destructive");
  });

  it.each(["running", "queued", "paused", "completed", "canceled"] as const)(
    "is muted for %s with no throttle or notice",
    (status) => {
      expect(subtitleTone(task({ status }))).toBe("muted");
    },
  );
});

describe("colorFor", () => {
  it("uses the warning token when throttled", () => {
    expect(colorFor(task({ status: "failed", error_code: "network/throttled" }))).toBe("var(--warning)");
  });

  it("uses the warning token while retrying", () => {
    expect(
      colorFor(task({ status: "running", notice: { code: "network/timeout", attempt: 1, max: 3 } })),
    ).toBe("var(--warning)");
  });

  it("uses the destructive token for a plain failure", () => {
    expect(colorFor(task({ status: "failed", error_code: "network/timeout" }))).toBe("var(--destructive)");
  });

  it("uses the success token when completed", () => {
    expect(colorFor(task({ status: "completed" }))).toBe("var(--success)");
  });

  it("uses the muted2 token when canceled", () => {
    expect(colorFor(task({ status: "canceled" }))).toBe("var(--muted2)");
  });

  it.each(["running", "queued", "paused"] as const)("uses the primary token for %s", (status) => {
    expect(colorFor(task({ status }))).toBe("var(--primary)");
  });
});

describe("provenanceTitle", () => {
  it("joins the connection name and bucket", () => {
    expect(provenanceTitle("My Bucket Co", "c1", "photos")).toBe("My Bucket Co · photos");
  });

  it("falls back to the bare connection id when the connection is gone", () => {
    expect(provenanceTitle(undefined, "c1", "photos")).toBe("c1 · photos");
  });
});
