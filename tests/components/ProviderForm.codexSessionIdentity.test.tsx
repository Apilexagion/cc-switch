import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClientProvider } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import {
  ProviderForm,
  type ProviderFormProps,
  type ProviderFormValues,
} from "@/components/providers/forms/ProviderForm";
import type { ProviderMeta } from "@/types";
import { createTestQueryClient } from "../utils/testQueryClient";

vi.mock("@/components/JsonEditor", () => ({
  default: ({ value }: { value: string }) => (
    <textarea aria-label="配置预览" value={value} readOnly />
  ),
}));

const continuationLabel = "解除中转站轮次限制";
const diagnosticsLabel = "启用会话身份脱敏诊断";
const settingsConfig = {
  auth: { OPENAI_API_KEY: "mock-key" },
  config: [
    'model_provider = "custom"',
    'model = "mock-model"',
    "[model_providers.custom]",
    'name = "Mock"',
    'base_url = "https://mock.invalid/v1"',
    'wire_api = "responses"',
    "requires_openai_auth = true",
  ].join("\n"),
};

function formElement(
  meta: ProviderMeta | undefined,
  onSubmit: (values: ProviderFormValues) => void,
  modeView: ProviderFormProps["modeView"] = "direct",
  name = "模拟供应商甲",
) {
  return (
    <QueryClientProvider client={createTestQueryClient()}>
      <ProviderForm
        appId="codex"
        providerId={name}
        modeView={modeView}
        submitLabel="保存供应商"
        onSubmit={onSubmit}
        onCancel={vi.fn()}
        initialData={{ name, category: "third_party", settingsConfig, meta }}
      />
    </QueryClientProvider>
  );
}

function persistedMeta(meta: ProviderMeta | undefined): ProviderMeta {
  return JSON.parse(JSON.stringify(meta ?? {})) as ProviderMeta;
}

