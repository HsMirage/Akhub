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
  /**
   * 被服务端截断的列表名（§7.4）。
   *
   * 列表接口有 1000 条上限；超了要明说，否则管理员会以为配置里就只有这些。
   */
  truncated: string[];
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
  const [truncated, setTruncated] = useState<string[]>([]);

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
        groups: groups.data,
        accounts: accounts.data,
        models: models.data,
        targets: targets.data,
        requests: requests.data,
      });
      // 哪个列表被截断了要说出来（§7.4）。
      setTruncated(
        (
          [
            ["分组", groups],
            ["账号", accounts],
            ["逻辑模型", models],
            ["调度目标", targets],
          ] as const
        )
          .filter(([, page]) => page.total > page.data.length)
          .map(([name, page]) => name + "（" + page.data.length + "/" + page.total + "）"),
      );
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

  return { data, loading, error, truncated, refresh };
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
/**
 * 解析 `#/route?key=value` 里的查询串。
 *
 * 从请求记录跳到"调度目标/上游账号"时要带着目标或账号 ID 做高亮定位，
 * 但 hash 路由的主键仍然是 route 本身，所以这里把查询串单独解析出来，
 * 不改变原有的 route 语义。
 */
function parseHash(hash: string): { route: string; params: URLSearchParams } {
  const raw = hash.replace(/^#\/?/, "");
  const [path = "", query = ""] = raw.split("?", 2);
  return { route: path, params: new URLSearchParams(query) };
}

export function useRoute<T extends string>(routes: readonly T[], fallback: T) {
  const parse = useCallback((): { route: T; params: URLSearchParams } => {
    const { route, params } = parseHash(window.location.hash);
    const known = (routes as readonly string[]).includes(route) ? (route as T) : fallback;
    return { route: known, params };
  }, [routes, fallback]);

  const [state, setState] = useState(parse);

  useEffect(() => {
    const onChange = () => setState(parse());
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, [parse]);

  const navigate = useCallback((next: T) => {
    window.location.hash = `/${next}`;
  }, []);

  // 第三个返回值是查询参数，向后兼容：原有的 `const [route, navigate] = ...` 不受影响。
  return [state.route, navigate, state.params] as const;
}

/** 当前 hash 里的查询参数；页面用它读取"带定位目标"的跳转参数。 */
export function useRouteParams(): URLSearchParams {
  const parse = useCallback(() => parseHash(window.location.hash).params, []);
  const [params, setParams] = useState(parse);
  useEffect(() => {
    const onChange = () => setParams(parse());
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, [parse]);
  return params;
}

/** 跳到某个页面并带上查询参数（用于跨页定位）。 */
export function navigateTo(route: string, params: Record<string, string>): void {
  const query = new URLSearchParams(params).toString();
  window.location.hash = query ? `/${route}?${query}` : `/${route}`;
}
