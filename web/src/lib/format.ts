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

/** 审计动作 → 中文标签。未知动作原样返回，新增动作时不会丢信息。 */
const CHANGE_ACTION_LABELS: Record<string, string> = {
  update_settings: "修改系统设置",
  change_password: "修改管理员密码",
  create_group: "新建分组",
  update_group: "更新分组",
  delete_group: "删除分组",
  regenerate_group_key: "重置分组 Key",
  create_account: "新建上游账号",
  update_account: "更新上游账号",
  delete_account: "删除上游账号",
  copy_account: "复制上游账号",
  move_account: "迁移账号分组",
  test_account: "测试上游账号",
  refresh_multiplier: "刷新单个账号倍率",
  refresh_all_multipliers: "批量刷新倍率",
  save_new_api_site: "保存站点凭据",
  delete_new_api_site: "删除站点凭据",
  create_logical_model: "新建逻辑模型",
  update_logical_model: "更新逻辑模型",
  delete_logical_model: "删除逻辑模型",
  create_target: "添加调度目标",
  update_target: "更新调度目标",
  delete_target: "移除调度目标",
  refresh_account_models: "刷新模型目录",
  select_account_models: "更新模型选择集",
  sync_account_models: "执行托管同步",
  add_manual_model: "手动添加模型",
  update_aliases: "更新模型名",
  calibrate_account: "校准倍率",
  backup_export: "导出配置备份",
  backup_import: "恢复配置备份",
};

export function formatChangeAction(action: string): string {
  return CHANGE_ACTION_LABELS[action] ?? action;
}

export function formatChangeResult(result: string): string {
  if (result === "ok" || result === "success") return "成功";
  if (result === "failed" || result === "error") return "失败";
  return result;
}

/** 尝试结果枚举 → 中文标签。 */
export function formatAttemptOutcome(outcome: string): string {
  const labels: Record<string, string> = {
    ok: "成功",
    failed: "失败",
    missing_endpoint: "端点不存在",
  };
  return labels[outcome] ?? outcome;
}

/** 秒 → 人类可读时长。用于设置项旁的换算提示。 */
export function humanizeSeconds(seconds: number): string {
  if (!Number.isFinite(seconds)) return "—";
  if (seconds === 0) return "0";
  if (seconds % 86_400 === 0) return `${seconds / 86_400} 天`;
  if (seconds % 3_600 === 0) return `${seconds / 3_600} 小时`;
  if (seconds % 60 === 0) return `${seconds / 60} 分钟`;
  return `${seconds} 秒`;
}

/** "更新于 X 分钟前"。 */
export function formatUpdatedAgo(lastUpdated: number | null, now: number): string {
  if (lastUpdated === null) return "尚未成功加载";
  const delta = Math.max(0, Math.floor((now - lastUpdated) / 1000));
  if (delta < 20) return "刚刚更新";
  if (delta < 3600) return `${Math.floor(delta / 60)} 分钟前更新`;
  if (delta < 86400) return `${Math.floor(delta / 3600)} 小时前更新`;
  return `${Math.floor(delta / 86400)} 天前更新`;
}
