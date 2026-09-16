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
    allow_degrade     INTEGER NOT NULL,
    created_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS upstream_accounts (
    id                    TEXT PRIMARY KEY,
    group_id              TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    name                  TEXT NOT NULL,
    upstream_type         TEXT NOT NULL,
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
    -- 上一次自动同步完成的时间；下一次同步时刻在此基础上加间隔与抖动。
    model_synced_at       INTEGER,
    created_at            INTEGER NOT NULL,
    UNIQUE (group_id, name)
);

CREATE INDEX IF NOT EXISTS idx_accounts_group ON upstream_accounts(group_id);

-- 加密的上游凭据。与账号一一对应，独立成表以便日志与查询默认不触碰密文。
CREATE TABLE IF NOT EXISTS upstream_secrets (
    account_id    TEXT PRIMARY KEY REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    api_key       BLOB NOT NULL,
    -- New API 倍率探针的访问令牌，与推理用的 sk-xxx 是两把不同的凭据（§11.2）。
    new_api_token BLOB,
    updated_at    INTEGER NOT NULL
);

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
    bound_at      INTEGER NOT NULL,
    last_used_at  INTEGER NOT NULL
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
    config_version   INTEGER
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
    -- 对外名：别名应用后的名字，通常也是逻辑模型名。
    public_name  TEXT NOT NULL,
    -- 选择集成员。排除过的模型保留记录但 selected = 0（§16.2）。
    selected     INTEGER NOT NULL,
    -- 上游列表里已经消失但仍在选择集内：停止新请求，重新出现自动解除（§16.5）。
    missing      INTEGER NOT NULL DEFAULT 0,
    discovered_at INTEGER NOT NULL,
    PRIMARY KEY (account_id, upstream_model)
);

-- 账号级模型别名表（§16.4）：上游真名 → 对外名，在勾选对话框之前应用。
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
