import { invoke } from "@tauri-apps/api/core";

export interface DiagnosticsConfig {
  captureBodies: boolean;
}

export interface AttemptDiagnostics {
  providerId: string;
  providerName: string;
  model: string;
  startedMs: number;
  preparedMs: number | null;
  headersMs: number | null;
  firstContentMs: number | null;
  durationMs: number | null;
  statusCode: number | null;
  error: string | null;
  canceled: boolean;
}

export interface RequestDiagnostics {
  traceId: string;
  captureBodies: boolean;
  requestBody: string;
  responseBody: string;
  requestTruncated: boolean;
  responseTruncated: boolean;
  attempts: AttemptDiagnostics[];
  attemptsTruncated: boolean;
}

export interface ResponsesFieldSummary {
  name: string;
  kind:
    | "header"
    | "null"
    | "boolean"
    | "number"
    | "string"
    | "array"
    | "object";
  editable: boolean;
  preview?: number | boolean | string;
}

export interface ResponsesFieldsSnapshot {
  observedAtMs: number;
  headers: ResponsesFieldSummary[];
  bodyFields: ResponsesFieldSummary[];
  truncated: boolean;
}

export const requestDiagnosticsApi = {
  get: (requestId: string): Promise<RequestDiagnostics | null> =>
    invoke("get_request_diagnostics", { requestId }),
  getConfig: (): Promise<DiagnosticsConfig> =>
    invoke("get_request_diagnostics_config"),
  setConfig: (config: DiagnosticsConfig): Promise<void> =>
    invoke("set_request_diagnostics_config", { config }),
  observeCodexResponsesFields: (
    providerId: string,
  ): Promise<ResponsesFieldsSnapshot | null> =>
    invoke("observe_codex_responses_fields", { providerId }),
};
