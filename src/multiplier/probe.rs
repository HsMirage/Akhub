//! Sub2API 与 New API 的倍率探针（§11.2）。
//!
//! 两个探针都遵守 §23.3 对后台请求的约束：独立超时、响应体上限、Content-Type
//! 检查，以及发起前的 DNS Rebinding 复查。任何一项校验不过就返回错误，绝不
//! "猜一个 x1 顶上"——猜错的方向恰好是亏钱的方向。

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::domain::{Multiplier, MultiplierMode};
use crate::security::url_guard;
use crate::upstream::UpstreamClient;

/// 探针独立超时，不受请求总超时影响。
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// 响应体上限。倍率信息只有几百字节，超过这个量级说明拿错了东西。
const MAX_BODY: usize = 64 * 1024;

/// 一次成功探测的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// 上游声明的当前有效倍率，尚未乘账号校准系数。
    pub multiplier: Multiplier,
    /// 上游声明的观察时间（Unix 秒）。与 Akhub 自己的拉取时间不是一回事。
    pub observed_at: Option<i64>,
    /// 峰值时段。存在时由 Akhub 本地换算当前倍率，不必等下一轮刷新（§11.3）。
    pub peak: Option<PeakSchedule>,
    /// 这次倍率取自哪个分组。Sub2API 是 Key 级计费，没有分组概念，恒为 `None`。
    pub group: Option<String>,
    /// 分组是从哪来的，供后台说明"这个倍率凭什么可信"。
    pub group_source: Option<GroupSource>,
}

/// 倍率取自哪个分组、这个分组是怎么定下来的。
///
/// New API 的 relay 计费看的是**令牌分组**：`sk-` 令牌自己绑了分组时，它覆盖
/// 用户的默认分组（`middleware/auth.go` 的 TokenAuth）。而 `/api/user/self/groups`
/// 返回的倍率矩阵是按**用户分组**算的。两者在"令牌绑了分组"时不是同一个数，
/// 所以必须把"这个倍率代表哪个分组"一并记下来，后台才能说清它可不可信。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSource {
    /// 管理员在账号上显式填的分组：以它为准，拿不到就报错。
    Configured,
    /// 这把 Key 自己在 New API 上绑定的分组（`/api/token/search`）。
    Token,
    /// 令牌没绑分组时，用户的默认分组（`/api/user/self`）。
    User,
    /// 以上都问不出来时的兜底：可用分组里的最高档，保守估高。
    HighestFallback,
}

impl GroupSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Token => "token",
            Self::User => "user",
            Self::HighestFallback => "highest_fallback",
        }
    }

    /// 人话说明，直接进后台提示语。
    pub fn label(self) -> &'static str {
        match self {
            Self::Configured => "账号上填写的分组",
            Self::Token => "这把 Key 在 New API 上绑定的分组",
            Self::User => "该用户在 New API 上的默认分组",
            Self::HighestFallback => "可用分组中的最高档（保守估算，可能高于真实档位）",
        }
    }
}

impl Reading {
    /// 结合峰值时段算出此刻应当使用的倍率。
    pub fn current(&self, now_unix: i64) -> Multiplier {
        match &self.peak {
            Some(peak) if peak.covers(now_unix) => peak.multiplier,
            _ => self.multiplier,
        }
    }
}

/// 峰值时段与峰值倍率。
///
/// 时段一律按 UTC 解释：上游不会附带时区，而按 Akhub 所在机器的本地时区解释
/// 会让同一份配置在不同机器上算出不同倍率。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeakSchedule {
    pub multiplier: Multiplier,
    /// 每段的 `[起, 止)` 秒偏移（UTC 当日 0 点起算）。跨零点的段已被拆开。
    windows: Vec<(u32, u32)>,
}

const DAY_SECONDS: u32 = 24 * 60 * 60;

impl PeakSchedule {
    /// 从 `"HH:MM"` 形式的时段构造，跨零点的段拆成两段。
    fn new(multiplier: Multiplier, raw: &[RawWindow]) -> Result<Self> {
        let mut windows = Vec::with_capacity(raw.len());
        for window in raw {
            let start = parse_clock(&window.start)?;
            let end = parse_clock(&window.end)?;
            if start == end {
                bail!("峰值时段的起止时间相同：{}", window.start);
            }
            if start < end {
                windows.push((start, end));
            } else {
                windows.push((start, DAY_SECONDS));
                windows.push((0, end));
            }
        }
        Ok(Self {
            multiplier,
            windows,
        })
    }

    /// 给定 Unix 秒是否落在峰值时段内。
    pub fn covers(&self, now_unix: i64) -> bool {
        let seconds = now_unix.rem_euclid(i64::from(DAY_SECONDS)) as u32;
        self.windows
            .iter()
            .any(|(start, end)| seconds >= *start && seconds < *end)
    }
}

fn parse_clock(raw: &str) -> Result<u32> {
    let (hours, minutes) = raw
        .split_once(':')
        .with_context(|| format!("峰值时段格式必须是 HH:MM：{raw}"))?;
    let hours: u32 = hours
        .parse()
        .with_context(|| format!("峰值时段的小时非法：{raw}"))?;
    let minutes: u32 = minutes
        .parse()
        .with_context(|| format!("峰值时段的分钟非法：{raw}"))?;
    if hours > 23 || minutes > 59 {
        bail!("峰值时段超出合法范围：{raw}");
    }
    Ok(hours * 3600 + minutes * 60)
}

// -------------------------------------------------------------- 站点类型识别

/// 一次识别得出的站点类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    Sub2Api,
    NewApi,
}

impl Detected {
    pub fn mode(self) -> MultiplierMode {
        match self {
            Self::Sub2Api => MultiplierMode::Sub2Api,
            Self::NewApi => MultiplierMode::NewApi,
        }
    }

    /// 简短的名字，用于给管理员的提示语。
    pub fn label(self) -> &'static str {
        match self {
            Self::Sub2Api => "Sub2API",
            Self::NewApi => "New API",
        }
    }

    /// 探测这一档"不认"时用的接口名，用于拼装给管理员看的错误。
    #[allow(dead_code)]
    pub fn probe_label(self) -> &'static str {
        match self {
            Self::Sub2Api => "Sub2API 计费接口（/v1/sub2api/billing）",
            Self::NewApi => "New API 分组接口（/api/user/self/groups）",
        }
    }
}

