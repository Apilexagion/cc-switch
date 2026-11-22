import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClientProvider } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import { ProviderForm } from "@/components/providers/forms/ProviderForm";
import { createTestQueryClient } from "../utils/testQueryClient";

const notifications = vi.hoisted(() => ({ error: vi.fn() }));
vi.mock("@/lib/toast", () => ({ toast: { error: notifications.error, success: vi.fn() } }));
vi.mock("@/components/providers/forms/CodexConfigEditor", () => ({
  default: ({ authValue, onAuthChange }: { authValue: string; onAuthChange: (value: string) => void }) => (
    <textarea aria-label="Auth JSON draft" value={authValue} onChange={(event) => onAuthChange(event.target.value)} />
  ),
}));
vi.mock("@/components/providers/forms/GeminiConfigEditor", () => ({
  default: ({ configValue, onConfigChange }: { configValue: string; onConfigChange: (value: string) => void }) => (
    <textarea aria-label="Settings JSON draft" value={configValue} onChange={(event) => onConfigChange(event.target.value)} />
  ),
}));
vi.mock("@/components/providers/forms/hooks", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/components/providers/forms/hooks")>();
  const signedOut = { isAuthenticated: false, isStatusSuccess: true, isStatusError: false, accounts: [] };
  return { ...actual, useCopilotAuth: () => signedOut, useCodexOauth: () => signedOut, useXaiOauth: () => signedOut };
});

describe("provider JSON drafts", () => {
  it.each(["codex", "gemini", "agy"] as const)("refuses an invalid %s draft instead of submitting old settings", async (appId) => {
    const submit = vi.fn();
    notifications.error.mockClear();
    render(
      <QueryClientProvider client={createTestQueryClient()}>
        <ProviderForm appId={appId} providerId="draft" submitLabel="save-provider" onSubmit={submit} onCancel={vi.fn()}
          initialData={{ name: "Draft", category: "third_party", settingsConfig: appId === "codex"
            ? { auth: { OPENAI_API_KEY: "placeholder" }, config: 'model = "m"\nmodel_provider = "custom"\n[model_providers.custom]\nname = "custom"\nbase_url = "https://relay.example/v1"\nwire_api = "responses"\n' }
            : { env: { GEMINI_API_KEY: "placeholder", GOOGLE_GEMINI_BASE_URL: "https://relay.example", GEMINI_MODEL: "m" }, config: {} } }} />
      </QueryClientProvider>,
    );
    const editor = await screen.findByRole("textbox", { name: appId === "codex" ? "Auth JSON draft" : "Settings JSON draft" });
    fireEvent.change(editor, { target: { value: "{" } });
    fireEvent.click(screen.getByRole("button", { name: "save-provider" }));
    await waitFor(() => expect(notifications.error).toHaveBeenCalled());
    expect(submit).not.toHaveBeenCalled();
    expect(editor).toHaveValue("{");
  });
});
