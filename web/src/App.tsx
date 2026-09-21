import { useCallback, useEffect, useState } from "react";
import { api } from "./lib/api";
import { useData, useNow, useRoute, useTheme } from "./lib/store";
import { ROUTES, ROUTE_META, type Route } from "./routes";
import { Button, ConfirmDialog, Skeleton, ToastProvider, useToast } from "./components/ui";
import { CommandPalette } from "./components/CommandPalette";
import { GlossaryDialog } from "./components/GlossaryDialog";
import { VersionDialog } from "./components/VersionDialog";
import {
  IconBook,
  IconGauge,
  IconKey,
  IconLayers,
  IconList,
  IconLogout,
  IconMenu,
  IconMoon,
  IconRefresh,
  IconRoute,
  IconSearch,
  IconServer,
  IconSettings,
  IconSun,
} from "./components/Icons";
import type { UpdateStatus } from "./lib/types";
import { formatUpdatedAgo } from "./lib/format";
import { Gate } from "./pages/Gate";
import { Overview } from "./pages/Overview";
import { Groups } from "./pages/Groups";
import { Accounts } from "./pages/Accounts";
import { Targets } from "./pages/Targets";
import { Requests } from "./pages/Requests";
import { Cost } from "./pages/Cost";
import { Settings as SettingsPage } from "./pages/SettingsPage";

type Session = "checking" | "gate" | "authenticated";

export default function App() {
  return (
    <ToastProvider>
      <Root />
    </ToastProvider>
  );
}

function Root() {
  const [session, setSession] = useState<Session>("checking");
  const [needsSetup, setNeedsSetup] = useState(false);
  const [masterKeyFromEnv, setMasterKeyFromEnv] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  // 主题必须在这一层生效：登录页也要跟随系统偏好与用户选择，
  // 否则浅色用户每次登录都会先被闪一屏深色。
  const theme = useTheme();

  /** 用 /overview 探测会话：它是最轻的受保护接口。 */
  const probe = useCallback(async () => {
    try {
      const status = await api.setupStatus();
      setNeedsSetup(status.needs_setup);
      setMasterKeyFromEnv(status.master_key_from_env);
      if (status.needs_setup) {
        setSession("gate");
        return;
      }
      await api.overview();
      setSession("authenticated");
    } catch {
      // 无论是会话过期还是服务暂时不可用，都退回登录页由用户重试。
      setSession("gate");
    }
  }, []);

  useEffect(() => {
    void probe();
  }, [probe]);

  const signedOut = useCallback((reason?: string) => {
    setNotice(reason ?? null);
    setSession("gate");
  }, []);

  if (session === "checking") {
    return (
      <div className="gate">
        <div className="gate-card gate-loading">
          <div className="brand-mark" style={{ width: 38, height: 38, fontSize: 18 }}>
            A
          </div>
          <div className="stack" style={{ alignItems: "center", gap: 8 }}>
            <h1 style={{ fontSize: 17 }}>正在连接 Akhub</h1>
            <p className="card-desc" style={{ margin: 0 }}>
              正在读取会话状态，请稍候…
            </p>
            <span className="spinner" aria-hidden="true" />
          </div>
        </div>
      </div>
    );
  }

  if (session === "gate") {
    return (
      <Gate
        needsSetup={needsSetup}
        masterKeyFromEnv={masterKeyFromEnv}
        notice={notice}
      />
    );
  }

  return <Console theme={theme} onSignedOut={signedOut} />;
}

const NAV: { route: Route; icon: React.ReactNode }[] = [
  { route: "overview", icon: <IconGauge /> },
  { route: "groups", icon: <IconKey /> },
  { route: "accounts", icon: <IconServer /> },
  { route: "targets", icon: <IconRoute /> },
  { route: "requests", icon: <IconList /> },
  { route: "cost", icon: <IconLayers /> },
  { route: "settings", icon: <IconSettings /> },
];

const NAV_HINTS: Partial<Record<Route, string>> = {
  overview: "配置状态与近期流量",
  groups: "调度硬边界与下游 Key",
  accounts: "上游凭据、模型命名与限制",
  targets: "按模型查看账号评分与可用性",
  requests: "请求元数据与调度诊断",
  cost: "按逻辑模型查看倍率与用量",
  settings: "系统口径、管理员与备份",
};