/// 探针响应的 HTTP 状态码。单独成类型是为了让识别动作能可靠地分辨
/// "这个端点不存在"与"这次请求没能到达上游"——用错误文本做判断太脆。
#[derive(Debug)]
struct HttpStatus(u16);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}

impl std::error::Error for HttpStatus {}

/// 上游回了 200，但响应不是这个管理接口的形状。
///
/// 这类错误与 404 等价：路径后面没有我们要的接口。为了不改动对外错误文本，
/// 它只作为错误链上的一个标记，由 verdict 认领。
#[derive(Debug)]
struct NotThisApi;

impl std::fmt::Display for NotThisApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("响应形状不是这个接口")
    }
}

impl std::error::Error for NotThisApi {}

/// 一次候选探测的裁定。识别动作只允许在 NoMatch 时换下一个候选。
enum Verdict<T> {
    Ok(T),
    /// 明确不是这个接口（HTTP 404/405，或 200 但形状完全不对）。
    NoMatch,
    /// 说不准：网络、TLS、超时、5xx、鉴权失败，或上游返回了自己的错误。
    /// 这种情况必须如实上报，绝不能当成"不是这个站点"而改写来源。
    Inconclusive(anyhow::Error),
}

/// 把一次候选探测的结果归类。
///
/// 401/403 是**有意**归入"说不准"的：它说明路径上确实有这个接口，只是凭据
/// 不被接受。站点类型的判断没有错，错的是凭据，所以要报错而不是换候选。
fn verdict<T>(result: Result<T>) -> Verdict<T> {
    let error = match result {
        Ok(value) => return Verdict::Ok(value),
        Err(error) => error,
    };
    let status = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<HttpStatus>())
        .map(|status| status.0);
    if matches!(status, Some(404 | 405)) || error.chain().any(|cause| cause.is::<NotThisApi>()) {
        return Verdict::NoMatch;
    }
    Verdict::Inconclusive(error)
}

/// 识别站点类型：先 Sub2API（只要一把 Key），不认再试 New API（要额外凭据）。
///
/// 顺序不能反：绝大多数账号都配了 Key 却没配 New API 的访问令牌与用户 ID，
/// 先试 New API 只会多打一次注定失败的请求。没有凭据的候选**直接跳过**，
/// 不制造探测流量。
///
/// 这个函数不写任何配置：识别成功与否都不改账号，调用方拿到结论后再决定。
/// 探测失败按分类如实报错——猜一个来源写进去，只会让接下来的每一轮刷新都打
/// 在错误的接口上。
///
/// **已知限制**：Sub2API 是 Key 级计费，而这里用的 API Key 是账号 Key 池的第一
/// 把（镜像，§4.2.1）。多 Key 账号识别出的来源对全体 Key 成立，倍率则未必。
pub async fn detect(
    client: &UpstreamClient,
    base_url: &str,
    api_key: &str,
    new_api_credentials: Option<(&str, &str)>,
    allow_private: bool,
) -> Result<Detected> {
    // 1) Sub2API：只要一把 Key，先问它。
    match verdict(sub2api(client, base_url, api_key, allow_private).await) {
        Verdict::Ok(_) => return Ok(Detected::Sub2Api),
        // 说不准就**立刻停**：403 说明路径上确实有这个接口，只是凭据不被接受；
        // 网络失败更是与"这个站是什么"无关。继续换候选会把这些故障悄悄伪装成
        // "识别成功"，代价是之后每一轮刷新都打在错误的接口上。
        Verdict::Inconclusive(error) => bail!(
            "识别失败：{}：{}",
            Detected::Sub2Api.probe_label(),
            sanitize_probe_error(&error)
        ),
        Verdict::NoMatch => {}
    }

    // 2) New API：需要额外的访问令牌与用户 ID。没凭据就如实说明"没问过"，
    //    而不是把"没探测"说成"它也不认"。
    let Some((token, user_id)) = new_api_credentials else {
        bail!(
            "识别未能得出结论：{}不认这个站点；{}需要访问令牌与用户 ID 才能探测（可在账号编辑页填写，或在设置页按站点配置一次）",
            Detected::Sub2Api.probe_label(),
            Detected::NewApi.probe_label()
        )
    };
    // 识别时也带上这把 Key：New API 会用 `api_key` 反查它自己绑的分组，
    // 让"识别成功"与"之后每轮刷新的取值口径"保持一致。
    match verdict(
        new_api(
            client,
            base_url,
            token,
            user_id,
            None,
            Some(api_key),
            allow_private,
        )
        .await,
    ) {
        Verdict::Ok(_) => Ok(Detected::NewApi),
        Verdict::Inconclusive(error) => bail!(
            "识别失败：{}：{}",
            Detected::NewApi.probe_label(),
            sanitize_probe_error(&error)
        ),
        // 走到这里说明两个候选都明确回了"没有这个接口"。给管理员两个最可能的
        // 解释，而不是只丢一句"都不认"——地址拼错（比如上游把管理接口放在某个
        // 子路径下）与站点确实不是这两家，处理方式完全不同。
        Verdict::NoMatch => bail!(
            "这个站点既不认{}，也不认{}；如果确认它属于其中之一，请检查 Base URL 的路径部分（探测地址由 Base URL 的站点根加接口路径拼成）",
            Detected::Sub2Api.probe_label(),
            Detected::NewApi.probe_label()
        ),
    }
}

/// 识别路径上的错误文本统一脱敏，避免上游把凭据回显在正文里（§20.2）。
fn sanitize_probe_error(error: &anyhow::Error) -> String {
    let text = crate::security::redact::text(&format!("{error:#}"));
    // 错误链最外层往往只是"探针返回 HTTP 404"，真正的形状错误在里层，
    // 这里整条链一起给出，便于管理员判断到底哪一步不匹配。
    text.trim().chars().take(300).collect()
}

// ------------------------------------------------------------------ Sub2API

