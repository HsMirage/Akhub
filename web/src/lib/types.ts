/** 后台 API 的类型契约。字段与 Rust 侧的 DTO 一一对应。 */

export type Protocol = "openai_chat" | "openai_responses" | "anthropic_messages";

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

/**
 * 列表接口的分页信封（§7.4）。
 *
 * 服务端默认返回 200 条、最多 1000 条；\`total\` 大于 \`data.length\` 就说明
 * 被截断了，界面要明说，不能让人以为配置里就只有这么多。
 */
export interface Page<T> {
  data: T[];
  total: number;
  limit: number;
  offset: number;
}

export interface Group {
  id: string;
  name: string;
  /** 只有前缀；完整 Key 仅在创建与重新生成时返回一次。 */
  key_prefix: string;
  multiplier_limit: string;
  weights: SchedulingWeights;
  queue_capacity: number;
  /** 层内目标全忙时的最长排队时间；0 表示跟随请求总超时。 */
  max_wait_secs: number;
  allow_degrade: boolean;
  allow_managed_background: boolean;
  logical_models: number;
  dispatch_targets: number;
  /** 该分组当前的告警（§6.3）。 */
  alerts: GroupAlert[];
}

/** 一条分组级告警（§6.3）。 */
export interface GroupAlert {
  level: "danger" | "warn";
  text: string;
}

export interface Account {
  id: string;
  /** 所属分组；`null` 表示未分配，账号不参与任何调度（§4.2.3）。 */
  group_id: string | null;
  name: string;
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
  /** 账号自己没填凭据、但该 Base URL 配了站点级凭据（§6.4）。 */
  uses_site_credentials: boolean;
  limits: Limits;
  allow_private_network: boolean;
  enabled: boolean;
  /** 模型自动同步：全量托管上游模型，忽略选择集（§16.2）。 */
  auto_sync: boolean;
  /** 账号级"隐藏原始模型"：打开后只暴露设置了"下游模型名"的模型。 */
  hide_original: boolean;
  /** 上一次托管同步完成的时间戳；null 表示从未同步。 */
  model_synced_at: number | null;
  /** 账号级健康摘要（§6.9）：列表行内徽标用它。 */
  health: AccountHealth;
  /** 账号内 Key 池的元数据（§4.2.1）。**不含明文**，只有标签、限额与摘要前缀。 */
  keys: AccountKey[];
}

/// 账号内的一把 Key。**绝不包含明文凭据**（§23.2）。
export interface AccountKey {
  id: string;
  label: string;
  enabled: boolean;
  limits: Limits;
  /// 凭据摘要前 8 位，用于在同一账号的多把 Key 之间对号。
  digest_prefix: string;
  health: AccountKeyHealth;
}

/// 一把 Key 的健康摘要（§4.2.1）。
export interface AccountKeyHealth {
  id: string;
  label: string;
  enabled: boolean;
  digest_prefix: string;
  limits: Limits;
  /// active / quota_exhausted / key_invalid / half_open
  status: string;
  /// 熔断冷却剩余秒数。
  cooldown_secs: number | null;
  /// 当前在途请求数。
  inflight: number;
}

/// 提交给后台的一把 Key（§4.2.1）。
///
/// \`api_key\` 留空表示"保持已保存的那把不变"——后台从不回吐明文，所以"没改"
/// 在传参上就是"有 id、没明文"。没有 \`id\` 的条目会被当作新增。
export interface AccountKeyInput {
  id?: string;
  api_key?: string;
  label?: string;
  limits?: Limits;
  enabled?: boolean;
}

/** 账号级健康摘要（§6.9）。 */
export interface AccountHealth {
  /** active / cooldown / half_open / quota_exhausted / key_invalid / no_key / disabled */
  status: string;
  reason: string | null;
  /** 该账号下的目标状态计数。 */
  targets: Record<string, number>;
  target_total: number;
  /** 逐把 Key 的状态（§4.2.1）。 */
  keys: AccountKeyHealth[];
  /** Key 总数与其中启用的数量。 */
  key_total: number;
  key_enabled: number;
}

/** 账号模型目录里的一行（§16.2 / §16.4）。 */
export interface AccountModel {
  upstream_model: string;
  /** 下游模型名；未设置时等于上游模型名。 */
  public_name: string;
  /** 所属账号的"隐藏原始模型名"开关当前值。 */
  hide_original: boolean;
  /** 该行对应的下游模型名（等于 public_name）。 */
  logical_model_name: string;
  /** 下游实际可用的全部名称；账号隐藏原始名且未设下游模型名时为空。 */
  exposed_names: string[];
  selected: boolean;
  /** 上游列表已消失但仍在选择集内：停止新请求（§16.5）。 */
  missing: boolean;
  /** 仅"获取模型"响应里有意义：本次拉取新出现的模型。 */
  is_new: boolean;
  /** 管理员明确停用过的模型（§16.2）。与"从没出现过"分开。 */
  excluded: boolean;
}

