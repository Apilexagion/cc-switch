import { useQuery } from "@tanstack/react-query";
import { promptsApi, type AppId } from "@/lib/api";
import { appHas } from "@/config/appSpecs";

/**
 * 提示词查询的 key。提示词页自己的列表走 usePromptActions；写完后 invalidate
 * `promptKeys.all`，其余读提示词的查询跟着刷新。
 */
export const promptKeys = {
  all: ["prompts"] as const,
  list: (app: AppId) => ["prompts", "list", app] as const,
  location: (app: AppId) => ["prompts", "location", app] as const,
};

/** 应用页对应的提示词应用；不支持提示词的返回 null。 */
export function promptAppOf(app: AppId): AppId | null {
  if (app === "claude-desktop") return "claude";
  return appHas(app, "prompts") ? app : null;
}

export function usePromptFileLocationQuery(app: AppId) {
  return useQuery({
    queryKey: promptKeys.location(app),
    queryFn: () => promptsApi.getFileLocation(app),
    staleTime: 30_000,
  });
}
