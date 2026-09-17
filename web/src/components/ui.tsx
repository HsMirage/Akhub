/** 通用 UI 原语。全部无状态、无副作用，样式来自 components.css。 */
import type { Score } from "../lib/types";
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { IconCheck, IconCopy, IconX } from "./Icons";

/* ---------------------------------------------------------------- 按钮 */

type ButtonVariant = "primary" | "secondary" | "ghost" | "danger";

export function Button({
  variant = "secondary",
  size,
  icon,
  children,
  ...props
}: {
  variant?: ButtonVariant;
  size?: "sm";
  icon?: ReactNode;
} & React.ButtonHTMLAttributes<HTMLButtonElement>) {
  const classes = ["btn", `btn-${variant}`];
  if (size === "sm") classes.push("btn-sm");
  if (!children) classes.push("btn-icon");
  return (
    <button type="button" {...props} className={classes.join(" ")}>
      {icon}
      {children}
    </button>
  );
}

/* ---------------------------------------------------------------- 卡片 */

export function Card({
  title,
  description,
  actions,
  children,
  padded = false,
}: {
  title?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  padded?: boolean;
}) {
  return (
    <section className="card">
      {(title || actions) && (
        <header className="card-head">
          <div>
            {title && <h2 className="card-title">{title}</h2>}
            {description && <p className="card-desc">{description}</p>}
          </div>
          {actions && <div className="card-actions">{actions}</div>}
        </header>
      )}
      {padded ? <div className="card-body">{children}</div> : children}
    </section>
  );
}

/* ------------------------------------------------------------ 徽标/状态 */

export function Badge({
  tone = "neutral",
  dot,
  children,
}: {
  tone?: "neutral" | "success" | "warn" | "danger" | "info" | "accent";
  dot?: boolean;
  children: ReactNode;
}) {
  return (
    <span className={`badge badge-${tone}`}>
      {dot && <span className="dot" />}
      {children}
    </span>
  );
}

/* ---------------------------------------------------------------- 表单 */

export function Field({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: string;
  children: (id: string) => ReactNode;
}) {
  const id = useId();
  return (
    <div className="field">
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      {children(id)}
      {error ? (
        <span className="field-error">{error}</span>
      ) : (
        hint && <span className="field-hint">{hint}</span>
      )}
    </div>
  );
}

export function Switch({
  checked,
  onChange,
  label,
  hint,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: ReactNode;
  hint?: ReactNode;
}) {
  return (
    <label className="switch">
      <input
        type="checkbox"
        checked={checked}
        onChange={(event) => onChange(event.target.checked)}
      />
      <span className="switch-track" />
      <span>
        <span style={{ fontSize: 13.5 }}>{label}</span>
        {hint && (
          <span className="field-hint" style={{ display: "block" }}>
            {hint}
          </span>
        )}
      </span>
    </label>
  );
}

/* ------------------------------------------------------------ 抽屉/弹窗 */

/** Esc 关闭 + 打开时锁定背景滚动。抽屉与弹窗共用。 */
function useDismissable(open: boolean, onClose: () => void) {
  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    const previous = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.removeEventListener("keydown", onKey);
      document.body.style.overflow = previous;
    };
  }, [open, onClose]);
}

export function Drawer({
  open,
  onClose,
  title,
  description,
  footer,
  children,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  description?: ReactNode;
  footer?: ReactNode;
  children: ReactNode;
}) {
  useDismissable(open, onClose);
  const firstField = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    // 打开后把焦点送进表单，键盘用户不必先 Tab 穿过整页。
    firstField.current?.querySelector<HTMLElement>("input, select, textarea")?.focus();
  }, [open]);

  if (!open) return null;
  return (
    <>
      <div className="overlay" onClick={onClose} />
      <aside className="drawer" role="dialog" aria-modal="true" aria-label={title}>
        <header className="drawer-head">
          <div>
            <h2 className="card-title">{title}</h2>
            {description && <p className="card-desc">{description}</p>}
          </div>
          <div className="spacer" />
          <Button variant="ghost" icon={<IconX />} onClick={onClose} aria-label="关闭" />
        </header>
        <div className="drawer-body" ref={firstField}>
          {children}
        </div>
        {footer && <footer className="drawer-foot">{footer}</footer>}
      </aside>
    </>
  );
}

export function Modal({
  open,
  onClose,
  title,
  className,
  footer,
  children,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  className?: string;
  footer?: ReactNode;
  children: ReactNode;
}) {
  useDismissable(open, onClose);
  if (!open) return null;
  return (
    <>
      <div className="overlay" onClick={onClose} />
      <div
        className={className ? `modal ${className}` : "modal"}
        role="dialog"
        aria-modal="true"
        aria-label={title}
      >
        <header className="modal-head">
          <h2 className="card-title">{title}</h2>
          <div className="spacer" />
          <Button variant="ghost" icon={<IconX />} onClick={onClose} aria-label="关闭" />
        </header>
        <div className="modal-body">{children}</div>
        {footer && <footer className="modal-foot">{footer}</footer>}
      </div>
    </>
  );
}

