import type {
  Account,
  AccountKeyInput,
  AccountModel,
  Alias,
  AvailableModel,
  CalibrationRecord,
  CalibrationResult,
  CostView,
  DispatchTarget,
  Group,
  KeyReveal,
  Limits,
  LogicalModel,
  Overview,
  Page,
  RequestFilters,
  RequestPage,
  SelectionWarning,
  Settings,
  SettingsPatch,
  SetupStatus,
  NewApiGroupOption,
  NewApiSite,
  TestResult,
  DetectMultiplierResult,
  MultiplierRefreshResult,
  BatchMultiplierRefreshResult,
  UpdateStatus,
  UpdateOutcome,
} from "./types";

const BASE = "/admin/api";
/** 写操作必须携带的自定义头，配合 SameSite=Strict 构成 CSRF 防护。 */
const CSRF_HEADER = "x-akhub-csrf";
/** 乐观锁：服务端在每个响应里回带当前配置版本（§7.4）。 */
const CONFIG_VERSION_HEADER = "x-akhub-config-version";

/**
 * 最近一次从服务端读到的配置版本。
 *
 * 每次响应都会刷新它，写操作把它带回去；服务端发现版本对不上就返回 409，
 * 从而把"打开两个标签页各改一半、后保存的覆盖先保存的"挡在门外（§7.4）。
 * 用 0 表示"还没读到过"，此时不带头部，服务端按兼容模式处理。
 */
let configVersion = 0;

/** 当前已知的配置版本，供界面判断是否需要提示重新加载。 */
export function knownConfigVersion(): number {
  return configVersion;
}

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

  /** 配置版本冲突：别人已经改过配置，需要重新加载再保存（§7.4）。 */
  get configConflict(): boolean {
    return this.status === 409 && this.message.includes("config_conflict");
  }
}

/**
 * 写操作的串行队列（§7.4）。
 *
 * 乐观锁比的是"客户端手上的版本号"，而**我们自己**的成功写入也会让版本号 +1。
 * 于是两个写请求只要重叠，后发的那一个就带着已经过期的版本号，被服务端判成
 * "配置已被其他会话修改"——明明只有一个管理员在操作，却收到冲突提示。
 *
 * 所以写操作必须排队：同一时刻只允许一个写在途，后到的等前一个结束（此时
 * `configVersion` 已被它的响应刷新）再发出。读操作不受影响，仍然并发。
 */
let writeChain: Promise<unknown> = Promise.resolve();

/**
 * 排队执行一个写请求，返回它自己的响应。
 *
 * 队列本身用 `catch` 吞掉前一个请求的错误：前一个失败不该拦住后一个，
 * 每个调用者只关心自己那次的结果。
 */
function enqueueWrite<T>(run: () => Promise<T>): Promise<T> {
  const next = writeChain.then(run, run);
  writeChain = next.catch(() => undefined);
  return next;
}

async function request<T>(
  path: string,
  init: RequestInit & { method?: string } = {},
): Promise<T> {
  const method = init.method ?? "GET";
  // 写操作排队，读操作直发。
  const send = () => sendOnce<T>(path, init, method);
  return method === "GET" ? send() : enqueueWrite(send);
}

