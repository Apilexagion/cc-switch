import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ConfigInspection } from "@/components/providers/mode/ConfigInspection";
import { ToolPicker } from "@/components/mcp/ToolPicker";
const api = vi.hoisted(() => ({
  inspect: vi.fn(),
  applyInspected: vi.fn(),
  listTools: vi.fn(),
  setToolBlocked: vi.fn(),
}));
vi.mock("@/lib/api/proxy", () => ({ proxyApi: api }));
vi.mock("@/lib/api/mcp", () => ({ mcpApi: api }));
describe("explicit Magpie-derived actions", () => {
  beforeEach(() => vi.resetAllMocks());
  it("inspects without writing and binds reapply to the inspected version", async () => {
    api.inspect.mockResolvedValue({
      token: "snapshot",
      rows: [
        {
          file: "settings.json",
          field: "env.ANTHROPIC_BASE_URL",
          current: "change",
          exit: "remove",
        },
      ],
    });
    api.applyInspected.mockResolvedValue(undefined);
    render(<ConfigInspection app="claude" repair />);
    await screen.findByText(/env.ANTHROPIC_BASE_URL/);
    expect(api.applyInspected).not.toHaveBeenCalled();
    fireEvent.click(
      screen.getByRole("button", { name: "magpiePorts.reapply" }),
    );
    await waitFor(() =>
      expect(api.applyInspected).toHaveBeenCalledWith(
        "claude",
        "snapshot",
        false,
      ),
    );
  });
  it("does not launch discovery on open and cannot clear a native deny", async () => {
    api.listTools.mockResolvedValue({
      names: ["read_file", "write_file"],
      disabled: [],
      native: ["write_file"],
    });
    api.setToolBlocked.mockResolvedValue(undefined);
    render(<ToolPicker id="files" />);
    fireEvent.click(screen.getByText("magpiePorts.toolPolicy"));
    expect(api.listTools).not.toHaveBeenCalled();
    fireEvent.click(
      screen.getByRole("button", { name: "magpiePorts.listTools" }),
    );
    const native = await screen.findByRole("checkbox", { name: "write_file" });
    expect(native).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox", { name: "read_file" }));
    await waitFor(() =>
      expect(api.setToolBlocked).toHaveBeenCalledWith(
        "files",
        "codex",
        "read_file",
        true,
      ),
    );
  });
});
