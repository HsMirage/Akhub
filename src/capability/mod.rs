//! 模型能力：内置目录、证据学习与查询（§16.6、§16.7）。
//!
//! 三层证据来源，优先级固定（§16.6）：
//!
//! 1. **明确的真实请求结果**——本进程里学到的东西，最高优先级。
//! 2. **上游接口返回**——同上，只是证据来自上游的明确声明。
//! 3. **Akhub 内置适配规则**——由协议层直接判定，不经过这里。
//! 4. **开源能力目录**——随二进制发布的 LiteLLM 精简快照，只提供初始判断。
//!
//! 目录版本随源数据写死；运行中绝不读取远程仓库，也不静默热替换（§16.6）。

use std::collections::HashMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::upstream::evidence::TTL;

/// 随二进制发布的能力目录快照（§16.6）。构建期嵌入，运行期只读。
static CATALOG: &str = include_str!("../../assets/capabilities.json");

/// 内置目录里描述一个模型能力的字段。
///
/// 只保留 Akhub 调度真正用得上的判断：能不能带工具、能不能吐 Schema、
/// 能不能看图、上下文有多长。价格数据从源头就被裁掉，Akhub 不维护价格表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCapability {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub function_calling: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parallel_function_calling: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub response_schema: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vision: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pdf_input: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prompt_caching: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system_messages: bool,
    pub max_input_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

/// 内置目录的查询入口。
pub fn builtin() -> &'static Catalog {
    use std::sync::OnceLock;
    static CATALOG_CACHE: OnceLock<Catalog> = OnceLock::new();
    CATALOG_CACHE.get_or_init(|| {
        serde_json::from_str(CATALOG).expect("内置能力目录损坏：assets/capabilities.json 无法解析")
    })
}

/// 解析后的内置目录。
#[derive(Debug, Clone)]
pub struct Catalog {
    pub revision: String,
    models: HashMap<String, ModelCapability>,
}

impl Catalog {
    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// 精确名查询（§16.6：目录只提供初始能力判断）。
    pub fn get(&self, model: &str) -> Option<&ModelCapability> {
        self.models.get(model)
    }

    /// 目录对这个模型的某项能力怎么说（§16.6）。
    ///
    /// 三个返回值含义严格区分：
    /// - \`Some(true)\`：目录明确说支持；
    /// - \`Some(false)\`：目录明确说不支持；
    /// - \`None\`：目录里没有这个模型，或该项字段没写——**一律按未知处理**。
    ///
    /// 把 \`None\` 当成 \`Some(false)\` 会让目录里没收录的新模型全部被降权，
    /// 那比不接入还糟。
    pub fn supports(&self, model: &str, capability: &str) -> Option<bool> {
        let entry = self.models.get(model)?;
        match capability {
            "function_calling" => Some(entry.function_calling),
            "vision" => Some(entry.vision),
            "response_schema" => Some(entry.response_schema),
            "reasoning" => Some(entry.reasoning),
            // 其余能力目录不表态。capability 词汇表见 DEGRADABLE 与端点规则。
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Catalog {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            revision: String,
            models: HashMap<String, ModelCapability>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            revision: raw.revision,
            models: raw.models,
        })
    }
}

// ---------------------------------------------------------------- 能力学习

/// 一项可以用"上游明确拒绝"来学习的能力（§16.7）。
///
/// 词表分**宽窄两级**。窄能力只描述"本次请求要的那个形状"：上游拒绝
/// `forced_tool_choice` 并不等于整个 `function_calling` 不可用。两者混为
/// 一谈的代价是——一次强制工具选择失败会封掉这个账号模型上**全部**工具调用，
/// 而客户端里"带工具"的请求从此全军覆没（现场事故）。
///
/// 匹配按**由窄到宽**的顺序，先命中先算数；命中的能力还必须真的出现在本次
/// 请求的需求里（[`crate::protocol::canonical::Request::requested_capabilities`]），
/// 否则不记证据——一条归因不到本次请求的拒绝，不能拿来关掉别的请求的通道。
pub struct CapabilitySpec {
    /// 词表里的能力名，与降级标记、硬性资格过滤共用。
    pub name: &'static str,
    /// 上游拒绝该能力时的措辞（小写子串匹配，由窄到宽排列）。
    pub patterns: &'static [&'static str],
    /// 要攒够几次**独立**证据才升级为硬性不合格。
    ///
    /// 单次失败不作数：一次参数写错、一次上游抖动都会留下一条长期封禁，而
    /// 管理员看到的只是一句"模型没有可用的调度目标"。
    pub strikes: u32,
    /// 升级之后的存续时长。窄能力只影响一种请求形状，恢复快、代价小，用短
    /// 周期；宽能力影响整类流量，用长周期。
    pub ttl: std::time::Duration,
}

