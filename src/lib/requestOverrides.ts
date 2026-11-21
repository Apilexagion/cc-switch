import type {
  LocalProxyRequestOverrides,
  LocalProxyResponsesRules,
} from "@/types";

export interface RequestOverrideJsonResult {
  value?: Record<string, unknown>;
  error?: string;
}

export interface HeaderOverrideValidationResult {
  headers?: Record<string, string>;
  error?: string;
}

export interface ResponsesRuleDraft {
  target: "header" | "body";
  name: string;
  action: "keep" | "set" | "remove";
  value: string;
}

// RFC 9110 HTTP field-name token. Keep this aligned with Rust's
// http::HeaderName parser for user-facing validation.
export function isValidHttpHeaderName(name: string): boolean {
  return /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(name);
}

export const PROTECTED_LOCAL_PROXY_HEADER_NAMES = new Set([
  "host",
  "content-length",
  "transfer-encoding",
  "connection",
  "proxy-authorization",
  "proxy-authenticate",
  "te",
  "trailer",
  "upgrade",
  "accept-encoding",
  "content-type",
  "authorization",
  "x-api-key",
  "x-goog-api-key",
  "chatgpt-account-id",
  "session_id",
  "x-client-request-id",
  "x-codex-window-id",
  "x-forwarded-host",
  "x-forwarded-port",
  "x-forwarded-proto",
  "forwarded",
  "cf-connecting-ip",
  "cf-ipcountry",
  "cf-ray",
  "cf-visitor",
  "true-client-ip",
  "fastly-client-ip",
  "x-azure-clientip",
  "x-azure-fdid",
  "x-azure-ref",
  "akamai-origin-hop",
  "x-akamai-config-log-detail",
  "x-request-id",
  "x-correlation-id",
  "x-trace-id",
  "x-amzn-trace-id",
  "x-b3-traceid",
  "x-b3-spanid",
  "x-b3-parentspanid",
  "x-b3-sampled",
  "traceparent",
  "tracestate",
]);

export function isProtectedLocalProxyHeaderName(name: string): boolean {
  return PROTECTED_LOCAL_PROXY_HEADER_NAMES.has(name.toLowerCase());
}

// Keep this aligned with Rust's http::HeaderValue runtime guard: only control
// characters other than tab are rejected.
export function isValidHttpHeaderValue(value: string): boolean {
  // eslint-disable-next-line no-control-regex
  return !/[\x00-\x08\x0a-\x1f\x7f]/.test(value);
}

export function isPlainObject(
  value: unknown,
): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function parseRequestOverrideJson(
  raw: string,
): RequestOverrideJsonResult {
  const trimmed = raw.trim();
  if (!trimmed) return {};

  try {
    const parsed = JSON.parse(trimmed);
    if (!isPlainObject(parsed)) {
      return { error: "JSON must be an object" };
    }
    return { value: parsed };
  } catch (error) {
    return {
      error: error instanceof Error ? error.message : "Invalid JSON",
    };
  }
}

export function parseBodyOverrideJson(raw: string): RequestOverrideJsonResult {
  const parsed = parseRequestOverrideJson(raw);
  if (parsed.error || !parsed.value) {
    return parsed;
  }
  if (Object.prototype.hasOwnProperty.call(parsed.value, "stream")) {
    return { error: 'Body override must not include protocol field "stream"' };
  }
  return parsed;
}

export function parseHeaderOverrideJson(
  raw: string,
): HeaderOverrideValidationResult {
  const parsed = parseRequestOverrideJson(raw);
  if (parsed.error || !parsed.value) {
    return parsed.error ? { error: parsed.error } : {};
  }

  const headers: Record<string, string> = {};
  for (const [name, value] of Object.entries(parsed.value)) {
    const headerName = name.trim();
    if (!headerName) {
      return { error: "Header name must not be empty" };
    }
    if (!isValidHttpHeaderName(headerName)) {
      return { error: `Header "${name}" name is not a valid HTTP token` };
    }
    if (typeof value !== "string") {
      return { error: `Header "${name}" value must be a string` };
    }
    if (!isValidHttpHeaderValue(value)) {
      return { error: `Header "${name}" value contains control characters` };
    }
    const normalizedHeaderName = headerName.toLowerCase();
    if (Object.prototype.hasOwnProperty.call(headers, normalizedHeaderName)) {
      return {
        error: `Header "${name}" duplicates another header after case normalization`,
      };
    }
    if (isProtectedLocalProxyHeaderName(normalizedHeaderName)) {
      return {
        error: `Header "${name}" is managed by the local proxy and cannot be overridden`,
      };
    }
    headers[normalizedHeaderName] = value;
  }

  return { headers };
}

export function formatRequestOverrideObject(
  value: Record<string, unknown> | undefined,
): string {
  if (!value || Object.keys(value).length === 0) return "";
  return JSON.stringify(value, null, 2);
}

