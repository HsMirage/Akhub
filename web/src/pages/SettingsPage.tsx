/** 设置页：可编辑系统设置、管理员账号、版本信息与配置备份恢复。 */
import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import type { Data } from "../lib/store";
import type { NewApiSite, Settings, SettingsNumericField, SettingsPatch } from "../lib/types";
import { formatBytes } from "../lib/format";
import { Button, Card, ConfirmDialog, Field, useToast } from "../components/ui";

const SETTING_FIELDS: readonly {
  key: SettingsNumericField;
  label: string;
  unit: string;
}[] = [
  { key: "request_timeout_secs", label: "请求总超时", unit: "秒" },
  { key: "max_request_bytes", label: "请求体上限", unit: "bytes" },
  { key: "retention_days", label: "请求元数据保留", unit: "天" },
  { key: "response_state_days", label: "Responses 状态保留", unit: "天" },
  { key: "shutdown_grace_secs", label: "关闭宽限期", unit: "秒" },
  { key: "multiplier_refresh_secs", label: "自动倍率刷新间隔", unit: "秒" },
  { key: "model_sync_secs", label: "模型同步间隔", unit: "秒" },
];

type SettingsForm = Record<SettingsNumericField, string>;

function settingsToForm(settings: Settings): SettingsForm {
  return {
    request_timeout_secs: String(settings.request_timeout_secs),
    max_request_bytes: String(settings.max_request_bytes),
    retention_days: String(settings.retention_days),
    response_state_days: String(settings.response_state_days),
    shutdown_grace_secs: String(settings.shutdown_grace_secs),
    multiplier_refresh_secs: String(settings.multiplier_refresh_secs),
    model_sync_secs: String(settings.model_sync_secs),
  };
}

function formatSettingValue(key: SettingsNumericField, value: number): string {
  if (key === "max_request_bytes") return `${formatBytes(value)}（${value.toLocaleString()} bytes）`;
  const unit = SETTING_FIELDS.find((field) => field.key === key)?.unit ?? "";
  return `${value.toLocaleString()} ${unit}`;
}

function parseSettingNumber(raw: string): number | null {
  const text = raw.trim();
  if (!text) return null;
  const value = Number(text);
  return Number.isSafeInteger(value) ? value : null;
}

function buildSettingsPatch(form: SettingsForm, settings: Settings): SettingsPatch {
  const values: SettingsPatch = {};
  for (const field of SETTING_FIELDS) {
    const value = parseSettingNumber(form[field.key]);
    if (value !== null && value !== settings[field.key]) values[field.key] = value;
  }
  return values;
}

