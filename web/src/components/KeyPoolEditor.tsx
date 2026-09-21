/**
 * 账号内 Key 池编辑器（§4.2.1）。
 *
 * 一个账号可以放多把 Key，它们之间的关系与多个账号完全一致：负载均衡、粘性、
 * 熔断、限额都按"每一把 Key"独立生效。这个组件的职责只有两件——
 *
 * 1. 让管理员一次把多把 Key 填进来，而不是为每把 Key 建一个账号；
 * 2. 如实显示每把 Key 的健康状态，让"哪几把已经死了"一眼可见。
 *
 * **后台从不回吐明文**（§23.2），所以已保存的 Key 在界面上只有标签、摘要前缀与
 * 状态；要换就整把重新粘贴。提交时"有 id、没明文"就是"这一把没动"。
 */
import { useState } from "react";
import type { AccountKey, AccountKeyInput, Limits } from "../lib/types";
import { Badge, Button, Field, InfoTip } from "./ui";
import { IconPlus, IconTrash } from "./Icons";

/** 编辑器里的一行：已保存的（有 id）或正在新增的（无 id）。 */
export interface KeyDraft {
  /** 已保存那把的行 ID；新增的行没有。 */
  id?: string;
  /** 明文。已保存的 Key 留空表示"保持不变"。 */
  api_key: string;
  label: string;
  enabled: boolean;
  /** 从已保存的 Key 带过来的健康状态，仅用于展示。 */
  health?: AccountKey;
  /** Key 级限额覆盖（逐项，留空表示继承账号）。 */
  rpm: string;
  tpm: string;
  max_concurrency: string;
}

export function emptyDraft(): KeyDraft {
  return {
    api_key: "",
    label: "",
    enabled: true,
    rpm: "",
    tpm: "",
    max_concurrency: "",
  };
}

/** 把后台返回的 Key 池转成编辑器草稿。明文永远是空的。 */
export function draftsFromKeys(keys: AccountKey[]): KeyDraft[] {
  return keys.map((key) => ({
    id: key.id,
    api_key: "",
    label: key.label,
    enabled: key.enabled,
    health: key,
    rpm: key.limits.rpm?.toString() ?? "",
    tpm: key.limits.tpm?.toString() ?? "",
    max_concurrency: key.limits.max_concurrency?.toString() ?? "",
  }));
}

/** 解析一个限额输入框：空串表示"继承账号"，非法值返回 null 由调用方报错。 */
function parseLimit(raw: string): number | null | undefined {
  const text = raw.trim();
  if (!text) return null;
  const value = Number(text);
  return Number.isInteger(value) && value > 0 ? value : undefined;
}

/** 校验草稿，返回第一条错误；全部合法时返回 null。 */
export function validateDrafts(drafts: KeyDraft[]): string | null {
  if (drafts.length === 0) return null;
  for (const [index, draft] of drafts.entries()) {
    const position = draft.label.trim() || `第 ${index + 1} 把`;
    if (!draft.id && !draft.api_key.trim()) {
      return `${position}还没有填写 API Key`;
    }
    if (draft.label.length > 64) {
      return `${position}的标签不能超过 64 个字符`;
    }
    for (const [name, raw] of [
      ["RPM", draft.rpm],
      ["TPM", draft.tpm],
      ["最大并发", draft.max_concurrency],
    ] as const) {
      if (parseLimit(raw) === undefined) {
        return `${position}的${name}必须是正整数，留空表示继承账号`;
      }
    }
  }
  return null;
}

/** 把草稿转成提交给后台的形状。 */
export function draftsToInputs(drafts: KeyDraft[]): AccountKeyInput[] {
  return drafts.map((draft) => {
    const limits: Limits = {
      rpm: parseLimit(draft.rpm) ?? null,
      tpm: parseLimit(draft.tpm) ?? null,
      max_concurrency: parseLimit(draft.max_concurrency) ?? null,
    };
    const hasLimits =
      limits.rpm !== null || limits.tpm !== null || limits.max_concurrency !== null;
    return {
      // 没有明文的已保存 Key：只提交 id，后台照旧沿用原密文。
      ...(draft.id ? { id: draft.id } : {}),
      ...(draft.api_key.trim() ? { api_key: draft.api_key.trim() } : {}),
      label: draft.label.trim(),
      enabled: draft.enabled,
      ...(hasLimits ? { limits } : {}),
    };
  });
}

/** 一把 Key 的状态徽标文案。 */
function keyStatusLabel(status: string): string {
  switch (status) {
    case "active":
      return "正常";
    case "half_open":
      return "半开试探";
    case "quota_exhausted":
      return "额度耗尽";
    case "key_invalid":
      return "Key 失效";
    default:
      return status;
  }
}

function keyStatusTone(status: string): "success" | "warn" | "danger" | "neutral" {
  switch (status) {
    case "active":
      return "success";
    case "half_open":
      return "warn";
    case "quota_exhausted":
      return "danger";
    case "key_invalid":
      return "danger";
    default:
      return "neutral";
  }
}

