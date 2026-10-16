/**
 * 版本与更新面板：点侧边栏顶部的版本号打开（交互参考 sub2api 的首页版本号）。
 *
 * 面板必须回答三个问题，缺一个管理员就只能靠猜：
 * 现在跑的是哪个版本、上游最新是哪个版本、这台机器该怎么升级。
 *
 * **一键升级**是主路径（参考 sub2api 的后台在线升级）：点一下就走完
 * 下载 → 校验 → 替换 → 重启；容器部署走同一条路（替换容器内可写的那份二进制，
 * 退出后由 restart 策略拉起）。
 * 只有走不通的部署（源码构建、Windows、容器但没挂代理）才退回到"给一条
 * 可复制的命令"——让按钮点了没反应是最糟的设计。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, api } from "../lib/api";
import type { UpdateOutcome, UpdateProgress, UpdateStatus } from "../lib/types";
import { Button, CopyButton, Modal, useToast } from "./ui";
import { IconAlert, IconCheck, IconDownload, IconRefresh } from "./Icons";

/** 部署方式的中文名：面板里只出现这一个位置，没必要放进 types.ts。 */
const DEPLOY_LABELS: Record<string, string> = {
  binary: "原生二进制",
  docker: "Docker 容器",
  source: "源码构建",
  windows: "Windows 二进制",
};