/// Sub2API 的 Key 级计费响应（兼容两代现场形状）。
///
/// 现场（2026-09，`sub2api:latest`）返回 `sub2api.key_billing`：
/// `schema_version` / `billing_scope` / `effective_rate_multiplier`，观察时间是
/// RFC3339 字符串。早期文档形状是 `billing` / `version` / `scope` /
/// `effective_multiplier`，观察时间是 Unix 秒。两种都接受；字段缺失或取值非法
/// 时报错，绝不退回默认倍率。
#[derive(Debug, Deserialize)]
struct Sub2ApiBilling {
    object: String,
    #[serde(alias = "schema_version")]
    version: u32,
    #[serde(alias = "billing_scope")]
    scope: String,
    /// 早期形状：有效倍率。
    #[serde(default)]
    effective_multiplier: Option<serde_json::Value>,
    /// 现场形状：有效倍率。
    #[serde(default)]
    effective_rate_multiplier: Option<serde_json::Value>,
    /// 现场形状的备用来源：站点解析后的倍率。
    #[serde(default)]
    resolved_rate_multiplier: Option<serde_json::Value>,
    #[serde(default)]
    observed_at: Option<serde_json::Value>,
    #[serde(default)]
    peak_multiplier: Option<serde_json::Value>,
    #[serde(default)]
    peak_windows: Vec<RawWindow>,
}

impl Sub2ApiBilling {
    /// 有效倍率的取值优先级：早期字段 → 现场字段 → 解析后的倍率。
    fn effective(&self) -> Option<&serde_json::Value> {
        self.effective_multiplier
            .as_ref()
            .or(self.effective_rate_multiplier.as_ref())
            .or(self.resolved_rate_multiplier.as_ref())
    }

    /// 观察时间归一成 Unix 秒（两种现场形状都接受）。
    fn observed_unix(&self) -> Result<Option<i64>> {
        let Some(raw) = self.observed_at.as_ref() else {
            return Ok(None);
        };
        if let Some(seconds) = raw.as_i64() {
            return Ok(Some(seconds));
        }
        if let Some(text) = raw.as_str() {
            let parsed =
                time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                    .with_context(|| format!("Sub2API 计费响应的观察时间无法解析：{text}"))?;
            return Ok(Some(parsed.unix_timestamp()));
        }
        bail!("Sub2API 计费响应的观察时间既不是 Unix 秒也不是 RFC3339 字符串")
    }
}

#[derive(Debug, Deserialize)]
struct RawWindow {
    start: String,
    end: String,
}

/// Sub2API 目前唯一被支持的响应版本。
const SUB2API_VERSION: u32 = 1;

/// 调用 Key 级 `/v1/sub2api/billing`。一把 API Key 即可，不需要额外凭据。
pub async fn sub2api(
    client: &UpstreamClient,
    base_url: &str,
    api_key: &str,
    allow_private: bool,
) -> Result<Reading> {
    let url = probe_url(base_url, "v1/sub2api/billing")?;
    url_guard::assert_resolvable(&url, allow_private).await?;
    let endpoint = url.to_string();

    let response = client
        .http_for(allow_private)
        .get(url)
        .bearer_auth(api_key)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .context("Sub2API 计费接口请求失败")?;
    let body = read_json_body(&endpoint, response).await?;
    let billing: Sub2ApiBilling = serde_json::from_slice(&body).map_err(|error| {
        anyhow::Error::new(error)
            .context(NotThisApi)
            .context("Sub2API 计费响应结构不符合预期")
    })?;

    if billing.object != "billing" && billing.object != "sub2api.key_billing" {
        bail!("Sub2API 计费响应的对象类型非法：{}", billing.object);
    }
    if billing.version != SUB2API_VERSION {
        bail!(
            "Sub2API 计费响应版本 {} 不受支持，只支持 {SUB2API_VERSION}",
            billing.version
        );
    }
    // 只接受 Key 级计费：站点级或用户级的数字回答不了"我这把 Key 多少倍"。
    // 现场把 Key 级写成 `token`，与早期 `key` 是同一层级。
    if billing.scope != "key" && billing.scope != "token" {
        bail!("Sub2API 计费范围不是 Key 级：{}", billing.scope);
    }

    let raw_multiplier = billing
        .effective()
        .context("Sub2API 计费响应缺少有效倍率字段")?;
    let multiplier = to_multiplier(raw_multiplier).context("Sub2API 计费响应中的有效倍率非法")?;
    let observed_at = billing.observed_unix()?;
    let peak = match billing.peak_multiplier {
        Some(raw) if !billing.peak_windows.is_empty() => {
            let value = to_multiplier(&raw).context("Sub2API 计费响应中的峰值倍率非法")?;
            Some(PeakSchedule::new(value, &billing.peak_windows)?)
        }
        _ => None,
    };

    Ok(Reading {
        multiplier,
        observed_at,
        peak,
        // Sub2API 是 Key 级计费，没有"我属于哪个分组"这一层。
        group: None,
        group_source: None,
    })
}

// ------------------------------------------------------------------ New API

/// New API 的分组倍率响应。
#[derive(Debug, Deserialize)]
struct NewApiGroups {
    success: bool,
    #[serde(default)]
    message: String,
    data: Option<std::collections::HashMap<String, serde_json::Value>>,
}