export function Settings({ data, refresh }: { data: Data; refresh: () => Promise<void> }) {
  const toast = useToast();
  const settings = data.settings;

  const [settingsForm, setSettingsForm] = useState<SettingsForm>(() => settingsToForm(settings));
  const [settingsSaving, setSettingsSaving] = useState(false);
  const [adminUsername, setAdminUsername] = useState("admin");
  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [passwordSaving, setPasswordSaving] = useState(false);

  // 备份恢复成功后父级会重新拉取 settings；只有服务端值变化时才重置表单。
  useEffect(() => {
    setSettingsForm(settingsToForm(settings));
  }, [settings]);

  const settingValidation = useMemo(() => {
    const errors: Partial<Record<SettingsNumericField, string>> = {};
    for (const field of SETTING_FIELDS) {
      const raw = settingsForm[field.key].trim();
      const value = parseSettingNumber(raw);
      const limit = settings.limits[field.key];
      if (!raw || value === null) {
        errors[field.key] = "请输入整数";
      } else if (limit && (value < limit.min || value > limit.max)) {
        errors[field.key] = `必须在 ${formatSettingValue(field.key, limit.min)} 至 ${formatSettingValue(field.key, limit.max)} 之间`;
      }
    }
    return errors;
  }, [settings, settingsForm]);

  const settingsPatch = useMemo(
    () => buildSettingsPatch(settingsForm, settings),
    [settings, settingsForm],
  );
  const hasSettingsChanges = Object.keys(settingsPatch).length > 0;
  const settingsInvalid = Object.keys(settingValidation).length > 0;

  const saveSettings = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (settingsSaving || settingsInvalid || !hasSettingsChanges) return;
    setSettingsSaving(true);
    try {
      const updated = await api.updateSettings(settingsPatch);
      setSettingsForm(settingsToForm(updated));
      const changedRequiringRestart = SETTING_FIELDS.filter(
        (field) =>
          Object.prototype.hasOwnProperty.call(settingsPatch, field.key) &&
          updated.restart_required.includes(field.key),
      ).map((field) => field.label);
      if (changedRequiringRestart.length > 0) {
        toast.success(`系统设置已保存；${changedRequiringRestart.join("、")}将在下次重启生效`);
      } else {
        toast.success("系统设置已保存并立即生效");
      }
      await refresh();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "保存设置失败");
    } finally {
      setSettingsSaving(false);
    }
  };

  const newPasswordError =
    newPassword.length > 0 && newPassword.length < 12 ? "新密码至少需要 12 个字符" : undefined;
  const confirmPasswordError =
    confirmPassword.length > 0 && confirmPassword !== newPassword
      ? "两次输入的新密码不一致"
      : undefined;
  const passwordInvalid =
    !currentPassword ||
    !newPassword ||
    !confirmPassword ||
    newPassword.length < 12 ||
    newPassword !== confirmPassword;

  const changePassword = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (passwordSaving || passwordInvalid) return;
    setPasswordSaving(true);
    try {
      const result = await api.changePassword(currentPassword, newPassword);
      setAdminUsername(result.username || "admin");
      setCurrentPassword("");
      setNewPassword("");
      setConfirmPassword("");
      toast.success("管理员密码已更新");
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "修改密码失败");
    } finally {
      setPasswordSaving(false);
    }
  };

  // 站点级 New API 凭据：一个 Base URL 配一次，账号自动继承（§6.4）。
  const [sites, setSites] = useState<NewApiSite[]>([]);
  const [siteBaseUrl, setSiteBaseUrl] = useState("");
  const [siteUserId, setSiteUserId] = useState("");
  const [siteToken, setSiteToken] = useState("");
  const [siteSaving, setSiteSaving] = useState(false);
  const [deletingSite, setDeletingSite] = useState<string | null>(null);

  const loadSites = async () => {
    try {
      const result = await api.newApiSites();
      setSites(result.sites);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "读取站点凭据失败");
    }
  };

  useEffect(() => {
    void loadSites();
    // 只在进入设置页时拉一次。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const saveSite = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (siteSaving) return;
    setSiteSaving(true);
    try {
      await api.saveNewApiSite(
        siteBaseUrl.trim(),
        siteUserId.trim(),
        siteToken.trim() || undefined,
      );
      setSiteToken("");
      await loadSites();
      toast.success("站点凭据已保存；该 Base URL 下的账号会自动使用它");
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "保存站点凭据失败");
    } finally {
      setSiteSaving(false);
    }
  };

  const removeSite = async (baseUrl: string) => {
    setDeletingSite(baseUrl);
    try {
      await api.deleteNewApiSite(baseUrl);
      await loadSites();
      toast.success("站点凭据已删除");
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "删除站点凭据失败");
    } finally {
      setDeletingSite(null);
    }
  };

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
      <Card
        title="系统设置"
        description="修改后立即热生效；关闭宽限期需要下次重启。范围由服务端实时返回。"
        actions={
          <Button
            type="submit"
            form="settings-form"
            variant="primary"
            disabled={settingsSaving || settingsInvalid || !hasSettingsChanges}
          >
            {settingsSaving && <span className="spinner" aria-hidden="true" />}
            {settingsSaving ? "保存中…" : "保存"}
          </Button>
        }
      >
        <form id="settings-form" className="card-body form-grid" onSubmit={saveSettings}>
          <div className="settings-grid">
            {SETTING_FIELDS.map((field) => {
              const limit = settings.limits[field.key];
              return (
                <Field
                  key={field.key}
                  label={field.label}
                  error={settingValidation[field.key]}
                  hint={
                    limit
                      ? `范围：${formatSettingValue(field.key, limit.min)} 至 ${formatSettingValue(field.key, limit.max)}`
                      : `单位：${field.unit}`
                  }
                >
                  {(id) => (
                    <div className="input-with-unit">
                      <input
                        id={id}
                        className="input mono"
                        type="number"
                        inputMode="numeric"
                        min={limit?.min}
                        max={limit?.max}
                        step={1}
                        value={settingsForm[field.key]}
                        onChange={(event) =>
                          setSettingsForm((current) => ({
                            ...current,
                            [field.key]: event.target.value,
                          }))
                        }
                      />
                      <span className="input-unit">{field.unit}</span>
                    </div>
                  )}
                </Field>
              );
            })}
          </div>
          <p className="settings-note">
            请求体上限按 bytes 保存；保留天数填 0 表示关闭对应记录保留。只有发生变化的字段会提交。
          </p>
        </form>
      </Card>

      <Card title="管理员账号" description="用户名只读；修改密码成功后当前会话会继续保持有效。">
        <form className="card-body form-grid" onSubmit={changePassword}>
          <div className="form-row-2">
            <Field label="用户名" hint="当前登录账号">
              {(id) => (
                <input
                  id={id}
                  className="input mono"
                  value={adminUsername}
                  readOnly
                  aria-readonly="true"
                />
              )}
            </Field>
            <Field label="当前密码">
              {(id) => (
                <input
                  id={id}
                  className="input"
                  type="password"
                  value={currentPassword}
                  autoComplete="current-password"
                  onChange={(event) => setCurrentPassword(event.target.value)}
                />
              )}
            </Field>
          </div>
          <div className="form-row-2">
            <Field
              label="新密码"
              error={newPasswordError}
              hint="至少 12 个字符，并且不能与当前密码相同。"
            >
              {(id) => (
                <input
                  id={id}
                  className="input"
                  type="password"
                  value={newPassword}
                  autoComplete="new-password"
                  onChange={(event) => setNewPassword(event.target.value)}
                />
              )}
            </Field>
            <Field label="确认新密码" error={confirmPasswordError}>
              {(id) => (
                <input
                  id={id}
                  className="input"
                  type="password"
                  value={confirmPassword}
                  autoComplete="new-password"
                  onChange={(event) => setConfirmPassword(event.target.value)}
                />
              )}
            </Field>
          </div>
          <div className="row" style={{ justifyContent: "flex-end" }}>
            <Button type="submit" variant="primary" disabled={passwordSaving || passwordInvalid}>
              {passwordSaving && <span className="spinner" aria-hidden="true" />}
              {passwordSaving ? "修改中…" : "修改密码"}
            </Button>
          </div>
        </form>
      </Card>

      <Card
        title="New API 站点凭据"
        description="一个 Base URL 只配一次访问令牌与用户 ID，该站点下的账号自动继承；账号自己填的凭据优先。"
      >
        <div className="card-body">
          <div className="table-wrap">
            <table className="data">
              <thead>
                <tr>
                  <th>Base URL</th>
                  <th>用户 ID</th>
                  <th style={{ width: 90 }} />
                </tr>
              </thead>
              <tbody>
                {sites.length === 0 ? (
                  <tr>
                    <td colSpan={3} className="text-faint">
                      还没有站点级凭据；下面的表单保存后即时生效。
                    </td>
                  </tr>
                ) : (
                  sites.map((site) => (
                    <tr key={site.base_url}>
                      <td className="mono cell-strong">{site.base_url}</td>
                      <td className="mono">{site.user_id}</td>
                      <td>
                        <Button
                          variant="ghost"
                          size="sm"
                          disabled={deletingSite === site.base_url}
                          onClick={() => void removeSite(site.base_url)}
                        >
                          {deletingSite === site.base_url ? "删除中…" : "删除"}
                        </Button>
                      </td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </div>
          <form className="form-grid" style={{ marginTop: 12 }} onSubmit={saveSite}>
            <div className="form-row-2">
              <Field label="Base URL" hint="例如 https://ai.hsnb.fun">
                {(id) => (
                  <input
                    id={id}
                    className="input mono"
                    value={siteBaseUrl}
                    onChange={(event) => setSiteBaseUrl(event.target.value)}
                    placeholder="https://..."
                  />
                )}
              </Field>
              <Field label="用户 ID" hint="New API 个人设置页显示的用户 ID">
                {(id) => (
                  <input
                    id={id}
                    className="input mono"
                    value={siteUserId}
                    onChange={(event) => setSiteUserId(event.target.value)}
                    placeholder="1"
                  />
                )}
              </Field>
            </div>
            <Field
              label="访问令牌"
              hint="New API 个人设置页生成，不是登录密码；留空表示沿用已保存的令牌。"
            >
              {(id) => (
                <input
                  id={id}
                  className="input mono"
                  type="password"
                  value={siteToken}
                  autoComplete="off"
                  onChange={(event) => setSiteToken(event.target.value)}
                  placeholder="留空则保持不变"
                />
              )}
            </Field>
            <div className="row" style={{ justifyContent: "flex-end" }}>
              <Button
                type="submit"
                variant="primary"
                disabled={siteSaving || !siteBaseUrl.trim() || !siteUserId.trim()}
              >
                {siteSaving && <span className="spinner" aria-hidden="true" />}
                {siteSaving ? "保存中…" : "保存站点凭据"}
              </Button>
            </div>
          </form>
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
                <tr>
                  <td className="text-faint">源码仓库</td>
                  <td>
                    <a
                      className="external-link"
                      href="https://github.com/HsMirage/Akhub"
                      target="_blank"
                      rel="noreferrer"
                    >
                      github.com/HsMirage/Akhub ↗
                    </a>
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
