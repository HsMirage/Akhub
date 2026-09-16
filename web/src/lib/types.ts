/** 后台 API 的类型契约。字段与 Rust 侧的 DTO 一一对应。 */

export type Protocol = "openai_chat" | "openai_responses" | "anthropic_messages";

export type UpstreamType =
  | "openai"
  | "anthropic"
  | "new_api"
  | "sub2api"
  | "openai_compatible";

export type ModelOrigin = "auto" | "manual";

/** 倍率来源：手动，或由后台探针周期刷新的两种自动来源（§11.2）。 */
export type MultiplierMode = "manual" | "sub2api" | "new_api";

/** 倍率状态：已知、宽限期内过期、宽限期结束硬停（§12.2）。 */
export type MultiplierStatus = "known" | "multiplier_stale" | "multiplier_unknown";

/** 目标的动态运行状态（§12.2）。 */
export type TargetStatus =
  | "active"
  | "cooldown"
  | "half_open"
  | "quota_exhausted"
  | "key_invalid";

/** RPM / TPM / 最大并发；null 表示不限（§17.1）。 */
export interface Limits {
  rpm: number | null;
  tpm: number | null;
  max_concurrency: number | null;
}

export interface SchedulingWeights {
  multiplier: number;
  reliability: number;
  first_token: number;
  throughput: number;
}

export interface Group {
  id: string;
  name: string;
  /** 只有前缀；完整 Key 仅在创建与重新生成时返回一次。 */
  key_prefix: string;
  multiplier_limit: string;
  weights: SchedulingWeights;
  queue_capacity: number;
  allow_degrade: boolean;
  logical_models: number;
  dispatch_targets: number;
}

export interface Account {
  id: string;
  group_id: string;
  name: string;
  upstream_type: UpstreamType;
  base_url: string;
  preferred_protocol: Protocol;
  adaptive_protocol: boolean;
  default_priority: number;
  calibration: string;
  multiplier_mode: MultiplierMode;
  manual_multiplier: string;
  /** 此刻真正生效的有效倍率，已含动态刷新结果与校准系数。 */
  effective_multiplier: string;
  multiplier_status: MultiplierStatus;
  /** 倍率已过期多少秒；仅宽限期内有值。 */
  multiplier_stale_for: number | null;
  multiplier_error: string | null;
  new_api_user_id: string | null;
  new_api_group: string | null;
  /** 是否已保存 New API 访问令牌。令牌本身绝不回吐。 */
  has_new_api_token: boolean;
  limits: Limits;
  allow_private_network: boolean;
  enabled: boolean;
  /** 模型自动同步：全量托管上游模型，忽略选择集（§16.2）。 */
  auto_sync: boolean;
  /** 上一次托管同步完成的时间戳；null 表示从未同步。 */
  model_synced_at: number | null;
}

/** 账号模型目录里的一行（§16.2 的选择集状态）。 */
export interface AccountModel {
  upstream_model: string;
  public_name: string;
  selected: boolean;
  /** 上游列表已消失但仍在选择集内：停止新请求（§16.5）。 */
  missing: boolean;
  /** 仅"获取模型"响应里有意义：本次拉取新出现的模型。 */
  is_new: boolean;
}

/** 账号级模型别名：上游真名 → 对外名（§16.4）。 */
export interface Alias {
  upstream_model: string;
  public_name: string;
}

/** 取消勾选前的二次确认行（§16.3）：最近 24 小时的调用次数。 */
export interface SelectionWarning {
  public_name: string;
  calls: number;
}

export interface LogicalModel {
  id: string;
  group_id: string;
  name: string;
  origin: ModelOrigin;
  enabled: boolean;
  dispatch_targets: number;
  /** 是否会出现在 /v1/models 中。 */
  listed: boolean;
}

/** 综合评分与四个分维得分（§9.4）。 */
export interface Score {
  total: number;
  multiplier: number;
  reliability: number;
  first_token: number;
  throughput: number;
  samples: number;
  /** 样本不足 20 时性能三维用的是保守中性分。 */
  warm: boolean;
}

export interface DispatchTarget {
  id: string;
  logical_model_id: string;
  account_id: string;
  upstream_model: string;
  priority_override: number | null;
  /** 实际生效的优先级：目标覆盖值优先于账号默认值。 */
  priority: number;
  limits: Limits;
  /** 账号默认值与目标覆盖合并后的实际限制。 */
  effective_limits: Limits;
  enabled: boolean;
  status: TargetStatus;
  cooldown_secs: number | null;
  inflight: number;
  score: Score | null;
}

export interface RequestRecord {
  request_id: string;
  started_at: number;
  duration_ms: number;
  protocol: Protocol;
  streaming: boolean;
  group_id: string | null;
  logical_model: string | null;
  target_id: string | null;
  account_id: string | null;
  upstream_model: string | null;
  request_bytes: number;
  upstream_status: number | null;
  http_status: number;
  error_code: string | null;
  /** 实际使用的上游端点；跨协议时与下游入口不同。 */
  endpoint: string | null;
  /** 为完成请求丢弃的白名单能力，逗号分隔；无降级时为 null（§14.8）。 */
  degraded: string | null;
  effective_multiplier: string | null;
  cheapest_multiplier: string | null;
  dearest_multiplier: string | null;
  attempts: number;
  queued_ms: number;
  sticky_hit: boolean;
}

export interface MultiplierAlert {
  account_id: string;
  name: string;
  stale_for?: number | null;
}

