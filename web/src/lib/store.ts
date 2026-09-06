/** 全局数据装载：一次拉齐所有资源，任何写操作后统一 refresh。 */
import { useCallback, useEffect, useState } from "react";
import { ApiError, api } from "./api";
import type {
  Account,
  DispatchTarget,
  Group,
  LogicalModel,
  Overview,
  RequestRecord,
  Settings,
} from "./types";

export interface Data {
  overview: Overview;
  settings: Settings;
  groups: Group[];
  accounts: Account[];
  models: LogicalModel[];
  targets: DispatchTarget[];
  requests: RequestRecord[];
}

export interface DataStore {
  data: Data | null;
  loading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
}

/**
 * 资源之间互相引用（目标要显示模型名与账号名），分页与增量同步在这个规模下
 * 只会带来不一致的中间态。整体重取一次，代价是几个 KB 的 JSON。
 */
export function useData(active: boolean, onUnauthorized: () => void): DataStore {
  const [data, setData] = useState<Data | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [overview, settings, groups, accounts, models, targets, requests] =
        await Promise.all([
          api.overview(),
          api.settings(),
          api.groups(),
          api.accounts(),
          api.models(),
          api.targets(),
          api.requests(),
        ]);
      setData({
        overview,
        settings,
        groups,
        accounts,
        models,
        targets,
        requests: requests.data,
      });
      setError(null);
    } catch (cause) {
      if (cause instanceof ApiError && cause.unauthorized) {
        onUnauthorized();
        return;
      }
      setError(cause instanceof Error ? cause.message : "加载失败");
    } finally {
      setLoading(false);
    }
  }, [onUnauthorized]);

  useEffect(() => {
    if (!active) return;
    setLoading(true);
    void refresh();
  }, [active, refresh]);

  return { data, loading, error, refresh };
}

/** 主题：跟随系统，允许手动覆盖并记住选择。 */
export function useTheme() {
  const [theme, setTheme] = useState<"dark" | "light">(() => {
    const stored = localStorage.getItem("akhub-theme");
    if (stored === "dark" || stored === "light") return stored;
    return window.matchMedia("(prefers-color-scheme: light)").matches
      ? "light"
      : "dark";
  });

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem("akhub-theme", theme);
  }, [theme]);

  return {
    theme,
    toggle: () => setTheme((current) => (current === "dark" ? "light" : "dark")),
  };
}

/** 极简 hash 路由：内嵌单页应用不需要服务端配合的 history 模式。 */
export function useRoute<T extends string>(routes: readonly T[], fallback: T) {
  const parse = useCallback((): T => {
    const raw = window.location.hash.replace(/^#\/?/, "");
    return (routes as readonly string[]).includes(raw) ? (raw as T) : fallback;
  }, [routes, fallback]);

  const [route, setRoute] = useState<T>(parse);

  useEffect(() => {
    const onChange = () => setRoute(parse());
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, [parse]);

  const navigate = useCallback((next: T) => {
    window.location.hash = `/${next}`;
  }, []);

  return [route, navigate] as const;
}
