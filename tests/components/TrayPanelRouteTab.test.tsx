import { fireEvent, render, screen, within } from "@testing-library/react";
import { QueryClientProvider } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import { RouteTab } from "@/tray-panel/RouteTab";
import { trayPanelApi, type TrayRoute } from "@/tray-panel/api";
import { createTestQueryClient } from "../utils/testQueryClient";

vi.mock("@/tray-panel/api", () => ({ trayPanelApi: { routes: vi.fn() } }));
vi.mock("@/lib/api/usage", () => ({
  usageApi: {
    getUsageSummary: vi
      .fn()
      .mockResolvedValue({ totalRequests: 12, realTotalTokens: 3000 }),
  },
}));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: "en" } }),
}));
vi.mock("@/components/shell/AppGlyph", () => ({
  APP_DISPLAY_NAME: { "claude-desktop": "Claude Desktop" },
  AppGlyph: () => null,
}));

function request(id: number, agentId: string | null): TrayRoute {
  return {
    id,
    app: "claude-desktop",
    provider: "fixture",
    model: "same-model",
    servedModel: "same-model",
    caller: {
      kind: agentId ? "collab_spawn" : null,
      agentId,
      parentId: agentId ? "parent-fixture" : null,
      name: null,
    },
    phase: id === 8 ? "answering" : "done",
    time: id * 1000,
    attempts: 1,
    status: 200,
    durationMs: id === 8 ? null : 2000,
    tokens: null,
    outputTokens: null,
    firstTokenMs: null,
  };
}

describe("tray route activity", () => {
  it("shows six requests and keeps independently tagged same-model calls in the filtered view", async () => {
    const data = Array.from({ length: 8 }, (_, i) =>
      request(8 - i, i % 2 === 1 ? `worker-${i}` : null),
    );
    vi.mocked(trayPanelApi.routes).mockResolvedValue(data);
    const { container } = render(
      <QueryClientProvider client={createTestQueryClient()}>
        <RouteTab visible refreshKey={1} onOpenUsage={() => {}} />
      </QueryClientProvider>,
    );
    await screen.findByText("trayRoutes.phase.answering");
    expect(container.querySelectorAll("details")).toHaveLength(6);
    fireEvent.click(screen.getByRole("button", { name: "trayRoutes.more" }));
    expect(container.querySelectorAll("details")).toHaveLength(8);
    fireEvent.click(
      screen.getByRole("checkbox", { name: "trayRoutes.onlyChildren" }),
    );
    const rows = container.querySelectorAll("details");
    expect(rows).toHaveLength(4);
    fireEvent.click(rows[0].querySelector("summary")!);
    expect(within(rows[0]).getByText("worker-1")).toBeVisible();
    expect(within(rows[0]).getByText("parent-fixture")).toBeVisible();
    expect(screen.queryByText("trayRoutes.phase.answering")).toBeNull();
  });
});
