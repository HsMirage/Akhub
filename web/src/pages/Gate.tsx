/** 首次设置与登录。两者共用一套表单，只有文案和目标接口不同。 */
import { useRef, useState } from "react";
import { ApiError, api } from "../lib/api";
import { MIN_PASSWORD_LENGTH, USERNAME_STORAGE_KEY } from "../lib/policy";
import { Button, Field } from "../components/ui";
import { IconAlert, IconInfo } from "../components/Icons";

export function Gate({
  needsSetup,
  masterKeyFromEnv,
  notice,
}: {
  needsSetup: boolean;
  masterKeyFromEnv: boolean;
  /** 会话过期等场景带来的提示，例如"登录状态已过期"。 */
  notice?: string | null;
}) {
  const [fieldError, setFieldError] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [showPassword, setShowPassword] = useState(false);
  const [capsLock, setCapsLock] = useState(false);
  const formRef = useRef<HTMLFormElement>(null);

  const submit = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    // 从 DOM 直接取值：浏览器自动填充不一定触发 React 的 onChange，
    // 读 FormData 可以避免"页面已经填好、state 里还是空"的登录失败。
    const data = new FormData(event.currentTarget);
    const username = String(data.get("username") ?? "").trim();
    const password = String(data.get("password") ?? "");

    if (needsSetup && password.length < MIN_PASSWORD_LENGTH) {
      setFieldError(`密码至少需要 ${MIN_PASSWORD_LENGTH} 个字符`);
      setFormError(null);
      formRef.current?.querySelector<HTMLInputElement>('input[name="password"]')?.focus();
      return;
    }

    setBusy(true);
    setFormError(null);
    setFieldError(null);
    try {
      if (needsSetup) {
        await api.setup(username, password);
      } else {
        await api.login(username, password);
      }
      // 记住用户名，设置页显示当前账号时不用再猜（后端不提供 whoami）。
      localStorage.setItem(USERNAME_STORAGE_KEY, username);
      // 必须做一次真实导航。若只切换 React state，表单会在没有页面导航的
      // 情况下被卸载，Firefox/Safari 等浏览器可能不提示保存本次登录。
      // 导航前保留表单值，让密码管理器有机会识别这次成功提交。
      window.location.reload();
    } catch (cause) {
      setFormError(cause instanceof ApiError ? cause.message : "操作失败");
      setBusy(false);
      // 认证失败是表单级问题：把焦点送回用户名，并提示两个字段都检查。
      formRef.current?.querySelector<HTMLInputElement>('input[name="username"]')?.focus();
    }
  };

  return (
    <div className="gate">
      <form className="gate-card" autoComplete="on" onSubmit={submit} ref={formRef}>
        <div className="gate-head">
          <div className="brand-mark" style={{ width: 38, height: 38, fontSize: 18 }}>
            A
          </div>
          <div>
            <h1 style={{ fontSize: 19 }}>
              {needsSetup ? "设置管理员密码" : "登录 Akhub"}
            </h1>
            <p className="card-desc" style={{ marginTop: 4 }}>
              {needsSetup
                ? "首次启动只需这一步，不必先创建分组或账号。"
                : "AI 协议网关与组内负载均衡"}
            </p>
          </div>
        </div>

        {notice && (
          <div className="callout callout-info" role="status">
            <IconInfo size={15} />
            <span>{notice}</span>
          </div>
        )}

        {formError && (
          <div className="callout callout-danger" role="alert">
            <IconAlert size={15} />
            <span>{formError}</span>
          </div>
        )}

        <div className="stack">
          <Field
            label="用户名"
            hint={needsSetup ? "创建后不可修改，建议保留 admin。" : undefined}
          >
            {(id) => (
              <input
                id={id}
                name="username"
                className="input"
                defaultValue="admin"
                autoComplete="username"
                autoCapitalize="none"
                spellCheck={false}
                autoFocus
                required
              />
            )}
          </Field>

          <Field
            label="密码"
            hint={
              needsSetup
                ? `至少 ${MIN_PASSWORD_LENGTH} 个字符。忘记后只能清空数据目录重来。`
                : undefined
            }
            error={fieldError ?? undefined}
          >
            {(id) => (
              <div className="input-affix">
                <input
                  id={id}
                  name="password"
                  className="input"
                  type={showPassword ? "text" : "password"}
                  autoComplete={needsSetup ? "new-password" : "current-password"}
                  required
                  onKeyUp={(event) =>
                    setCapsLock(event.getModifierState("CapsLock"))
                  }
                  onKeyDown={(event) =>
                    setCapsLock(event.getModifierState("CapsLock"))
                  }
                />
                <button
                  type="button"
                  className="input-affix-button"
                  aria-label={showPassword ? "隐藏密码" : "显示密码"}
                  aria-pressed={showPassword}
                  onClick={() => setShowPassword((current) => !current)}
                >
                  {showPassword ? "隐藏" : "显示"}
                </button>
              </div>
            )}
          </Field>

          {capsLock && (
            <div className="field-hint caps-hint" role="status">
              <IconAlert size={13} /> 大写锁定已开启
            </div>
          )}

          <Button type="submit" variant="primary" disabled={busy}>
            {busy ? "处理中…" : needsSetup ? "创建管理员" : "登录"}
          </Button>
        </div>

        {needsSetup && (
          <div className="callout callout-info">
            <span>
              主密钥来源：{masterKeyFromEnv ? "环境变量 AKHUB_MASTER_KEY" : "数据目录中的 master.key"}
              。主密钥丢失后，已保存的上游 Key 无法恢复。
            </span>
          </div>
        )}
      </form>
    </div>
  );
}