/// 能力词表。
///
/// 词表**顺序不是判定规则**：归因取"离拒绝措辞最近"的能力，同距时取更长的措辞
/// （见 [`indicated_capability`]）。这里的先后只在两者完全打平时兜底，所以把
/// 窄能力放在前面。中文措辞与 [`REJECTION_HINTS`] 对齐：中转站常回中文，
/// 只认英文会让这一类拒绝被静默丢掉。
const CAPABILITIES: &[CapabilitySpec] = &[
    CapabilitySpec {
        name: "forced_tool_choice",
        patterns: &[
            "forced tool_choice",
            "tool_choice",
            "强制工具选择",
            "工具选择",
        ],
        strikes: 2,
        ttl: std::time::Duration::from_secs(15 * 60),
    },
    CapabilitySpec {
        name: "function_calling",
        patterns: &[
            "function calling",
            "function_calling",
            "function call",
            "tool use",
            "tool call",
            "tools",
            "tool",
            "函数调用",
            "工具调用",
            "工具",
        ],
        strikes: 2,
        ttl: TTL,
    },
    CapabilitySpec {
        name: "vision",
        patterns: &["vision", "image", "pdf", "视觉", "图片", "图像"],
        strikes: 2,
        ttl: TTL,
    },
    CapabilitySpec {
        name: "response_schema",
        patterns: &[
            "structured output",
            "response_format",
            "json schema",
            "schema",
            "结构化输出",
            "响应格式",
        ],
        strikes: 2,
        ttl: TTL,
    },
    CapabilitySpec {
        name: "reasoning",
        patterns: &["thinking", "reasoning", "思考", "推理"],
        strikes: 2,
        ttl: TTL,
    },
];

/// 词表之外的兜底：不敢漏记，但也不给它窄能力的短周期。
static UNKNOWN_SPEC: CapabilitySpec = CapabilitySpec {
    name: "unknown",
    patterns: &[],
    strikes: 1,
    ttl: TTL,
};

/// 查词表。词表外的能力名按兜底规格处理（保留旧行为，不至于漏记）。
fn spec_of(capability: &str) -> &'static CapabilitySpec {
    CAPABILITIES
        .iter()
        .find(|spec| spec.name == capability)
        .unwrap_or(&UNKNOWN_SPEC)
}

/// 证据链的有效窗口：超过这么久没有新证据，之前攒的次数作废。
///
/// 窗口要够长，否则低流量组合永远攒不够证据（每次都按第一次失败算），等于这套
/// 学习机制对它不生效；也要够短，免得昨天的一次孤立抖动被拿来给今天背书。
/// 6 小时：一次真实故障的反复重试一定落在窗口内，跨天的两次偶发不会。
const EVIDENCE_WINDOW: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// 一条能力限制的证据链（§16.7）。
#[derive(Debug, Clone)]
struct Limitation {
    /// 已攒到的独立证据次数。
    strikes: u32,
    /// 最近一次证据的时刻。
    last_seen: std::time::Instant,
    /// 攒够证据、升级为硬性不合格的时刻；还没攒够时为 `None`。
    effective_at: Option<std::time::Instant>,
}

/// 超过这么多条限制才做一次清理。正常规模（几十条）下清理是纯浪费，而它要
/// 持写锁、是 O(n)。
const PRUNE_THRESHOLD: usize = 256;

/// 限制表的键：**账号 × 模型 × 能力**三元组。
///
/// 能力必须进键。早先的版本按"账号 × 模型"存一条，后学到的能力会**覆盖**
/// 先学到的：同一次请求先被证明不支持 vision、再被证明不支持 function_calling，
/// 前一条证据就无声消失了。
type LimitationKey = (String, String, String);

/// 进程内能力证据（§16.7）。
///
/// 只缓存**明确"不支持"**：普通 400、5xx、超时和网络错误不能证明能力不支持，
/// 调用方在记录前必须先判定错误形状。一条限制要攒够词表规定的证据次数才生效，
/// 生效后的存续时长由词表给出；模型列表变化、账号协议变化、适配器版本变化时
/// 由配置重载路径调用 [`Capabilities::clear`] 立即失效。
///
/// 不持久化：重启后第一个请求用一次明确失败重新学会，代价可控，而把它写进
/// 数据库要多一张表和一条恢复路径。
#[derive(Default)]
pub struct Capabilities {
    limitations: RwLock<HashMap<LimitationKey, Limitation>>,
}

