import { describe, expect, it } from "vitest";
import { MAX_AGENT_ENV_VARS } from "@silverhand/shared";
import {
  envDraftFromVars,
  envDraftIsDirty,
  normalizeEnvDraft,
  parseEnvPasteLine,
  validateEnvDraft,
} from "./agent-env-editor";

describe("agent-env-editor helpers", () => {
  it("drops completely blank rows when normalizing and comparing dirty state", () => {
    const saved = [{ name: "SILVERHAND_ACP_AUTO_COMPACT", value: "200000" }];
    const rows = [...envDraftFromVars(saved), { id: "blank-1", name: "   ", value: "" }];
    expect(normalizeEnvDraft(rows)).toEqual(saved);
    expect(envDraftIsDirty(saved, rows)).toBe(false);
    expect(envDraftIsDirty(saved, [...rows, { id: "new-1", name: "FOO", value: "bar" }])).toBe(
      true,
    );
  });

  it("validates names, duplicates, required names, NUL values, and entry caps", () => {
    const valid = validateEnvDraft([
      { id: "1", name: "SILVERHAND_ACP_AUTO_COMPACT", value: "800K" },
      { id: "2", name: "_EMPTY_VAL", value: "" },
    ]);
    expect(valid.ok).toBe(true);
    expect(valid.rowErrors.size).toBe(0);

    const invalid = validateEnvDraft([
      { id: "missing", name: "", value: "only-value" },
      { id: "bad", name: "1INVALID", value: "x" },
      { id: "dup1", name: "FOO", value: "1" },
      { id: "dup2", name: "FOO", value: "2" },
      { id: "nul", name: "BAR", value: "a\0b" },
    ]);
    expect(invalid.ok).toBe(false);
    expect(invalid.rowErrors.get("missing")).toBe("required");
    expect(invalid.rowErrors.get("bad")).toBe("invalid_name");
    expect(invalid.rowErrors.get("dup2")).toBe("duplicate");
    expect(invalid.rowErrors.get("nul")).toBe("invalid_value");

    const overflow = Array.from({ length: MAX_AGENT_ENV_VARS + 1 }, (_, i) => ({
      id: `r-${i}`,
      name: `VAR_${i}`,
      value: "1",
    }));
    const overflowResult = validateEnvDraft(overflow);
    expect(overflowResult.ok).toBe(false);
    expect(overflowResult.tooMany).toBe(true);
  });

  it("splits pasted NAME=VALUE single-line pairs cleanly", () => {
    expect(parseEnvPasteLine("SILVERHAND_ACP_AUTO_COMPACT=200000")).toEqual({
      name: "SILVERHAND_ACP_AUTO_COMPACT",
      value: "200000",
    });
    expect(parseEnvPasteLine("TOKEN=a=b=c")).toEqual({ name: "TOKEN", value: "a=b=c" });
    expect(parseEnvPasteLine("not-an-assignment")).toBeNull();
    expect(parseEnvPasteLine("1BAD=val")).toBeNull();
    expect(parseEnvPasteLine("A=1\nB=2")).toBeNull();
  });
});
