import { useState } from "react";
import { useTranslation } from "react-i18next";
import { mcpApi, type ToolNames } from "@/lib/api/mcp";
import { Button } from "@/components/ui/button";
export function ToolPicker({ id }: { id: string }) {
  const { t } = useTranslation();
  const [app, setApp] = useState("codex");
  const [list, setList] = useState<ToolNames | null>(null);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const load = async () => {
    setBusy(true);
    setErr("");
    try {
      setList(await mcpApi.listTools(id, app));
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };
  const change = async (name: string, blocked: boolean) => {
    if (!list || busy) return;
    setBusy(true);
    setErr("");
    try {
      await mcpApi.setToolBlocked(id, app, name, blocked);
      setList({
        ...list,
        disabled: blocked
          ? [...list.disabled, name]
          : list.disabled.filter((t) => t !== name),
      });
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <details className="rounded-panel border border-border p-2 text-caption">
      <summary>{t("magpiePorts.toolPolicy")}</summary>
      <p>{t("magpiePorts.toolNote")}</p>
      <fieldset disabled={busy}>
        <select
          aria-label={t("magpiePorts.client")}
          value={app}
          onChange={(e) => {
            setApp(e.target.value);
            setList(null);
            setErr("");
          }}
        >
          <option value="codex">Codex</option>
          <option value="claude">Claude Code</option>
          <option value="gemini">Gemini CLI</option>
        </select>{" "}
        <Button
          type="button"
          size="compact"
          variant="neutral"
          onClick={() => void load()}
        >
          {t("magpiePorts.listTools")}
        </Button>
        {list && (
          <ul className="max-h-48 overflow-auto">
            {Array.from(
              new Set([...list.names, ...list.disabled, ...list.native]),
            )
              .sort()
              .map((n) => (
                <li key={n}>
                  <label>
                    <input
                      type="checkbox"
                      checked={
                        !list.all &&
                        !list.disabled.includes(n) &&
                        !list.native.includes(n)
                      }
                      disabled={
                        list.all ||
                        list.native.includes(n) ||
                        (app === "claude" &&
                          (!/^[A-Za-z0-9_-]+$/.test(n) || n.includes("__")))
                      }
                      onChange={(e) => void change(n, !e.target.checked)}
                    />{" "}
                    {n}
                  </label>
                </li>
              ))}
          </ul>
        )}
      </fieldset>
      {err && <p role="alert">{err}</p>}
    </details>
  );
}