/** 确认弹窗。破坏性操作一律走它，不用浏览器原生 confirm。 */
export function ConfirmDialog({
  open,
  title,
  message,
  confirmLabel = "确认",
  danger,
  onConfirm,
  onClose,
}: {
  open: boolean;
  title: string;
  message: ReactNode;
  confirmLabel?: string;
  danger?: boolean;
  onConfirm: () => void;
  onClose: () => void;
}) {
  return (
    <Modal
      open={open}
      onClose={onClose}
      title={title}
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button
            variant={danger ? "danger" : "primary"}
            onClick={() => {
              onConfirm();
              onClose();
            }}
          >
            {confirmLabel}
          </Button>
        </>
      }
    >
      <p style={{ margin: 0, lineHeight: 1.7 }}>{message}</p>
    </Modal>
  );
}

/* ---------------------------------------------------------------- 提示 */

interface Toast {
  id: number;
  tone: "success" | "error";
  message: string;
}

const ToastContext = createContext<(tone: Toast["tone"], message: string) => void>(
  () => {},
);

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const next = useRef(0);

  const push = useCallback((tone: Toast["tone"], message: string) => {
    const id = next.current++;
    setToasts((current) => [...current, { id, tone, message }]);
    // 错误留得久一些：用户往往需要读完整句错误信息。
    setTimeout(
      () => setToasts((current) => current.filter((t) => t.id !== id)),
      tone === "error" ? 6000 : 3200,
    );
  }, []);

  return (
    <ToastContext.Provider value={push}>
      {children}
      <div className="toasts" role="status" aria-live="polite">
        {toasts.map((toast) => (
          <div key={toast.id} className={`toast toast-${toast.tone}`}>
            <span style={{ marginTop: 2, flexShrink: 0 }}>
              {toast.tone === "success" ? <IconCheck size={14} /> : <IconX size={14} />}
            </span>
            <span>{toast.message}</span>
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

export function useToast() {
  const push = useContext(ToastContext);
  return useMemo(
    () => ({
      success: (message: string) => push("success", message),
      error: (message: string) => push("error", message),
    }),
    [push],
  );
}

/* ------------------------------------------------------------ 复制按钮 */

export function CopyButton({ value, label }: { value: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  const toast = useToast();

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      // 非 HTTPS 环境下剪贴板 API 不可用，明确告知而不是静默失败。
      toast.error("浏览器拒绝了剪贴板访问，请手动选中复制");
    }
  };

  return (
    <Button
      size="sm"
      icon={copied ? <IconCheck size={14} /> : <IconCopy size={14} />}
      onClick={copy}
    >
      {copied ? "已复制" : (label ?? "复制")}
    </Button>
  );
}

/* ------------------------------------------------------------ 空状态等 */

export function EmptyState({
  icon,
  title,
  description,
  action,
}: {
  icon: ReactNode;
  title: string;
  description: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="empty">
      <div className="empty-icon">{icon}</div>
      <div className="empty-title">{title}</div>
      <p className="empty-desc">{description}</p>
      {action}
    </div>
  );
}

export function Skeleton({ rows = 3 }: { rows?: number }) {
  return (
    <div className="card-body stack" style={{ gap: 12 }}>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="skeleton" style={{ width: `${100 - i * 12}%` }} />
      ))}
    </div>
  );
}

/**
 * 综合评分与四个分维得分的紧凑可视化（§6.9）。
 *
 * 权重调错时，一眼就能看出是哪一维把分数拉下去的，而不必打开每个目标细看。
 */
export function ScoreMeter({ score }: { score: Score }) {
  const dimensions: { key: keyof Score; label: string }[] = [
    { key: "multiplier", label: "倍率" },
    { key: "reliability", label: "可靠性" },
    { key: "first_token", label: "首字延迟" },
    { key: "throughput", label: "输出速度" },
  ];
  return (
    <span className="score" title={score.warm ? undefined : `样本 ${score.samples}/20，性能三维暂用中性分`}>
      <span className="score-total mono">{score.total.toFixed(2)}</span>
      <span className="score-bars">
        {dimensions.map(({ key, label }) => {
          const value = Number(score[key]);
          return (
            <span key={key} className="score-bar" title={`${label} ${value.toFixed(2)}`}>
              <i style={{ width: `${Math.round(value * 100)}%` }} />
            </span>
          );
        })}
      </span>
      {!score.warm && <span className="score-cold">冷启动</span>}
    </span>
  );
}
