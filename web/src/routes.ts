/** 路由定义。集中在这里，侧栏与页面切换共用同一份真相。 */
export const ROUTES = [
  "overview",
  "groups",
  "accounts",
  "targets",
  "requests",
  "cost",
  "settings",
] as const;

export type Route = (typeof ROUTES)[number];

export const ROUTE_META: Record<Route, { title: string; subtitle: string }> = {
  overview: { title: "概览", subtitle: "配置状态与近期流量" },
  groups: { title: "分组", subtitle: "调度硬边界，每组一把下游 Key" },
  accounts: { title: "上游账号", subtitle: "凭据、模型命名与限制" },
  targets: { title: "调度视图", subtitle: "按模型查看账号评分、可用性与速度" },
  requests: { title: "请求记录", subtitle: "只含元数据，不含正文" },
  cost: { title: "成本视图", subtitle: "按逻辑模型分组，不做跨模型加总" },
  settings: { title: "设置", subtitle: "系统口径、版本与配置备份" },
};
