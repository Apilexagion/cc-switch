import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import WorkspaceFileEditor from "@/components/workspace/WorkspaceFileEditor";
import { workspaceApi } from "@/lib/api/workspace";
import { hasUnsavedChanges } from "@/lib/unsavedChanges";

vi.mock("@/lib/api/workspace", () => ({
  workspaceApi: { readFile: vi.fn(), writeFile: vi.fn() },
}));
vi.mock("@/components/MarkdownEditor", () => ({
  default: ({ value, onChange }: { value: string; onChange: (value: string) => void }) => (
    <textarea aria-label="Content" value={value} onChange={(event) => onChange(event.target.value)} />
  ),
}));

function deferred() {
  let resolve!: (value: string) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<string>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

describe("WorkspaceFileEditor opening", () => {
  it("waits for content and ignores an older file read", async () => {
    const first = deferred();
    const second = deferred();
    vi.mocked(workspaceApi.readFile)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    const onClose = vi.fn();
    const view = render(
      <WorkspaceFileEditor isOpen filename="A.md" onClose={onClose} />,
    );
    expect(screen.queryByRole("heading")).toBeNull();
    expect(screen.queryByText("prompts.loading")).toBeNull();
    view.rerender(
      <WorkspaceFileEditor isOpen filename="B.md" onClose={onClose} />,
    );
    await act(async () => second.resolve("B content"));
    expect(screen.getByRole("textbox")).toHaveValue("B content");
    expect(screen.getByRole("textbox").closest(".bg-app")).toHaveStyle({
      opacity: "1",
    });
    await act(async () => first.resolve("A content"));
    expect(screen.getByRole("textbox")).toHaveValue("B content");
  });

  it("does not reopen after closing during a read", async () => {
    const read = deferred();
    vi.mocked(workspaceApi.readFile).mockReturnValueOnce(read.promise);
    const onClose = vi.fn();
    const view = render(
      <WorkspaceFileEditor isOpen filename="A.md" onClose={onClose} />,
    );
    view.rerender(
      <WorkspaceFileEditor isOpen={false} filename="A.md" onClose={onClose} />,
    );
    await act(async () => read.resolve("late content"));
    expect(screen.queryByRole("textbox")).toBeNull();
  });

  it("returns to the list on failure instead of opening an empty editor", async () => {
    const read = deferred();
    vi.mocked(workspaceApi.readFile).mockReturnValueOnce(read.promise);
    const onClose = vi.fn();
    render(<WorkspaceFileEditor isOpen filename="A.md" onClose={onClose} />);
    await act(async () => read.reject(new Error("read failed")));
    expect(onClose).toHaveBeenCalledOnce();
    expect(screen.queryByRole("textbox")).toBeNull();
  });
});


describe("WorkspaceFileEditor draft protection", () => {
  it("registers edits, preserves a cancelled leave, and clears dirty after save", async () => {
    vi.mocked(workspaceApi.readFile).mockResolvedValueOnce("saved");
    vi.mocked(workspaceApi.writeFile).mockResolvedValue(undefined);
    const close = vi.fn();
    render(<WorkspaceFileEditor isOpen filename="AGENTS.md" onClose={close} />);
    const editor = await screen.findByRole("textbox");
    fireEvent.change(editor, { target: { value: "draft" } });
    await waitFor(() => expect(hasUnsavedChanges()).toBe(true));
    fireEvent.keyDown(window, { key: "Escape" });
    expect(close).not.toHaveBeenCalled();
    fireEvent.click(await screen.findByRole("button", { name: "common.unsavedLeaveCancel" }));
    expect(editor).toHaveValue("draft");
    fireEvent.click(screen.getByRole("button", { name: "common.save" }));
    await waitFor(() => expect(workspaceApi.writeFile).toHaveBeenCalledWith("AGENTS.md", "draft"));
    await waitFor(() => expect(hasUnsavedChanges()).toBe(false));
    fireEvent.keyDown(window, { key: "Escape" });
    expect(close).toHaveBeenCalledOnce();
  });
});
