/** 全局数据装载：一次拉齐所有资源，任何写操作后统一 refresh。 */
import { useCallback, useEffect, useRef, useState } from "react";
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
  /**
   * 服务端各配置列表的总条数。
   *
   * 列表本身已经翻页取全（见 `requestAllPages`），所以正常情况下
   * `targets.length === totals.targets`。把总数一并留下，是为了让页面能
   * 明确显示"已加载 N / 共 M"，而不是让管理员自己猜这份列表是不是完整的。
   */
  totals: {
    groups: number;
    accounts: number;
    models: number;
    targets: number;
  };
  requests: RequestRecord[];
}

export interface DataStore {
  data: Data | null;
  loading: boolean;
  error: string | null;
  /**
   * 服务端**没能**返回完整的列表名（§7.4）。
   *
   * 配置列表在客户端会按服务端上限自动翻页取全，所以这里通常为空；
   * 一旦非空就说明服务端确实没给全（异常、或某次分页失败了），必须明说，
   * 否则管理员会以为配置里就只有这些。
   */
  truncated: string[];
  /** 最近一次成功刷新的时间（毫秒）；用于展示"更新于 X 分钟前"。 */
  lastUpdated: number | null;
  /** 返回是否成功，便于调用方决定提示成功还是失败。 */
  refresh: () => Promise<boolean>;
}

/**
 * 资源之间互相引用（目标要显示模型名与账号名），分页与增量同步在这个规模下
 * 只会带来不一致的中间态。整体重取一次。
 *
 * 两条约束来自实测（管理端与服务器通常隔着几千公里，单程约一秒）：
 *
 * 1. **同一时刻只允许一次全量刷新。** 一次模型管理里的保存会连着触发好几次
 *    `refresh`，并发发出去只会互相排队、把"更新时间"推来推去。这里用共享的
 *    in-flight Promise 把它们合并掉。
 * 2. **请求记录不进全量刷新。** `/requests` 只服务"请求记录"页面，却是全量
 *    刷新里最贵的一个（服务端要连表取每次尝试明细）。模型管理之类的写操作
 *    根本不需要它；请求记录页自己会在筛选或翻页时按需拉取。
 */
export function useData(active: boolean, onUnauthorized: () => void): DataStore {
  const [data, setData] = useState<Data | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [truncated, setTruncated] = useState<string[]>([]);
  const [lastUpdated, setLastUpdated] = useState<number | null>(null);

  /** 合并并发刷新：同一时刻只跑一次，后到的等同一份结果。 */
  const inFlight = useRef<Promise<boolean> | null>(null);
  const unauthorizedRef = useRef(onUnauthorized);
  unauthorizedRef.current = onUnauthorized;

  const runRefresh = useCallback(async (): Promise<boolean> => {
    try {
      const [overview, settings, groups, accounts, models, targets] = await Promise.all([
        api.overview(),
        api.settings(),
        api.groups(),
        api.accounts(),
        api.models(),
        api.targets(),
      ]);
      setData((current) => ({
        overview,
        settings,
        groups: groups.data,
        accounts: accounts.data,
        models: models.data,
        targets: targets.data,
        totals: {
          groups: groups.total,
          accounts: accounts.total,
          models: models.total,
          targets: targets.total,
        },
        // 请求记录不在这次刷新里：保留上一次的结果，别把页面清空。
        requests: current?.requests ?? [],
      }));
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
      setLastUpdated(Date.now());
      return true;
    } catch (cause) {
      if (cause instanceof ApiError && cause.unauthorized) {
        unauthorizedRef.current();
        return false;
      }
      setError(cause instanceof Error ? cause.message : "加载失败");
      return false;
    } finally {
      setLoading(false);
    }
  }, []);

  const refresh = useCallback((): Promise<boolean> => {
    if (inFlight.current) return inFlight.current;
    const pending = runRefresh().finally(() => {
      inFlight.current = null;
    });
    inFlight.current = pending;
    return pending;
  }, [runRefresh]);

  useEffect(() => {
    if (!active) return;
    setLoading(true);
    void refresh();
  }, [active, refresh]);

  return { data, loading, error, truncated, lastUpdated, refresh };
}

/** 定时重渲染用的当前时间；用于"更新于 X 分钟前"这类相对时间展示。 */
export function useNow(intervalMs = 30_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(timer);
  }, [intervalMs]);
  return now;
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
  // 旧版「逻辑模型」入口已经并入账号的模型管理：保留旧链接可用（§16 修订）。
  const route = path === "models" ? "accounts" : path;
  return { route, params: new URLSearchParams(query) };
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

  const navigate = useCallback((next: T, params?: Record<string, string>) => {
    const query = params && Object.keys(params).length > 0
      ? "?" + new URLSearchParams(params).toString()
      : "";
    window.location.hash = `/${next}${query}`;
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