export function KeyPoolEditor({
  drafts,
  editing,
  testing,
  testResults,
  onChange,
  onTest,
}: {
  drafts: KeyDraft[];
  /** 编辑已有账号时才能测试连接。 */
  editing: boolean;
  testing: boolean;
  /** 测试结果，按 Key 的明文以外的标识对齐；新增行用序号兜底。 */
  testResults: Record<string, { ok: boolean; message: string }>;
  onChange: (next: KeyDraft[]) => void;
  onTest: () => void;
}) {
  const [showKey, setShowKey] = useState<Record<number, boolean>>({});
  const [expanded, setExpanded] = useState<Record<number, boolean>>({});

  const update = (index: number, patch: Partial<KeyDraft>) => {
    onChange(drafts.map((draft, i) => (i === index ? { ...draft, ...patch } : draft)));
  };

  const error = validateDrafts(drafts);

  return (
    <Field
      label="API Key 池"
      error={error ?? undefined}
      hint="一个账号可以放多把 Key：它们之间的关系与多个账号完全一致，负载均衡、粘性、熔断与限额都按每一把独立生效。"
    >
      {() => (
        <>
          <div className="key-pool">
            {drafts.map((draft, index) => {
              const health = draft.health?.health;
              const result = testResults[draft.id ?? `new-${index}`];
              const position = draft.label.trim() || `Key ${index + 1}`;
              const open = expanded[index] ?? false;
              return (
                <div className="key-pool-row" key={draft.id ?? `new-${index}`}>
                  <div className="key-pool-head">
                    <span className="key-pool-index">#{index + 1}</span>
                    <input
                      className="input key-pool-label"
                      value={draft.label}
                      onChange={(e) => update(index, { label: e.target.value })}
                      placeholder="标签（可留空）"
                      aria-label={`${position}的标签`}
                    />
                    <div className="input-affix key-pool-secret">
                      <input
                        className="input mono"
                        type={showKey[index] ? "text" : "password"}
                        value={draft.api_key}
                        onChange={(e) => update(index, { api_key: e.target.value })}
                        placeholder={
                          draft.id ? "留空则不变（已保存）" : "sk-…"
                        }
                        autoComplete="new-password"
                        aria-label={`${position}的 API Key`}
                      />
                      <button
                        type="button"
                        className="input-affix-button"
                        aria-label={showKey[index] ? "隐藏" : "显示"}
                        aria-pressed={!!showKey[index]}
                        onClick={() =>
                          setShowKey((current) => ({
                            ...current,
                            [index]: !current[index],
                          }))
                        }
                      >
                        {showKey[index] ? "隐藏" : "显示"}
                      </button>
                    </div>
                    {health && (
                      <Badge tone={keyStatusTone(health.status)}>
                        {keyStatusLabel(health.status)}
                        {health.cooldown_secs != null && ` ${health.cooldown_secs}s`}
                      </Badge>
                    )}
                    {result && (
                      <Badge tone={result.ok ? "success" : "danger"}>
                        {result.ok ? "测试通过" : "测试失败"}
                      </Badge>
                    )}
                    <button
                      type="button"
                      className="btn btn-ghost btn-sm"
                      aria-expanded={open}
                      onClick={() =>
                        setExpanded((current) => ({ ...current, [index]: !open }))
                      }
                    >
                      {open ? "收起限额" : "限额"}
                    </button>
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={`删除${position}`}
                      onClick={() => onChange(drafts.filter((_, i) => i !== index))}
                    >
                      <IconTrash />
                    </Button>
                  </div>

                  {draft.id && health && (
                    <div className="key-pool-meta">
                      <span className="mono">{draft.health?.digest_prefix}</span>
                      <span className="muted">已保存的凭据无法回显；粘贴新值即覆盖</span>
                      {health.inflight > 0 && <span>在途 {health.inflight}</span>}
                    </div>
                  )}

                  {open && (
                    <div className="key-pool-limits">
                      <label>
                        <span>RPM</span>
                        <input
                          className="input"
                          value={draft.rpm}
                          onChange={(e) => update(index, { rpm: e.target.value })}
                          placeholder="继承账号"
                          inputMode="numeric"
                        />
                      </label>
                      <label>
                        <span>TPM</span>
                        <input
                          className="input"
                          value={draft.tpm}
                          onChange={(e) => update(index, { tpm: e.target.value })}
                          placeholder="继承账号"
                          inputMode="numeric"
                        />
                      </label>
                      <label>
                        <span>最大并发</span>
                        <input
                          className="input"
                          value={draft.max_concurrency}
                          onChange={(e) =>
                            update(index, { max_concurrency: e.target.value })
                          }
                          placeholder="继承账号"
                          inputMode="numeric"
                        />
                      </label>
                      <label className="key-pool-enabled">
                        <input
                          type="checkbox"
                          checked={draft.enabled}
                          onChange={(e) => update(index, { enabled: e.target.checked })}
                        />
                        <span>启用</span>
                      </label>
                    </div>
                  )}
                </div>
              );
            })}

            {drafts.length === 0 && (
              <div className="key-pool-empty muted">
                还没有任何 Key。这个账号在填好之前无法承接任何请求。
              </div>
            )}
          </div>

          <div className="row key-pool-actions">
            <Button
              size="sm"
              variant="secondary"
              icon={<IconPlus />}
              onClick={() => onChange([...drafts, emptyDraft()])}
            >
              添加一把 Key
            </Button>
            {editing && (
              <Button
                size="sm"
                variant="secondary"
                disabled={testing}
                title="逐把 Key 发送一次真实测试请求，确认哪几把已经失效"
                onClick={onTest}
              >
                {testing && <span className="spinner spinner-sm" aria-hidden="true" />}
                {testing ? "测试中…" : "测试全部 Key"}
              </Button>
            )}
            <InfoTip label="Key 池怎么工作">
              <p>
                额度按凭据计：Key 出问题只影响它自己，其他 Key 继续服务。
              </p>
              <p>
                同一个会话始终落在同一把 Key 上，不会打断上游的前缀缓存。
              </p>
            </InfoTip>
          </div>
        </>
      )}
    </Field>
  );
}
