import type {
  Account,
  AccountModel,
  Alias,
  CalibrationRecord,
  CalibrationResult,
  CostView,
  DispatchTarget,
  Group,
  KeyReveal,
  Limits,
  LogicalModel,
  Overview,
  RequestRecord,
  SelectionWarning,
  Settings,
  SettingsPatch,
  SetupStatus,
  NewApiGroupOption,
  TestResult,
  MultiplierRefreshResult,
} from "./types";

const BASE = "/admin/api";
/** 写操作必须携带的自定义头，配合 SameSite=Strict 构成 CSRF 防护。 */
const CSRF_HEADER = "x-akhub-csrf";

/** 后台接口返回的错误。`unauthorized` 让上层能区分"要重新登录"与"操作失败"。 */
export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    /** 409 响应附带的二次确认数据（§16.3：有流量的模型取消勾选）。 */
    readonly payload?: unknown,
  ) {
    super(message);
    this.name = "ApiError";
  }

  get unauthorized(): boolean {
    return this.status === 401;
  }
}

async function request<T>(
  path: string,
  init: RequestInit & { method?: string } = {},
): Promise<T> {
  const method = init.method ?? "GET";
  const headers = new Headers(init.headers);
  if (init.body) headers.set("content-type", "application/json");
  if (method !== "GET") headers.set(CSRF_HEADER, "1");

  let response: Response;
  try {
    response = await fetch(BASE + path, { ...init, method, headers });
  } catch {
    throw new ApiError("无法连接到 Akhub 服务", 0);
  }

  if (response.status === 204) return undefined as T;

  const text = await response.text();
  const body = text ? safeParse(text) : null;

  if (!response.ok) {
    const message =
      (body && typeof body === "object" && "error" in body
        ? String((body as { error: unknown }).error)
        : null) ?? `请求失败（HTTP ${response.status}）`;
    throw new ApiError(message, response.status, body);
  }
  return body as T;
}

function safeParse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

const post = <T>(path: string, body?: unknown) =>
  request<T>(path, { method: "POST", body: body ? JSON.stringify(body) : undefined });
const patch = <T>(path: string, body: unknown) =>
  request<T>(path, { method: "PATCH", body: JSON.stringify(body) });
const del = (path: string) => request<void>(path, { method: "DELETE" });

/** 新建分组的输入。倍率一律以字符串传递，避免浮点误差。 */
export interface GroupInput {
  name: string;
  multiplier_limit: string;
  weights?: Group["weights"];
  queue_capacity?: number;
  allow_degrade?: boolean;
}

export interface AccountInput {
  group_id: string;
  name: string;
  upstream_type: Account["upstream_type"];
  base_url: string;
  api_key: string;
  preferred_protocol: Account["preferred_protocol"];
  adaptive_protocol?: boolean;
  default_priority?: number;
  calibration?: string;
  multiplier_mode?: Account["multiplier_mode"];
  manual_multiplier?: string;
  new_api_token?: string;
  new_api_user_id?: string;
  new_api_group?: string;
  limits?: Limits;
  allow_private_network?: boolean;
  enabled?: boolean;
  /** 开启后全量托管上游模型，忽略选择集（§16.2）。 */
  auto_sync?: boolean;
}

export interface TargetInput {
  logical_model_id: string;
  account_id: string;
  upstream_model: string;
  priority_override?: number | null;
  limits?: Limits;
  enabled?: boolean;
}