/** 分组下游账号目录里可直接选择的模型。 */
export interface AvailableModel {
  public_name: string;
  accounts: string[];
}

/** 兼容旧接口：上游模型名 → 下游模型名（§16.4）。 */
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
  /** 时间衰减后的有效样本量。评分采信的是它，不是累计条数。 */
  effective_samples: number;
  /** 判定可信的有效样本量门槛，由后端下发，界面不得写死。 */
  warm_threshold: number;
  /** 有效样本量不足门槛时性能三维用的是保守中性分。 */
  warm: boolean;
  /**
   * 各维度的加权贡献（得分 × 权重 ÷ 100），四项之和即总分（§6.5）。
   *
   * 只有归一化得分时，得回分组页查权重才能判断"是哪一维把分数拉下去的"；
   * 有贡献值就能直接横向比较。
   */
  contribution: {
    multiplier: number;
    reliability: number;
    first_token: number;
    throughput: number;
  };
}

/** 调度视图「首字 / 速度」两列背后的样本（§6.5）。 */
export interface TargetStatsSample {
  /** 该目标在这个维度上采到的**累计**样本条数。 */
  samples: number;
  /** 时间衰减后的有效样本量；它才是评分采信的数字（§9.4）。 */
  effective_samples: number;
  /** 可信门槛，由后端下发（§9.4）。 */
  warm_threshold: number;
  /** 有效样本量是否已够门槛；不够时数字只作参考（§9.4）。 */
  warm: boolean;
  /** 这些样本来自哪种下游协议。 */
  protocol: Protocol;
  /** 是否流式请求的样本。 */
  streaming: boolean;
}

export interface DispatchTarget {
  id: string;
  logical_model_id: string;
  account_id: string;
  upstream_model: string;
  /** 是否隐藏上游原始模型名。 */
  hide_original: boolean;
  /** 历史字段，恒为 null；调度目标不再支持独立优先级覆盖。 */
  priority_override: number | null;
  /** 实际生效的优先级：统一来自账号默认人工优先级。 */
  priority: number;
  limits: Limits;
  /** 账号默认值与目标覆盖合并后的实际限制。 */
  effective_limits: Limits;
  enabled: boolean;
  status: TargetStatus;
  cooldown_secs: number | null;
  inflight: number;
  score: Score | null;
  /** 首字延迟的当前 EWMA（毫秒）；一项样本都没采到时为 null（§6.5）。 */
  first_token_ms: number | null;
  /** 输出速度的当前 EWMA（token/秒）（§6.5）。 */
  output_tps: number | null;
  /** 非流式总延迟的当前 EWMA（毫秒）（§6.5）。 */
  total_ms: number | null;
  /**
   * 上面三列背后的样本口径；一个样本都没有时为 null。
   *
   * 有效样本不足门槛时三列照样给值，界面必须把样本数标出来——只给热目标显示，
   * 等于让刚开始拿流量的账号永远没有数据可看（§6.5）。
   */
  stats: TargetStatsSample | null;
  /** 暂停原因；正常参与调度时为 null（§6.5）。 */
  pause_reason: string | null;
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
  /** 首个语义块到达耗时（毫秒）；非流式或上游未给时为 null（§6.6）。 */
  first_token_ms: number | null;
  /** 上游上报的输入/输出 Token；没上报就是 null，不做估算（§6.8）。 */
  input_tokens: number | null;
  output_tokens: number | null;
  /** 产生这条记录时的配置快照版本。 */
  config_version: number | null;
  /** 为保住前缀缓存等待的毫秒数；与 queued_ms 分开（§6.6、§24.1）。 */
  sticky_wait_ms: number | null;
  /** 这次粘性等待用的缓存新鲜度系数（§10.3 的三档）。 */
  sticky_freshness: number | null;
  /** 输出速度（token/秒）。上游没上报 Token 时为 null。 */
  output_tps: number | null;
  /** Token 细分（§11.6）：缓存读/写与思考。上游没上报就是 null，不估算。 */
  cache_read_tokens: number | null;
  cache_write_tokens: number | null;
  reasoning_tokens: number | null;
  /** 有效倍率来源：auto / manual（§24.1）。 */
  multiplier_source: string | null;
  /** 记录时刻的额度状态（§24.1）。 */
  quota_status: string | null;
  /** 候选过滤原因摘要，形如 "倍率超限×2,能力不支持×1"（§24.1）。 */
  filter_summary: string | null;
  /** 最终选中的层（优先级数字）（§24.1）。 */
  selected_layer: number | null;
  /** 每次上游尝试的明细（§6.6）。 */
  attempts_detail: AttemptRecord[];
}