export function formatResponsesRulesDraft(
  overrides: LocalProxyRequestOverrides | undefined,
): ResponsesRuleDraft[] {
  const rules = overrides?.responsesRules ?? overrides?.responsesRemovals;
  if (!rules) return [];

  // If an imported file contains both actions, show the winning removal.
  const rows = new Map<string, ResponsesRuleDraft>();
  const put = (row: ResponsesRuleDraft) => {
    const name = normalizedResponsesRuleName(row);
    rows.set(`${row.target}:${name}`, { ...row, name });
  };
  for (const [name, value] of Object.entries(rules.setHeaders ?? {})) {
    put({ target: "header", name, action: "set", value });
  }
  for (const [name, value] of Object.entries(rules.setBodyFields ?? {})) {
    put({
      target: "body",
      name,
      action: "set",
      value: JSON.stringify(value) ?? "",
    });
  }
  for (const name of rules.removeHeaders ?? []) {
    put({ target: "header", name, action: "remove", value: "" });
  }
  for (const name of rules.removeBodyFields ?? []) {
    put({ target: "body", name, action: "remove", value: "" });
  }
  return [...rows.values()];
}

export function normalizedResponsesRuleName({
  target,
  name,
}: Pick<ResponsesRuleDraft, "target" | "name">): string {
  const trimmed = name.trim();
  return target === "header" ? trimmed.toLowerCase() : trimmed;
}

function looksLikeCredentialName(name: string): boolean {
  // Matches proxy/forwarder/responses_fields.rs. Raw credentials are never
  // observed, and recognized credential fields cannot be edited by these rules.
  return (
    /api[-_.]?key/.test(name.toLowerCase()) ||
    name
      .toLowerCase()
      .split(/[-_.]/)
      .some((part) =>
        [
          "auth",
          "authorization",
          "authentication",
          "token",
          "secret",
          "password",
          "cookie",
          "credential",
        ].includes(part),
      )
  );
}

export function parseResponsesRulesDraft(draft: ResponsesRuleDraft[]): {
  rules?: LocalProxyResponsesRules;
  error?: string;
} {
  const seen = new Set<string>();
  const removeHeaders: string[] = [];
  const removeBodyFields: string[] = [];
  const setHeaders = new Map<string, string>();
  const setBodyFields = new Map<string, unknown>();
  for (const row of draft) {
    if (row.action === "keep") continue;
    const name = normalizedResponsesRuleName(row);
    if (!name) return { error: "Field name must not be empty" };
    if (row.target === "header" && !isValidHttpHeaderName(row.name.trim())) {
      return { error: `Header "${name}" name is not a valid HTTP token` };
    }
    if (
      looksLikeCredentialName(name) ||
      (row.target === "header"
        ? isProtectedLocalProxyHeaderName(name) ||
          ["accept", "content-encoding"].includes(name)
        : name.startsWith("_") ||
          /\p{Cc}/u.test(name) ||
          ["model", "input", "stream", "session_id"].includes(name))
    ) {
      return { error: `Managed field "${name}" cannot be changed` };
    }
    const key = `${row.target}:${name}`;
    if (seen.has(key)) return { error: `Duplicate rule for "${name}"` };
    seen.add(key);

    if (row.action === "remove") {
      (row.target === "header" ? removeHeaders : removeBodyFields).push(name);
    } else if (row.target === "header") {
      if (!isValidHttpHeaderValue(row.value)) {
        return { error: `Header "${name}" value contains control characters` };
      }
      setHeaders.set(name, row.value);
    } else {
      try {
        // Preserve JSON types; literal null is a value, not a removal.
        setBodyFields.set(
          name,
          JSON.parse(row.value, (_key, value) => {
            if (typeof value === "number" && !Number.isFinite(value))
              throw new Error("invalid number");
            return value;
          }),
        );
      } catch {
        return {
          error: `Body field "${name}" replacement must be a valid JSON value`,
        };
      }
    }
  }

  const rules: LocalProxyResponsesRules = {};
  if (setHeaders.size) rules.setHeaders = Object.fromEntries(setHeaders);
  if (setBodyFields.size)
    rules.setBodyFields = Object.fromEntries(setBodyFields);
  if (removeHeaders.length) rules.removeHeaders = removeHeaders;
  if (removeBodyFields.length) rules.removeBodyFields = removeBodyFields;
  return seen.size ? { rules } : {};
}

export function buildLocalProxyRequestOverrides(
  headersJson: string,
  bodyJson: string,
  responsesRulesDraft?: ResponsesRuleDraft[],
): { overrides?: LocalProxyRequestOverrides; error?: string } {
  const headerResult = parseHeaderOverrideJson(headersJson);
  if (headerResult.error) {
    return { error: headerResult.error };
  }

  const bodyResult = parseBodyOverrideJson(bodyJson);
  if (bodyResult.error) {
    return { error: bodyResult.error };
  }

  const overrides: LocalProxyRequestOverrides = {};
  if (headerResult.headers && Object.keys(headerResult.headers).length > 0) {
    overrides.headers = headerResult.headers;
  }
  if (bodyResult.value && Object.keys(bodyResult.value).length > 0) {
    overrides.body = bodyResult.value;
  }
  if (responsesRulesDraft) {
    const { rules, error } = parseResponsesRulesDraft(responsesRulesDraft);
    if (error) return { error };
    if (rules) overrides.responsesRules = rules;
  }

  return Object.keys(overrides).length > 0 ? { overrides } : {};
}