export const api = {
  setupStatus: () => request<SetupStatus>("/setup/status"),
  setup: (username: string, password: string) =>
    post<{ username: string }>("/setup", { username, password }),
  login: (username: string, password: string) =>
    post<{ username: string }>("/auth/login", { username, password }),
  logout: () => post<void>("/auth/logout"),

  overview: () => request<Overview>("/overview"),
  settings: () => request<Settings>("/settings"),

  groups: () => request<Group[]>("/groups"),
  createGroup: (input: GroupInput) => post<KeyReveal>("/groups", input),
  updateGroup: (id: string, input: Partial<GroupInput>) =>
    patch<Group>(`/groups/${id}`, input),
  deleteGroup: (id: string) => del(`/groups/${id}`),
  regenerateKey: (id: string) => post<KeyReveal>(`/groups/${id}/regenerate-key`),

  accounts: () => request<Account[]>("/accounts"),
  createAccount: (input: AccountInput) => post<Account>("/accounts", input),
  updateAccount: (id: string, input: Partial<AccountInput>) =>
    patch<Account>(`/accounts/${id}`, input),
  deleteAccount: (id: string) => del(`/accounts/${id}`),
  refreshMultiplier: (id: string) =>
    post<MultiplierRefreshResult>(`/accounts/${id}/refresh-multiplier`),
  /** 拉取该账号可用的 New API 分组（只读，不落库）。 */
  multiplierGroups: (id: string) =>
    request<{ groups: NewApiGroupOption[] }>(`/accounts/${id}/multiplier-groups`),
  /** 一键独立复制：停用状态的「名称 - 副本」，Key 重新加密（§6.4）。 */
  copyAccount: (id: string) => post<Account>(`/accounts/${id}/copy`),
  /** 测试连接：发一次真实 `hi`，不参与任何统计（§6.4）。 */
  testAccount: (id: string, model?: string) =>
    post<TestResult>(`/accounts/${id}/test`, { model: model || undefined }),
  /** 校准助手：按单模型对账反算校准系数（§6.8）。 */
  calibrate: (id: string, logicalModel: string, reported: string, periodStart?: number) =>
    post<CalibrationResult>(`/accounts/${id}/calibrate`, {
      logical_model: logicalModel,
      reported,
      period_start: periodStart,
    }),
  calibrations: (id: string) =>
    request<{ data: CalibrationRecord[] }>(`/accounts/${id}/calibrations`),

  /** 成本页：按逻辑模型分组，绝不跨模型加总（§6.8）。 */
  cost: (period: "day" | "month" = "day") =>
    request<CostView>(`/cost?period=${period}`),

  /** 导出加密配置备份，返回信封 JSON 文本（§23.5）。 */
  exportBackup: async (password: string) => {
    const response = await fetch(BASE + "/backup/export", {
      method: "POST",
      headers: { "content-type": "application/json", [CSRF_HEADER]: "1" },
      body: JSON.stringify({ password }),
    });
    const text = await response.text();
    if (!response.ok) {
      const body = safeParse(text) as { error?: string } | null;
      throw new ApiError(body?.error ?? `导出失败（HTTP ${response.status}）`, response.status);
    }
    return text;
  },
  /** 校验并原子恢复一份备份（§23.5）。 */
  importBackup: (password: string, content: string) =>
    post<{ groups: number; accounts: number; logical_models: number; dispatch_targets: number }>(
      "/backup/import",
      { password, content },
    ),

  /** 当前模型目录快照（§16.2 的选择集状态）。 */
  accountModels: (id: string) => request<AccountModel[]>(`/accounts/${id}/models`),
  /** 拉取上游模型列表并合并目录（§16.1）。失败时保留原目录，只报错。 */
  refreshAccountModels: (id: string) =>
    post<AccountModel[]>(`/accounts/${id}/models/refresh`),
  /** 手动添加上游模型并纳入调度（§16.5）。 */
  addManualModel: (id: string, upstreamModel: string, publicName?: string) =>
    post<void>(`/accounts/${id}/models`, {
      upstream_model: upstreamModel,
      public_name: publicName || undefined,
    }),
  /** 批量应用选择集。409 时返回二次确认数据而不是抛错（§16.3）。 */
  selectAccountModels: async (
    id: string,
    selected: string[],
    force = false,
  ): Promise<
    { warnings: SelectionWarning[] } | { created_targets: number; removed_targets: number }
  > => {
    try {
      return await post<{ created_targets: number; removed_targets: number }>(
        `/accounts/${id}/models/select`,
        { selected, force },
      );
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 409) {
        const payload = cause.payload as { warnings?: SelectionWarning[] } | null;
        return { warnings: payload?.warnings ?? [] };
      }
      throw cause;
    }
  },
  /** 立即执行一轮托管同步（§16.2）。 */
  syncAccountModels: (id: string) =>
    post<{ managed_models: number }>(`/accounts/${id}/models/sync`),

  aliases: (id: string) => request<Alias[]>(`/accounts/${id}/aliases`),
  updateAliases: (id: string, aliases: Alias[]) =>
    request<void>(`/accounts/${id}/aliases`, {
      method: "PUT",
      body: JSON.stringify({ aliases }),
    }),

  models: () => request<LogicalModel[]>("/logical-models"),
  createModel: (input: { group_id: string; name: string; enabled?: boolean }) =>
    post<LogicalModel>("/logical-models", input),
  updateModel: (id: string, input: { name?: string; enabled?: boolean }) =>
    patch<void>(`/logical-models/${id}`, input),
  deleteModel: (id: string) => del(`/logical-models/${id}`),

  targets: () => request<DispatchTarget[]>("/targets"),
  createTarget: (input: TargetInput) => post<DispatchTarget>("/targets", input),
  updateTarget: (id: string, input: Partial<TargetInput>) =>
    patch<DispatchTarget>(`/targets/${id}`, input),
  deleteTarget: (id: string) => del(`/targets/${id}`),

  requests: (limit = 60, offset = 0) =>
    request<{ data: RequestRecord[] }>(`/requests?limit=${limit}&offset=${offset}`),

  /** 只提交发生变化的系统设置；后端会立即热生效。 */
  updateSettings: (values: SettingsPatch) => patch<Settings>("/settings", values),
  /** 修改管理员密码，成功后后端会下发新会话 Cookie。 */
  changePassword: (current: string, next: string) =>
    post<{ username: string }>("/auth/password", {
      current_password: current,
      new_password: next,
    }),
};