export type RequestStatus = "ok" | "error";

/** 请求记录查询条件。未提供的字段不会出现在查询串中。 */
export interface RequestFilters {
  limit?: number;
  offset?: number;
  since?: number;
  until?: number;
  request_id?: string;
  group_id?: string;
  logical_model?: string;
  target_id?: string;
  account_id?: string;
  status?: RequestStatus;
  error_code?: string;
}

export interface RequestPage {
  data: RequestRecord[];
  /** 符合查询条件的总数，不只是当前页条数。 */
  total: number;
}

/** 一次上游尝试的明细（§6.6）。 */
export interface AttemptRecord {
  seq: number;
  target_id: string | null;
  account_id: string | null;
  upstream_model: string | null;
  endpoint: string | null;
  started_at: number;
  duration_ms: number;
  /** ok / failed / missing_endpoint。 */
  outcome: string;
  error_code: string | null;
  /** 这次失败是否计入尝试预算；廉价失败不计（§13.1）。 */
  counts_against_budget: boolean;
}

export interface MultiplierAlert {
  account_id: string;
  name: string;
  stale_for?: number | null;
}

export interface RecentError {
  request_id: string;
  started_at: number;
  logical_model: string | null;
  target_id: string | null;
  http_status: number;
  error_code: string | null;
}

export interface RecentChange {
  occurred_at: number;
  actor: string;
  action: string;
  object: string;
  result: string;
}

/** 概览趋势图的一个小时桶（§6.2）。 */
export interface TrendPoint {
  bucket_start: number;
  requests: number;
  success: number;
}

