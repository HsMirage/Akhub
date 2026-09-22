/** 通用 UI 原语。全部无状态、无副作用，样式来自 components.css。 */
import type { Score } from "../lib/types";
import {
  cloneElement,
  createContext,
  isValidElement,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { IconCheck, IconChevronDown, IconCopy, IconInfo, IconMore, IconX } from "./Icons";

/* ---------------------------------------------------------------- 按钮 */

/**
 * 把浮层挂到 `document.body` 上并计算 fixed 坐标。
 *
 * 表格容器为了横向滚动设了 `overflow-x: auto`，任何绝对定位的子浮层都会
 * 被裁掉一截，首列/操作列的 sticky 层级也会盖到浮层上。挂 body 之后
 * 既不受裁剪影响，也不参与表格的层叠上下文。
 */
type PopoverSide = "top" | "bottom";

function usePopoverPosition(
  open: boolean,
  trigger: React.RefObject<HTMLElement | null>,
  popup: React.RefObject<HTMLElement | null>,
  side: PopoverSide,
  revision = 0,
) {
  const [position, setPosition] = useState<{ top: number; left: number } | null>(null);

  useEffect(() => {
    if (!open || !trigger.current) {
      setPosition(null);
      return;
    }
    const update = () => {
      const rect = trigger.current?.getBoundingClientRect();
      const popupEl = popup.current;
      if (!rect || !popupEl) return;
      const width = popupEl.offsetWidth || 200;
      const height = popupEl.offsetHeight || 120;
      const margin = 8;
      let top: number;
      if (side === "top") {
        top = rect.top - height - 6;
        if (top < margin) top = rect.bottom + 6;
      } else {
        top = rect.bottom + 6;
        if (top + height > window.innerHeight - margin) {
          top = Math.max(margin, rect.top - height - 6);
        }
      }
      const left = Math.max(
        margin,
        Math.min(rect.left + rect.width / 2 - width / 2, window.innerWidth - width - margin),
      );
      setPosition({ top, left });
    };
    const frame = window.requestAnimationFrame(update);
    window.addEventListener("resize", update);
    window.addEventListener("scroll", update, true);
    return () => {
      window.cancelAnimationFrame(frame);
      window.removeEventListener("resize", update);
      window.removeEventListener("scroll", update, true);
    };
  }, [open, trigger, popup, side, revision]);

  return position;
}

type ButtonVariant = "primary" | "secondary" | "ghost" | "danger";

export function Button({
  variant = "secondary",
  size,
  icon,
  children,
  className,
  ...props
}: {
  variant?: ButtonVariant;
  size?: "sm";
  icon?: ReactNode;
  className?: string;
} & React.ButtonHTMLAttributes<HTMLButtonElement>) {
  const classes = ["btn", `btn-${variant}`];
  if (size === "sm") classes.push("btn-sm");
  if (!children) classes.push("btn-icon");
  // 传入的 className 参与合并，而不是覆盖组件自己的类名。
  if (className) classes.push(className);
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
  const errorId = id + "-error";
  const control = children(id);
  // 错误必须和控件建立可访问性关联：否则屏幕阅读器只看到一段红色文字，
  // 却不知道是哪个字段出错。
  const enhanced =
    error && isValidElement(control)
      ? cloneElement(control as ReactElement<Record<string, unknown>>, {
          "aria-invalid": true,
          "aria-describedby": errorId,
        })
      : control;
  return (
    <div className="field">
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      {enhanced}
      {error ? (
        <span className="field-error" id={errorId} role="alert">
          {error}
        </span>
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

/**
 * Esc 关闭 + 背景滚动锁定 + 焦点陷阱 + 关闭后焦点归还。
 *
 * 焦点管理是弹层最基本的可用性要求：少了它，键盘用户 Tab 会穿到遮罩后面，
 * 关闭后又要从页面顶部重新找触发按钮。
 */
function useDismissable(
  open: boolean,
  onClose: () => void,
  container: React.RefObject<HTMLElement | null>,
) {
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

  useEffect(() => {
    if (!open) return;
    const previouslyFocused =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;

    const focusables = () =>
      Array.from(
        container.current?.querySelectorAll<HTMLElement>(
          'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
        ) ?? [],
      ).filter((el) => el.offsetParent !== null || el === document.activeElement);

    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        closeRef.current();
        return;
      }
      if (event.key !== "Tab") return;
      const list = focusables();
      if (list.length === 0) {
        event.preventDefault();
        container.current?.focus();
        return;
      }
      const first = list[0] as HTMLElement;
      const last = list[list.length - 1] as HTMLElement;
      const active = document.activeElement as HTMLElement | null;
      if (event.shiftKey && (active === first || !container.current?.contains(active))) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && (active === last || !container.current?.contains(active))) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener("keydown", onKey, true);
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.removeEventListener("keydown", onKey, true);
      document.body.style.overflow = previousOverflow;
      // 关闭时把焦点还给触发按钮；找不到目标时退回原处，不影响后续 Tab。
      previouslyFocused?.focus?.({ preventScroll: true });
    };
  }, [open, container]);
}

/** 有未保存修改时，关闭动作先走确认，避免整屏表单被一次误点丢掉。 */
function useCloseGuard(open: boolean, dirty: boolean | undefined, onClose: () => void) {
  const [confirming, setConfirming] = useState(false);
  useEffect(() => {
    if (!open) setConfirming(false);
  }, [open]);
  const requestClose = useCallback(() => {
    if (dirty) setConfirming(true);
    else onClose();
  }, [dirty, onClose]);
  return { confirming, setConfirming, requestClose };
}

export function Drawer({
  open,
  onClose,
  title,
  description,
  footer,
  children,
  closeOnOverlay = true,
  dirty,
  size,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  description?: ReactNode;
  footer?: ReactNode;
  children: ReactNode;
  /** Key 展示等不可恢复场景禁止点遮罩关闭，避免误触丢信息。 */
  closeOnOverlay?: boolean;
  /** 有未保存修改时，关闭前弹确认。 */
  dirty?: boolean;
  size?: "lg";
}) {
  const dialogRef = useRef<HTMLElement>(null);
  const firstField = useRef<HTMLDivElement>(null);
  const { confirming, setConfirming, requestClose } = useCloseGuard(open, dirty, onClose);
  useDismissable(open && !confirming, requestClose, dialogRef);

  useEffect(() => {
    if (!open) return;
    // 打开后把焦点送进表单，键盘用户不必先 Tab 穿过整页。
    firstField.current?.querySelector<HTMLElement>("input, select, textarea")?.focus();
  }, [open]);

  if (!open) return null;
  return (
    <>
      <div
        className="overlay"
        onClick={closeOnOverlay ? requestClose : undefined}
      />
      <aside
        className={size === "lg" ? "drawer drawer-lg" : "drawer"}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        ref={dialogRef}
      >
        <header className="drawer-head">
          <div>
            <h2 className="card-title">{title}</h2>
            {description && <p className="card-desc">{description}</p>}
          </div>
          <div className="spacer" />
          <Button variant="ghost" icon={<IconX />} onClick={requestClose} aria-label="关闭" />
        </header>
        <div className="drawer-body" ref={firstField}>
          {children}
        </div>
        {footer && <footer className="drawer-foot">{footer}</footer>}
      </aside>
      <ConfirmDialog
        open={confirming}
        title="放弃未保存的修改？"
        message="关闭后当前填写的内容不会保存。"
        confirmLabel="放弃修改"
        danger
        onClose={() => setConfirming(false)}
        onConfirm={() => {
          setConfirming(false);
          onClose();
        }}
      />
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
  dirty,
  size,
  closeOnOverlay = true,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  className?: string;
  footer?: ReactNode;
  children: ReactNode;
  /** 有未保存修改时，关闭前弹确认。 */
  dirty?: boolean;
  size?: "lg";
  /** Key 展示等不可恢复场景禁止点遮罩关闭。 */
  closeOnOverlay?: boolean;
}) {
  const dialogRef = useRef<HTMLDivElement>(null);
  const { confirming, setConfirming, requestClose } = useCloseGuard(open, dirty, onClose);
  useDismissable(open && !confirming, requestClose, dialogRef);

  if (!open) return null;
  return (
    <>
      <div className="overlay" onClick={closeOnOverlay ? requestClose : undefined} />
      <div
        className={["modal", size === "lg" ? "modal-lg" : "", className ?? ""]
          .filter(Boolean)
          .join(" ")}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        ref={dialogRef}
      >
        <header className="modal-head">
          <h2 className="card-title">{title}</h2>
          <div className="spacer" />
          <Button variant="ghost" icon={<IconX />} onClick={requestClose} aria-label="关闭" />
        </header>
        <div className="modal-body">{children}</div>
        {footer && <footer className="modal-foot">{footer}</footer>}
      </div>
      <ConfirmDialog
        open={confirming}
        title="放弃未保存的修改？"
        message="关闭后当前修改不会保存。"
        confirmLabel="放弃修改"
        danger
        onClose={() => setConfirming(false)}
        onConfirm={() => {
          setConfirming(false);
          onClose();
        }}
      />
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
  requireText,
  requireLabel,
  onConfirm,
  onClose,
}: {
  open: boolean;
  title: string;
  message: ReactNode;
  confirmLabel?: string;
  danger?: boolean;
  /** 需要用户原样输入这段文字才能继续（删除分组、恢复备份等不可逆操作）。 */
  requireText?: string;
  requireLabel?: string;
  onConfirm: () => void;
  onClose: () => void;
}) {
  const [text, setText] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);
  const inputId = useId();
  useEffect(() => {
    if (!open) setText("");
    else if (requireText) {
      // 弹窗打开后把焦点放到确认输入框，键盘用户不必再找。
      window.setTimeout(() => inputRef.current?.focus(), 30);
    }
  }, [open, requireText]);
  const allowed = !requireText || text.trim() === requireText;

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
            disabled={!allowed}
            onClick={() => {
              if (!allowed) return;
              onConfirm();
              onClose();
            }}
          >
            {confirmLabel}
          </Button>
        </>
      }
    >
      <div className="stack" style={{ gap: 12 }}>
        <p style={{ margin: 0, lineHeight: 1.7 }}>{message}</p>
        {requireText && (
          <div className="field">
            <label className="field-label" htmlFor={inputId}>
              {requireLabel ?? `请输入「${requireText}」以确认`}
            </label>
            <input
              id={inputId}
              ref={inputRef}
              className="input mono"
              value={text}
              autoComplete="off"
              onChange={(event) => setText(event.target.value)}
            />
          </div>
        )}
      </div>
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
          <div
            key={toast.id}
            className={`toast toast-${toast.tone}`}
            role={toast.tone === "error" ? "alert" : "status"}
          >
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

export function CopyButton({
  value,
  label,
  iconOnly,
  onCopied,
}: {
  value: string;
  label?: string;
  /** 只显示图标：用于表格单元格等空间紧张的位置。 */
  iconOnly?: boolean;
  /** 复制成功后的回调，用于联动"我已保存"等确认状态。 */
  onCopied?: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const toast = useToast();

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      onCopied?.();
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
      title={iconOnly ? (label ?? "复制") : undefined}
      aria-label={iconOnly ? (label ?? "复制") : undefined}
    >
      {iconOnly ? null : copied ? "已复制" : (label ?? "复制")}
    </Button>
  );
}

/* ------------------------------------------------------- 说明提示/菜单/分段 */

/**
 * 可点击、可聚焦的说明提示。关键解释不能只放在 title 里：触屏与键盘用户看不到。
 */
export function InfoTip({
  label,
  children,
  className,
}: {
  label: string;
  children: ReactNode;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLSpanElement>(null);
  const pop = useRef<HTMLSpanElement>(null);
  const id = useId();
  const position = usePopoverPosition(open, root, pop, "top");

  useEffect(() => {
    if (!open) return;
    const onDoc = (event: MouseEvent) => {
      const target = event.target as Node;
      if (!root.current?.contains(target) && !pop.current?.contains(target)) {
        setOpen(false);
      }
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <span className={className ? `info-tip ${className}` : "info-tip"} ref={root}>
      <button
        type="button"
        className="info-tip-button"
        aria-label={label}
        aria-expanded={open}
        aria-describedby={open ? id : undefined}
        onClick={() => setOpen((current) => !current)}
      >
        <IconInfo size={13} />
      </button>
      {open &&
        createPortal(
          <span
            role="tooltip"
            id={id}
            ref={pop}
            className="info-tip-pop is-portal"
            style={position ? { top: position.top, left: position.left } : undefined}
          >
            {children}
          </span>,
          document.body,
        )}
    </span>
  );
}

export interface MenuItem {
  label: string;
  onSelect: () => void;
  icon?: ReactNode;
  danger?: boolean;
  disabled?: boolean;
  hint?: string;
}

/**
 * 行内"更多操作"菜单。列表页把低频操作收进来，既减少横向占用，
 * 也让每个操作有完整的文字标签，不必只靠图标猜。
 */
export function Menu({
  label,
  items,
  disabled,
}: {
  label: string;
  items: MenuItem[];
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  const position = usePopoverPosition(open, trigger, pop, "bottom", items.length);

  useEffect(() => {
    if (!open) return;
    const onDoc = (event: MouseEvent) => {
      const target = event.target as Node;
      if (!root.current?.contains(target) && !pop.current?.contains(target)) {
        setOpen(false);
      }
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setOpen(false);
        trigger.current?.focus();
      }
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="menu" ref={root}>
      <button
        ref={trigger}
        type="button"
        className="btn btn-secondary btn-sm btn-icon"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
      >
        <IconMore size={15} />
      </button>
      {open &&
        createPortal(
          <div
            ref={pop}
            className="menu-pop is-portal"
            role="menu"
            aria-label={label}
            style={position ? { top: position.top, left: position.left } : undefined}
          >
            {items.map((item) => (
              <button
                key={item.label}
                type="button"
                role="menuitem"
                className={`menu-item${item.danger ? " is-danger" : ""}`}
                disabled={item.disabled}
                title={item.hint}
                onClick={() => {
                  setOpen(false);
                  item.onSelect();
                }}
              >
                {item.icon}
                <span>{item.label}</span>
              </button>
            ))}
          </div>,
          document.body,
        )}
    </div>
  );
}

/** 分段控件：替代用 primary/ghost 按钮模拟的选中态。 */
export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
  disabled,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (value: T) => void;
  label: string;
  disabled?: boolean;
}) {
  return (
    <div className="segmented" role="tablist" aria-label={label}>
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          role="tab"
          aria-selected={option.value === value}
          className={`segmented-item${option.value === value ? " is-active" : ""}`}
          disabled={disabled}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

/** 抽屉/长表单里的分区。高级配置默认折叠，降低新用户的首屏压力。 */
export function FormSection({
  title,
  description,
  children,
  collapsible = false,
  defaultOpen = true,
  badge,
}: {
  title: string;
  description?: ReactNode;
  children: ReactNode;
  collapsible?: boolean;
  defaultOpen?: boolean;
  badge?: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <section className="form-section">
      {collapsible ? (
        <button
          type="button"
          className="form-section-head is-button"
          aria-expanded={open}
          onClick={() => setOpen((current) => !current)}
        >
          <span className="form-section-title">{title}</span>
          {badge}
          <span className="spacer" />
          <IconChevronDown size={14} className={open ? "chevron is-open" : "chevron"} />
        </button>
      ) : (
        <div className="form-section-head">
          <span className="form-section-title">{title}</span>
          {badge}
        </div>
      )}
      {open && (
        <div className="form-section-body">
          {description && <p className="form-section-desc">{description}</p>}
          {children}
        </div>
      )}
    </section>
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
  // 门槛与有效样本量都来自后端：界面写死数字会在调参之后开始说谎（§9.4）。
  const cold = score.warm
    ? ""
    : `；有效样本 ${score.effective_samples.toFixed(1)}/${score.warm_threshold}，性能三维暂用中性分`;
  return (
    <span className="score">
      <span
        className="score-total mono"
        title={`综合评分 ${score.total.toFixed(2)}${cold}`}
      >
        {score.total.toFixed(2)}
      </span>
      <span className="score-bars">
        {dimensions.map(({ key, label }) => {
          const value = Number(score[key]);
          // 提示里同时给归一化得分与该维对总分的实际贡献：只看得分容易误判
          // ——"这一维很低"和"这一维把总分拉下去了"是两件事（§6.5）。
          const share = Number(score.contribution[key as keyof typeof score.contribution]);
          const pct = score.total > 0 ? Math.round((share / score.total) * 100) : 0;
          const suffix = score.total > 0 ? `（占总分 ${pct}%）` : "";
          return (
            <span
              key={key}
              className="score-bar"
              title={`${label}：得分 ${value.toFixed(2)}，贡献 ${share.toFixed(3)}${suffix}`}
            >
              <i style={{ width: `${Math.round(value * 100)}%` }} />
            </span>
          );
        })}
      </span>
      {!score.warm && <span className="score-cold">冷启动</span>}
    </span>
  );
}
