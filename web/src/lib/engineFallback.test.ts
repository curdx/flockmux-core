import { describe, it, expect, vi, beforeEach } from "vitest";
import type { TFunction } from "i18next";

vi.mock("@/lib/toast", () => ({
  toast: { warning: vi.fn() },
}));

import { toast } from "@/lib/toast";
import {
  billingSurfaceLabel,
  fallbackNotice,
  notifySpawnFallbacks,
} from "./engineFallback";

// Billing red line: an engine fallback (user asked for claude, machine only
// has e.g. API-billed reasonix) must NEVER pass silently. These pin the
// notice wording inputs and the "only toasts on a real fallback" contract.

// Minimal i18next stand-in: records the key it was asked for and renders the
// defaultValue with {{var}} interpolation — what the helper falls back to when
// a locale key is absent.
const t = vi.fn((key: string, opts?: Record<string, unknown>) => {
  const tpl = (opts?.defaultValue as string | undefined) ?? key;
  return tpl.replace(/\{\{(\w+)\}\}/g, (_, name) => String(opts?.[name] ?? ""));
}) as unknown as TFunction;

beforeEach(() => {
  vi.mocked(toast.warning).mockClear();
  vi.mocked(t).mockClear();
});

describe("billingSurfaceLabel", () => {
  it("forwards the surface token verbatim as the locale key suffix", () => {
    billingSurfaceLabel(t, "api-key");
    expect(vi.mocked(t)).toHaveBeenCalledWith(
      "engineFallback.surface.api-key",
      expect.anything(),
    );
    billingSurfaceLabel(t, "interactive-subscription");
    expect(vi.mocked(t)).toHaveBeenCalledWith(
      "engineFallback.surface.interactive-subscription",
      expect.anything(),
    );
  });

  it("maps a missing surface to the unknown key (never a crash)", () => {
    billingSurfaceLabel(t, null);
    expect(vi.mocked(t)).toHaveBeenCalledWith(
      "engineFallback.surface.unknown",
      expect.anything(),
    );
  });
});

describe("fallbackNotice", () => {
  it("names the requested engine, the substitute, and its billing surface", () => {
    const msg = fallbackNotice(t, {
      cli: "reasonix",
      fallback_from: "claude",
      billing_surface: "api-key",
    });
    expect(msg).toContain("claude");
    expect(msg).toContain("reasonix");
  });
});

describe("notifySpawnFallbacks", () => {
  it("toasts once per fallback, with a long duration (billing-relevant)", () => {
    notifySpawnFallbacks(t, [
      { cli: "claude" }, // requested engine spawned — silent
      { cli: "reasonix", fallback_from: "claude", billing_surface: "api-key" },
      { cli: "codex", fallback_from: "opencode", billing_surface: "cli-account" },
    ]);
    expect(vi.mocked(toast.warning)).toHaveBeenCalledTimes(2);
    expect(vi.mocked(toast.warning).mock.calls[0][1]).toMatchObject({
      duration: 8000,
    });
  });

  it("stays silent when no fallback happened", () => {
    notifySpawnFallbacks(t, [
      { cli: "claude", fallback_from: null },
      { cli: "codex" },
    ]);
    expect(vi.mocked(toast.warning)).not.toHaveBeenCalled();
  });
});