export interface Overview {
  config_version: number;
  groups: number;
  logical_models: number;
  /** 当前下游可用的模型名数量（一个模型的多个名称会分别计数）。 */
  listable_models: number;
  /** 零目标、因此没有出现在 /v1/models 里的模型数。 */
  unlisted_models: number;
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
  /** 数据目录（§6.1）：主密钥、SQLite 与临时文件都在这里。 */
  data_dir: string;
  /** 运行指标的统计窗口，当前固定为 24 小时。 */
  window_secs: number;
  /** 保留期为 0：明细不落库，这里的数字来自内存汇总，只覆盖当日（§24.2）。 */
  retention_off: boolean;
  requests: number;
  success_rate: number | null;
  avg_latency_ms: number | null;
  p50_latency_ms: number | null;
  p95_latency_ms: number | null;
  in_flight: number;
  queued: number;
  queue_timeouts: number;
  recent_errors: RecentError[];
  recent_changes: RecentChange[];
  /** 趋势图的桶宽（秒），当前为 3600。 */
  trend_bucket_secs: number;
  /** 近 24 小时的请求趋势，空桶已由后端补齐。 */
  trend: TrendPoint[];
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
  /** 协议适配层版本（§6.7）。它变了，学到的能力证据会整体失效。 */
  adapter_version: string;
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

/** 站点级 New API 凭据：一个 Base URL 配一次，账号自动继承（§6.4）。 */
export interface NewApiSite {
  base_url: string;
  user_id: string;
}

/** 同步执行倍率探测后的结果。 */
export interface MultiplierRefreshResult {
  refreshed: boolean;
  effective_multiplier: string;
  /** 某些上游不提供观察时间时后端会返回 null。 */
  observed_at: number | null;
  notice: string;
}

/** 批量刷新倍率的结果：逐账号收集，一个失败不影响其余（§11.3）。 */
export interface BatchMultiplierRefreshResult {
  total: number;
  refreshed: number;
  failed: number;
  results: {
    account_id: string;
    name: string;
    effective_multiplier: string;
    observed_at: number | null;
  }[];
  errors: { account_id: string; name: string; error: string }[];
  notice: string;
}

/** 成本页的一条账号流量行（§6.8）。 */
export interface CostAccountRow {
  account_id: string;
  name: string;
  requests: number;
  /** 该账号在该逻辑模型上的 Token 用量（上游没上报时为 0）。 */
  tokens: number;
  share: number;
  effective_multiplier: string | null;
}

/** 成本页的一个逻辑模型块（§6.8）：绝不跨模型加总。 */
export interface CostModel {
  group_id: string;
  logical_model: string;
  requests: number;
  tokens: number;
  /** 该模型块的占比口径：tokens 或 requests。 */
  share_basis: "tokens" | "requests";
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
  total_tokens: number;
  /** 保留期为 0：只统计当日，选了"本月"也只有当天数据（§24.2）。 */
  retention_off: boolean;
  /** 全局占比口径：区间内出现过 Token 就按 Token，否则退回请求数。 */
  share_basis: "tokens" | "requests";
  account_shares: {
    account_id: string;
    name: string;
    requests: number;
    tokens: number;
    share: number;
  }[];
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
  /** 逐把 Key 的结果（§4.2.1）：多 Key 账号最有用的诊断动作。 */
  keys?: TestKeyResult[];
  /** 通过测试的 Key 数量，与总数一起给出"2/3 可用"。 */
  healthy_keys?: number;
  key_total?: number;
}

/** 单把 Key 的测试结果。只带标签，不带凭据。 */
export interface TestKeyResult {
  id: string;
  label: string;
  enabled: boolean;
  ok: boolean;
  status: number;
  latency_ms: number;
  message: string;
}

export interface SetupStatus {
  needs_setup: boolean;
  master_key_from_env: boolean;
  /** 数据目录（§6.1）：主密钥、SQLite 与临时文件都在这里。 */
  data_dir: string;
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

/** 部署形态：决定「立即更新」是否可用、该给哪条升级命令。 */
export type DeployKind = "binary" | "docker" | "source" | "windows";

/** 一次"识别倍率来源"的结果（POST /admin/api/accounts/{id}/detect-multiplier-source）。 */
export interface DetectMultiplierResult {
  /** 识别出的来源，已写回账号。 */
  multiplier_mode: MultiplierMode;
  detected: boolean;
  notice: string;
  /** 识别成功、但紧接着的首次探测失败时的原因（按 §11.4 进入宽限期）。 */
  probe_error?: string;
}

/** 一次版本检查的结果（GET /admin/api/system/update）。 */
export interface UpdateStatus {
  /** 是否启用了更新检查（服务端可用 AKHUB_UPDATE_DISABLED 关闭）。 */
  enabled: boolean;
  /** 当前进程的版本。 */
  current: string;
  /** 上游最新版本；查询失败时为 null。 */
  latest: string | null;
  has_update: boolean;
  release_url: string | null;
  release_name: string | null;
  published_at: string | null;
  /** Release 说明的摘要。 */
  notes: string | null;
  checked_at: number;
  /** 这次结果是否来自服务端缓存。 */
  cached: boolean;
  /** 查询失败的原因：拿不到 GitHub 时明确说，而不是假装已是最新。 */
  error: string | null;
  deploy: DeployKind;
  can_self_update: boolean;
  can_restart: boolean;
  update_hint: string | null;
  update_command: string | null;
  /** 已落盘但还没重启生效的版本。 */
  pending_version: string | null;
}

/** 自更新成功后的结果（POST /admin/api/system/update）。 */
export interface UpdateOutcome {
  from: string;
  to: string;
  path: string;
  /** 旧二进制的备份路径。 */
  backup: string | null;
  need_restart: boolean;
  can_restart: boolean;
  restart_command: string;
}

export interface UpdateStatus {
  /** 是否启用了更新检查（服务端可用 AKHUB_UPDATE_DISABLED 关闭）。 */
  enabled: boolean;
  /** 当前进程的版本。 */
  current: string;
  /** 上游最新版本；查询失败时为 null。 */
  latest: string | null;
  has_update: boolean;
  release_url: string | null;
  release_name: string | null;
  published_at: string | null;
  /** Release 说明的摘要。 */
  notes: string | null;
  checked_at: number;
  /** 这次结果是否来自服务端缓存。 */
  cached: boolean;
  /** 查询失败的原因：拿不到 GitHub 时明确说，而不是假装已是最新。 */
  error: string | null;
  deploy: DeployKind;
  can_self_update: boolean;
  can_restart: boolean;
  update_hint: string | null;
  update_command: string | null;
  /** 已落盘但还没重启生效的版本。 */
  pending_version: string | null;
}

/** 自更新成功后的结果（POST /admin/api/system/update）。 */
export interface UpdateOutcome {
  from: string;
  to: string;
  path: string;
  /** 旧二进制的备份路径。 */
  backup: string | null;
  need_restart: boolean;
  can_restart: boolean;
  restart_command: string;
}