/// 调用 `GET /api/user/self/groups` 取倍率，并**先问清楚这把 Key 属于哪个分组**。
///
/// 必须用分组接口而不是 `/api/pricing` 或 `/api/ratio_config`：后两者返回的是
/// 全站倍率表，回答不了"我这把 Key 属于哪一档"（§11.2）。
///
/// **分组解析顺序**（这是本函数与旧版最大的区别）：
///
/// 1. 账号上显式填的 `group`：管理员说了算，取不到就报错，绝不静默换一个档位；
/// 2. 这把 Key 自己在 New API 上绑定的分组（`/api/token/search`）；
/// 3. 用户的默认分组（`/api/user/self`）；
/// 4. 以上都问不出来，才退回"可用分组的最高档"。
///
/// 前两步都不能省。New API 的 relay 计费按 **令牌分组** 走：令牌绑了分组时它
/// 覆盖用户默认分组；而分组接口返回的倍率矩阵是按 **用户分组** 算的。旧版直接
/// 拿整个矩阵取 `max()`，于是"令牌绑在 ccmax（0.85）"的账号会被算成该用户可用
/// 分组里的最高档——现场症状就是倍率显示 1X 而实际是 0.85x。
pub async fn new_api(
    client: &UpstreamClient,
    base_url: &str,
    access_token: &str,
    user_id: &str,
    group: Option<&str>,
    api_key: Option<&str>,
    allow_private: bool,
) -> Result<Reading> {
    let data = fetch_new_api_groups(client, base_url, access_token, user_id, allow_private).await?;

    // 显式配置优先：管理员填了分组就只认它。
    if let Some(name) = group.filter(|name| !name.trim().is_empty()) {
        let raw = data
            .get(name)
            .with_context(|| format!("New API 上不存在分组「{name}」，或这把 Key 无权使用"))?;
        let multiplier =
            group_ratio(raw).with_context(|| format!("New API 分组「{name}」的倍率非法"))?;
        return Ok(reading(multiplier, name, GroupSource::Configured));
    }

    // 没填分组：问出这把 Key 实际落在哪一档。两步查询都是**尽力而为**——
    // 老版本 New API 可能没有这些路由，站点也可能限流；问不出来就降级，
    // 绝不让一次倍率刷新整体失败（拿不到准确档位 ≠ 拿不到倍率）。
    if let Some((name, source)) = resolve_new_api_group(
        client,
        base_url,
        access_token,
        user_id,
        api_key,
        allow_private,
    )
    .await
    {
        if let Some(Ok(multiplier)) = data.get(&name).map(group_ratio) {
            return Ok(reading(multiplier, &name, source));
        }
        tracing::warn!(
            group = name,
            "New API 上解析出的分组不在可用分组表里，退回按最高档保守估算"
        );
    }

    // 兜底：不知道自己在哪一档时，把成本估高才是安全方向。
    let (name, raw) = data
        .iter()
        .filter_map(|(name, raw)| group_ratio(raw).ok().map(|ratio| (ratio, name, raw)))
        .max_by_key(|(ratio, _, _)| *ratio)
        .map(|(_, name, raw)| (name.clone(), raw))
        .context("New API 返回的分组中没有一个带合法倍率")?;
    let multiplier = group_ratio(raw).context("New API 最高档分组的倍率非法")?;
    Ok(reading(multiplier, &name, GroupSource::HighestFallback))
}

fn reading(multiplier: Multiplier, group: &str, source: GroupSource) -> Reading {
    Reading {
        multiplier,
        observed_at: None,
        peak: None,
        group: Some(group.to_string()),
        group_source: Some(source),
    }
}

/// 问出这把 Key 实际生效的分组：先看令牌自己绑的分组，再看用户的默认分组。
///
/// 两个接口都要"访问令牌 + `New-Api-User`"这一对凭据。任何一步失败都只是
/// **这一次问不出来**，返回 `None` 让调用方走保守兜底，而不是让倍率刷新失败。
///
/// 后台的"预览"动作也走这里：让表单在保存前就能显示"这把 Key 落在哪一档"，
/// 与保存之后每轮刷新用的判定口径完全一致。
pub async fn resolve_group_for_preview(
    client: &UpstreamClient,
    base_url: &str,
    access_token: &str,
    user_id: &str,
    api_key: Option<&str>,
    allow_private: bool,
) -> Result<Option<(String, GroupSource)>> {
    Ok(resolve_new_api_group(
        client,
        base_url,
        access_token,
        user_id,
        api_key,
        allow_private,
    )
    .await)
}

async fn resolve_new_api_group(
    client: &UpstreamClient,
    base_url: &str,
    access_token: &str,
    user_id: &str,
    api_key: Option<&str>,
    allow_private: bool,
) -> Option<(String, GroupSource)> {
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        match new_api_key_group(client, base_url, access_token, user_id, key, allow_private).await {
            Ok(Some(group)) if !group.trim().is_empty() => {
                return Some((group, GroupSource::Token));
            }
            // 令牌没绑分组是**正常状态**：relay 此时用用户分组。
            Ok(_) => {}
            Err(error) => tracing::debug!(
                %error,
                "查不到 New API 令牌分组，改用用户默认分组"
            ),
        }
    }
    match new_api_user_group(client, base_url, access_token, user_id, allow_private).await {
        Ok(Some(group)) if !group.trim().is_empty() => Some((group, GroupSource::User)),
        Ok(_) => None,
        Err(error) => {
            tracing::debug!(%error, "查不到 New API 用户分组，退回按最高档保守估算");
            None
        }
    }
}

/// 一个可选的 New API 分组。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApiGroup {
    pub name: String,
    pub ratio: Multiplier,
    pub description: Option<String>,
}

