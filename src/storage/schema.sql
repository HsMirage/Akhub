-- Akhub 数据库结构。所有语句必须幂等：启动时无条件执行一次。
-- 时间统一使用 Unix 秒（INTEGER），倍率统一使用放大 10^6 倍的定点整数（§20.3）。

CREATE TABLE IF NOT EXISTS admin_users (
    id             TEXT PRIMARY KEY,
    username       TEXT NOT NULL UNIQUE,
    password_hash  TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS app_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS groups (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    key_prefix        TEXT NOT NULL,
    key_digest_hex    TEXT NOT NULL UNIQUE,
    multiplier_limit  INTEGER NOT NULL,
    weight_multiplier INTEGER NOT NULL,
    weight_reliability INTEGER NOT NULL,
    weight_first_token INTEGER NOT NULL,
    weight_throughput INTEGER NOT NULL,
    queue_capacity    INTEGER NOT NULL,
    -- 层内全忙时最多等多久（秒）；0 表示跟随请求总超时（§6.3、§13.6）。
    max_wait_secs     INTEGER NOT NULL DEFAULT 60,
    allow_degrade     INTEGER NOT NULL,
    -- 上游不支持原生后台时，是否允许网关托管后台任务（第二期 §29.1）。默认关。
    allow_managed_background INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS upstream_accounts (
    id                    TEXT PRIMARY KEY,
    group_id              TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    name                  TEXT NOT NULL,
    -- 历史列：早期用来区分官方直连、中转站与“OpenAI 兼容”，但全仓没有一处
    -- 按它分支——端点由协议决定，倍率来源由 multiplier_mode 决定。v12 起不再
    -- 写入也不用，只为兼容不认识新结构的旧二进制而保留（§4.2）。
    upstream_type         TEXT NOT NULL DEFAULT '',
    base_url              TEXT NOT NULL,
    preferred_protocol    TEXT NOT NULL,
    adaptive_protocol     INTEGER NOT NULL,
    default_priority      INTEGER NOT NULL,
    calibration           INTEGER NOT NULL,
    multiplier_mode       TEXT NOT NULL,
    manual_multiplier     INTEGER NOT NULL,
    new_api_user_id       TEXT,
    new_api_group         TEXT,
    limit_rpm             INTEGER,
    limit_tpm             INTEGER,
    limit_concurrency     INTEGER,
    allow_private_network INTEGER NOT NULL,
    enabled               INTEGER NOT NULL,
    -- 模型自动同步开关（§16.2）。打开后忽略选择集，全量托管上游模型。
    auto_sync             INTEGER NOT NULL DEFAULT 0,
    -- 账号级"隐藏原始模型"开关。打开后只暴露设置了"下游模型名"的模型；
    -- 没有设置下游模型名的模型不会对下游开放（§16.4 修订）。
    hide_original         INTEGER NOT NULL DEFAULT 0,
    -- 上一次自动同步完成的时间；下一次同步时刻在此基础上加间隔与抖动。
    model_synced_at       INTEGER,
    created_at            INTEGER NOT NULL,
    UNIQUE (group_id, name)
);

CREATE INDEX IF NOT EXISTS idx_accounts_group ON upstream_accounts(group_id);

-- 加密的上游凭据。与账号一一对应，独立成表以便日志与查询默认不触碰密文。
--
-- `api_key` 同时是 Key 池第一把 Key 的**镜像**：Key 池（见下）是唯一真相，
-- 这里保留一份是为了让还不认识 Key 池的旧二进制、以及旧备份的恢复路径
-- 仍然能读到凭据。写入 Key 池时一并更新它。
CREATE TABLE IF NOT EXISTS upstream_secrets (
    account_id    TEXT PRIMARY KEY REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    api_key       BLOB NOT NULL,
    -- New API 倍率探针的访问令牌，与推理用的 sk-xxx 是两把不同的凭据（§11.2）。
    new_api_token BLOB,
    updated_at    INTEGER NOT NULL,
    -- 老库的单把 Key 是否已经展开进 Key 池（§4.2.1 的迁移）。
    keys_migrated INTEGER NOT NULL DEFAULT 0
);

-- 账号内 Key 池（§4.2.1）。每把 Key 独立密封、独立启用开关与限额覆盖。
-- 凭据级状态（熔断、额度、RPM/并发）按 credential_digest 归类，不按 id：
-- 改标签或换 nonce 重新密封同一把 Key 时，健康状态必须对得上。
CREATE TABLE IF NOT EXISTS upstream_account_keys (
    id                TEXT PRIMARY KEY,
    account_id        TEXT NOT NULL REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    -- 后台展示用的标签。留空时界面按序号显示。
    label             TEXT NOT NULL DEFAULT '',
    sealed_key        BLOB NOT NULL,
    -- 明文凭据经主密钥 pepper 派生的摘要（hex）。绝不用于鉴权，只用于归类状态。
    credential_digest TEXT NOT NULL,
    -- Key 级限额覆盖，逐项盖住账号默认值；NULL 表示继承账号。
    limit_rpm         INTEGER,
    limit_tpm         INTEGER,
    limit_concurrency INTEGER,
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_account_keys ON upstream_account_keys(account_id, created_at);

CREATE TABLE IF NOT EXISTS logical_models (
    id         TEXT PRIMARY KEY,
    group_id   TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    origin     TEXT NOT NULL,
    enabled    INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (group_id, name)
);

CREATE TABLE IF NOT EXISTS dispatch_targets (
    id                TEXT PRIMARY KEY,
    logical_model_id  TEXT NOT NULL REFERENCES logical_models(id) ON DELETE CASCADE,
    account_id        TEXT NOT NULL REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    upstream_model    TEXT NOT NULL,
    -- 对外暴露上游原始名：账号模型管理里的"隐藏原始模型"关掉时为 0。
    -- 同一逻辑模型下的目标各自决定是否暴露自己的上游名（§16.4）。
    hide_original     INTEGER NOT NULL DEFAULT 0,
    -- 历史字段：调度目标不再支持独立优先级覆盖，统一继承账号人工优先级。
    -- 保留列以免影响旧备份恢复；配置装配时忽略它（§9.2 的修订）。
    priority_override INTEGER,
    limit_rpm         INTEGER,
    limit_tpm         INTEGER,
    limit_concurrency INTEGER,
    enabled           INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    UNIQUE (logical_model_id, account_id, upstream_model)
);

CREATE INDEX IF NOT EXISTS idx_targets_model ON dispatch_targets(logical_model_id);

-- 按账号的倍率状态（§11.4）。热路径只读内存，这里是重启后的起点。
CREATE TABLE IF NOT EXISTS multiplier_snapshots (
    account_id   TEXT PRIMARY KEY REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    multiplier   INTEGER NOT NULL,
    source       TEXT NOT NULL,
    status       TEXT NOT NULL,
    -- 上游声明的观察时间；与 refreshed_at 不同，后者是 Akhub 自己的拉取时间。
    observed_at  INTEGER,
    refreshed_at INTEGER NOT NULL,
    -- 进入 multiplier_stale 的时刻，宽限期从这里开始计。
    stale_since  INTEGER,
    last_error   TEXT
);

-- 粘性前缀哈希 → 目标（§10.2、§20.1）。只在首次绑定与换号时变化，量很小。
CREATE TABLE IF NOT EXISTS sticky_bindings (
    sticky_key    TEXT PRIMARY KEY,
    group_id      TEXT NOT NULL,
    logical_model TEXT NOT NULL,
    target_id     TEXT NOT NULL,
    -- 绑定的那把 Key（凭据摘要）。粘性绑定绑的是"目标 + Key"：上游的 prompt
    -- cache 按凭据隔离，只绑目标会在账号内换 Key 时把整份前缀缓存作废
    -- （§4.2.1 的不变量 B）。旧快照里为 NULL，下一次使用即补齐。
    credential_digest TEXT,
    bound_at      INTEGER NOT NULL,
    last_used_at  INTEGER NOT NULL,
    -- 最近一次**真正换过目标**的时刻（不是"最近一次被使用"）。迁移冷却用它，
    -- 所以不能在每次成功重绑时刷新：否则迟滞窗口永远走不完（§10.1 修订）。
    -- 旧快照为 NULL，表示"还没迁移过"，下一次使用即补齐。
    migrated_at   INTEGER
);

CREATE INDEX IF NOT EXISTS idx_sticky_used ON sticky_bindings(last_used_at);

-- 每个目标的性能 EWMA 快照（§9.3、§20.1）。后台任务每 60 秒批量写一次。
CREATE TABLE IF NOT EXISTS target_perf_snapshot (
    target_id      TEXT NOT NULL,
    protocol       TEXT NOT NULL,
    streaming      INTEGER NOT NULL,
    samples        INTEGER NOT NULL,
    success_rate   REAL NOT NULL,
    first_token_ms REAL NOT NULL,
    total_ms       REAL NOT NULL,
    output_tps     REAL NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (target_id, protocol, streaming)
);

-- 分钟级目标性能聚合（§20.1、§22）。后台任务把 request_records 滚进这里，
-- 后台的 /admin/api/metrics 直接读它，不再对明细表做全表扫描。
CREATE TABLE IF NOT EXISTS performance_buckets (
    -- 桶起点（unix 秒，按 bucket_secs 对齐）。
    bucket_start      INTEGER NOT NULL,
    target_id         TEXT NOT NULL,
    protocol          TEXT NOT NULL,
    streaming         INTEGER NOT NULL,
    requests          INTEGER NOT NULL,
    success           INTEGER NOT NULL,
    -- 延迟与吞吐的累加量，除以 requests 得到桶内均值。
    total_ms_sum      INTEGER NOT NULL,
    first_token_sum   INTEGER NOT NULL,
    -- 只有上报了首字的请求才计入，避免把非流式请求的 0 拉进均值。
    first_token_count INTEGER NOT NULL,
    output_tokens_sum INTEGER NOT NULL,
    -- 失败分类计数（§9.3 的 429/5xx/损坏响应维度）。
    rate_limited      INTEGER NOT NULL DEFAULT 0,
    server_errors     INTEGER NOT NULL DEFAULT 0,
    protocol_errors   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_start, target_id, protocol, streaming)
);

CREATE INDEX IF NOT EXISTS idx_buckets_time ON performance_buckets(bucket_start DESC);
CREATE INDEX IF NOT EXISTS idx_buckets_target ON performance_buckets(target_id, bucket_start DESC);

-- 不含正文的请求元数据（§24.1）。
CREATE TABLE IF NOT EXISTS request_records (
    request_id       TEXT PRIMARY KEY,
    started_at       INTEGER NOT NULL,
    duration_ms      INTEGER NOT NULL,
    protocol         TEXT NOT NULL,
    streaming        INTEGER NOT NULL,
    group_id         TEXT,
    logical_model    TEXT,
    target_id        TEXT,
    account_id       TEXT,
    -- 本次请求最终落在账号内的哪把 Key 上（§4.2.1）。只记内部 ID 与标签，
    -- 绝不记凭据本身（§23.4、§24.1）。
    key_id           TEXT,
    key_label        TEXT,
    upstream_model   TEXT,
    request_bytes    INTEGER NOT NULL,
    upstream_status  INTEGER,
    http_status      INTEGER NOT NULL,
    error_code       TEXT,
    -- 实际使用的上游端点，以及为完成请求丢弃的白名单能力（§6.6、§14.8）。
    endpoint         TEXT,
    degraded         TEXT,
    -- 本次实际使用的有效倍率，以及当时同一逻辑模型内最便宜/最贵的合格目标
    -- 倍率。反事实基准只能在请求发生时记录，事后无法重算（§11.6）。
    effective_multiplier INTEGER,
    cheapest_multiplier  INTEGER,
    dearest_multiplier   INTEGER,
    -- 尝试过的目标个数、排队时长与是否命中粘性，用于诊断调度行为。
    attempts         INTEGER NOT NULL,
    queued_ms        INTEGER NOT NULL,
    sticky_hit       INTEGER NOT NULL,
    -- 诊断用的用量与时机：首字延迟、输入/输出 token、当时的配置版本（§6.6）。
    first_token_ms   INTEGER,
    input_tokens     INTEGER,
    output_tokens    INTEGER,
    config_version   INTEGER,
    -- Token 细分（§11.6）：缓存读写与思考 Token。上游没上报就是 NULL，不估算。
    cache_read_tokens  INTEGER,
    cache_write_tokens INTEGER,
    reasoning_tokens   INTEGER,
    -- 粘性等待与普通排队分开计（§6.6、§24.1）：粘性等待是为了保住前缀缓存，
    -- 与临时容量不足的排队是两个不同的成本，混在一个 queued_ms 里无法诊断。
    sticky_wait_ms   INTEGER,
    -- 缓存新鲜度系数（§10.3 的三档），解释这次为什么愿意等/不愿意等。
    sticky_freshness REAL,
    -- 输出速度（token/秒），流式与非流式都算得出（§24.1）。
    output_tps       REAL,
    -- 倍率来源（auto/manual）与本次资格的判定结果（§24.1）。
    multiplier_source TEXT,
    quota_status      TEXT,
    -- 候选过滤原因与最终选中的层（§24.1）。只在诊断时读，不参与调度。
    filter_summary   TEXT,
    selected_layer   INTEGER
);

CREATE INDEX IF NOT EXISTS idx_records_started ON request_records(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_records_group ON request_records(group_id, started_at DESC);

-- 每次上游尝试的明细（§6.6）：换了几个目标、各自用了哪个端点、耗时多久、
-- 为什么失败、这次失败是否计入尝试预算。与请求记录按 request_id 关联。
CREATE TABLE IF NOT EXISTS request_attempts (
    request_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    target_id   TEXT,
    account_id  TEXT,
    -- 这次尝试用哪把 Key。多 Key 之后"换了几个目标"已经不足以解释
    -- "为什么换了三次才通"（§4.2.1、§6.6）。
    key_id      TEXT,
    key_label   TEXT,
    upstream_model TEXT,
    endpoint    TEXT,
    started_at  INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    -- ok / failed / missing_endpoint；error_code 是网关错误码（失败时）。
    outcome     TEXT NOT NULL,
    error_code  TEXT,
    counts_against_budget INTEGER NOT NULL,
    PRIMARY KEY (request_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_attempts_request ON request_attempts(request_id);

-- Responses 状态链（§15.1、§15.2）。网关 ID → 上游 ID 的定位映射是必存的
-- 最小集；可重放正文（加密）只在 store 未显式关闭且保留期大于 0 时保存。
CREATE TABLE IF NOT EXISTS response_states (
    gateway_id     TEXT PRIMARY KEY,
    -- 响应所属分组。查询、删除与引用都必须先过这道门（§26.8）。
    group_id       TEXT NOT NULL,
    logical_model  TEXT NOT NULL,
    account_id     TEXT,
    target_id      TEXT,
    -- 创建这条响应时用的那把 Key（§4.2.1、§15.1）。上游的 resp_xxx 是
    -- 凭据隔离的资源，生命周期代理与链式续写都必须回到同一把 Key。
    key_id         TEXT,
    endpoint       TEXT,
    upstream_id    TEXT,
    -- 加密的可重放正文：输入项、输出项与工具项的 JSON（§15.2）。
    sealed_body    BLOB,
    -- 生成这次响应的入口协议，重建输入时按它解析正文。
    protocol       TEXT,
    created_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_states_expiry ON response_states(expires_at);

-- 账号级模型选择集（§16.2）。主键是上游真名：两个不同的上游名可以经别名
-- 归并到同一个对外名，它们各自占一行，选择后成为同一逻辑模型的两个目标。
CREATE TABLE IF NOT EXISTS account_models (
    account_id   TEXT NOT NULL REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    -- 上游真名（规范化后，保留大小写）。
    upstream_model TEXT NOT NULL,
    -- 对外名：别名应用后的名字，也是逻辑模型名；未设置别名时等于上游真名。
    public_name  TEXT NOT NULL,
    -- 别名设置后是否只暴露对外名（隐藏原始上游名）。
    hide_original INTEGER NOT NULL DEFAULT 0,
    -- 选择集成员。排除过的模型保留记录但 selected = 0（§16.2）。
    selected     INTEGER NOT NULL,
    -- 上游列表里已经消失但仍在选择集内：停止新请求，重新出现自动解除（§16.5）。
    missing      INTEGER NOT NULL DEFAULT 0,
    discovered_at INTEGER NOT NULL,
    PRIMARY KEY (account_id, upstream_model)
);

-- 旧版的独立别名表（§16.4）。v8 起别名就是 account_models.public_name + hide_original，
-- 这里保留建表只为兼容旧备份文件的恢复，运行时不再以它为准。
CREATE TABLE IF NOT EXISTS account_aliases (
    account_id     TEXT NOT NULL REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    upstream_model TEXT NOT NULL,
    public_name    TEXT NOT NULL,
    PRIMARY KEY (account_id, upstream_model)
);

-- 校准助手的对账记录（§6.8）。必须按单个模型对账：总用量对账会随模型组合
-- 漂移，同一个账号连续两个月能算出两个不同的系数。
CREATE TABLE IF NOT EXISTS calibration_records (
    id               TEXT PRIMARY KEY,
    account_id       TEXT NOT NULL REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    logical_model    TEXT NOT NULL,
    -- 对账区间（unix 秒）与该区间本账号在该模型上的用量口径。
    period_start     INTEGER NOT NULL,
    period_end       INTEGER NOT NULL,
    gateway_requests INTEGER NOT NULL,
    -- 站点后台报的扣费倍率或账面值，定点整数；反算出的校准系数。
    reported         TEXT NOT NULL,
    calibration      TEXT NOT NULL,
    created_at       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_calibration_account ON calibration_records(account_id, created_at DESC);

CREATE TABLE IF NOT EXISTS admin_audit_log (
    id         TEXT PRIMARY KEY,
    occurred_at INTEGER NOT NULL,
    actor      TEXT NOT NULL,
    action     TEXT NOT NULL,
    object     TEXT NOT NULL,
    result     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_audit_time ON admin_audit_log(occurred_at DESC);

-- New API 站点级凭据（§6.4、§11.2）：一个 Base URL 只需配一次访问令牌与
-- 用户 ID，该站点下的账号自动继承；账号自己的凭据优先。
CREATE TABLE IF NOT EXISTS new_api_sites (
    base_url    TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL,
    sealed_token BLOB NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- 网关托管的后台任务（计划 §29.1）。状态与执行者都落 SQLite：
-- 进程重启后能明确区分"还在跑""跟着上游走""已中断"，绝不停留在 in_progress。
CREATE TABLE IF NOT EXISTS background_tasks (
    id            TEXT PRIMARY KEY,
    group_id      TEXT NOT NULL,
    logical_model TEXT NOT NULL,
    account_id    TEXT,
    target_id     TEXT,
    -- queued / running / completed / incomplete / failed / cancelled / interrupted
    status        TEXT NOT NULL,
    upstream_id   TEXT,
    created_at    INTEGER NOT NULL,
    -- 任务循环每次推进都会更新；重启时用它判断是否为遗留任务。
    heartbeat_at  INTEGER NOT NULL,
    finished_at   INTEGER,
    error_code    TEXT,
    -- 加密的输出正文（可重放历史），与响应状态链同一套密封格式。
    sealed_output BLOB,
    expires_at    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_background_status ON background_tasks(status, heartbeat_at);
CREATE INDEX IF NOT EXISTS idx_background_expiry ON background_tasks(expires_at);
