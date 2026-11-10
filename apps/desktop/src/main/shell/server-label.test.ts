import { describe, expect, it } from "vitest";
import { serverLabel } from "./server-label.js";

describe("serverLabel", () => {
  it("keeps only the host", () => {
    expect(serverLabel("https://api.silverhand.build/")).toBe("api.silverhand.build");
    expect(serverLabel("https://api.silverhand.build:443/v1")).toBe("api.silverhand.build");
  });

  it("keeps a port that is not the default", () => {
    expect(serverLabel("http://192.168.1.5:8080")).toBe("192.168.1.5:8080");
    expect(serverLabel("http://[::1]:3000")).toBe("[::1]:3000");
  });

  it("cuts a long host in the middle", () => {
    const label = serverLabel("https://a-really-long-subdomain-name.internal.example.com");
    expect(label).toHaveLength(32);
    expect(label.startsWith("a-really-long-su")).toBe(true);
    expect(label.endsWith("ernal.example.com".slice(-15))).toBe(true);
    expect(label).toContain("…");
  });

  it("shows an unparseable address as typed", () => {
    expect(serverLabel("not a url")).toBe("not a url");
  });
});