function Console({
  theme,
  onSignedOut,
}: {
  theme: ReturnType<typeof useTheme>;
  onSignedOut: (reason?: string) => void;
}) {
  const toast = useToast();
  const { theme: mode, toggle } = theme;
  const [route, navigate, params] = useRoute(ROUTES, "overview");
  // useData 的 refresh 依赖这个回调；必须是稳定引用，否则每次渲染都会触发
  // "拉数据 → setState → 重新渲染 → 新的回调 → 再拉数据"的无限循环。
  const handleUnauthorized = useCallback(
    () => onSignedOut("登录状态已过期，请重新登录。"),
    [onSignedOut],
  );
  const { data, loading, error, truncated, lastUpdated, refresh } = useData(
    true,
    handleUnauthorized,
  );
  const now = useNow(30_000);

  const [menuOpen, setMenuOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [glossaryOpen, setGlossaryOpen] = useState(false);
  const [conflictOpen, setConflictOpen] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  /** 自动刷新间隔（秒）；0 表示关闭。 */
  const [autoRefreshSecs, setAutoRefreshSecs] = useState(0);
  /** 版本与更新面板。 */
  const [versionOpen, setVersionOpen] = useState(false);
  /** 版本检查结果：侧边栏拿它决定版本号上要不要亮"有新版本"的小圆点。 */
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus | null>(null);

  const meta = ROUTE_META[route];

  // 路由变化时更新浏览器标题并收起移动端菜单。
  useEffect(() => {
    document.title = `${meta.title} · Akhub`;
    setMenuOpen(false);
  }, [meta.title]);

  // 多标签页并发编辑：后端返回 config_conflict 时给出明确的"重新加载"路径。
  useEffect(() => {
    const onConflict = () => setConflictOpen(true);
    window.addEventListener("akhub-config-conflict", onConflict);
    return () => window.removeEventListener("akhub-config-conflict", onConflict);
  }, []);

  // 进入控制台时查一次版本。服务端缓存 30 分钟，开销可以忽略；失败也不打扰
  // 用户：面板里会把失败原因原样写出来。
  useEffect(() => {
    let alive = true;
    api
      .updateStatus()
      .then((status) => {
        if (alive) setUpdateStatus(status);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);

  // 全局搜索快捷键。
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((current) => !current);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // 自动刷新：静默执行，失败由顶部错误条承接，不弹 toast 打扰。
  useEffect(() => {
    if (autoRefreshSecs <= 0) return;
    const timer = window.setInterval(() => {
      void refresh();
    }, autoRefreshSecs * 1000);
    return () => window.clearInterval(timer);
  }, [autoRefreshSecs, refresh]);

  const doRefresh = useCallback(async () => {
    if (refreshing) return;
    setRefreshing(true);
    const ok = await refresh();
    setRefreshing(false);
    if (ok) toast.success("数据已更新");
    else toast.error("刷新失败，请检查服务状态后重试");
  }, [refresh, refreshing, toast]);

  const signOut = async () => {
    try {
      await api.logout();
    } finally {
      onSignedOut();
    }
  };

  const counts: Partial<Record<Route, number>> = data
    ? {
        groups: data.groups.length,
        accounts: data.accounts.length,
        targets: data.targets.length,
      }
    : {};

  return (
    <div className={`shell${menuOpen ? " menu-open" : ""}`}>
      {menuOpen && (
        <div
          className="sidebar-scrim"
          aria-hidden="true"
          onClick={() => setMenuOpen(false)}
        />
      )}
      <aside className={`sidebar${menuOpen ? " is-open" : ""}`}>
        <div className="brand">
          <div className="brand-mark">A</div>
          <div>
            <div className="brand-name">Akhub</div>
            <button
              type="button"
              className={`brand-version${updateStatus?.has_update ? " has-update" : ""}`}
              onClick={() => setVersionOpen(true)}
              aria-label="版本与更新"
              title={
                updateStatus?.has_update
                  ? `有新版本 v${updateStatus.latest}，点开查看如何升级`
                  : "版本与更新"
              }
            >
              v{data?.settings.version ?? "…"}
              {updateStatus?.has_update && (
                <span className="brand-version-dot" aria-hidden="true" />
              )}
            </button>
          </div>
          <button
            type="button"
            className="btn btn-ghost btn-icon sidebar-close"
            aria-label="关闭导航"
            onClick={() => setMenuOpen(false)}
          >
            <IconMenu />
          </button>
        </div>

        <nav className="nav" aria-label="主导航">
          <div className="nav-label">控制台</div>
          {NAV.map(({ route: item, icon }) => (
            <button
              key={item}
              className="nav-item"
              aria-current={route === item ? "page" : undefined}
              title={NAV_HINTS[item]}
              onClick={() => navigate(item)}
            >
              {icon}
              {ROUTE_META[item].title}
              {counts[item] !== undefined && (
                <span className="nav-count">{counts[item]}</span>
              )}
            </button>
          ))}
        </nav>

        <div className="sidebar-footer">
          <a
            className="repo-link"
            href="https://ai.hsnb.fun/"
            target="_blank"
            rel="noreferrer"
          >
            <span className="repo-link-mark repo-link-mark-accent" aria-hidden="true">
              ✦
            </span>
            幻境MirageAI
          </a>
          <a
            className="repo-link"
            href="https://github.com/HsMirage/Akhub"
            target="_blank"
            rel="noreferrer"
          >
            <span className="repo-link-mark" aria-hidden="true">↗</span>
            GitHub 仓库
          </a>
          <button className="nav-item" onClick={() => setGlossaryOpen(true)}>
            <IconBook />
            术语表
          </button>
          <button className="nav-item" onClick={toggle}>
            {mode === "dark" ? <IconSun /> : <IconMoon />}
            {mode === "dark" ? "浅色主题" : "深色主题"}
          </button>
          <button className="nav-item" onClick={() => void signOut()}>
            <IconLogout />
            退出登录
          </button>
        </div>
      </aside>

      <main className="main">
        <header className="topbar">
          <button
            type="button"
            className="hamburger mobile-only"
            aria-label="打开导航"
            aria-expanded={menuOpen}
            onClick={() => setMenuOpen(true)}
          >
            <IconMenu />
          </button>
          <div className="topbar-heading">
            <h1 className="topbar-title">{meta.title}</h1>
            <p className="topbar-sub">{meta.subtitle}</p>
          </div>
          <div className="topbar-actions">
            <Button
              size="sm"
              variant="secondary"
              icon={<IconSearch size={14} />}
              onClick={() => setPaletteOpen(true)}
              title="搜索与跳转（Ctrl / ⌘ + K）"
              aria-label="搜索与跳转"
            >
              <span className="hide-sm">搜索</span>
              <kbd className="kbd hide-sm">⌘K</kbd>
            </Button>
            <span className="freshness tabular hide-md" title="数据的最近成功更新时间">
              {formatUpdatedAgo(lastUpdated, now)}
            </span>
            <label className="topbar-refresh-select">
              <span className="hide-sm">自动刷新</span>
              <select
                className="select select-sm"
                value={autoRefreshSecs}
                aria-label="自动刷新间隔"
                onChange={(event) => setAutoRefreshSecs(Number(event.target.value))}
              >
                <option value={0}>关</option>
                <option value={30}>30 秒</option>
                <option value={60}>1 分钟</option>
                <option value={300}>5 分钟</option>
              </select>
            </label>
            <Button
              icon={
                refreshing ? (
                  <span className="spinner spinner-sm" aria-hidden="true" />
                ) : (
                  <IconRefresh size={14} />
                )
              }
              onClick={() => void doRefresh()}
              disabled={refreshing}
              title="刷新数据"
              aria-label="刷新数据"
              size="sm"
            >
              <span className="hide-sm">{refreshing ? "刷新中…" : "刷新"}</span>
            </Button>
          </div>
        </header>

        <div className="content">
          <div className="content-inner">
            {loading && !data && <Skeleton rows={4} />}
            {error && (
              <div className="callout callout-warn" role="alert">
                <span style={{ flex: 1 }}>
                  {data
                    ? "刷新失败，当前展示的是上次成功加载的数据。"
                    : "加载失败。"}
                  {error}
                </span>
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => void doRefresh()}
                  disabled={refreshing}
                >
                  重试
                </Button>
              </div>
            )}
            {/* 列表被服务端截断时必须明说（§7.4）：否则管理员会以为配置里
                就只有这些，而漏掉的恰恰可能是出问题的那一条。

                配置列表在客户端会按服务端单次上限自动翻页取全（api.requestAllPages），
                所以正常情况下这里不会出现；一旦出现就说明**服务端确实没返回完整数据**。
                这时提"请先筛选"是错的——筛选只作用于本地已加载的集合，看不到没取到的那部分，
                该说的是重试与查日志。 */}
            {truncated.length > 0 && (
              <div className="callout callout-warn">
                <span style={{ flex: 1 }}>
                  服务端未能返回完整数据：{truncated.join("、")}（已加载 / 总数）。
                  界面已按服务端上限自动翻页；请点「重新加载」重试，
                  若持续出现请查看服务端日志。
                </span>
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => void doRefresh()}
                  disabled={refreshing}
                >
                  重新加载
                </Button>
              </div>
            )}
            {data && (
              <>
                {route === "overview" && (
                  <Overview data={data} navigate={navigate} />
                )}
                {route === "groups" && <Groups data={data} refresh={refresh} />}
                {route === "accounts" && (
                  <Accounts data={data} refresh={refresh} />
                )}
                {route === "targets" && (
                  <Targets data={data} navigate={navigate} />
                )}
                {route === "requests" && (
                  <Requests data={data} refresh={refresh} initialParams={params} />
                )}
                {route === "cost" && <Cost data={data} navigate={navigate} />}
                {route === "settings" && (
                  <SettingsPage data={data} refresh={refresh} />
                )}
              </>
            )}
          </div>
        </div>
      </main>

      {data && (
        <CommandPalette
          open={paletteOpen}
          onClose={() => setPaletteOpen(false)}
          data={data}
          navigate={navigate}
        />
      )}
      <GlossaryDialog open={glossaryOpen} onClose={() => setGlossaryOpen(false)} />
      <VersionDialog
        open={versionOpen}
        onClose={() => setVersionOpen(false)}
        status={updateStatus}
        onStatus={setUpdateStatus}
        onUpdated={() => void refresh()}
      />
      <ConfirmDialog
        open={conflictOpen}
        title="配置已被其他会话修改"
        confirmLabel="重新加载"
        message="另一个标签页或管理员刚刚修改了配置。继续在当前页面保存可能覆盖对方的修改。请重新加载后重试；重新加载会放弃本页尚未保存的修改。"
        onClose={() => setConflictOpen(false)}
        onConfirm={() => window.location.reload()}
      />
    </div>
  );
}