describe("Codex 供应商会话身份设置", () => {
  it("旧配置默认关闭，保存时不写入默认关闭字段", async () => {
    const onSubmit = vi.fn<(values: ProviderFormValues) => void>();
    render(formElement(undefined, onSubmit));
    expect(
      screen.getByRole("switch", { name: continuationLabel }),
    ).not.toBeChecked();
    expect(
      screen.getByRole("switch", { name: diagnosticsLabel }),
    ).not.toBeChecked();
    expect(
      screen.queryByRole("switch", {
        name: "到限后保留完整记录继续对话",
      }),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "保存供应商" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    const saved = persistedMeta(onSubmit.mock.calls[0][0].meta);
    expect(saved).not.toHaveProperty("codexSessionIdentityRotation");
    expect(saved).not.toHaveProperty("codexSessionHistoryArchive");
    expect(saved).not.toHaveProperty("codexSessionIdentityDiagnostics");
  });

  it.each(["direct", "route", "stack"] as const)(
    "%s 布局保存后重新编辑回显，可独立关闭诊断",
    async (modeView) => {
      const onSubmit = vi.fn<(values: ProviderFormValues) => void>();
      const view = render(formElement(undefined, onSubmit, modeView));
      fireEvent.click(screen.getByRole("switch", { name: continuationLabel }));
      fireEvent.click(screen.getByRole("switch", { name: diagnosticsLabel }));
      fireEvent.click(screen.getByRole("button", { name: "保存供应商" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
      const saved = onSubmit.mock.calls[0][0];
      expect(saved.meta).toMatchObject({
        codexSessionIdentityRotation: true,
        codexSessionHistoryArchive: true,
        codexSessionIdentityDiagnostics: true,
      });
      view.unmount();
      render(formElement(saved.meta, onSubmit, modeView));
      expect(
        screen.getByRole("switch", { name: continuationLabel }),
      ).toBeChecked();
      expect(
        screen.getByRole("switch", { name: diagnosticsLabel }),
      ).toBeChecked();
      fireEvent.click(screen.getByRole("switch", { name: diagnosticsLabel }));
      fireEvent.click(screen.getByRole("button", { name: "保存供应商" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(2));
      const persisted = persistedMeta(onSubmit.mock.calls[1][0].meta);
      expect(persisted).toMatchObject({
        codexSessionIdentityRotation: true,
        codexSessionHistoryArchive: true,
      });
      expect(persisted).not.toHaveProperty("codexSessionIdentityDiagnostics");
    },
  );

  it("切换编辑供应商时不沿用上一家的开关", async () => {
    const onSubmit = vi.fn<(values: ProviderFormValues) => void>();
    const view = render(
      formElement(
        {
          codexSessionIdentityRotation: true,
          codexSessionHistoryArchive: true,
        },
        onSubmit,
      ),
    );
    expect(
      screen.getByRole("switch", { name: continuationLabel }),
    ).toBeChecked();
    view.rerender(formElement(undefined, onSubmit, "direct", "模拟供应商乙"));
    await waitFor(() =>
      expect(
        screen.getByRole("switch", { name: continuationLabel }),
      ).not.toBeChecked(),
    );
    expect(
      screen.getByRole("switch", { name: diagnosticsLabel }),
    ).not.toBeChecked();
  });

  it("旧配置只有身份轮换时，保存会补齐完整历史归档", async () => {
    const onSubmit = vi.fn<(values: ProviderFormValues) => void>();
    render(
      formElement(
        {
          codexSessionIdentityRotation: true,
          codexSessionHistoryArchive: false,
        },
        onSubmit,
      ),
    );
    expect(
      screen.getByRole("switch", { name: continuationLabel }),
    ).toBeChecked();
    fireEvent.click(screen.getByRole("button", { name: "保存供应商" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit.mock.calls[0][0].meta).toMatchObject({
      codexSessionIdentityRotation: true,
      codexSessionHistoryArchive: true,
    });
  });

  it("旧配置只有历史归档时按关闭处理，保存时清除两个兼容字段", async () => {
    const onSubmit = vi.fn<(values: ProviderFormValues) => void>();
    render(
      formElement(
        {
          codexSessionIdentityRotation: false,
          codexSessionHistoryArchive: true,
        },
        onSubmit,
      ),
    );
    expect(
      screen.getByRole("switch", { name: continuationLabel }),
    ).not.toBeChecked();
    fireEvent.click(screen.getByRole("button", { name: "保存供应商" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    const saved = persistedMeta(onSubmit.mock.calls[0][0].meta);
    expect(saved).not.toHaveProperty("codexSessionIdentityRotation");
    expect(saved).not.toHaveProperty("codexSessionHistoryArchive");
  });

  it.each(["openai_chat", "anthropic"] as const)(
    "%s 转换路径保留已保存的开关并说明不适用",
    async (apiFormat) => {
      const onSubmit = vi.fn<(values: ProviderFormValues) => void>();
      render(
        formElement(
          { apiFormat, codexSessionIdentityRotation: true },
          onSubmit,
        ),
      );
      expect(
        screen.getByRole("switch", { name: continuationLabel }),
      ).toBeChecked();
      const scopeDescription = screen.getByText(
        /只适用于经本地代理转发的第三方原生 Responses/,
      );
      expect(scopeDescription).toBeVisible();
      expect(
        screen.getByText(/预读 SSE 开头直到出现输出/),
      ).toBeVisible();
      expect(scopeDescription).toHaveTextContent(
        /官方 ChatGPT \/ Codex OAuth、直连及 Chat \/ Anthropic 转换不生效/,
      );
      fireEvent.click(screen.getByRole("button", { name: "保存供应商" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
      expect(onSubmit.mock.calls[0][0].meta?.codexSessionIdentityRotation).toBe(
        true,
      );
      expect(onSubmit.mock.calls[0][0].meta?.codexSessionHistoryArchive).toBe(
        true,
      );
    },
  );
});