export interface Overview {
  config_version: number;
  groups: number;
  logical_models: number;
  listable_models: number;
  dispatch_targets: number;
  target_status: Partial<Record<TargetStatus, number>>;
  multiplier_stale: MultiplierAlert[];
  multiplier_unknown: MultiplierAlert[];
  /** 超过半数账号同一轮刷新失败：探针侧系统性故障（§11.4）。 */
  probe_systemic_failure: boolean;
  sticky_bindings: number;
  /** 已证实不存在的上游端点条数（§14.2）。 */
  missing_endpoints: number;
  dropped_request_records: number;
  master_key_from_env: boolean;
}

export interface Settings {
  request_timeout_secs: number;
  max_request_bytes: number;
  retention_days: number;
  /** Responses 可重放状态的保留天数（§15.2）。 */
  response_state_days: number;
  shutdown_grace_secs: number;
  multiplier_refresh_secs: number;
  /** 模型自动同步间隔秒数（§16.2）。 */
  model_sync_secs: number;
  /** 内置能力目录版本（§6.7）。 */
  capability_catalog_revision: string;
  version: string;
  /** 修改后需要下一次重启才能生效的字段。 */
  restart_required: string[];
  /** 每个可编辑设置允许的最小/最大值。 */
  limits: Record<string, { min: number; max: number }>;
}

/** 设置接口允许通过 PATCH 修改的数字字段。 */
export type SettingsNumericField =
  | "request_timeout_secs"
  | "max_request_bytes"
  | "retention_days"
  | "response_state_days"
  | "shutdown_grace_secs"
  | "multiplier_refresh_secs"
  | "model_sync_secs";

export type SettingsPatch = Partial<Pick<Settings, SettingsNumericField>>;

/** New API 的一个可用分组（账号编辑页的分组下拉框）。 */
export interface NewApiGroupOption {
  name: string;
  ratio: string;
  description: string | null;
}

/** 同步执行倍率探测后的结果。 */
export interface MultiplierRefreshResult {
  refreshed: boolean;
  effective_multiplier: string;
  /** 某些上游不提供观察时间时后端会返回 null。 */
  observed_at: number | null;
  notice: string;
}

/** 成本页的一条账号流量行（§6.8）。 */
export interface CostAccountRow {
  account_id: string;
  name: string;
  requests: number;
  share: number;
  effective_multiplier: string | null;
}

/** 成本页的一个逻辑模型块（§6.8）：绝不跨模型加总。 */
export interface CostModel {
  group_id: string;
  logical_model: string;
  requests: number;
  accounts: CostAccountRow[];
  weighted_avg_multiplier: string | null;
  cheapest_multiplier: string | null;
  dearest_multiplier: string | null;
  /** 全用最便宜目标还能再省的比例；单目标或无样本时为 null。 */
  saving_vs_cheapest: number | null;
  single_target: boolean;
}

export interface CostView {
  period: string;
  since: number;
  total_requests: number;
  account_shares: { account_id: string; name: string; requests: number; share: number }[];
  models: CostModel[];
}

/** 校准助手的返回：反算出的系数与依据（§6.8）。 */
export interface CalibrationResult {
  calibration: string;
  reported: string;
  group_avg_multiplier: string;
  gateway_requests: number;
  model_requests: number;
  exclusive: boolean;
  notice: string;
}

export interface CalibrationRecord {
  id: string;
  logical_model: string;
  period_start: number;
  period_end: number;
  gateway_requests: number;
  reported: string;
  calibration: string;
  created_at: number;
}

/** 账号测试连接的结果（§6.4）。 */
export interface TestResult {
  ok: boolean;
  status: number;
  latency_ms: number;
  model: string;
  message: string;
}

export interface SetupStatus {
  needs_setup: boolean;
  master_key_from_env: boolean;
}

/** 创建分组与重新生成 Key 的响应，`key` 是唯一一次出现的明文。 */
export interface KeyReveal {
  group: Group;
  key: string;
  notice: string;
}

export const PROTOCOL_LABELS: Record<Protocol, string> = {
  openai_chat: "OpenAI Chat",
  openai_responses: "OpenAI Responses",
  anthropic_messages: "Anthropic Messages",
};

export const MULTIPLIER_MODE_LABELS: Record<MultiplierMode, string> = {
  manual: "手动",
  sub2api: "自动 · Sub2API",
  new_api: "自动 · New API",
};

export const TARGET_STATUS_LABELS: Record<TargetStatus, string> = {
  active: "正常",
  cooldown: "冷却中",
  half_open: "半开试运行",
  quota_exhausted: "额度耗尽",
  key_invalid: "Key 失效",
};

/** 上游端点的展示名。请求记录里用它说明这次实际打到了哪条路由。 */
export const ENDPOINT_LABELS: Record<string, string> = {
  chat_completions: "Chat",
  responses: "Responses",
  messages: "Messages",
  count_tokens: "CountTokens",
};

export const UPSTREAM_LABELS: Record<UpstreamType, string> = {
  openai_compatible: "OpenAI 兼容",
  openai: "OpenAI",
  anthropic: "Anthropic",
  new_api: "New API",
  sub2api: "Sub2API",
};

/** 各上游类型的默认端点建议，用于新建账号时预填。 */
export const UPSTREAM_DEFAULTS: Record<
  UpstreamType,
  { base_url: string; protocol: Protocol }
> = {
  openai_compatible: { base_url: "", protocol: "openai_chat" },
  openai: { base_url: "https://api.openai.com", protocol: "openai_chat" },
  anthropic: {
    base_url: "https://api.anthropic.com",
    protocol: "anthropic_messages",
  },
  new_api: { base_url: "", protocol: "openai_chat" },
  sub2api: { base_url: "", protocol: "openai_chat" },
};