/// 后台展示用的一条能力限制（§23.5：屏蔽必须有出口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitationView {
    pub account_id: String,
    pub model: String,
    pub capability: String,
    pub strikes: u32,
    pub required_strikes: u32,
    /// 是否已经生效（生效才算硬性不合格）。
    pub effective: bool,
    /// 生效了多少秒；未生效时为 0。
    pub in_effect_secs: u64,
    /// 还有多少秒过期；未生效时为 0。
    pub expires_in_secs: u64,
}

impl Capabilities {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次"这个账号的这个模型明确不支持这个能力"。
    ///
    /// 同一条证据链内累加次数；攒够 [`CapabilitySpec::strikes`] 次才真正生效。
    /// 已经生效的再命中一次，就把到期时刻往后推——上游还在拒绝，说明证据是新的。
    pub fn note_unsupported(
        &self,
        account_id: &str,
        model: &str,
        capability: &str,
        now: std::time::Instant,
    ) {
        let spec = spec_of(capability);
        let Ok(mut map) = self.limitations.write() else {
            return;
        };
        let key = (
            account_id.to_string(),
            model.to_string(),
            capability.to_string(),
        );
        let entry = map.entry(key).or_insert(Limitation {
            strikes: 0,
            last_seen: now,
            effective_at: None,
        });
        // 上一条限制已经过期：证据链作废，重新数。不复位的话，过期之后**一条**
        // 新证据就能立刻重新封满一个完整周期，与"多次独立证据才生效"自相矛盾。
        if entry.effective_at.is_some_and(|at| now >= at + spec.ttl) {
            entry.strikes = 0;
            entry.effective_at = None;
        }
        // 证据链断得太久，之前攒的次数同样不算数。
        if entry.effective_at.is_none() && now.duration_since(entry.last_seen) > EVIDENCE_WINDOW {
            entry.strikes = 0;
        }
        entry.strikes = entry.strikes.saturating_add(1);
        entry.last_seen = now;
        if entry.effective_at.is_some() || entry.strikes >= spec.strikes {
            entry.effective_at = Some(now);
        }
        // 清理是 O(n) 且要持写锁：只在表明显变大时才做，别让每一次上游拒绝都
        // 去扫一遍整张表。
        if map.len() > PRUNE_THRESHOLD {
            prune(&mut map, now);
        }
    }

    /// 该模型此刻是否已被证实不支持该能力（**生效**的限制才算数）。
    pub fn is_unsupported(
        &self,
        account_id: &str,
        model: &str,
        capability: &str,
        now: std::time::Instant,
    ) -> bool {
        let key = (
            account_id.to_string(),
            model.to_string(),
            capability.to_string(),
        );
        let entry = self
            .limitations
            .read()
            .ok()
            .and_then(|map| map.get(&key).cloned());
        entry.is_some_and(|limitation| limitation.is_effective(spec_of(capability), now))
    }

    /// 一条限制的原始状态：还在攒证据的条目也会返回（供后台展示）。
    pub fn limitation(
        &self,
        account_id: &str,
        model: &str,
        capability: &str,
        now: std::time::Instant,
    ) -> Option<LimitationView> {
        let key = (
            account_id.to_string(),
            model.to_string(),
            capability.to_string(),
        );
        let entry = self.limitations.read().ok()?.get(&key).cloned()?;
        let spec = spec_of(capability);
        let effective = entry.is_effective(spec, now);
        let live = effective
            || (entry.effective_at.is_none()
                && now.duration_since(entry.last_seen) <= EVIDENCE_WINDOW);
        if !live {
            return None;
        }
        Some(LimitationView {
            account_id: account_id.to_string(),
            model: model.to_string(),
            capability: capability.to_string(),
            strikes: entry.strikes,
            required_strikes: spec.strikes,
            effective,
            in_effect_secs: entry
                .effective_at
                .filter(|_| effective)
                .map(|at| now.duration_since(at).as_secs())
                .unwrap_or(0),
            expires_in_secs: entry
                .effective_at
                .filter(|_| effective)
                .map(|at| (at + spec.ttl).saturating_duration_since(now).as_secs())
                .unwrap_or(0),
        })
    }

    /// 当前全部"看得到"的限制（已生效的 + 还在攒证据的），排序稳定。
    pub fn snapshot(&self, now: std::time::Instant) -> Vec<LimitationView> {
        let keys: Vec<LimitationKey> = match self.limitations.read() {
            Ok(map) => map.keys().cloned().collect(),
            Err(_) => return Vec::new(),
        };
        let mut views: Vec<LimitationView> = keys
            .into_iter()
            .filter_map(|(account, model, capability)| {
                self.limitation(&account, &model, &capability, now)
            })
            .collect();
        views.sort_by(|a, b| {
            (&a.account_id, &a.model, &a.capability).cmp(&(&b.account_id, &b.model, &b.capability))
        });
        views
    }