export function VersionDialog({
  open,
  onClose,
  status,
  onStatus,
  onUpdated,
}: {
  open: boolean;
  onClose: () => void;
  /** 侧边栏挂载时已经查到的结果，避免打开面板时先闪一屏空白。 */
  status: UpdateStatus | null;
  onStatus: (status: UpdateStatus) => void;
  /** 更新成功后的回调：让控制台重新拉一次数据（版本号会变）。 */
  onUpdated: () => void;
}) {
  const toast = useToast();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [updating, setUpdating] = useState(false);
  /** 一键升级进行中（含服务端执行与随后的重启等待）。 */
  const [upgrading, setUpgrading] = useState(false);
  const [outcome, setOutcome] = useState<UpdateOutcome | null>(null);
  const [restarting, setRestarting] = useState(false);
  /** 轮询用的定时器：关闭面板或卸载时必须清掉，否则后台会一直打接口。 */
  const pollTimer = useRef<number | null>(null);
  // 这次升级的目标版本与升级前的版本：等新进程上线时用它们判断"真的换过来了"。
  // 用 ref 而不是 state：轮询回调里要读到最新值，且改动不该触发重渲染。
  const targetVersion = useRef<string | null>(null);
  const previousVersion = useRef<string | null>(null);

  const check = useCallback(
    async (refresh: boolean) => {
      setLoading(true);
      setError(null);
      try {
        onStatus(await api.updateStatus(refresh));
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "版本检查失败");
      } finally {
        setLoading(false);
      }
    },
    [onStatus],
  );

  useEffect(() => {
    if (!open) return;
    setOutcome(null);
    void check(false);
  }, [open, check]);

  useEffect(
    () => () => {
      if (pollTimer.current !== null) window.clearInterval(pollTimer.current);
    },
    [],
  );

  const update = async () => {
    setUpdating(true);
    try {
      const result = await api.runUpdate();
      setOutcome(result.outcome);
      toast.success("已更新到 v" + result.outcome.to + "，重启服务后生效");
      // 版本号在「设置」里也跟着变，让外层重新拉一次。
      onUpdated();
      void check(true);
    } catch (cause) {
      toast.error(cause instanceof ApiError ? cause.message : "更新失败");
    } finally {
      setUpdating(false);
    }
  };

  /**
   * 是否需要跟踪进度。
   *
   * 不能只看本地的 `upgrading`：它只是这个组件的状态。升级期间关掉面板再打开
   * （或者换个标签页、刷新页面）时它是 false，而服务端其实还在跑——只认本地
   * 状态会让面板显示"正在升级…"却再也不轮询，永远等不到结束。
   *
   * `status` 里那份 progress 就是服务端的真相，两者取或即可。
   */
  const tracking = upgrading || (status?.progress.phase ?? "idle") === "running";

  /**
   * 一键升级：一次点击走完下载 → 校验 → 替换 → 重启。
   *
   * 服务端立刻回执（容器路径要几分钟），随后靠轮询 status.progress 跟踪：
   * `running` 时显示当前步骤，`done` 时说明重启正在进行、开始等新进程上线，
   * `failed` 时把服务端给的原因原样显示。
   */
  const upgrade = async () => {
    setUpgrading(true);
    // **在点击这一刻**记下升级前的版本与目标版本。
    //
    // 不能等到轮询回调里再读 `status.current`：那时新进程可能已经起来、status 已经
    // 是**新**版本了，`live !== previous` 就永远不成立，页面永远等不到刷新。
    previousVersion.current = status?.current ?? null;
    targetVersion.current = status?.latest ?? null;
    try {
      const result = await api.runUpgrade();
      toast.success(result.notice);
      // 立刻拉一次：进度可能已经是 running 了，别让面板空等一个轮询周期。
      void check(true);
    } catch (cause) {
      setUpgrading(false);
      toast.error(cause instanceof ApiError ? cause.message : "升级失败");
    }
  };

  /**
   * 升级进行中时轮询进度；一旦服务端说完成，就进入"等新进程上线"。
   *
   * 依赖里刻意只有 `open` 与 `upgrading`：轮询期间 status 每次刷新都会变，
   * 跟着它重跑会把定时器反复重建，变成事实上打不完的请求。
   */
  useEffect(() => {
    if (!open || !tracking) return;
    // 是否已经看到过服务端在执行。用来识别"进程被换掉了"：新进程的进度是
    // 全新的 idle，只有见过 running 才说明中间发生过替换。
    let sawRunning = false;
    let consecutiveFailures = 0;
    // 目标版本以服务端记下的为准（点击时那份只是兜底）。
    //
    // 刻意**不**在这里更新 `previousVersion`：轮询期间 `status` 可能已经被新进程
    // 刷新成新版本，写进去会让"版本变了"这个判据永远不成立。
    targetVersion.current = progress.to ?? targetVersion.current;
    const timer = window.setInterval(() => {
      void api
        .updateStatus(true)
        .then((next) => {
          consecutiveFailures = 0;
          onStatus(next);
          if (next.progress.phase === "running") {
            sawRunning = true;
            return;
          }
          if (next.progress.phase === "failed") {
            setUpgrading(false);
            toast.error(next.progress.error ?? "升级失败");
            return;
          }
          // 走到这里只有两种可能，**两种都意味着旧进程要走了**：
          //
          // - `done`：服务端已经替换完成、正在（或即将）优雅关闭；
          // - `idle`：我们先前明明看到过 running，现在却是全新的 idle——
          //   说明旧进程已经被**换掉**了（容器路径下 `docker compose up -d`
          //   会重建容器，内存里的进度随之消失）。
          //
          // 容器路径上 `done` 往往根本来不及被前端看到：重建容器会在宿主代理
          // 返回之前就把当前进程杀掉。早期实现只认 `done`，于是这里永远等不到，
          // 面板卡在"升级中…"、页面永不刷新。
          if (next.progress.phase === "done" || sawRunning) {
            setUpgrading(false);
            setRestarting(true);
            // 目标版本取服务端记下的那一个；拿不到就退化为"版本号变了就算好"。
            startWaiting(next.progress.to ?? null, next.current ?? null);
          }
        })
        .catch(() => {
          // 服务正在重启时接口会短暂失败。但**不能无限期地吞掉**：旧进程退出后
          // 这里会一直失败下去，而"一直在重启"与"已经重启完"从错误里分不出来。
          // 连续失败两次（约 3 秒）就认定它已经进入重启，交给 startWaiting 去等
          // 新进程上线——那一步本来就会一直轮询 /health/version。
          consecutiveFailures += 1;
          if (consecutiveFailures >= 2) {
            setUpgrading(false);
            setRestarting(true);
            startWaiting(targetVersion.current, previousVersion.current);
          }
        });
    }, 1500);
    return () => window.clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, tracking]);

  /**
   * 等新进程上线：轮询 /health/version，**确认换人了**再刷新页面。
   *
   * 只判断"接口能不能应答"是不够的：升级完成后旧进程还要优雅关闭一段时间（最多
   * 180 秒），这期间它**仍然在正常应答 /health/version，并且报的是旧版本号**。按
   * "能应答就刷新"的话，页面会在旧进程还活着的时候刷新，用户看到的还是旧版本——
   * 明明升级成功了，看起来却像没生效。
   *
   * 判据分两种：
   *
   * - **升级**（知道目标版本）：等版本号变成目标版本；
   * - **单纯重启**（版本号不会变）：等"先失败过一次、再恢复应答"——旧进程退出时
   *   连接必然被拒，那就是换人的信号。只等版本号变化会让这条路径永远等不到。
   */
  const startWaiting = (target: string | null, previous: string | null) => {
    if (pollTimer.current !== null) window.clearInterval(pollTimer.current);
    let waited = 0;
    // 是否观察到过"服务不在线"：单纯重启靠它判断旧进程已经走了。
    let sawOutage = false;
    pollTimer.current = window.setInterval(async () => {
      waited += 1;
      try {
        const response = await fetch("/health/version", { cache: "no-store" });
        if (response.ok) {
          const body = (await response.json().catch(() => null)) as
            | { version?: string }
            | null;
          const live = body?.version ?? null;
          const ready = target
            ? live === target
            : previous
              ? live !== null && live !== previous
              : sawOutage;
          if (ready) {
            if (pollTimer.current !== null) window.clearInterval(pollTimer.current);
            window.location.reload();
            return;
          }
        }
      } catch {
        // 旧进程已经退出、新进程还没起来：这正是"换人了"的证据。
        sawOutage = true;
      }
      // 容器路径要等旧进程优雅关闭（最长 180 秒）再加上新进程启动，给足时间。
      if (waited >= 300 && pollTimer.current !== null) {
        window.clearInterval(pollTimer.current);
        setRestarting(false);
        setUpgrading(false);
        toast.error("等待服务重启超时，请确认服务状态后手工刷新页面");
      }
    }, 1000);
  };

  const restart = async () => {
    setRestarting(true);
    try {
      await api.restartService();
    } catch {
      // 服务端可能在响应发出后立刻开始关闭，这里失败不代表重启没发生。
    }
    // 单纯重启不会换版本号，只能靠"断过一次再恢复"判断换人了。
    startWaiting(null, null);
  };

  const hasUpdate = status?.has_update ?? false;
  /** 服务端报告的一键升级进度（没有 status 时按 idle 处理）。 */
  const progress: UpdateProgress = status?.progress ?? {
    phase: "idle",
    to: null,
    step: null,
    error: null,
    started_at: 0,
  };
  /** 一键升级正在服务端执行（本地点击后到服务端确认之前也算）。 */
  const busyUpgrade = upgrading || progress.phase === "running";
  /**
   * 面板实际要显示的阶段。**本地状态优先于服务端进度**。
   *
   * 服务端的 progress 会在进程被替换之后变成陈旧值：容器路径下 `docker compose
   * up -d` 重建容器时，旧进程内存里那份 "running" 是最后一次成功拉取的结果，
   * 而新进程根本还没被问到。若直接渲染它，面板会一直停在"正在升级…"，
   * 即使我们早已进入等待新进程的状态（复审时实测到就是这个表现）。
   */
  const phase: UpdateProgress["phase"] =
    progress.phase === "failed"
      ? "failed"
      : restarting
        ? "done"
        : busyUpgrade
          ? "running"
          : progress.phase;
  /** 这次部署能不能一键升级（二进制总是可以；容器要挂了宿主代理）。 */
  const canOneClick = hasUpdate && (status?.can_one_click ?? false);
  const backupNote = outcome?.backup ? "（旧版本备份在 " + outcome.backup + "）" : "";
  /**
   * 已落盘、等重启生效的版本。
   *
   * 除了本次请求的结果，也读服务端状态里的 pending_version：更新期间刷新过
   * 页面、或者请求被浏览器掐断时，进度不会丢——重新打开面板就能接着重启。
   */
  const pendingVersion = outcome?.to ?? status?.pending_version ?? null;

  return (
    <Modal open={open} onClose={onClose} title="版本与更新">
      <div className="version-panel">
        <div className="version-headline">
          <span className="version-number mono">v{status?.current ?? "…"}</span>
          {status?.enabled === false ? (
            <span className="badge badge-neutral">检查已关闭</span>
          ) : status?.error ? (
            <span className="badge badge-warn">未知</span>
          ) : hasUpdate ? (
            <span className="badge badge-warn">有新版本 v{status?.latest}</span>
          ) : status ? (
            <span className="badge badge-success">
              <IconCheck size={12} />
              已是最新
            </span>
          ) : null}
        </div>

        {phase !== "idle" && (
          <div
            className={
              phase === "failed" ? "callout callout-warn" : "callout callout-info"
            }
            role="status"
          >
            {phase === "running" && (
              <span className="spinner spinner-sm" aria-hidden="true" />
            )}
            <span style={{ flex: 1 }}>
              {phase === "running" && (
                <>正在升级到 v{progress.to ?? "…"}：{progress.step ?? "处理中"}…</>
              )}
              {phase === "done" && (
                <>v{progress.to ?? "…"} 已就绪，正在等待新进程上线…</>
              )}
              {phase === "failed" && (
                <>升级到 v{progress.to ?? "…"} 失败：{progress.error ?? "原因未知"}</>
              )}
            </span>
          </div>
        )}

        {pendingVersion && (
          <div className="callout callout-info" role="status">
            <span style={{ flex: 1 }}>
              {outcome ? (
                <>
                  已把 v{outcome.to} 写进 <span className="mono">{outcome.path}</span>
                  {backupNote}。当前进程仍是 v{outcome.from}，重启服务后才生效。
                </>
              ) : (
                <>服务端已经把 v{pendingVersion} 写进磁盘，重启服务后生效。</>
              )}
            </span>
          </div>
        )}

        {error && (
          <div className="callout callout-warn" role="alert">
            <IconAlert size={14} />
            <span style={{ flex: 1 }}>{error}</span>
          </div>
        )}

        {status?.error && !error && (
          <div className="callout callout-warn" role="alert">
            <IconAlert size={14} />
            <span style={{ flex: 1 }}>查询最新版本失败：{status.error}</span>
          </div>
        )}

        {status?.enabled === false && (
          <p className="card-desc">
            服务端已关闭更新检查（AKHUB_UPDATE_DISABLED）。当前版本 v{status.current}。
          </p>
        )}

        {status && status.enabled && !status.error && (
          <>
            <dl className="version-facts">
              <div>
                <dt>当前版本</dt>
                <dd className="mono">v{status.current}</dd>
              </div>
              <div>
                <dt>最新版本</dt>
                <dd className="mono">
                  {status.latest ? "v" + status.latest : "—"}
                  {status.cached && <span className="version-cached">缓存</span>}
                </dd>
              </div>
              <div>
                <dt>部署方式</dt>
                <dd>{DEPLOY_LABELS[status.deploy] ?? status.deploy}</dd>
              </div>
            </dl>

            {status.notes && hasUpdate && (
              <details className="version-notes">
                <summary>Release 说明</summary>
                <pre>{status.notes}</pre>
              </details>
            )}

            {status.update_hint && <p className="card-desc">{status.update_hint}</p>}

            {hasUpdate && status.update_command && (
              <div className="version-command">
                <code>{status.update_command}</code>
                <CopyButton value={status.update_command} label="复制命令" />
              </div>
            )}
          </>
        )}

        <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
          <Button
            size="sm"
            variant="secondary"
            icon={
              loading ? (
                <span className="spinner spinner-sm" aria-hidden="true" />
              ) : (
                <IconRefresh size={14} />
              )
            }
            onClick={() => void check(true)}
            disabled={loading || updating || restarting}
          >
            重新检查
          </Button>
          {canOneClick && !pendingVersion && (
            <Button
              size="sm"
              variant="primary"
              icon={
                busyUpgrade ? (
                  <span className="spinner spinner-sm" aria-hidden="true" />
                ) : (
                  <IconDownload size={14} />
                )
              }
              onClick={() => void upgrade()}
              disabled={busyUpgrade || restarting}
            >
              {busyUpgrade ? "升级中…" : `一键升级到 v${status?.latest}`}
            </Button>
          )}
          {/* 走不通一键升级的原生二进制（理论上不会出现：Binary 恒可一键），
              保留旧的两步入口作为兜底。 */}
          {hasUpdate && status?.can_self_update && !canOneClick && !pendingVersion && (
            <Button
              size="sm"
              variant="secondary"
              icon={
                updating ? (
                  <span className="spinner spinner-sm" aria-hidden="true" />
                ) : (
                  <IconDownload size={14} />
                )
              }
              onClick={() => void update()}
              disabled={updating}
            >
              {updating ? "下载中…" : "只下载更新"}
            </Button>
          )}
          {pendingVersion && status?.can_restart && (
            <Button
              size="sm"
              variant="primary"
              icon={
                restarting ? (
                  <span className="spinner spinner-sm" aria-hidden="true" />
                ) : (
                  <IconRefresh size={14} />
                )
              }
              onClick={() => void restart()}
              disabled={restarting}
            >
              {restarting ? "重启中…" : "重启服务"}
            </Button>
          )}
          {status?.release_url && (
            <a
              className="btn btn-secondary btn-sm"
              href={status.release_url}
              target="_blank"
              rel="noreferrer"
            >
              查看 Release ↗
            </a>
          )}
        </div>

        {pendingVersion && status && !status.can_restart && (
          <p className="card-desc">
            没检测到会自动拉起服务的监督进程，请手工重启：
            <span className="mono">
              {status.deploy === "docker" ? " docker compose up -d" : " systemctl restart akhub"}
            </span>
          </p>
        )}
      </div>
    </Modal>
  );
}
