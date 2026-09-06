/** 展示层的格式化工具。只做显示，不参与任何判定逻辑。 */
import type { Limits } from "./types";

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(2)} s`;
  return `${Math.floor(ms / 60_000)}m ${Math.round((ms % 60_000) / 1000)}s`;
}

export function formatTime(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleString(undefined, {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

/** 相对时间。列表里比绝对时间更容易扫读。 */
export function formatRelative(unixSeconds: number): string {
  const delta = Date.now() / 1000 - unixSeconds;
  if (delta < 60) return "刚刚";
  if (delta < 3600) return `${Math.floor(delta / 60)} 分钟前`;
  if (delta < 86400) return `${Math.floor(delta / 3600)} 小时前`;
  return `${Math.floor(delta / 86400)} 天前`;
}

/** HTTP 状态码对应的语义色。 */
export function statusTone(status: number): "success" | "warn" | "danger" | "neutral" {
  if (status < 300) return "success";
  if (status < 400) return "neutral";
  if (status < 500) return "warn";
  return "danger";
}

/** 校验倍率的十进制格式，与 Rust 侧 `Multiplier::parse` 的规则保持一致。 */
export function validateMultiplier(raw: string): string | null {
  const text = raw.trim();
  if (!text) return "不能为空";
  if (!/^\d*\.?\d*$/.test(text) || text === ".") return "只能填写非负十进制数";
  const [, fraction = ""] = text.split(".");
  if (fraction.length > 6) return "最多保留 6 位小数";
  return null;
}

/** 把大数压成 200K / 1.5M 这种可扫读的形式。 */
export function formatCompact(value: number): string {
  if (value < 1000) return String(value);
  if (value < 1_000_000) {
    const k = value / 1000;
    return `${Number.isInteger(k) ? k : k.toFixed(1)}K`;
  }
  const m = value / 1_000_000;
  return `${Number.isInteger(m) ? m : m.toFixed(1)}M`;
}

/** RPM / TPM / 并发的一行摘要；全部留空时显示「不限」。 */
export function formatLimits(limits: Limits): string {
  const parts: string[] = [];
  if (limits.rpm !== null) parts.push(`RPM ${formatCompact(limits.rpm)}`);
  if (limits.tpm !== null) parts.push(`TPM ${formatCompact(limits.tpm)}`);
  if (limits.max_concurrency !== null) parts.push(`并发 ${limits.max_concurrency}`);
  return parts.length === 0 ? "不限" : parts.join(" · ");
}

/** 「倍率已过期 N 分钟」里的 N。 */
export function formatStaleFor(seconds: number): string {
  if (seconds < 60) return "不到 1 分钟";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟`;
  return `${Math.floor(seconds / 3600)} 小时 ${Math.floor((seconds % 3600) / 60)} 分钟`;
}

/** 0–1 的分数按两位小数显示。 */
export function formatScore(value: number): string {
  return value.toFixed(2);
}

/**
 * 解析限制输入框：空串是「不限」，其余必须是正整数。
 * 返回 `undefined` 表示非法。
 */
export function parseLimit(raw: string): number | null | undefined {
  const text = raw.trim();
  if (text === "") return null;
  const value = Number(text);
  if (!Number.isInteger(value) || value <= 0) return undefined;
  return value;
}
