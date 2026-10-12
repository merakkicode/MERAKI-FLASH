import assert from "node:assert/strict";
import test from "node:test";
import { isFlashBusy, isTerminalPhase, type FlashPhase } from "../src/lib/flashState.ts";

test("cancelled libera os controles e o fechamento em todas as fases ativas", () => {
  const active: FlashPhase[] = ["preparing", "analyzing", "splitting", "formatting", "extracting", "writing", "syncing", "verifying"];
  for (const phase of active) {
    assert.equal(isFlashBusy(phase), true);
    assert.equal(isTerminalPhase(phase), false);
    const terminal: FlashPhase = "cancelled";
    assert.equal(isTerminalPhase(terminal), true);
    assert.equal(isFlashBusy(terminal), false);
  }
});

test("confirmação bloqueia os controles; fases finais e idle os liberam", () => {
  for (const phase of ["idle", "done", "error", "cancelled"] as const) {
    assert.equal(isFlashBusy(phase), false);
    assert.equal(isFlashBusy(phase, true), true);
  }
  assert.equal(isTerminalPhase("idle"), false);
});