/// 列出该访问令牌可用的分组与倍率，供后台在下拉框里选择（§6.4）。
///
/// 按倍率升序返回，让最便宜的分组排在最前面。
pub async fn new_api_groups(
    client: &UpstreamClient,
    base_url: &str,
    access_token: &str,
    user_id: &str,
    allow_private: bool,
) -> Result<Vec<NewApiGroup>> {
    let data = fetch_new_api_groups(client, base_url, access_token, user_id, allow_private).await?;
    let mut groups: Vec<NewApiGroup> = data
        .iter()
        .filter_map(|(name, raw)| {
            group_ratio(raw).ok().map(|ratio| NewApiGroup {
                name: name.clone(),
                ratio,
                description: raw
                    .get("desc")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect();
    groups.sort_by(|left, right| {
        left.ratio
            .cmp(&right.ratio)
            .then_with(|| left.name.cmp(&right.name))
    });
    if groups.is_empty() {
        bail!("New API 返回的分组中没有一个带合法倍率");
    }
    Ok(groups)
}

/// 拉取并校验 `/api/user/self/groups` 的原始分组表。
async fn fetch_new_api_groups(
    client: &UpstreamClient,
    base_url: &str,
    access_token: &str,
    user_id: &str,
    allow_private: bool,
) -> Result<std::collections::HashMap<String, serde_json::Value>> {
    let url = probe_url(base_url, "api/user/self/groups")?;
    url_guard::assert_resolvable(&url, allow_private).await?;
    let endpoint = url.to_string();

    let response = client
        .http_for(allow_private)
        .get(url)
        // New API 的访问令牌直接放在 Authorization 里，不带 Bearer 前缀。
        .header(reqwest::header::AUTHORIZATION, access_token)
        .header("New-Api-User", user_id)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .context("New API 分组接口请求失败")?;
    let body = read_json_body(&endpoint, response).await?;
    let groups: NewApiGroups = serde_json::from_slice(&body).map_err(|error| {
        anyhow::Error::new(error)
            .context(NotThisApi)
            .context("New API 分组响应结构不符合预期")
    })?;

    if !groups.success {
        // 回了一张 success:false 的 JSON：这个站点确实有 New API 的接口，只是
        // 这次没通过鉴权或用户 ID 不对。识别动作必须把它当“说不准”，不能换候选。
        let reason = if groups.message.is_empty() {
            "未提供原因（通常是访问令牌过期或用户 ID 不匹配）".to_string()
        } else {
            groups.message.clone()
        };
        return Err(
            anyhow::Error::new(NotThisApi).context(format!("New API 分组接口返回失败：{reason}"))
        );
    }
    groups
        .data
        .filter(|data| !data.is_empty())
        .context("New API 分组接口没有返回任何可用分组")
}

/// 查这把 Key 在 New API 上**自己绑定的分组**（`GET /api/token/search?token=sk-...`）。
///
/// 这是 relay 计费真正用的分组：令牌绑了分组时它覆盖用户默认分组。返回 `None`
/// 表示令牌存在但没绑分组（正常状态），此时应由用户默认分组接管。
///
/// 为什么用 `search` 而不是列表接口：`GET /api/token/` 只支持分页，无法按 Key
/// 过滤，逐页翻找既慢又会在令牌多时漏掉。`search` 的 `token` 参数正是按 Key 值
/// 精确匹配（服务端自己会剥掉 `sk-` 前缀），且强制限定在本人名下。
///
/// 返回体里 Key 是**掩码**的，但服务端是用完整 Key 匹配的，掩码不影响我们读
/// `group` 字段——它始终是明文。
pub async fn new_api_key_group(
    client: &UpstreamClient,
    base_url: &str,
    access_token: &str,
    user_id: &str,
    api_key: &str,
    allow_private: bool,
) -> Result<Option<String>> {
    let mut url = probe_url(base_url, "api/token/search")?;
    url.query_pairs_mut().append_pair("token", api_key.trim());
    url_guard::assert_resolvable(&url, allow_private).await?;
    // 报错里**只带路径**，不带查询串：`?token=` 后面就是这把 Key 的明文，而
    // 这条错误会经 sanitize 落进 multiplier_snapshot.last_error 并显示在后台。
    // 脱敏不能当唯一防线——它对全角标点结尾的串判不出来（§23.4）。
    let endpoint = probe_url(base_url, "api/token/search")?.to_string();

    let response = new_api_get(client, url, access_token, user_id, allow_private).await?;
    let body = read_json_body(&endpoint, response).await?;
    let payload: NewApiEnvelope = serde_json::from_slice(&body)
        .map_err(|error| anyhow::Error::new(error).context("New API 令牌查询响应结构不符合预期"))?;
    if !payload.success {
        bail!(
            "New API 令牌查询失败：{}",
            payload.reason("访问令牌或用户 ID 可能不正确")
        );
    }

    // 分页信封里的 items；兼容直接把数组放在 data 上的老形状。
    let items = payload
        .data
        .as_ref()
        .and_then(|data| data.get("items"))
        .and_then(serde_json::Value::as_array)
        .or_else(|| payload.data.as_ref().and_then(serde_json::Value::as_array));
    let Some(items) = items else {
        bail!("New API 令牌查询响应里没有令牌列表");
    };

    // 服务端是按 Key 值精确匹配的，所以命中多条只可能是同名 Key 的极端情况；
    // 取第一条即可，并且**只认真正绑了分组的那条**，避免被空 group 抢先。
    let mut seen_without_group = false;
    for item in items {
        match item.get("group").and_then(serde_json::Value::as_str) {
            Some(group) if !group.trim().is_empty() => return Ok(Some(group.to_string())),
            _ => seen_without_group = true,
        }
    }
    if seen_without_group {
        return Ok(None);
    }
    Ok(None)
}

/// 查用户的默认分组（`GET /api/user/self` 的 `data.group`）。
///
/// 令牌没绑分组时，relay 用的就是它。拿不到就返回 `None`，交给保守兜底。
pub async fn new_api_user_group(
    client: &UpstreamClient,
    base_url: &str,
    access_token: &str,
    user_id: &str,
    allow_private: bool,
) -> Result<Option<String>> {
    let url = probe_url(base_url, "api/user/self")?;
    url_guard::assert_resolvable(&url, allow_private).await?;
    let endpoint = url.to_string();

    let response = new_api_get(client, url, access_token, user_id, allow_private).await?;
    let body = read_json_body(&endpoint, response).await?;
    let payload: NewApiEnvelope = serde_json::from_slice(&body)
        .map_err(|error| anyhow::Error::new(error).context("New API 用户信息响应结构不符合预期"))?;
    if !payload.success {
        bail!(
            "New API 用户信息查询失败：{}",
            payload.reason("访问令牌或用户 ID 可能不正确")
        );
    }
    Ok(payload
        .data
        .as_ref()
        .and_then(|data| data.get("group"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|group| !group.trim().is_empty()))
}

/// 发一次带 New API 双凭据的 GET。
///
/// New API 的访问令牌直接放在 `Authorization` 里，**不带 `Bearer` 前缀**；
/// `New-Api-User` 必须与令牌所属用户一致，否则上游直接 401。
async fn new_api_get(
    client: &UpstreamClient,
    url: reqwest::Url,
    access_token: &str,
    user_id: &str,
    allow_private: bool,
) -> Result<reqwest::Response> {
    client
        .http_for(allow_private)
        .get(url)
        .header(reqwest::header::AUTHORIZATION, access_token)
        .header("New-Api-User", user_id)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        // 就地脱敏：reqwest 的错误文本里带完整 URL，而令牌查询的 URL 查询串里
        // 就是 Key 的明文。这里脱一次，调用方无论是落库、回后台还是写日志都不会
        // 泄漏——不能指望每个调用点都记得再过一遍（§23.4）。
        .map_err(|error| anyhow::anyhow!("{}", crate::security::redact::text(&error.to_string())))
        .context("New API 请求失败")
}

/// New API 统一的 `{success, message, data}` 信封。
#[derive(Debug, Deserialize)]
struct NewApiEnvelope {
    success: bool,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

impl NewApiEnvelope {
    fn reason(&self, fallback: &str) -> String {
        if self.message.trim().is_empty() {
            fallback.to_string()
        } else {
            self.message.clone()
        }
    }
}

/// 分组条目既可能是 `{"ratio": 0.5, "desc": "..."}`，也可能直接是数字。
fn group_ratio(raw: &serde_json::Value) -> Result<Multiplier> {
    match raw.get("ratio") {
        Some(value) => to_multiplier(value),
        None => to_multiplier(raw),
    }
}

// ---------------------------------------------------------------- 公共辅助

/// 把 JSON 数值或字符串转成定点倍率，绝不经过浮点比较（§20.3）。
fn to_multiplier(raw: &serde_json::Value) -> Result<Multiplier> {
    let text = match raw {
        serde_json::Value::String(text) => text.trim().to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        other => bail!("倍率必须是数字或字符串，实际是 {other}"),
    };
    // JSON 的浮点字面量可能带指数或超过 6 位小数，先按十进制文本裁剪。
    let normalized = normalize_decimal(&text)?;
    Multiplier::parse(&normalized).map_err(|error| anyhow::anyhow!("{error}"))
}

/// 把任意十进制文本收敛成 `Multiplier::parse` 能接受的形式。
fn normalize_decimal(text: &str) -> Result<String> {
    if text.contains(['e', 'E']) {
        // 指数形式先落到 f64 再定点化。倍率的量级只有 0.01–100，精度足够。
        let value: f64 = text.parse().context("倍率不是合法的十进制数")?;
        if !value.is_finite() || value < 0.0 {
            bail!("倍率必须是非负有限数：{text}");
        }
        return Ok(format!("{value:.6}"));
    }
    match text.split_once('.') {
        Some((int_part, frac)) if frac.len() > 6 => Ok(format!("{int_part}.{}", &frac[..6])),
        _ => Ok(text.to_string()),
    }
}

/// 拼出管理接口的地址：**去掉 Base URL 上的 `/v1` 版本段**，再拼
/// `/api/...` 或 `/v1/sub2api/billing`。
///
/// 与推理端点（[crate::upstream::build_url]）的规则刻意不同。推理端点把
/// `https://host/v1` 读作"已经带上版本段"，所以问模型列表时补的是 `/models`；
/// 而这两条管理接口是**站点级**的：New API 挂在 `/api/...`（不在 `/v1` 之下），
/// sub2api 挂在 `/{base_path}/v1/sub2api/billing`。管理员照面板上的约定把 Base URL
/// 填成 `https://host/v1` 时，照抄推理端点的规则会拼出
/// `https://host/v1/api/user/self/groups`——那是上游**推理网关**在回 404，不是
/// "这个站不认这个接口"，于是识别动作误判成"两种接口都不认"，普通刷新则每一轮
/// 都拿一个 404 回来（现场症状：倍率探测失败：探针返回 HTTP 404）。
///
/// 规则只有一条：末段正好是 `v1` 才剥掉，其余路径原样当作前缀。所以
/// `https://host/proxy` 与 `https://host/api/v1` 都会保留前缀（后者留下 `/api`），
/// 这两种填法本来就少见，而错误文本里带着实际请求的完整地址，猜错一次就能看清。
fn probe_url(base_url: &str, path: &str) -> Result<reqwest::Url> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let mut url = reqwest::Url::parse(trimmed).context("账号 Base URL 非法")?;
    // 查询串与片段不属于路径的一部分，在拼之前就去掉。
    url.set_query(None);
    url.set_fragment(None);
    let root = api_root_path(&url);
    url.set_path(&format!("{root}/{path}"));
    Ok(url)
}

/// 去掉末尾版本段后的站点前缀：形如 `` 或 `/api`，**不带结尾斜杠**。
///
/// 三段判定合在一起才不漏：`/v1` 是纯粹的版本段，剥完就是站点根（空串）；
/// `/api/v1` 剥掉后还剩前缀 `/api`；其余路径（`/proxy`、`/v1beta`）原样保留。
/// 少了最后那次 trim，`/api/v1` 会留下 `/api/`，拼出来就是
/// `/api//api/user/self/groups` 这种上游肯定不认的地址。
fn api_root_path(url: &reqwest::Url) -> String {
    let path = url.path().trim_end_matches('/');
    path.strip_suffix("/v1")
        .unwrap_or(path)
        .trim_end_matches('/')
        .to_string()
}

/// 读取响应体，校验状态码与 Content-Type，并施加大小上限（§23.3）。
///
/// 错误里带上探针地址：404 有两种完全不同的成因——“这个站点没有这个接口”
/// 与“地址拼错了”，而管理员能看到的只有这一行文本。给出实际请求的地址，
/// 才能一眼分辨是上游不认还是 Base URL 填得不对。
async fn read_json_body(endpoint: &str, response: reqwest::Response) -> Result<Vec<u8>> {
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if !status.is_success() {
        // 状态码单独挂一层：识别动作靠它区分“端点不存在”与“这次没到上游”，
        // 用错误文本判断太脆。
        return Err(
            anyhow::Error::new(HttpStatus(status.as_u16())).context(format!(
                "探针返回 HTTP {}（GET {endpoint}）",
                status.as_u16()
            )),
        );
    }
    if !content_type.contains("json") {
        bail!("探针响应的 Content-Type 不是 JSON：{content_type}");
    }

    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("读取探针响应失败")? {
        if body.len() + chunk.len() > MAX_BODY {
            bail!("探针响应超过 {MAX_BODY} 字节上限");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// 识别动作的裁定表。这三行是它与普通刷新之间最大的行为差异：
/// 只有“确实不是这个接口”才允许换下一个候选。
#[cfg(test)]
mod detection_tests {
    use super::*;

    fn error_with_status(code: u16) -> anyhow::Error {
        anyhow::Error::new(HttpStatus(code)).context(format!("探针返回 HTTP {code}"))
    }

    #[test]
    fn a_missing_endpoint_is_a_no_match() {
        for code in [404, 405] {
            match verdict::<()>(Err(error_with_status(code))) {
                Verdict::NoMatch => {}
                _ => panic!("HTTP {code} 应当判定为“不是这个接口”"),
            }
        }
    }

    #[test]
    fn a_wrong_response_shape_is_a_no_match() {
        // 200 但不是这个接口的 JSON：与 404 等价。
        match verdict::<()>(Err(anyhow::Error::new(NotThisApi).context("形状不对"))) {
            Verdict::NoMatch => {}
            _ => panic!("形状不匹配应当判定为“不是这个接口”"),
        }
    }

    #[test]
    fn authentication_and_transport_failures_are_inconclusive() {
        // 403 说明路径上确实有这个接口，只是凭据不被接受：换候选会掩盖真正的问题。
        for error in [
            error_with_status(403),
            error_with_status(401),
            error_with_status(500),
            anyhow::anyhow!("Sub2API 计费接口请求失败：连接被重置"),
        ] {
            match verdict::<()>(Err(error)) {
                Verdict::Inconclusive(_) => {}
                _ => panic!("鉴权与网络失败不能当成“不是这个接口”"),
            }
        }
    }

    #[test]
    fn inconclusive_errors_keep_their_whole_chain() {
        // 识别失败时管理员要能看到“为什么”，最外层那句 HTTP 状态码远远不够。
        let error = anyhow::Error::new(serde_json::Error::io(std::io::Error::other("boom")))
            .context(NotThisApi)
            .context("Sub2API 计费响应结构不符合预期");
        let text = sanitize_probe_error(&error);
        assert!(text.contains("Sub2API 计费响应结构不符合预期"), "{text}");
        assert!(text.contains("形状不是这个接口"), "{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 令牌查询的分页信封里带 group，且**只认真正绑了分组的那条**。
    #[test]
    fn the_token_search_envelope_exposes_the_bound_group() {
        let envelope: NewApiEnvelope = serde_json::from_value(json!({
            "success": true,
            "message": "",
            "data": {"items": [{"id": 7, "group": "ccmax"}]}
        }))
        .unwrap();
        assert!(envelope.success);
        let items = envelope
            .data
            .as_ref()
            .and_then(|data| data.get("items"))
            .and_then(serde_json::Value::as_array)
            .expect("items");
        assert_eq!(items[0]["group"], "ccmax");
    }

    /// 失败信封要能给出原因，便于把 401/限流如实报给管理员。
    #[test]
    fn a_failed_envelope_reports_its_reason() {
        let envelope: NewApiEnvelope = serde_json::from_value(json!({
            "success": false,
            "message": "无权进行此操作，New-Api-User 与登录用户不匹配"
        }))
        .unwrap();
        assert!(!envelope.success);
        assert!(envelope.reason("兜底").contains("New-Api-User"));

        // 没有 message 时用兜底文案，而不是回一句空话。
        let bare: NewApiEnvelope = serde_json::from_value(json!({"success": false})).unwrap();
        assert_eq!(bare.reason("兜底"), "兜底");
    }

    /// 分组来源的文案要能直接进后台提示语。
    #[test]
    fn group_sources_have_stable_wire_names() {
        assert_eq!(GroupSource::Configured.as_str(), "configured");
        assert_eq!(GroupSource::Token.as_str(), "token");
        assert_eq!(GroupSource::User.as_str(), "user");
        assert_eq!(GroupSource::HighestFallback.as_str(), "highest_fallback");
        // 兜底那一档必须自己说清"这是估算"，否则后台会把估高的数当成真值。
        assert!(GroupSource::HighestFallback.label().contains("保守"));
    }

    fn multiplier(raw: &str) -> Multiplier {
        Multiplier::parse(raw).unwrap()
    }

    /// 现场形状（sub2api:latest）：`sub2api.key_billing` + RFC3339 时间。
    #[test]
    fn the_live_sub2api_billing_shape_is_accepted() {
        let billing: Sub2ApiBilling = serde_json::from_value(json!({
            "object": "sub2api.key_billing",
            "schema_version": 1,
            "billing_scope": "token",
            "group_rate_multiplier": 0.16,
            "resolved_rate_multiplier": 0.16,
            "peak_rate_enabled": false,
            "effective_rate_multiplier": 0.16,
            "observed_at": "2026-09-15T17:24:50.924429747Z"
        }))
        .unwrap();
        assert_eq!(billing.version, 1);
        assert_eq!(billing.scope, "token");
        assert_eq!(
            to_multiplier(billing.effective().expect("现场形状的有效倍率")).unwrap(),
            multiplier("0.16")
        );
        let observed = billing.observed_unix().unwrap().expect("观察时间");
        assert_eq!(observed, 1_789_493_090, "RFC3339 必须归一成 Unix 秒");
    }

    /// 早期文档形状继续可用（向后兼容）。
    #[test]
    fn the_legacy_sub2api_billing_shape_still_works() {
        let billing: Sub2ApiBilling = serde_json::from_value(json!({
            "object": "billing",
            "version": 1,
            "scope": "key",
            "effective_multiplier": "0.5",
            "observed_at": 1_700_000_000
        }))
        .unwrap();
        assert_eq!(
            to_multiplier(billing.effective().unwrap()).unwrap(),
            multiplier("0.5")
        );
        assert_eq!(billing.observed_unix().unwrap(), Some(1_700_000_000));
    }

    /// 缺字段时不能悄悄退回默认倍率。
    #[test]
    fn a_billing_response_without_a_multiplier_is_rejected() {
        let billing: Sub2ApiBilling = serde_json::from_value(json!({
            "object": "sub2api.key_billing",
            "schema_version": 1,
            "billing_scope": "token"
        }))
        .unwrap();
        assert!(billing.effective().is_none());
    }

    #[test]
    fn numbers_and_strings_both_become_fixed_point() {
        assert_eq!(to_multiplier(&json!(0.5)).unwrap(), multiplier("0.5"));
        assert_eq!(to_multiplier(&json!("0.35")).unwrap(), multiplier("0.35"));
        assert_eq!(to_multiplier(&json!(1)).unwrap(), Multiplier::ONE);
        // 超过 6 位小数按定点精度截断，而不是整体拒绝。
        assert_eq!(
            to_multiplier(&json!("0.1234567")).unwrap(),
            multiplier("0.123456")
        );
        assert_eq!(to_multiplier(&json!(5e-1)).unwrap(), multiplier("0.5"));
    }

    #[test]
    fn malformed_multipliers_are_rejected_rather_than_defaulted() {
        // 猜一个默认值的方向恰好是亏钱的方向，所以只能报错。
        for raw in [json!("免费"), json!(-1), json!(null), json!({})] {
            assert!(to_multiplier(&raw).is_err(), "{raw} 应当被拒绝");
        }
    }

    #[test]
    fn a_new_api_group_entry_may_be_an_object_or_a_bare_number() {
        assert_eq!(
            group_ratio(&json!({"ratio": 0.5, "desc": "VIP"})).unwrap(),
            multiplier("0.5")
        );
        assert_eq!(group_ratio(&json!(0.25)).unwrap(), multiplier("0.25"));
    }

    #[test]
    fn peak_windows_wrap_around_midnight() {
        let peak = PeakSchedule::new(
            multiplier("1.5"),
            &[RawWindow {
                start: "22:00".into(),
                end: "08:00".into(),
            }],
        )
        .unwrap();

        // 23:00 UTC 与 03:00 UTC 都在峰值内，12:00 UTC 不在。
        assert!(peak.covers(23 * 3600));
        assert!(peak.covers(3 * 3600));
        assert!(!peak.covers(12 * 3600));
    }

    #[test]
    fn a_reading_switches_to_the_peak_multiplier_locally() {
        let reading = Reading {
            multiplier: multiplier("0.5"),
            observed_at: Some(0),
            group: None,
            group_source: None,
            peak: Some(
                PeakSchedule::new(
                    multiplier("1.5"),
                    &[RawWindow {
                        start: "22:00".into(),
                        end: "23:00".into(),
                    }],
                )
                .unwrap(),
            ),
        };
        // 峰值时段内不必等下一轮刷新就切换（§11.3）。
        assert_eq!(reading.current(22 * 3600 + 60), multiplier("1.5"));
        assert_eq!(reading.current(21 * 3600), multiplier("0.5"));
    }

    #[test]
    fn malformed_peak_windows_are_rejected() {
        for (start, end) in [("25:00", "08:00"), ("22:70", "08:00"), ("2200", "0800")] {
            assert!(
                PeakSchedule::new(
                    Multiplier::ONE,
                    &[RawWindow {
                        start: start.into(),
                        end: end.into()
                    }]
                )
                .is_err(),
                "{start}-{end} 应当被拒绝"
            );
        }
    }

    #[test]
    fn probe_urls_respect_an_existing_base_path() {
        assert_eq!(
            probe_url("https://host/proxy/", "v1/sub2api/billing")
                .unwrap()
                .as_str(),
            "https://host/proxy/v1/sub2api/billing"
        );
        assert_eq!(
            probe_url("https://host", "api/user/self/groups")
                .unwrap()
                .as_str(),
            "https://host/api/user/self/groups"
        );
        assert_eq!(
            probe_url("https://host/proxy/", "api/user/self/groups")
                .unwrap()
                .as_str(),
            "https://host/proxy/api/user/self/groups"
        );
    }

    /// 管理接口是站点级的：Base URL 末尾的 `/v1` 属于推理端点，必须剥掉。
    ///
    /// 现场症状就是这条规则缺失：Base URL 填成 `https://ai.hsnb.fun/v1` 的账号，
    /// New API 探针每天都在打 `/v1/api/user/self/groups`，拿回推理网关的 404，
    /// 探针报"倍率探测失败：探针返回 HTTP 404"，而真正该打的是
    /// `/api/user/self/groups`。
    #[test]
    fn a_v1_base_url_is_stripped_for_site_wide_probe_endpoints() {
        assert_eq!(
            probe_url("https://host/v1", "api/user/self/groups")
                .unwrap()
                .as_str(),
            "https://host/api/user/self/groups"
        );
        assert_eq!(
            probe_url("https://host/v1/", "v1/sub2api/billing")
                .unwrap()
                .as_str(),
            "https://host/v1/sub2api/billing"
        );
        // `/api/v1` 去掉版本段后还剩挂载前缀 `/api`（与 `https://host/proxy/`
        // 同一条规则）：前缀是"上游把服务挂在哪里"，而 `api/user/self/groups` 是
        // 应用自身的路由，两者叠加才是真实地址。猜错的代价只是这一次 404，而
        // 错误文本里带着实际请求的完整地址，管理员一眼能看出该把 Base URL 怎么改。
        assert_eq!(
            probe_url("https://host/api/v1", "api/user/self/groups")
                .unwrap()
                .as_str(),
            "https://host/api/api/user/self/groups"
        );
        // 只有末段正好是 `v1` 才剥；别的路径原样保留。
        assert_eq!(
            probe_url("https://host/v1beta", "api/user/self/groups")
                .unwrap()
                .as_str(),
            "https://host/v1beta/api/user/self/groups"
        );
        // 查询串与片段不参与拼接。
        assert_eq!(
            probe_url("https://host/v1?debug=1#x", "v1/sub2api/billing")
                .unwrap()
                .as_str(),
            "https://host/v1/sub2api/billing"
        );
        assert!(probe_url("不是地址", "v1/sub2api/billing").is_err());
    }
}
