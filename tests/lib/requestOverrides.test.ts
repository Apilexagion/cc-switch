import { describe, expect, it } from "vitest";
import {
  buildLocalProxyRequestOverrides,
  formatResponsesRulesDraft,
  isProtectedLocalProxyHeaderName,
  isValidHttpHeaderName,
  isValidHttpHeaderValue,
  parseBodyOverrideJson,
  parseHeaderOverrideJson,
  parseRequestOverrideJson,
  parseResponsesRulesDraft,
} from "@/lib/requestOverrides";

describe("requestOverrides", () => {
  it("treats empty JSON fields as unset", () => {
    expect(buildLocalProxyRequestOverrides("", "   ")).toEqual({});
  });

  it("parses header and body override objects", () => {
    expect(
      buildLocalProxyRequestOverrides(
        '{ "X-Test": "ok" }',
        '{ "temperature": 0.2 }',
      ),
    ).toEqual({
      overrides: {
        headers: { "x-test": "ok" },
        body: { temperature: 0.2 },
      },
    });
  });

  it("parses mixed final rules without changing ordinary overrides", () => {
    const responsesRules = {
      setHeaders: { "x-client": "" },
      setBodyFields: { temperature: 0.2, store: false, user: null },
      removeHeaders: ["session-id"],
      removeBodyFields: ["prompt_cache_key"],
    };
    expect(
      buildLocalProxyRequestOverrides(
        '{ "Session-ID": "from-override" }',
        '{ "user": null }',
        [
          {
            target: "header",
            name: " Session-ID ",
            action: "remove",
            value: "",
          },
          { target: "header", name: "X-Client", action: "set", value: "" },
          {
            target: "body",
            name: "prompt_cache_key",
            action: "remove",
            value: "",
          },
          { target: "body", name: "temperature", action: "set", value: "0.2" },
          { target: "body", name: "store", action: "set", value: "false" },
          { target: "body", name: "user", action: "set", value: "null" },
          {
            target: "header",
            name: "authorization",
            action: "keep",
            value: "",
          },
        ],
      ),
    ).toEqual({
      overrides: {
        headers: { "session-id": "from-override" },
        body: { user: null },
        responsesRules,
      },
    });
    expect(
      parseResponsesRulesDraft(formatResponsesRulesDraft({ responsesRules })),
    ).toEqual({ rules: responsesRules });
    expect(
      buildLocalProxyRequestOverrides(
        "",
        "",
        formatResponsesRulesDraft(undefined),
      ),
    ).toEqual({});
  });

  it("reloads legacy removal lists", () => {
    expect(
      formatResponsesRulesDraft({
        responsesRemovals: {
          removeHeaders: ["Session-ID"],
          removeBodyFields: ["prompt_cache_key"],
        },
      }),
    ).toEqual([
      { target: "header", name: "session-id", action: "remove", value: "" },
      { target: "body", name: "prompt_cache_key", action: "remove", value: "" },
    ]);
  });

  it.each([
    "Authorization",
    "Session_Id",
    "Cookie",
    "Accept",
    "bad header",
    "x-custom-api-key",
  ])("rejects managed or invalid header rules: %s", (name) => {
    expect(
      parseResponsesRulesDraft([
        { target: "header", name, action: "remove", value: "" },
      ]).error,
    ).toBeTruthy();
  });

  it.each(["stream", "session_id", "model", "input", "_private"])(
    "rejects protected body replacements: %s",
    (name) => {
      expect(
        parseResponsesRulesDraft([
          { target: "body", name, action: "set", value: "null" },
        ]).error,
      ).toBeTruthy();
    },
  );

  it("rejects duplicate header actions and invalid body replacement JSON", () => {
    expect(
      parseResponsesRulesDraft([
        { target: "header", name: "Session-ID", action: "remove", value: "" },
        {
          target: "header",
          name: "session-id",
          action: "set",
          value: "different",
        },
      ]).error,
    ).toContain("Duplicate rule");
    for (const value of ["", "unquoted text", "1e999"]) {
      expect(
        parseResponsesRulesDraft([
          { target: "body", name: "temperature", action: "set", value },
        ]).error,
      ).toContain("valid JSON");
    }
  });

  it("rejects non-object body overrides", () => {
    expect(parseRequestOverrideJson("[]").error).toBeTruthy();
  });

  it("rejects non-string header values", () => {
    expect(parseHeaderOverrideJson('{ "X-Test": 1 }').error).toBeTruthy();
  });

  it("rejects invalid header names", () => {
    expect(isValidHttpHeaderName("X-Test")).toBe(true);
    expect(isValidHttpHeaderName("X Foo")).toBe(false);
    expect(isValidHttpHeaderName("Authorization:")).toBe(false);
    expect(parseHeaderOverrideJson('{ "X Foo": "bar" }').error).toBeTruthy();
  });

  it("rejects duplicate header names after case normalization", () => {
    expect(
      parseHeaderOverrideJson('{ "X-Foo": "a", "x-foo": "b" }').error,
    ).toBeTruthy();
  });

  it("rejects protected proxy-managed header names", () => {
    expect(isProtectedLocalProxyHeaderName("Content-Type")).toBe(true);
    expect(isProtectedLocalProxyHeaderName("authorization")).toBe(true);
    expect(isProtectedLocalProxyHeaderName("X-Test")).toBe(false);
    expect(
      parseHeaderOverrideJson('{ "content-type": "text/plain" }').error,
    ).toBeTruthy();
    expect(
      parseHeaderOverrideJson('{ "Authorization": "Bearer x" }').error,
    ).toBeTruthy();
  });

  it("matches backend header value control-character rule", () => {
    expect(isValidHttpHeaderValue("hello\tworld")).toBe(true);
    expect(isValidHttpHeaderValue("hello\nworld")).toBe(false);
  });

  it("rejects stream in body overrides", () => {
    expect(parseBodyOverrideJson('{ "stream": true }').error).toBeTruthy();
    expect(
      buildLocalProxyRequestOverrides("", '{ "stream": false }').error,
    ).toBeTruthy();
  });
});