    /// 手动放行：删掉匹配的限制，三个过滤条件都是可选的通配。
    ///
    /// 返回删掉的条数。后台的"清除"按钮走这里——封禁必须有出口（§23.5）。
    pub fn forget(
        &self,
        account_id: Option<&str>,
        model: Option<&str>,
        capability: Option<&str>,
    ) -> usize {
        let Ok(mut map) = self.limitations.write() else {
            return 0;
        };
        let before = map.len();
        map.retain(|(account, entry_model, entry_capability), _| {
            let matches = |want: Option<&str>, have: &String| want.is_none_or(|w| w == have);
            !(matches(account_id, account)
                && matches(model, entry_model)
                && matches(capability, entry_capability))
        });
        before - map.len()
    }

    /// 全部失效。配置变化（模型列表、账号协议、适配器版本）时调用（§16.7）。
    pub fn clear(&self) {
        if let Ok(mut map) = self.limitations.write() {
            map.clear();
        }
    }

    /// 当前**已生效**的限制条数，供概览页告警用。
    pub fn len(&self, now: std::time::Instant) -> usize {
        self.limitations
            .read()
            .map(|map| {
                map.iter()
                    .filter(|((_, _, capability), entry)| {
                        entry.is_effective(spec_of(capability), now)
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn is_empty(&self, now: std::time::Instant) -> bool {
        self.len(now) == 0
    }
}

impl Limitation {
    /// 攒够证据且还在存续期内。
    fn is_effective(&self, spec: &CapabilitySpec, now: std::time::Instant) -> bool {
        self.effective_at.is_some_and(|at| now < at + spec.ttl)
    }
}

/// 清掉永远不可能再被读到的条目，避免长时间运行的进程里无限堆积。
fn prune(map: &mut HashMap<LimitationKey, Limitation>, now: std::time::Instant) {
    map.retain(|(_, _, capability), entry| {
        if entry.is_effective(spec_of(capability), now) {
            return true;
        }
        entry.effective_at.is_none() && now.duration_since(entry.last_seen) <= EVIDENCE_WINDOW
    });
}

/// 上游在拒绝时常用的说法。
const REJECTION_HINTS: &[&str] = &[
    "not support",
    "n't support",
    "no support",
    "unsupported",
    "不支持",
    "无法支持",
];

/// 从错误体里取出**可以拿来归因的文案**。
///
/// 只读错误文案字段（`error.message` / `detail` / `param` ……）里的字符串。
/// 整段 JSON 里同时躺着字段名与类型名，拿它做子串匹配会把 `tool_choice` 这样的
/// **字段名**当成"这个模型不支持工具能力"的证据——现场事故就是这么来的。
///
/// 一个可读字段都没有时才退回整段文本：非标准上游（错误形状各家不同）宁可多看
/// 一眼字段名，也不该把证据整个丢掉；反过来，有 `message` 时只用 `message`。
fn rejection_text(body: &serde_json::Value) -> Option<String> {
    let error = body.get("error").unwrap_or(body);
    let mut parts: Vec<String> = Vec::new();
    for field in ["message", "detail", "details", "reason", "param"] {
        collect_text(error.get(field), &mut parts);
    }
    if parts.is_empty() {
        return Some(body.to_string().to_lowercase());
    }
    Some(parts.join(" ").to_lowercase())
}

/// 从任意嵌套形状里收集字符串（`details` 可能是数组或对象）。
fn collect_text(value: Option<&serde_json::Value>, out: &mut Vec<String>) {
    match value {
        Some(serde_json::Value::String(text)) if !text.trim().is_empty() => out.push(text.clone()),
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                collect_text(Some(item), out);
            }
        }
        Some(serde_json::Value::Object(map)) => {
            for field in ["message", "detail", "reason", "param"] {
                collect_text(map.get(field), out);
            }
        }
        _ => {}
    }
}

/// 一处提及相对"拒绝措辞"的位置，`(类别, 间隔)`，小者优先。
///
/// 类别 0 = 出现在拒绝措辞**之前**——"X 不支持"的语序，X 最可能就是被拒的那个；
/// 类别 1 = 出现在之后——"`does not support X`" 的语序。先比类别再比间隔，
/// 于是 `structured outputs are not supported when tools are provided` 归到
/// structured outputs，而不是离得更近但只是副词的 `tools`。
fn rejection_proximity(text: &str, position: usize, length: usize) -> Option<(u8, usize)> {
    let mut best: Option<(u8, usize)> = None;
    for hint in REJECTION_HINTS {
        let mut from = 0;
        while let Some(found) = text[from..].find(hint) {
            let at = from + found;
            let end = at + hint.len();
            let (candidate, between) = if position + length <= at {
                (
                    (0u8, at - (position + length)),
                    &text[position + length..at],
                )
            } else if position >= end {
                ((1u8, position - end), &text[end..position])
            } else {
                ((0u8, 0), "")
            };
            // 跨句的能力词不算数：`This endpoint does not support streaming. For
            // images, use /v1/images` 拒绝的是 streaming，句号后面的 images 是
            // 另一句话里的东西。
            if between.chars().any(is_clause_break) {
                from = end;
                continue;
            }
            if best.is_none_or(|current| candidate < current) {
                best = Some(candidate);
            }
            from = end;
        }
    }
    best
}

/// 句子/分句的边界：跨过它之后的能力词与前面的拒绝无关。
fn is_clause_break(character: char) -> bool {
    matches!(
        character,
        '.' | '!' | '?' | ';' | '。' | '！' | '？' | '；' | '\n'
    )
}

/// 上游措辞指向的能力（**不考虑本次请求**）。
///
/// 取"离拒绝措辞最近"的能力，而不是词表里的第一个子串：上游说的是
/// `does not support forced tool_choice`，那被拒绝的就是 tool_choice；
/// `structured outputs are not supported when tools are provided` 说的是
/// structured outputs。按顺序找第一个子串会让后一句被裸词 `tool` 吞掉——
/// 那正是现场事故的同一类误伤：一次窄拒绝被记到宽能力上。
///
/// 距离相同时取更长的措辞（"structured output" 胜过 "schema"），再相同才回落到
/// 词表顺序（越窄的排越前）。
fn indicated_capability(status: u16, body: &serde_json::Value) -> Option<&'static str> {
    if status != 400 {
        return None;
    }
    let text = rejection_text(body)?;
    let mut best: Option<(u8, usize, usize, usize, &'static str)> = None;
    for (order, spec) in CAPABILITIES.iter().enumerate() {
        for pattern in spec.patterns {
            let mut from = 0;
            while let Some(found) = text[from..].find(pattern) {
                let position = from + found;
                from = position + pattern.len();
                let Some((category, gap)) = rejection_proximity(&text, position, pattern.len())
                else {
                    continue;
                };
                let better = match best {
                    None => true,
                    Some((best_category, best_gap, best_len, best_order, _)) => {
                        category < best_category
                            || (category == best_category
                                && (gap < best_gap
                                    || (gap == best_gap
                                        && (pattern.len() > best_len
                                            || (pattern.len() == best_len && order < best_order)))))
                    }
                };
                if better {
                    best = Some((category, gap, pattern.len(), order, spec.name));
                }
            }
        }
    }
    best.map(|(_, _, _, _, name)| name)
}

/// 上游错误能否证明"该模型不支持某能力"，并且该能力**确实出现在本次请求的需求里**。
///
/// 只有明确的拒绝才算数：模型名错误、参数错误、限流、5xx、超时与网络错误
/// 全部不能证明。`404` 单独处理——它更可能是"模型不存在"而不是"能力缺失"，
/// 由端点证据与模型目录去管，不在这里缓存。
///
/// 与 [`crate::protocol::canonical::Request::requested_capabilities`] 求交集
/// 是硬要求（§16.7）：上游说"不支持 forced tool_choice"时，本次请求若只用了
/// `tool_choice: auto`，就不能拿它去关掉 `function_calling`——否则一条归因
/// 不到本次请求的拒绝会封掉整类流量。
pub fn unsupported_from_error(
    status: u16,
    body: &serde_json::Value,
    requested: &[&str],
) -> Option<&'static str> {
    let indicated = indicated_capability(status, body)?;
    requested.contains(&indicated).then_some(indicated)
}

/// 上游措辞指向、但**本次请求没有用到**的能力（诊断用）。
///
/// 网关据此打一条日志：证据被丢掉的原因是"归因不到本次请求"，而不是"上游
/// 没有拒绝"。没有这条日志，丢掉证据会变成静默的。
pub fn unrequested_capability(
    status: u16,
    body: &serde_json::Value,
    requested: &[&str],
) -> Option<&'static str> {
    let indicated = indicated_capability(status, body)?;
    (!requested.contains(&indicated)).then_some(indicated)
}

/// 丢掉也不影响客户端工作的能力（§14.8 降级白名单）。
///
/// 请求需要的能力被证实不支持时：白名单内的照样发（最多降级），白名单外的
/// 该目标硬性不合格（§9.1）。
pub const DEGRADABLE: &[&str] = &["reasoning"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_parses_and_carries_no_prices() {
        let catalog = builtin();
        assert!(!catalog.is_empty());
        assert!(!catalog.revision().is_empty());
        let known = catalog.get("gpt-4o").expect("目录里应该有 gpt-4o");
        assert!(known.function_calling);
        assert!(known.vision);
        assert!(known.max_input_tokens.unwrap_or(0) > 100_000);
    }

    #[test]
    fn unknown_models_fall_back_to_none() {
        assert!(builtin().get("不存在的模型").is_none());
        // 目录里没有的模型 -> 能力未知，绝不等于"不支持"（§16.6）。
        assert_eq!(builtin().supports("不存在的模型", "vision"), None);
        // 目录不表态的能力也是未知，而不是不支持。
        assert_eq!(builtin().supports("gpt-4o", "some_exotic_capability"), None);
    }

    #[test]
    fn the_catalog_answers_the_capabilities_it_knows() {
        let catalog = builtin();
        // 只用目录里一定存在的模型断言，避免目录换版后测试变脆。
        assert_eq!(catalog.supports("gpt-4o", "vision"), Some(true));
        assert_eq!(catalog.supports("gpt-4o", "function_calling"), Some(true));
        // gpt-3.5-turbo 在 LiteLLM 目录里不标 vision。
        assert_eq!(
            catalog.supports("gpt-3.5-turbo", "vision"),
            Some(false),
            "目录明确标了不支持就要如实返回 false"
        );
    }

    /// 证据要攒够次数才生效：一次失败不能变成一条封禁。
    #[test]
    fn a_single_failure_is_not_enough() {
        let caps = Capabilities::new();
        let now = std::time::Instant::now();
        caps.note_unsupported("acc", "m1", "vision", now);
        assert!(
            !caps.is_unsupported("acc", "m1", "vision", now),
            "第一次失败只该留下证据，不该立刻封禁"
        );
        // 后台能看到"还在攒"的条目，否则这条证据是隐形的。
        let view = caps
            .limitation("acc", "m1", "vision", now)
            .expect("攒证据的条目也要可见");
        assert_eq!(
            (view.strikes, view.required_strikes, view.effective),
            (1, 2, false)
        );

        caps.note_unsupported("acc", "m1", "vision", now);
        assert!(caps.is_unsupported("acc", "m1", "vision", now));
    }

    /// 证据链断开就重新数：相隔很久的两次失败不构成"两次独立证据"。
    #[test]
    fn strikes_expire_out_of_the_evidence_window() {
        let caps = Capabilities::new();
        let now = std::time::Instant::now();
        caps.note_unsupported("acc", "m1", "vision", now);
        let later = now + EVIDENCE_WINDOW + std::time::Duration::from_secs(1);
        caps.note_unsupported("acc", "m1", "vision", later);
        assert!(
            !caps.is_unsupported("acc", "m1", "vision", later),
            "超过证据窗口的两次失败要重新数"
        );
        caps.note_unsupported("acc", "m1", "vision", later);
        assert!(caps.is_unsupported("acc", "m1", "vision", later));
    }

    /// 现场事故的回归：上游拒绝"强制工具选择"不等于整个 function_calling 不可用。
    #[test]
    fn a_narrow_rejection_never_bans_the_whole_capability() {
        let body = serde_json::json!({
            "error": {
                "message": "claude-opus-5-5 does not support forced tool_choice; use auto or none",
                "type": "invalid_request_error"
            }
        });
        let forced = ["function_calling", "forced_tool_choice"];
        assert_eq!(
            unsupported_from_error(400, &body, &forced),
            Some("forced_tool_choice"),
            "越窄的能力越优先"
        );
        // 只用了 tool_choice: auto 的请求不该被这条措辞连坐。
        assert_eq!(
            unsupported_from_error(400, &body, &["function_calling"]),
            None
        );
        assert_eq!(
            unrequested_capability(400, &body, &["function_calling"]),
            Some("forced_tool_choice")
        );

        let caps = Capabilities::new();
        let now = std::time::Instant::now();
        for _ in 0..2 {
            caps.note_unsupported("acc", "claude-opus-5-5", "forced_tool_choice", now);
        }
        assert!(caps.is_unsupported("acc", "claude-opus-5-5", "forced_tool_choice", now));
        assert!(
            !caps.is_unsupported("acc", "claude-opus-5-5", "function_calling", now),
            "窄封禁不能连坐整类工具调用"
        );
    }

    /// 归因取"最像被拒的那个"提及，而不是词表里的第一个子串。
    ///
    /// 这一条是**通用形态**的防复发：裸词 `tool` 一旦排在最前，会把 structured
    /// outputs / vision / thinking 的拒绝统统记成"工具能力不支持"。
    #[test]
    fn the_mention_closest_to_the_rejection_is_the_one_blamed() {
        let cases: &[(&str, &[&str], &str)] = &[
            (
                "claude-opus-5-5 does not support forced tool_choice; use auto or none",
                &["function_calling", "forced_tool_choice"],
                "forced_tool_choice",
            ),
            (
                "Structured outputs are not supported when tools are provided",
                &["function_calling", "response_schema"],
                "response_schema",
            ),
            (
                "image input is not supported together with tool results",
                &["function_calling", "vision"],
                "vision",
            ),
            (
                "Extended thinking is not supported when tool_choice forces tool use",
                &["function_calling", "forced_tool_choice", "reasoning"],
                "reasoning",
            ),
            (
                "tool_choice is not supported for this model",
                &["function_calling", "forced_tool_choice"],
                "forced_tool_choice",
            ),
            // 中文措辞与 REJECTION_HINTS 对齐：只认英文会让这一类拒绝静默漏记。
            ("该模型不支持图片输入", &["vision"], "vision"),
            (
                "当前分组不支持函数调用",
                &["function_calling"],
                "function_calling",
            ),
            // 没有任何能力词的拒绝：不记证据（一条无法归因的错误不能关掉整类流量）。
            (
                "claude-opus-5-5 requires adaptive thinking; omit thinking or use thinking.type=adaptive",
                &["reasoning"],
                "",
            ),
            // 跨句的能力词不算证据：拒绝的是 streaming，句号后面的 images 是别人的事。
            (
                "This endpoint does not support streaming. For images, use /v1/images",
                &["vision"],
                "",
            ),
        ];
        for (message, requested, expected) in cases {
            let body = serde_json::json!({"error": {"message": message}});
            let got = unsupported_from_error(400, &body, requested);
            if expected.is_empty() {
                assert_eq!(got, None, "{message}");
            } else {
                assert_eq!(got, Some(*expected), "{message}");
            }
        }
    }

    /// 上一条限制过期之后证据链要重新数：不能再靠一条新证据立刻封满一个周期。
    #[test]
    fn evidence_after_an_expired_ban_starts_over() {
        let caps = Capabilities::new();
        let now = std::time::Instant::now();
        for _ in 0..2 {
            caps.note_unsupported("acc", "m", "vision", now);
        }
        assert!(caps.is_unsupported("acc", "m", "vision", now));

        let after = now + TTL + std::time::Duration::from_secs(1);
        assert!(!caps.is_unsupported("acc", "m", "vision", after));
        caps.note_unsupported("acc", "m", "vision", after);
        assert!(
            !caps.is_unsupported("acc", "m", "vision", after),
            "过期之后的一条新证据不该立刻重建 24 小时封禁"
        );
        caps.note_unsupported("acc", "m", "vision", after);
        assert!(caps.is_unsupported("acc", "m", "vision", after));
    }

    /// 窄能力的存续期短于宽能力：恢复窗口不一样。
    #[test]
    fn narrow_capabilities_expire_sooner() {
        let narrow = spec_of("forced_tool_choice");
        assert!(narrow.ttl < spec_of("function_calling").ttl);
        let caps = Capabilities::new();
        let now = std::time::Instant::now();
        for _ in 0..narrow.strikes {
            caps.note_unsupported("acc", "m", "forced_tool_choice", now);
        }
        let inside = now + narrow.ttl - std::time::Duration::from_secs(1);
        assert!(caps.is_unsupported("acc", "m", "forced_tool_choice", inside));
        let outside = now + narrow.ttl + std::time::Duration::from_secs(1);
        assert!(!caps.is_unsupported("acc", "m", "forced_tool_choice", outside));
        assert!(caps.is_empty(outside));
    }

    /// 同一个账号模型上的多项能力互不覆盖。
    #[test]
    fn capabilities_are_stored_per_capability() {
        let caps = Capabilities::new();
        let now = std::time::Instant::now();
        for _ in 0..2 {
            caps.note_unsupported("acc", "m1", "vision", now);
            caps.note_unsupported("acc", "m1", "function_calling", now);
        }
        assert!(
            caps.is_unsupported("acc", "m1", "vision", now),
            "后学到的能力不能覆盖先学到的"
        );
        assert!(caps.is_unsupported("acc", "m1", "function_calling", now));
        assert_eq!(caps.len(now), 2);
    }

    #[test]
    fn limitations_are_scoped_and_clearable() {
        let caps = Capabilities::new();
        let now = std::time::Instant::now();
        for _ in 0..2 {
            caps.note_unsupported("acc", "m1", "vision", now);
        }
        assert!(caps.is_unsupported("acc", "m1", "vision", now));
        assert!(
            !caps.is_unsupported("acc", "m1", "function_calling", now),
            "能力粒度独立"
        );
        assert!(
            !caps.is_unsupported("acc", "m2", "vision", now),
            "模型粒度独立"
        );
        assert!(
            !caps.is_unsupported("acc2", "m1", "vision", now),
            "账号粒度独立"
        );

        // 手动放行：只删匹配的那一条。
        assert_eq!(caps.forget(Some("acc"), None, None), 1);
        assert!(!caps.is_unsupported("acc", "m1", "vision", now));

        for _ in 0..2 {
            caps.note_unsupported("acc", "m1", "vision", now);
        }
        caps.clear();
        assert!(caps.is_empty(now));
    }

    #[test]
    fn snapshots_expose_what_is_blocked_and_what_is_accumulating() {
        let caps = Capabilities::new();
        let now = std::time::Instant::now();
        caps.note_unsupported("acc", "m1", "vision", now);
        caps.note_unsupported("acc", "m1", "vision", now);
        caps.note_unsupported("acc", "m2", "forced_tool_choice", now);

        let snapshot = caps.snapshot(now);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].capability, "vision");
        assert!(snapshot[0].effective);
        assert!(snapshot[0].expires_in_secs > 0);
        assert_eq!(snapshot[1].capability, "forced_tool_choice");
        assert!(!snapshot[1].effective, "只失败一次还在攒证据");

