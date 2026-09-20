/**
 * 版本与更新面板：点侧边栏顶部的版本号打开（交互参考 sub2api 的首页版本号）。
 *
 * 面板必须回答三个问题，缺一个管理员就只能靠猜：
 * 现在跑的是哪个版本、上游最新是哪个版本、这台机器该怎么升级。
 * 「立即更新」只在原生二进制部署下出现；容器、源码构建与 Windows 给可复制的
 * 命令——让按钮点了没反应是最糟的设计。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, api } from "../lib/api";
import type { UpdateOutcome, UpdateStatus } from "../lib/types";
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
  const [outcome, setOutcome] = useState<UpdateOutcome | null>(null);
  const [restarting, setRestarting] = useState(false);
  /** 轮询用的定时器：关闭面板或卸载时必须清掉，否则后台会一直打接口。 */
  const pollTimer = useRef<number | null>(null);

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

  /** 重启后轮询 /health/version（公开端点）：能应答就刷新页面。 */
  const restart = async () => {
    setRestarting(true);
    try {
      await api.restartService();
    } catch {
      // 服务端可能在响应发出后立刻开始关闭，这里失败不代表重启没发生。
    }
    let waited = 0;
    pollTimer.current = window.setInterval(async () => {
      waited += 1;
      try {
        const response = await fetch("/health/version", { cache: "no-store" });
        if (response.ok) {
          if (pollTimer.current !== null) window.clearInterval(pollTimer.current);
          window.location.reload();
          return;
        }
      } catch {
        // 还没起来，继续等。
      }
      if (waited >= 45 && pollTimer.current !== null) {
        window.clearInterval(pollTimer.current);
        setRestarting(false);
        toast.error("等待服务重启超时，请确认服务状态后手工刷新页面");
      }
    }, 1000);
  };

  const hasUpdate = status?.has_update ?? false;
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
          {hasUpdate && status?.can_self_update && !pendingVersion && (
            <Button
              size="sm"
              variant="primary"
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
              {updating ? "更新中…" : "立即更新"}
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
