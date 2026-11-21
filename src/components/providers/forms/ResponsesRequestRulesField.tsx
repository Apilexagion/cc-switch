import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import { fieldClass, Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  requestDiagnosticsApi,
  type ResponsesFieldSummary,
} from "@/lib/api/requestDiagnostics";
import {
  normalizedResponsesRuleName,
  parseResponsesRulesDraft,
  type ResponsesRuleDraft,
} from "@/lib/requestOverrides";
import { cn } from "@/lib/utils";

type Target = ResponsesRuleDraft["target"];

function ObservedFields({
  providerId,
  selected,
  onPick,
}: {
  providerId: string;
  selected: Set<string>;
  onPick: (target: Target, field: ResponsesFieldSummary) => void;
}) {
  const { t } = useTranslation();
  const [watching, setWatching] = useState(false);
  const { data, isError } = useQuery({
    queryKey: ["codex-responses-fields", providerId],
    queryFn: () =>
      requestDiagnosticsApi.observeCodexResponsesFields(providerId),
    enabled: watching,
    refetchInterval: watching ? 1000 : false,
    refetchIntervalInBackground: true, // Codex may be running in another window.
    gcTime: 0,
    retry: false,
  });

  function list(
    title: string,
    target: Target,
    fields: ResponsesFieldSummary[],
  ) {
    return (
      <div className="min-w-0 space-y-1">
        <p className="text-xs font-medium">{title}</p>
        <ul aria-label={title} className="max-h-48 overflow-y-auto">
          {fields.map((field) => (
            <li
              key={field.name}
              className="flex items-center justify-between gap-2 border-t border-border py-1"
            >
              <span className="min-w-0 break-all font-mono text-xs">
                {field.name}
                <span className="ml-1 text-fg-2">
                  {field.preview === undefined
                    ? field.kind
                    : JSON.stringify(field.preview)}
                </span>
              </span>
              {field.editable ? (
                <Button
                  type="button"
                  size="compact"
                  variant="quiet"
                  disabled={selected.has(`${target}:${field.name}`)}
                  aria-label={t("providerForm.responsesFieldSelectNamed", {
                    defaultValue: "选择字段 {{name}}",
                    name: field.name,
                  })}
                  onClick={() => onPick(target, field)}
                >
                  {t("providerForm.responsesFieldSelect", {
                    defaultValue: "选择",
                  })}
                </Button>
              ) : (
                <span className="shrink-0 text-xs text-fg-2">
                  {t("providerForm.responsesFieldReadOnly", {
                    defaultValue: "只读",
                  })}
                </span>
              )}
            </li>
          ))}
        </ul>
      </div>
    );
  }

  return (
    <div className="space-y-2 rounded-control border border-border p-3">
      <Button
        type="button"
        size="compact"
        variant="neutral"
        aria-pressed={watching}
        onClick={() => setWatching((active) => !active)}
      >
        {watching
          ? t("providerForm.responsesFieldsStop", { defaultValue: "停止观察" })
          : t("providerForm.responsesFieldsObserve", {
              defaultValue: "观察实际请求",
            })}
      </Button>
      {watching && (
        <>
          <p className="text-xs leading-relaxed text-fg-2">
            {t("providerForm.responsesFieldsPrivacy", {
              defaultValue:
                "只显示字段名称、类型和少量非敏感值。不显示凭据、会话标识或对话正文，观察结果不落盘。",
            })}
          </p>
          {isError ? (
            <p role="status" className="text-xs text-destructive">
              {t("providerForm.responsesFieldsUnavailable", {
                defaultValue: "暂时无法读取出站字段。",
              })}
            </p>
          ) : data ? (
            <>
              <p className="text-xs text-fg-2">
                {t("providerForm.responsesFieldsLastSeen", {
                  defaultValue: "最近转发尝试：{{time}}",
                  time: new Date(data.observedAtMs).toLocaleTimeString(),
                })}
              </p>
              <div className="grid gap-3 md:grid-cols-2">
                {list(
                  t("providerForm.responsesFieldsHeaders", {
                    defaultValue: "请求头",
                  }),
                  "header",
                  data.headers,
                )}
                {list(
                  t("providerForm.responsesFieldsBody", {
                    defaultValue: "请求体顶层字段",
                  }),
                  "body",
                  data.bodyFields,
                )}
              </div>
              {data.truncated && (
                <p className="text-xs text-fg-2">
                  {t("providerForm.responsesFieldsTruncated", {
                    defaultValue: "字段过多，仅显示部分字段。",
                  })}
                </p>
              )}
            </>
          ) : (
            <p role="status" className="text-xs text-fg-2">
              {t("providerForm.responsesFieldsWaiting", {
                defaultValue:
                  "等待实际请求。请让 Codex 通过本地代理向这家供应商发起一轮对话。",
              })}
            </p>
          )}
        </>
      )}
    </div>
  );
}