async function sendOnce<T>(
  path: string,
  init: RequestInit & { method?: string },
  method: string,
): Promise<T> {
  const headers = new Headers(init.headers);
  if (init.body) headers.set("content-type", "application/json");
  if (method !== "GET") {
    headers.set(CSRF_HEADER, "1");
    // 版本号在**真正发出前**才读取：排队等待期间前一个写操作已经把
    // `configVersion` 刷新过了。用旧值就会把自己判成冲突（见 `enqueueWrite`）。
    if (configVersion > 0) {
      headers.set(CONFIG_VERSION_HEADER, String(configVersion));
    }
  }

  let response: Response;
  try {
    response = await fetch(BASE + path, { ...init, method, headers });
  } catch {
    throw new ApiError("无法连接到 Akhub 服务", 0);
  }

  // 服务端在**每次**响应里回带最新版本：写成功之后版本已经 bump，
  // 不跟着更新的话下一次写必然 409。
  //
  // 只在**成功**响应上采纳版本号。失败时一律保持旧版本：写失败说明配置没
  // 被这次改过，旧版本仍然有效；而版本冲突时更不能采纳，否则用户直接再点一次
  // 保存就会成功并悄悄覆盖别人的修改——"请重新加载"必须真的靠一次 GET 才能
  // 拿到新版本（§7.4）。
  const returned = response.headers.get(CONFIG_VERSION_HEADER);
  if (returned && response.ok) {
    const parsed = Number.parseInt(returned, 10);
    if (Number.isFinite(parsed)) configVersion = parsed;
  }

  if (response.status === 204) return undefined as T;

  const text = await response.text();
  const body = text ? safeParse(text) : null;

  if (!response.ok) {
    const message =
      (body && typeof body === "object" && "error" in body
        ? String((body as { error: unknown }).error)
        : null) ?? `请求失败（HTTP ${response.status}）`;
    // 并发编辑冲突交给全局处理：用户需要的是"重新加载"的明确路径，
    // 而不是 toast 里一行 config_conflict（§7.4）。
    if (response.status === 409 && message.includes("config_conflict")) {
      window.dispatchEvent(new CustomEvent("akhub-config-conflict"));
    }
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
  max_wait_secs?: number;
  allow_degrade?: boolean;
}

export interface AccountInput {
  /** 所属分组；`null` 表示未分配（§4.2.3）。新建时缺省即未分配。 */
  group_id?: string | null;
  name: string;
  base_url: string;
  /** 账号内 Key 池（§4.2.1）。整体替换；缺省时后台不动 Key 池。 */
  keys?: AccountKeyInput[];
  /** 旧版单 Key 字段。给出且 `keys` 缺省时等价于"把池换成这一把"。 */
  api_key?: string;
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
  /** 账号级隐藏原始模型；打开后没设置下游模型名的模型不对外暴露。 */
  hide_original?: boolean;
}

export interface TargetInput {
  logical_model_id: string;
  account_id: string;
  upstream_model: string;
  priority_override?: number | null;
  limits?: Limits;
  enabled?: boolean;
}

/**
 * 配置列表一次最多能要多少条（§7.4：列表上限 1000 条）。
 *
 * 后台的四个配置列表——分组、账号、逻辑模型、调度目标——在界面上是**整体使用**的：
 * 搜索、筛选、跨列表引用（目标 → 账号 / 模型）都建立在本地的完整集合上。
 * 只取一页再到本地过滤，会得到"配置里明明有、却搜不到"的假结果，模型与目标
 * 一多（比如迁移进来几十个上游账号之后），页面还会静默少显示一大截。
 */
const PAGE_SIZE_MAX = 1000;

/**
 * 取全一个配置列表的所有分页，返回合并后的 `Page`。
 *
 * 单页上限是服务端的硬约束，客户端能做的正确选择是**按 offset 翻到底**，
 * 而不是把第一页当成全部。正常情况下这里只发一次请求（1000 ≥ 配置规模）；
 * 超过上限时自动多发几次，界面拿到的永远是完整集合。
 *
 * 兜底：某一页返回空、或返回的长度不再增长（服务端异常）时立刻停下，
 * 用 `data.length >= total` 判断交给调用方，绝不在这里死循环。
 */
async function requestAllPages<T>(path: string): Promise<Page<T>> {
  const data: T[] = [];
  let total = 0;
  let limit = PAGE_SIZE_MAX;
  for (;;) {
    const page = await request<Page<T>>(
      `${path}?limit=${PAGE_SIZE_MAX}&offset=${data.length}`,
    );
    total = page.total;
    limit = page.limit;
    if (page.data.length === 0) break;
    data.push(...page.data);
    if (data.length >= page.total) break;
  }
  return { data, total: Math.max(total, data.length), limit, offset: 0 };
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

  groups: () => requestAllPages<Group>("/groups"),
  availableGroupModels: (id: string) =>
    request<{ models: AvailableModel[] }>(`/groups/${id}/available-models`),
  createGroup: (input: GroupInput) => post<KeyReveal>("/groups", input),
  updateGroup: (id: string, input: Partial<GroupInput>) =>
    patch<Group>(`/groups/${id}`, input),
  deleteGroup: (id: string) => del(`/groups/${id}`),
  regenerateKey: (id: string) => post<KeyReveal>(`/groups/${id}/regenerate-key`),

  accounts: () => requestAllPages<Account>("/accounts"),
  createAccount: (input: AccountInput) => post<Account>("/accounts", input),
  updateAccount: (id: string, input: Partial<AccountInput>) =>
    patch<Account>(`/accounts/${id}`, input),
  deleteAccount: (id: string) => del(`/accounts/${id}`),
  refreshMultiplier: (id: string) =>
    post<MultiplierRefreshResult>(`/accounts/${id}/refresh-multiplier`),
  /** 识别这个账号的倍率来源并写回（§11.2）：识别成功即成为该账号的倍率来源。 */
  detectMultiplierSource: (id: string) =>
    post<DetectMultiplierResult>(`/accounts/${id}/detect-multiplier-source`),
  /** 批量刷新所有自动倍率账号（§11.3、§27 阶段5）。 */
  refreshAllMultipliers: () =>
    post<BatchMultiplierRefreshResult>("/accounts/refresh-multipliers"),
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
  exportBackup: async (password: string) =>
    enqueueWrite(async () => {
    // 走同一条写队列：导出本身不改配置，但"导出与恢复不能交叉"。
    const headers: Record<string, string> = {
      "content-type": "application/json",
      [CSRF_HEADER]: "1",
    };
    if (configVersion > 0) headers[CONFIG_VERSION_HEADER] = String(configVersion);
    const response = await fetch(BASE + "/backup/export", {
      method: "POST",
      headers,
      body: JSON.stringify({ password }),
    });
    const text = await response.text();
    if (!response.ok) {
      const body = safeParse(text) as { error?: string } | null;
      throw new ApiError(body?.error ?? `导出失败（HTTP ${response.status}）`, response.status);
    }
    return text;
    }),
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
  /** 修改一行模型目录的下游模型名 / 启用状态（§16.3）。 */
  updateAccountModel: (
    id: string,
    payload: {
      upstream_model: string;
      /** null 表示不改；空字符串表示清空下游模型名。 */
      alias?: string | null;
      selected?: boolean;
    },
  ) => post<AccountModel[]>(`/accounts/${id}/models/update`, payload),
  /**
   * 批量应用模型目录改动（§16.3）。
   *
   * 界面的勾选是本地草稿 + 防抖：一次请求提交整批改动，服务端只调和一遍目标、
   * 只重载一遍配置。逐行 `updateAccountModel` 在几百个模型时会明显卡住。
   * 停用有流量的模型时返回 409，由调用方确认后带 `force` 重发。
   */
  applyAccountModels: (
    id: string,
    changes: {
      upstream_model: string;
      alias?: string;
      selected?: boolean;
      delete?: boolean;
    }[],
    force = false,
  ) => post<AccountModel[]>(`/accounts/${id}/models/apply`, { changes, force }),
  /** 从账号目录永久删除一行并移除其目标（§16.5）。 */
  deleteAccountModel: (id: string, upstreamModel: string) =>
    post<void>(`/accounts/${id}/models/delete`, { upstream_model: upstreamModel }),
  /** 把多行合并到同一个下游模型名（§16.4）。 */
  mergeAccountModels: (id: string, upstreamModels: string[], publicName: string) =>
    post<AccountModel[]>(`/accounts/${id}/models/merge`, {
      upstream_models: upstreamModels,
      public_name: publicName,
    }),
  /** 批量应用选择集。409 时返回二次确认数据而不是抛错（§16.3）。 */
  selectAccountModels: async (
    id: string,
    selected: string[],
    force = false,
  ): Promise<
    { needs_confirm: SelectionWarning[] } | { created_targets: number; removed_targets: number }
  > => {
    try {
      return await post<{ created_targets: number; removed_targets: number }>(
        `/accounts/${id}/models/select`,
        { selected, force },
      );
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 409) {
        const payload = cause.payload as
          | { needs_confirm?: SelectionWarning[]; warnings?: SelectionWarning[] }
          | null;
        return { needs_confirm: payload?.needs_confirm ?? payload?.warnings ?? [] };
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

  models: () => requestAllPages<LogicalModel>("/logical-models"),
  createModel: (input: { group_id: string; name: string; enabled?: boolean }) =>
    post<LogicalModel>("/logical-models", input),
  updateModel: (id: string, input: { name?: string; enabled?: boolean }) =>
    patch<void>(`/logical-models/${id}`, input),
  deleteModel: (id: string) => del(`/logical-models/${id}`),

  targets: () => requestAllPages<DispatchTarget>("/targets"),
  createTarget: (input: TargetInput) => post<DispatchTarget>("/targets", input),
  updateTarget: (id: string, input: Partial<TargetInput>) =>
    patch<DispatchTarget>(`/targets/${id}`, input),
  deleteTarget: (id: string) => del(`/targets/${id}`),

  requests: (filters: RequestFilters = {}) => {
    const params = new URLSearchParams();
    const setNumber = (key: string, value: number | undefined) => {
      if (value !== undefined) params.set(key, String(value));
    };
    const setString = (key: string, value: string | undefined) => {
      if (value !== undefined) params.set(key, value);
    };

    setNumber("limit", filters.limit);
    setNumber("offset", filters.offset);
    setNumber("since", filters.since);
    setNumber("until", filters.until);
    setString("request_id", filters.request_id);
    setString("group_id", filters.group_id);
    setString("logical_model", filters.logical_model);
    setString("target_id", filters.target_id);
    setString("account_id", filters.account_id);
    setString("status", filters.status);
    setString("error_code", filters.error_code);

    const query = params.toString();
    return request<RequestPage>(`/requests${query ? `?${query}` : ""}`);
  },

  /** 版本检查：默认走服务端 30 分钟缓存，refresh 表示用户手动重查。 */
  updateStatus: (refresh = false) =>
    request<UpdateStatus>(`/system/update${refresh ? "?refresh=1" : ""}`),
  /** 立即更新：下载 → 校验 sha256 → 原子替换二进制，重启后生效。 */
  runUpdate: () => post<{ outcome: UpdateOutcome }>("/system/update"),
  /** 重启服务：优雅关闭自己，交给 systemd 拉起新版本。 */
  restartService: () => post<{ restarting: boolean }>("/system/restart"),

  /** 只提交发生变化的系统设置；后端会立即热生效。 */
  updateSettings: (values: SettingsPatch) => patch<Settings>("/settings", values),
  /** 修改管理员密码，成功后后端会下发新会话 Cookie。 */
  changePassword: (current: string, next: string) =>
    post<{ username: string }>("/auth/password", {
      current_password: current,
      new_password: next,
    }),
  /** 站点级 New API 凭据（一个 Base URL 配一次，账号自动继承）。 */
  newApiSites: () => request<{ sites: NewApiSite[] }>("/new-api-sites"),
  saveNewApiSite: (base_url: string, user_id: string, access_token?: string) =>
    post<NewApiSite>("/new-api-sites", { base_url, user_id, access_token }),
  deleteNewApiSite: (base_url: string) =>
    del(`/new-api-sites?base_url=${encodeURIComponent(base_url)}`),
};
