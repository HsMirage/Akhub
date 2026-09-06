/**
 * 设置页（§6.7）：系统设置只读展示、版本信息与配置备份恢复。
 *
 * 第一期设置项由启动参数决定，不在后台修改；这里把口径展示出来，避免
 * "为什么和我记的不一样"只能去翻启动命令。高级算法参数不进入后台。
 */
import { useRef, useState } from "react";
import { api } from "../lib/api";
import type { Data } from "../lib/store";
import { formatBytes } from "../lib/format";
import { Button, Card, ConfirmDialog, Field, useToast } from "../components/ui";

export function Settings({ data, refresh }: { data: Data; refresh: () => Promise<void> }) {
  const toast = useToast();
  const settings = data.settings;

  const [exportPassword, setExportPassword] = useState("");
  const [exporting, setExporting] = useState(false);
  const [importPassword, setImportPassword] = useState("");
  const [importFile, setImportFile] = useState<File | null>(null);
  const [importing, setImporting] = useState(false);
  const [confirmImport, setConfirmImport] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);

  const downloadBackup = async () => {
    if (!exportPassword) return;
    setExporting(true);
    try {
      const content = await api.exportBackup(exportPassword);
      const blob = new Blob([content], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = `akhub-backup-${new Date().toISOString().slice(0, 10)}.json`;
      anchor.click();
      URL.revokeObjectURL(url);
      setExportPassword("");
      toast.success("备份已导出，请妥善保管备份口令");
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "导出失败");
    } finally {
      setExporting(false);
    }
  };

  const restoreBackup = async () => {
    if (!importFile || !importPassword) return;
    setImporting(true);
    try {
      const content = await importFile.text();
      const result = await api.importBackup(importPassword, content);
      toast.success(
        `已恢复 ${result.groups} 个分组、${result.accounts} 个账号、${result.logical_models} 个逻辑模型`,
      );
      setImportPassword("");
      setImportFile(null);
      if (fileInput.current) fileInput.current.value = "";
      await refresh();
      setConfirmImport(false);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "恢复失败");
      setConfirmImport(false);
    } finally {
      setImporting(false);
    }
  };

  return (
    <div className="stack" style={{ gap: 16 }}>
      <Card title="系统设置" description="第一期设置项由启动参数决定，不在后台修改。">
        <div className="card-body">
          <div className="table-wrap">
            <table className="data">
              <tbody>
                {(
                  [
                    ["请求总超时", `${settings.request_timeout_secs} 秒`],
                    ["请求体上限", formatBytes(settings.max_request_bytes)],
                    ["请求元数据保留", settings.retention_days === 0 ? "关闭（只保留内存汇总）" : `${settings.retention_days} 天`],
                    ["Responses 状态保留", settings.response_state_days === 0 ? "关闭" : `${settings.response_state_days} 天`],
                    ["自动倍率刷新间隔", `${settings.multiplier_refresh_secs} 秒`],
                    ["模型自动同步间隔", `${Math.round(settings.model_sync_secs / 60)} 分钟（带抖动）`],
                    ["优雅关闭宽限", `${settings.shutdown_grace_secs} 秒`],
                  ] as const
                ).map(([label, value]) => (
                  <tr key={label}>
                    <td className="text-faint" style={{ width: 200 }}>
                      {label}
                    </td>
                    <td className="mono">{value}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      </Card>

      <Card title="版本信息">
        <div className="card-body">
          <div className="table-wrap">
            <table className="data">
              <tbody>
                <tr>
                  <td className="text-faint" style={{ width: 200 }}>Akhub 版本</td>
                  <td className="mono">{settings.version}</td>
                </tr>
                <tr>
                  <td className="text-faint">内置能力目录版本</td>
                  <td className="mono">{settings.capability_catalog_revision}</td>
                </tr>
                <tr>
                  <td className="text-faint">主密钥来源</td>
                  <td>
                    {data.overview.master_key_from_env ? (
                      "环境变量 AKHUB_MASTER_KEY"
                    ) : (
                      "数据目录密钥文件"
                    )}
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </div>
      </Card>

      <Card
        title="配置备份"
        description="备份包含分组、账号（含上游 Key）、模型选择集、别名、逻辑模型与调度目标；不含请求记录与运行日志。备份整体用口令加密。"
      >
        <div className="card-body stack" style={{ gap: 16 }}>
          <div className="form-row-2">
            <Field label="导出：备份口令" hint="口令丢失 = 备份作废。Argon2id 派生密钥加密。">
              {(id) => (
                <div className="row" style={{ gap: 8 }}>
                  <input
                    id={id}
                    className="input"
                    type="password"
                    value={exportPassword}
                    placeholder="设置一个强口令"
                    autoComplete="new-password"
                    onChange={(e) => setExportPassword(e.target.value)}
                  />
                  <Button
                    variant="primary"
                    onClick={() => void downloadBackup()}
                    disabled={exporting || !exportPassword}
                  >
                    {exporting ? "导出中…" : "导出备份"}
                  </Button>
                </div>
              )}
            </Field>
            <Field label="恢复：选择备份文件与口令" hint="恢复会原子替换全部配置；成功后需要刷新页面。">
              {(id) => (
                <div className="row" style={{ gap: 8 }}>
                  <input
                    ref={fileInput}
                    id={id}
                    className="input"
                    type="file"
                    accept="application/json,.json"
                    onChange={(e) => setImportFile(e.target.files?.[0] ?? null)}
                  />
                  <input
                    className="input"
                    style={{ maxWidth: 160 }}
                    type="password"
                    placeholder="备份口令"
                    autoComplete="off"
                    value={importPassword}
                    onChange={(e) => setImportPassword(e.target.value)}
                  />
                  <Button
                    variant="danger"
                    onClick={() => setConfirmImport(true)}
                    disabled={importing || !importFile || !importPassword}
                  >
                    恢复
                  </Button>
                </div>
              )}
            </Field>
          </div>
        </div>
      </Card>

      <ConfirmDialog
        open={confirmImport}
        title="恢复配置备份"
        danger
        confirmLabel={importing ? "恢复中…" : "恢复"}
        message={
          <>
            恢复会<b>整体替换</b>当前全部分组、账号、逻辑模型与调度目标，现有配置不可找回。
            恢复成功后当前页面的其它管理员会话依然有效。
            <br />
            <br />
            确定要用「{importFile?.name}」恢复吗？
          </>
        }
        onClose={() => setConfirmImport(false)}
        onConfirm={() => void restoreBackup()}
      />
    </div>
  );
}
