import { useState } from "react";
import { useTranslation } from "react-i18next";
import {
  profilesApi,
  type CheckoutBinding,
  type CheckoutPreview,
  type Profile,
} from "@/lib/api/profiles";
import { settingsApi } from "@/lib/api/settings";
import { Button } from "@/components/ui/button";

export function CheckoutLibrary({ profiles }: { profiles: Profile[] }) {
  const { t } = useTranslation();
  const [b, setB] = useState<CheckoutBinding>({
    root: "",
    app: "claude",
    profile: null,
    allWorktrees: false,
    skillIndex: false,
  });
  const [preview, setPreview] = useState<CheckoutPreview | null>(null);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const update = (change: Partial<CheckoutBinding>) => {
    setB((old) => ({ ...old, ...change }));
    setPreview(null);
    setErr("");
  };
  const run = async (apply: boolean) => {
    if (busy || !b.root || (apply && !preview)) return;
    setBusy(true);
    setErr("");
    try {
      const p = await profilesApi.projectLibrary(
        b,
        apply ? preview!.token : undefined,
      );
      setPreview(apply ? null : p);
      if (apply) setErr(t("magpiePorts.checkoutDone"));
    } catch (e) {
      setPreview(null);
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <details className="mx-3 rounded-panel border border-border p-2 text-caption">
      <summary>{t("magpiePorts.checkout")}</summary>
      <fieldset disabled={busy} className="space-y-2 pt-2">
        <p>{t("magpiePorts.checkoutNote")}</p>
        <Button
          size="compact"
          variant="neutral"
          onClick={() =>
            void settingsApi
              .selectConfigDirectory()
              .then((root) => root && update({ root }))
              .catch((e) => setErr(String(e)))
          }
        >
          {b.root || t("magpiePorts.chooseRepo")}
        </Button>
        <div className="flex gap-2">
          <select
            aria-label={t("magpiePorts.client")}
            value={b.app}
            onChange={(e) =>
              update({ app: e.target.value as CheckoutBinding["app"] })
            }
          >
            <option value="claude">Claude Code</option>
            <option value="codex">Codex</option>
          </select>
          <select
            aria-label={t("profiles.manageTitle")}
            value={b.profile ?? ""}
            onChange={(e) => update({ profile: e.target.value || null })}
          >
            <option value="">{t("magpiePorts.unbind")}</option>
            {profiles.map((p) => (
              <option value={p.id} key={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </div>
        <label className="block">
          <input
            type="checkbox"
            checked={b.allWorktrees}
            onChange={(e) => update({ allWorktrees: e.target.checked })}
          />{" "}
          {t("magpiePorts.worktrees")}
        </label>
        <label className="block">
          <input
            type="checkbox"
            checked={b.skillIndex}
            onChange={(e) => update({ skillIndex: e.target.checked })}
          />{" "}
          {t("magpiePorts.skillIndex")}
        </label>
        <Button
          size="compact"
          variant="neutral"
          disabled={!b.root}
          onClick={() => void run(false)}
        >
          {t("magpiePorts.preview")}
        </Button>
        {preview && (
          <>
            <p>
              {t("magpiePorts.previous")}:{" "}
              {profiles.find((p) => p.id === preview.previousProfile)?.name ??
                "—"}
            </p>
            <ul className="max-h-32 overflow-auto">
              {preview.changes.map((r) => (
                <li key={r.checkout + "/" + r.file}>
                  {t("magpiePorts." + r.action)} · {r.checkout}/{r.file}
                </li>
              ))}
            </ul>
            <Button
              size="compact"
              variant="neutral"
              disabled={
                !preview.changes.length &&
                !b.profile &&
                !preview.previousProfile
              }
              onClick={() => void run(true)}
            >
              {t("magpiePorts.apply")}
            </Button>
          </>
        )}
        {err && <p role="status">{err}</p>}
      </fieldset>
    </details>
  );
}
