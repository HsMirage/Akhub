import { useCallback, useEffect, useState } from "react";
import { api } from "./lib/api";
import { useData, useRoute, useTheme } from "./lib/store";
import { ROUTES, ROUTE_META, type Route } from "./routes";
import { Button, Skeleton, ToastProvider, useToast } from "./components/ui";
import {
  IconCube,
  IconGauge,
  IconLayers,
  IconKey,
  IconList,
  IconLogout,
  IconMoon,
  IconRefresh,
  IconRoute,
  IconServer,
  IconSun,
} from "./components/Icons";
import { Gate } from "./pages/Gate";
import { Overview } from "./pages/Overview";
import { Groups } from "./pages/Groups";
import { Accounts } from "./pages/Accounts";
import { Models } from "./pages/Models";
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

  if (session === "checking") {
    return <div className="gate" />;
  }

  if (session === "gate") {
    return (
      <Gate
        needsSetup={needsSetup}
        masterKeyFromEnv={masterKeyFromEnv}
        onAuthenticated={() => {
          setNeedsSetup(false);
          setSession("authenticated");
        }}
      />
    );
  }

  return <Console theme={theme} onSignedOut={() => setSession("gate")} />;
}

const NAV: { route: Route; icon: React.ReactNode }[] = [
  { route: "overview", icon: <IconGauge /> },
  { route: "groups", icon: <IconKey /> },
  { route: "accounts", icon: <IconServer /> },
  { route: "models", icon: <IconCube /> },
  { route: "targets", icon: <IconRoute /> },
  { route: "requests", icon: <IconList /> },
  { route: "cost", icon: <IconLayers /> },
  { route: "settings", icon: <IconServer /> },
];

function Console({
  theme,
  onSignedOut,
}: {
  theme: ReturnType<typeof useTheme>;
  onSignedOut: () => void;
}) {
  const toast = useToast();
  const { theme: mode, toggle } = theme;
  const [route, navigate] = useRoute(ROUTES, "overview");
  const { data, loading, error, truncated, refresh } = useData(true, onSignedOut);

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
        models: data.models.length,
        targets: data.targets.length,
        requests: data.requests.length,
      }
    : {};

  const meta = ROUTE_META[route];

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark">A</div>
          <div>
            <div className="brand-name">Akhub</div>
            <div className="brand-version">v{data?.settings.version ?? "…"}</div>
          </div>
        </div>

        <nav className="nav">
          <div className="nav-label">控制台</div>
          {NAV.map(({ route: item, icon }) => (
            <button
              key={item}
              className="nav-item"
              aria-current={route === item ? "page" : undefined}
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
            href="https://github.com/HsMirage/Akhub"
            target="_blank"
            rel="noreferrer"
          >
            <span className="repo-link-mark" aria-hidden="true">↗</span>
            GitHub 仓库
          </a>
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
          <div>
            <h1 className="topbar-title">{meta.title}</h1>
            <p className="topbar-sub">{meta.subtitle}</p>
          </div>
          <div className="topbar-actions">
            <Button
              icon={<IconRefresh />}
              title="刷新数据"
              onClick={() => {
                void refresh();
                toast.success("已刷新");
              }}
            />
          </div>
        </header>

        <div className="content">
          <div className="content-inner">
            {loading && !data && <Skeleton rows={4} />}
            {error && (
              <div className="callout callout-warn" style={{ padding: 16 }}>
                <span>{error}</span>
              </div>
            )}
            {/* 列表被服务端截断时必须明说（§7.4）：否则管理员会以为配置里
                就只有这些，而漏掉的恰恰可能是出问题的那一条。 */}
            {truncated.length > 0 && (
              <div className="callout callout-warn" style={{ padding: 16 }}>
                <span>
                  列表超出单次返回上限，以下内容未完整显示：{truncated.join("、")}。
                  请用筛选条件缩小范围，或直接调用带 limit/offset 的接口。
                </span>
              </div>
            )}
            {data && (
              <>
                {route === "overview" && <Overview data={data} navigate={navigate} />}
                {route === "groups" && <Groups data={data} refresh={refresh} />}
                {route === "accounts" && <Accounts data={data} refresh={refresh} />}
                {route === "models" && <Models data={data} refresh={refresh} />}
                {route === "targets" && <Targets data={data} refresh={refresh} />}
                {route === "requests" && <Requests data={data} refresh={refresh} />}
                {route === "cost" && <Cost data={data} />}
                {route === "settings" && <SettingsPage data={data} refresh={refresh} />}
              </>
            )}
          </div>
        </div>
      </main>
    </div>
  );
}
