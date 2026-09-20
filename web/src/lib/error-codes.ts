/**
 * 网关错误码的中文解释。
 *
 * 错误码是排障入口，但新用户看到 `no_eligible_target` 并不知道该改哪里。
 * 这里给每个稳定错误码一句"这是什么 + 通常怎么处理"，界面用 InfoTip 呈现，
 * 原始错误码仍然保留，方便复制给上游或查日志。
 */
export interface ErrorCodeInfo {
  label: string;
  hint: string;
  /** 提示用户是否可以稍后重试。 */
  retryable: boolean;
}

const CODES: Record<string, ErrorCodeInfo> = {
  auth_invalid: {
    label: "下游 Key 无效",
    hint: "客户端使用的分组 Key 不正确或已重置。请核对 Authorization / x-api-key。",
    retryable: false,
  },
  model_not_found: {
    label: "模型不存在",
    hint: "请求的模型名在该分组内没有对应的模型。检查拼写，或到「上游账号 → 模型管理」确认该模型已启用且对外名正确。",
    retryable: false,
  },
  no_eligible_target: {
    label: "没有可用目标",
    hint: "该模型的所有上游目标都不可用（停用、冷却、额度耗尽或账号异常）。到「调度视图」查看暂停原因，或到账号的「模型管理」启用备用账号。",
    retryable: true,
  },
  multiplier_unknown: {
    label: "倍率未知",
    hint: "账号自动倍率刷新失败且已超过宽限期，相关目标被硬停。到「上游账号」页刷新倍率。",
    retryable: true,
  },
  multiplier_exceeded: {
    label: "超过分组倍率上限",
    hint: "所有可用目标的有效倍率都高于分组上限，请求被拒绝。到「分组」页调整上限，或补充更低倍率的目标。",
    retryable: false,
  },
  queue_full: {
    label: "队列已满",
    hint: "目标全忙且队列达到分组容量上限。稍后重试，或到「分组」页调大队列容量。",
    retryable: true,
  },
  queue_timeout: {
    label: "排队超时",
    hint: "在队列里等待超过分组的最长等待时间。可重试，或调大「队列最长等待」。",
    retryable: true,
  },
  request_too_large: {
    label: "请求体过大",
    hint: "请求超过设置的请求体上限。到「设置」页调整，或减小请求体积。",
    retryable: false,
  },
  unsupported_parameter: {
    label: "上游不支持该参数",
    hint: "请求参数无法被任何可用目标表达，Akhub 拒绝伪造结果。可改用上游支持的参数或更换目标。",
    retryable: false,
  },
  upstream_timeout: {
    label: "上游超时",
    hint: "上游在请求总超时时间内没有响应。可重试；若持续出现，检查上游状态或调大超时时间。",
    retryable: true,
  },
  upstream_exhausted: {
    label: "上游重试耗尽",
    hint: "同一请求内所有候选目标都尝试失败。到「请求记录」展开本次尝试明细查看每个目标的原因。",
    retryable: true,
  },
  upstream_protocol_error: {
    label: "上游协议错误",
    hint: "上游返回了无法解析为声明协议的响应。检查账号的首选协议与 Base URL 是否匹配。",
    retryable: true,
  },
  upstream_connection_aborted: {
    label: "上游连接中断",
    hint: "与上游的连接在完成前断开。可重试；若持续出现，检查网络或上游稳定性。",
    retryable: true,
  },
  missing_endpoint: {
    label: "上游没有该端点",
    hint: "上游不提供请求需要的端点，Akhub 已回退到跨协议转换。如果账号首选协议配置正确，通常不应出现。",
    retryable: false,
  },
  response_state_expired: {
    label: "Responses 状态已过期",
    hint: "续链所需的响应状态超过保留期已被清理。到「设置」页调大 Responses 状态保留天数。",
    retryable: false,
  },
  rate_limited: {
    label: "触发限流",
    hint: "命中账号或目标的 RPM / TPM / 并发限制。可重试，或调整限制配置。",
    retryable: true,
  },
  gateway_restart: {
    label: "网关重启中断",
    hint: "请求进行时 Akhub 进程重启，连接被中断。可重试。",
    retryable: true,
  },
  internal_error: {
    label: "网关内部错误",
    hint: "Akhub 自身处理失败。请查看服务端日志；若可复现请携带请求 ID 反馈。",
    retryable: true,
  },
};

export function errorCodeInfo(code: string | null | undefined): ErrorCodeInfo | null {
  if (!code) return null;
  return CODES[code] ?? null;
}

export function errorCodeLabel(code: string | null | undefined): string {
  if (!code) return "—";
  return CODES[code]?.label ?? code;
}
