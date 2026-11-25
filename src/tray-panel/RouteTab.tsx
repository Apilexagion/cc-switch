import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import {
  ArrowRight,
  ChevronDown,
  ExternalLink,
  GitBranch,
  Network,
} from "lucide-react";
import { AppGlyph, APP_DISPLAY_NAME } from "@/components/shell/AppGlyph";
import {
  formatTokensCompact,
  getLocaleFromLanguage,
} from "@/components/usage/format";
import { usageApi } from "@/lib/api/usage";
import type { AppId } from "@/lib/api/types";
import { resolveUsageRange } from "@/lib/usageRange";
import { trayPanelApi, type TrayRoute } from "./api";

const PAGE_SIZE = 6;
function isApp(app: string): app is AppId {
  return Object.prototype.hasOwnProperty.call(APP_DISPLAY_NAME, app);
}
function appName(app: string) {
  return isApp(app) ? APP_DISPLAY_NAME[app] : app;
}
function isSubagent(row: TrayRoute) {
  return ["collab_spawn", "thread_spawn", "agent_job"].includes(
    row.caller.kind ?? "",
  );
}
function isLive(row: TrayRoute) {
  return row.phase === "waiting" || row.phase === "answering";
}

function RequestRow({ row, locale }: { row: TrayRoute; locale: string }) {
  const { t } = useTranslation();
  const child = isSubagent(row);
  const kind = row.caller.kind;
  const purposes = [
    "guardian",
    "guardian_review",
    "review",
    "compact",
    "thread_title",
    "memory_consolidation",
    "memgen",
  ];
  const badge = child
    ? t("trayRoutes.child")
    : kind && purposes.includes(kind)
      ? t(`trayRoutes.purpose.${kind}`)
      : kind;
  const when = new Date(row.time).toLocaleTimeString(locale, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
  const elapsed = row.durationMs == null ? null : row.durationMs / 1000;
  const generationMs =
    row.durationMs != null && row.firstTokenMs != null
      ? row.durationMs - row.firstTokenMs
      : 0;
  const rate =
    row.phase === "done" && row.outputTokens && generationMs >= 1000
      ? (row.outputTokens * 1000) / generationMs
      : null;
  const stats = [
    row.tokens == null
      ? null
      : formatTokensCompact(row.tokens, locale) + " token",
    elapsed == null
      ? null
      : t("trayRoutes.elapsed", {
          seconds: elapsed.toLocaleString(locale, { maximumFractionDigits: 1 }),
        }),
    rate === null
      ? null
      : t("trayRoutes.speed", {
          speed: rate.toLocaleString(locale, { maximumFractionDigits: 1 }),
        }),
  ]
    .filter(Boolean)
    .join(" · ");
  const destination =
    row.phase === "done"
      ? row.servedModel || row.model
      : t(`trayRoutes.phase.${row.phase}`);
  return (
    <details className="group border-b border-black/[0.07] last:border-0 dark:border-white/10">
      <summary className="cursor-pointer list-none rounded-md px-2 py-1.5 text-xs focus-visible:outline-blue-500">
        <span className="flex min-w-0 items-center gap-1">
          {isApp(row.app) && <AppGlyph app={row.app} size={13} />}
          <span className="shrink-0 font-medium">{appName(row.app)}</span>
          <code
            title={row.model}
            className="min-w-0 truncate text-[10px] text-muted-foreground"
          >
            {row.model}
          </code>
          {badge && (
            <span
              className="max-w-24 shrink-0 truncate rounded border border-border px-1 text-[10px]"
              title={[badge, row.caller.name, row.caller.agentId]
                .filter(Boolean)
                .join(" · ")}
            >
              {badge}
              {child && (row.caller.name || row.caller.agentId)
                ? " · " + (row.caller.name || row.caller.agentId?.slice(-6))
                : ""}
            </span>
          )}
          <time className="ms-auto shrink-0 text-[10px] tabular-nums text-muted-foreground">
            {when}
          </time>
        </span>
        <span className="mt-0.5 flex min-w-0 items-center gap-1 text-[11px]">
          <ArrowRight
            aria-hidden="true"
            className="size-3 shrink-0 text-muted-foreground"
          />
          <span title={row.provider} className="min-w-0 truncate">
            {row.provider}
          </span>
          <span
            className={
              row.phase === "failed"
                ? "min-w-0 truncate text-red-500"
                : "min-w-0 truncate text-muted-foreground"
            }
          >
            {destination}
          </span>
        </span>
        {stats && (
          <span className="block text-end text-[10px] tabular-nums text-muted-foreground">
            {stats}
          </span>
        )}
      </summary>
      <div className="space-y-1 px-3 pb-2 text-[11px] text-muted-foreground">
        <p className="break-all">
          {[appName(row.app), row.provider, row.servedModel || row.model].join(
            " → ",
          )}
        </p>
        <p>
          {t("trayRoutes.attempts", { count: row.attempts })}
          {row.status != null && ` · HTTP ${row.status}`}
        </p>
        {row.caller.name && <p className="break-all">{row.caller.name}</p>}
        {row.caller.agentId && (
          <p className="break-all">
            {t("trayRoutes.agentId")} <code>{row.caller.agentId}</code>
          </p>
        )}
        {row.caller.parentId && (
          <p className="break-all">
            {t("trayRoutes.parentId")} <code>{row.caller.parentId}</code>
          </p>
        )}
      </div>
    </details>
  );
}

export function RouteTab({
  visible,
  refreshKey,
  onOpenUsage,
}: {
  visible: boolean;
  refreshKey: number;
  onOpenUsage: () => void;
}) {
  const { t, i18n } = useTranslation();
  const locale = getLocaleFromLanguage(
    i18n.resolvedLanguage || i18n.language || "en",
  );
  const [onlyChildren, setOnlyChildren] = useState(false);
  const [limit, setLimit] = useState(PAGE_SIZE);
  const feed = useQuery({
    queryKey: ["tray-routes", refreshKey],
    queryFn: trayPanelApi.routes,
    enabled: visible,
    refetchInterval: visible ? 1000 : false,
    gcTime: 0,
    placeholderData: (prev) => prev,
  });
  const summary = useQuery({
    queryKey: ["tray-routing-today", refreshKey],
    queryFn: () => {
      const { startDate, endDate } = resolveUsageRange({ preset: "today" });
      return usageApi.getUsageSummary(startDate, endDate);
    },
    enabled: visible,
    refetchInterval: visible ? 15_000 : false,
    gcTime: 0,
    placeholderData: (prev) => prev,
  });
  const rows = onlyChildren
    ? (feed.data ?? []).filter(isSubagent)
    : (feed.data ?? []);
  const focus = rows.find(isLive) ?? rows[0];
  return (
    <section
      className="space-y-1.5 px-2 pb-2"
      aria-label={t("trayRoutes.title")}
    >
      <div className="flex min-h-6 items-center justify-between gap-2 px-1 text-[11px]">
        <span className="min-w-0 truncate text-muted-foreground">
          {summary.data
            ? t("trayRoutes.today", {
                calls: summary.data.totalRequests.toLocaleString(locale),
                tokens: formatTokensCompact(
                  summary.data.realTotalTokens,
                  locale,
                ),
              })
            : ""}
        </span>
        <button
          type="button"
          title={t("nav.usage")}
          aria-label={t("nav.usage")}
          onClick={onOpenUsage}
          className="shrink-0 rounded p-1 text-muted-foreground hover:text-foreground"
        >
          <ExternalLink className="size-3.5" />
        </button>
      </div>
      {focus && (
        <div className="flex h-12 min-w-0 items-center justify-center gap-1.5 rounded-md bg-black/[0.04] px-2 dark:bg-white/[0.06]">
          <span className="min-w-0 truncate text-xs">{appName(focus.app)}</span>
          <ArrowRight
            aria-hidden="true"
            className="size-3 shrink-0 text-muted-foreground"
          />
          <Network
            aria-label="CC Switch"
            className={
              isLive(focus)
                ? "size-4 shrink-0 text-emerald-500"
                : "size-4 shrink-0 text-muted-foreground"
            }
          />
          <ArrowRight
            aria-hidden="true"
            className="size-3 shrink-0 text-muted-foreground"
          />
          <span className="min-w-0 truncate text-xs" title={focus.provider}>
            {focus.provider}
          </span>
        </div>
      )}
      <label className="flex items-center gap-1.5 px-1 py-0.5 text-[11px] text-muted-foreground">
        <input
          type="checkbox"
          checked={onlyChildren}
          onChange={(e) => {
            setOnlyChildren(e.target.checked);
            setLimit(PAGE_SIZE);
          }}
        />
        <GitBranch aria-hidden="true" className="size-3" />
        {t("trayRoutes.onlyChildren")}
      </label>
      {feed.isError && (
        <button
          type="button"
          onClick={() => void feed.refetch()}
          className="px-2 py-2 text-xs"
        >
          {t("common.retry")}
        </button>
      )}
      {rows.length === 0 && !feed.isError && (
        <p
          role="status"
          className="py-4 text-center text-xs text-muted-foreground"
        >
          {feed.isLoading
            ? t("common.loading")
            : t(onlyChildren ? "trayRoutes.noChildren" : "trayRoutes.empty")}
        </p>
      )}
      <div>
        {rows.slice(0, limit).map((row) => (
          <RequestRow key={row.id} row={row} locale={locale} />
        ))}
      </div>
      {rows.length > limit && (
        <button
          type="button"
          className="flex w-full items-center justify-center gap-1 py-1 text-xs"
          onClick={() => setLimit((n) => n + PAGE_SIZE)}
        >
          <ChevronDown className="size-3" />
          {t("trayRoutes.more")}
        </button>
      )}
    </section>
  );
}
