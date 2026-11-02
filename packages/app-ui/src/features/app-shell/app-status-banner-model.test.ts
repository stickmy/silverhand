import { describe, expect, it } from "vitest";
import { selectAppStatusBanner } from "./app-status-banner-model";
import type { ProtocolSkewView } from "./protocol-skew-presentation";

const skewView: ProtocolSkewView = {
  title: "Silverhand update required",
  detail:
    "This version is incompatible with the server. Update Silverhand to restore agent connectivity.",
};

describe("selectAppStatusBanner", () => {
  it("shows nothing when neither signal is active", () => {
    expect(
      selectAppStatusBanner({ online: true, unreachableVisible: false, skew: null }),
    ).toBeNull();
  });

  it("shows the skew banner when only skew is active", () => {
    expect(
      selectAppStatusBanner({ online: true, unreachableVisible: false, skew: skewView }),
    ).toEqual({
      kind: "protocol_skew",
      view: skewView,
    });
  });

  it("shows the unreachable banner when only unreachable is active", () => {
    expect(selectAppStatusBanner({ online: true, unreachableVisible: true, skew: null })).toEqual({
      kind: "backend_unreachable",
    });
  });

  it("prioritises backend_unreachable over protocol_skew when both are active", () => {
    // The overlap case Cindy flagged: a cached skew state plus a fresh
    // server drop. Unreachable must win the single slot.
    expect(
      selectAppStatusBanner({ online: true, unreachableVisible: true, skew: skewView }),
    ).toEqual({
      kind: "backend_unreachable",
    });
  });

  it("prioritises offline over every other signal", () => {
    expect(
      selectAppStatusBanner({ online: false, unreachableVisible: true, skew: skewView }),
    ).toEqual({
      kind: "offline",
    });
  });
});