        assert_eq!(caps.forget(None, Some("m1"), None), 1);
        assert_eq!(caps.snapshot(now).len(), 1);
        assert_eq!(caps.forget(None, None, None), 1);
        assert!(caps.snapshot(now).is_empty());
    }

    #[test]
    fn only_explicit_rejections_earn_a_limitation() {
        let unsupported = serde_json::json!({
            "error": {"message": "model does not support images", "type": "invalid_request_error"}
        });
        let asked = ["vision", "function_calling"];
        assert_eq!(
            unsupported_from_error(400, &unsupported, &asked),
            Some("vision")
        );

        // 参数错误、限流、模型不存在、5xx 都不能证明能力不支持（§16.7）。
        let plain =
            serde_json::json!({"error": {"message": "bad", "type": "invalid_request_error"}});
        assert_eq!(unsupported_from_error(400, &plain, &asked), None);
        assert_eq!(unsupported_from_error(404, &unsupported, &asked), None);
        assert_eq!(unsupported_from_error(429, &unsupported, &asked), None);
        assert_eq!(unsupported_from_error(500, &unsupported, &asked), None);

        // 拒绝说得清楚才能归因；说不清楚就不缓存。
        let vague = serde_json::json!({"error": {"message": "请求无法被支持", "type": "invalid_request_error"}});
        assert_eq!(unsupported_from_error(400, &vague, &asked), None);
    }

    /// 上游说"这个模型不支持 X"，而本次请求压根没用到 X：不记证据。
    #[test]
    fn a_capability_the_request_did_not_use_is_never_recorded() {
        let body = serde_json::json!({
            "error": {"message": "this model does not support vision", "type": "invalid_request_error"}
        });
        assert_eq!(
            unsupported_from_error(400, &body, &["function_calling"]),
            None
        );
        assert_eq!(
            unrequested_capability(400, &body, &["function_calling"]),
            Some("vision")
        );
    }

    /// 归因只看错误文案：字段名里出现的能力词不算证据。
    #[test]
    fn only_the_message_is_matched_not_the_whole_body() {
        let body = serde_json::json!({
            "error": {"message": "max_tokens is not supported by this model", "type": "invalid_request_error"},
            "tool_choice": "auto"
        });
        let asked = ["function_calling", "forced_tool_choice", "reasoning"];
        assert_eq!(
            unsupported_from_error(400, &body, &asked),
            None,
            "整段 JSON 里的字段名不能当证据"
        );
    }

    /// 没有标准错误体的上游照旧按整段文本判定（兼容）。
    #[test]
    fn bodies_without_a_message_field_still_fall_back_to_the_whole_text() {
        let body = serde_json::json!({"detail": "unsupported tool use for this model"});
        assert_eq!(
            unsupported_from_error(400, &body, &["function_calling"]),
            Some("function_calling")
        );
    }

    #[test]
    fn the_degrade_whitelist_matches_the_plan() {
        // 思考可以丢（§14.8），工具与 Schema 不行；强制工具选择同样不能静默丢。
        assert!(DEGRADABLE.contains(&"reasoning"));
        for capability in [
            "function_calling",
            "response_schema",
            "vision",
            "forced_tool_choice",
        ] {
            assert!(
                !DEGRADABLE.contains(&capability),
                "{capability} 不在降级白名单里"
            );
        }
    }
}