export function ResponsesRequestRulesField({
  providerId,
  value,
  onChange,
}: {
  providerId?: string;
  value: ResponsesRuleDraft[];
  onChange: (value: ResponsesRuleDraft[]) => void;
}) {
  const { t } = useTranslation();
  const { error } = parseResponsesRulesDraft(value);
  const selected = new Set(
    value.map((row) => `${row.target}:${normalizedResponsesRuleName(row)}`),
  );
  const add = (
    target: Target,
    name = "",
    preview?: ResponsesFieldSummary["preview"],
  ) => {
    if (name && selected.has(`${target}:${name}`)) return;
    onChange([
      ...value,
      {
        target,
        name,
        action: "keep",
        value:
          preview === undefined
            ? ""
            : target === "header"
              ? String(preview)
              : JSON.stringify(preview),
      },
    ]);
  };
  const update = (index: number, patch: Partial<ResponsesRuleDraft>) =>
    onChange(value.map((row, i) => (i === index ? { ...row, ...patch } : row)));

  return (
    <fieldset className="space-y-2">
      <legend className="text-sm font-medium">
        {t("providerForm.responsesRequestRules", {
          defaultValue: "Responses 出站字段规则",
        })}
      </legend>
      <p className="text-xs leading-relaxed text-fg-2">
        {t("providerForm.responsesRequestRulesHelp", {
          defaultValue:
            "只处理该供应商的原生 Responses 出站请求。规则在其他设置合并后、发送前生效，只替换或删除已存在的字段。",
        })}
      </p>
      {providerId && (
        <ObservedFields
          key={providerId}
          providerId={providerId}
          selected={selected}
          onPick={(target, field) => add(target, field.name, field.preview)}
        />
      )}
      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          size="compact"
          variant="neutral"
          onClick={() => add("header")}
        >
          {t("providerForm.responsesAddHeaderRule", {
            defaultValue: "添加请求头规则",
          })}
        </Button>
        <Button
          type="button"
          size="compact"
          variant="neutral"
          onClick={() => add("body")}
        >
          {t("providerForm.responsesAddBodyRule", {
            defaultValue: "添加请求体规则",
          })}
        </Button>
      </div>
      {value.map((row, index) => (
        <div
          key={index}
          className="space-y-2 rounded-control border border-border p-2"
        >
          <div className="flex flex-wrap items-center gap-2">
            <Input
              aria-label={
                row.target === "header"
                  ? t("providerForm.responsesHeaderName", {
                      defaultValue: "请求头名称",
                    })
                  : t("providerForm.responsesBodyName", {
                      defaultValue: "请求体字段名称",
                    })
              }
              placeholder={
                row.target === "header" ? "x-example-header" : "temperature"
              }
              value={row.name}
              onChange={(event) => update(index, { name: event.target.value })}
              className="min-w-32 flex-1 font-mono"
            />
            <select
              aria-label={t("providerForm.responsesRuleAction", {
                defaultValue: "处理方式",
              })}
              className={cn(fieldClass, "h-control w-auto")}
              value={row.action}
              onChange={(event) =>
                update(index, {
                  action: event.target.value as ResponsesRuleDraft["action"],
                })
              }
            >
              <option value="keep">
                {t("providerForm.responsesRuleKeep", {
                  defaultValue: "保留原样",
                })}
              </option>
              <option value="set">
                {t("providerForm.responsesRuleSet", { defaultValue: "替换值" })}
              </option>
              <option value="remove">
                {t("providerForm.responsesRuleRemove", {
                  defaultValue: "删除字段",
                })}
              </option>
            </select>
            <Button
              type="button"
              size="compact"
              variant="quiet"
              onClick={() => onChange(value.filter((_, i) => i !== index))}
            >
              {t("providerForm.responsesRuleClear", {
                defaultValue: "移除规则",
              })}
            </Button>
          </div>
          {row.action === "set" &&
            (row.target === "header" ? (
              <Input
                aria-label={t("providerForm.responsesHeaderValue", {
                  defaultValue: "请求头替换值",
                })}
                value={row.value}
                onChange={(event) =>
                  update(index, { value: event.target.value })
                }
                className="font-mono"
              />
            ) : (
              <Textarea
                aria-label={t("providerForm.responsesBodyValue", {
                  defaultValue: "请求体替换值（JSON）",
                })}
                placeholder={'0.2 / false / "text" / null'}
                value={row.value}
                onChange={(event) =>
                  update(index, { value: event.target.value })
                }
                className="min-h-14 resize-y font-mono text-xs"
              />
            ))}
        </div>
      ))}
      {error && (
        <p className="text-xs text-destructive" role="alert">
          {t("providerForm.responsesRequestRulesInvalid", {
            error,
            defaultValue: "字段规则无效：{{error}}",
          })}
        </p>
      )}
    </fieldset>
  );
}
