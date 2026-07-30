import { describe, expect, it } from "vitest";
import {
  R2_JURISDICTIONS,
  isKnownJurisdiction,
  parseR2Endpoint,
  r2Endpoint,
} from "./r2";

/**
 * These cases are deliberately the same table `provider::r2.rs`'s own tests
 * use. The two implementations are twins and must not drift: a change to one
 * that isn't mirrored in the other shows up as a saved connection whose
 * endpoint the form can no longer parse (or, worse, parses differently than
 * the backend does).
 */

const ACCOUNT = "a1b2c3d4e5f60718293a4b5c6d7e8f90";

describe("r2Endpoint", () => {
  it("gives the default jurisdiction no hostname label", () => {
    expect(r2Endpoint(ACCOUNT, "")).toBe(
      `https://${ACCOUNT}.r2.cloudflarestorage.com`,
    );
  });

  it("makes every other jurisdiction its own hostname label", () => {
    expect(r2Endpoint(ACCOUNT, "eu")).toBe(
      `https://${ACCOUNT}.eu.r2.cloudflarestorage.com`,
    );
    expect(r2Endpoint("abc", "fedramp")).toBe(
      "https://abc.fedramp.r2.cloudflarestorage.com",
    );
  });

  it("trims and lowercases both inputs", () => {
    expect(r2Endpoint("  ABC123 ", " EU ")).toBe(
      "https://abc123.eu.r2.cloudflarestorage.com",
    );
  });
});

describe("parseR2Endpoint", () => {
  it("parses a default-jurisdiction endpoint", () => {
    expect(parseR2Endpoint(`https://${ACCOUNT}.r2.cloudflarestorage.com`)).toEqual({
      accountId: ACCOUNT,
      jurisdiction: "",
    });
  });

  it("parses an eu-jurisdiction endpoint", () => {
    expect(
      parseR2Endpoint(`https://${ACCOUNT}.eu.r2.cloudflarestorage.com`),
    ).toEqual({ accountId: ACCOUNT, jurisdiction: "eu" });
  });

  // The single most useful paste in the whole form: the R2 dashboard shows
  // the endpoint without a scheme, and that is what users copy.
  it("parses a schemeless host", () => {
    expect(parseR2Endpoint("abc.r2.cloudflarestorage.com")).toEqual({
      accountId: "abc",
      jurisdiction: "",
    });
  });

  it("ignores scheme, case, path, port and surrounding whitespace", () => {
    const expected = { accountId: "abc", jurisdiction: "eu" };
    expect(
      parseR2Endpoint("  HTTPS://ABC.EU.R2.CloudflareStorage.com/some/path?x=1  "),
    ).toEqual(expected);
    expect(parseR2Endpoint("http://abc.eu.r2.cloudflarestorage.com:443")).toEqual(
      expected,
    );
  });

  it("round-trips with r2Endpoint for every jurisdiction the app offers", () => {
    for (const { id } of R2_JURISDICTIONS) {
      expect(parseR2Endpoint(r2Endpoint(ACCOUNT, id))).toEqual({
        accountId: ACCOUNT,
        jurisdiction: id,
      });
    }
  });

  // A jurisdiction Cloudflare adds after this build shipped must survive a
  // load/save cycle rather than being silently rewritten to the default.
  it("returns an unrecognized jurisdiction label verbatim", () => {
    expect(parseR2Endpoint("https://abc.apac.r2.cloudflarestorage.com")).toEqual({
      accountId: "abc",
      jurisdiction: "apac",
    });
  });

  it("rejects hosts that are not R2 endpoints", () => {
    for (const host of [
      "https://s3.amazonaws.com",
      "https://oss-cn-hangzhou.aliyuncs.com",
      "https://files.example.com",
      "https://cdn.example.com",
      "http://127.0.0.1:9000",
      "",
    ]) {
      expect(parseR2Endpoint(host)).toBeUndefined();
    }
  });

  it("rejects the bare suffix with no account label", () => {
    expect(parseR2Endpoint("https://r2.cloudflarestorage.com")).toBeUndefined();
  });

  // R2 never produces a three-label name, so this is rejected rather than
  // guessed at.
  it("rejects a host with too many labels", () => {
    expect(
      parseR2Endpoint("https://a.b.c.r2.cloudflarestorage.com"),
    ).toBeUndefined();
  });
});

describe("isKnownJurisdiction", () => {
  it("accepts every jurisdiction in the picker, including the default", () => {
    for (const { id } of R2_JURISDICTIONS) {
      expect(isKnownJurisdiction(id)).toBe(true);
    }
  });

  it("rejects one this build does not offer", () => {
    expect(isKnownJurisdiction("apac")).toBe(false);
  });

  it("offers exactly the default, eu and fedramp jurisdictions", () => {
    expect(R2_JURISDICTIONS.map((j) => j.id)).toEqual(["", "eu", "fedramp"]);
  });
});
