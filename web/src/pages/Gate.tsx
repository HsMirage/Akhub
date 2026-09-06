/** 首次设置与登录。两者共用一套表单，只有文案和目标接口不同。 */
import { useState } from "react";
import { ApiError, api } from "../lib/api";
import { Button, Field } from "../components/ui";

export function Gate({
  needsSetup,
  masterKeyFromEnv,
  onAuthenticated,
}: {
  needsSetup: boolean;
  masterKeyFromEnv: boolean;
  onAuthenticated: () => void;
}) {
  const [username, setUsername] = useState("admin");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      if (needsSetup) {
        await api.setup(username, password);
      } else {
        await api.login(username, password);
      }
      onAuthenticated();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "操作失败");
      setBusy(false);
    }
  };

  return (
    <div className="gate">
      <form className="gate-card" onSubmit={submit}>
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

        <div className="stack">
          <Field label="用户名">
            {(id) => (
              <input
                id={id}
                className="input"
                value={username}
                autoComplete="username"
                onChange={(e) => setUsername(e.target.value)}
                required
              />
            )}
          </Field>

          <Field
            label="密码"
            hint={needsSetup ? "至少 8 个字符。忘记后只能清空数据目录重来。" : undefined}
            error={error ?? undefined}
          >
            {(id) => (
              <input
                id={id}
                className="input"
                type="password"
                value={password}
                autoComplete={needsSetup ? "new-password" : "current-password"}
                onChange={(e) => setPassword(e.target.value)}
                required
              />
            )}
          </Field>

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
