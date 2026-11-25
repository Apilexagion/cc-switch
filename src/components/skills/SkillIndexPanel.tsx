import { useState } from "react";
import { useTranslation } from "react-i18next";
import { skillsApi, type IndexCatalog } from "@/lib/api/skills";
import { Button } from "@/components/ui/button";
export function SkillIndexPanel() {
  const { t } = useTranslation();
  const [app, setApp] = useState<"codex" | "claude">("codex");
  const [view, setView] = useState<IndexCatalog | null>(null);
  const [ids, setIds] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const run = async (save: boolean) => {
    if (busy) return;
    setBusy(true);
    setMessage("");
    try {
      const result = await skillsApi.index(app, save ? ids : undefined);
      setView(result);
      setIds(
        result.ids.filter((id) => result.choices.some((c) => c.id === id)),
      );
      if (save) setMessage(t("magpiePorts.indexSaved"));
    } catch (e) {
      setMessage(String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <details className="mx-6 shrink-0 rounded-panel border border-border px-3 py-1 text-caption">
      <summary>{t("magpiePorts.globalIndex")}</summary>
      <p>{t("magpiePorts.indexNote")}</p>
      <fieldset disabled={busy}>
        <select
          aria-label={t("magpiePorts.client")}
          value={app}
          onChange={(e) => {
            setApp(e.target.value as "claude" | "codex");
            setIds([]);
            setMessage("");
            setView(null);
          }}
        >
          <option value="codex">Codex</option>
          <option value="claude">Claude Code</option>
        </select>{" "}
        <Button
          type="button"
          size="compact"
          variant="neutral"
          onClick={() => void run(false)}
        >
          {t("common.refresh")}
        </Button>
        {view && (
          <>
            <ul className="max-h-40 overflow-auto">
              {view.choices.map((i) => (
                <li key={i.id}>
                  <label>
                    <input
                      type="checkbox"
                      checked={ids.includes(i.id)}
                      onChange={(e) =>
                        setIds((prev) =>
                          e.target.checked
                            ? [...prev, i.id]
                            : prev.filter((x) => x !== i.id),
                        )
                      }
                    />{" "}
                    {i.name} {i.direct && t("magpiePorts.alsoDirect")}
                  </label>
                </li>
              ))}
            </ul>
            <Button
              type="button"
              size="compact"
              variant="neutral"
              onClick={() => void run(true)}
            >
              {t("common.save")}
            </Button>
          </>
        )}
      </fieldset>
      {message && <p role="status">{message}</p>}
    </details>
  );
}
