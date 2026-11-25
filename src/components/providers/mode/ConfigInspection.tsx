import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { proxyApi, type ConfigPreview } from "@/lib/api/proxy";
import { Button } from "@/components/ui/button";
import { extractErrorMessage } from "@/utils/errorUtils";

export const canInspect = (app: string) =>
  ["claude", "codex", "gemini", "grokbuild"].includes(app);

export function ConfigInspection({
  app,
  onToken,
  onUnavailable,
  repair = false,
}: {
  app: string;
  onToken?: (s: string | null) => void;
  onUnavailable?: (failed: boolean) => void;
  repair?: boolean;
}) {
  const { t } = useTranslation();
  const [preview, setPreview] = useState<ConfigPreview | null>(null);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let cancel = false;
    setPreview(null);
    onToken?.(null);
    onUnavailable?.(false);
    setError("");
    proxyApi.inspect(app).then(
      (p) => {
        if (!cancel) {
          setPreview(p);
          onToken?.(p.token);
        }
      },
      (e) => {
        if (!cancel) {
          setError(extractErrorMessage(e) || String(e));
          onUnavailable?.(true);
        }
      },
    );
    return () => {
      cancel = true;
    };
  }, [app, revision, onToken, onUnavailable]);
  const apply = async () => {
    if (!preview || busy) return;
    setBusy(true);
    try {
      await proxyApi.applyInspected(app, preview.token, false);
      setRevision((v) => v + 1);
    } catch (e) {
      setError(extractErrorMessage(e) || t("common.error"));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="space-y-2 rounded-panel border border-border p-3 text-caption">
      <strong>{t("magpiePorts.inspect")}</strong>
      <p>{t("magpiePorts.inspectNote")}</p>
      {error && (
        <p role="alert" className="text-danger-text">
          {error}
        </p>
      )}
      {preview ? (
        preview.rows.length ? (
          <ul className="max-h-48 overflow-auto">
            {preview.rows.map((r) => (
              <li key={r.file + ":" + r.field}>
                <code>
                  {r.file} · {r.field}
                </code>{" "}
                — {t("magpiePorts.current")}: {t("magpiePorts." + r.current)};{" "}
                {t("magpiePorts.exit")}: {t("magpiePorts." + r.exit)}
              </li>
            ))}
          </ul>
        ) : (
          <p>{t("magpiePorts.noDiff")}</p>
        )
      ) : (
        !error && <p>{t("common.loading")}</p>
      )}
      <Button
        size="compact"
        variant="neutral"
        disabled={busy}
        onClick={() => setRevision((v) => v + 1)}
      >
        {t("common.refresh")}
      </Button>
      {repair &&
        preview?.rows.some(
          (r) => r.current !== "keep" && r.current !== "conditional",
        ) && (
          <Button
            size="compact"
            variant="neutral"
            disabled={busy}
            onClick={() => void apply()}
          >
            {t("magpiePorts.reapply")}
          </Button>
        )}
    </div>
  );
}
