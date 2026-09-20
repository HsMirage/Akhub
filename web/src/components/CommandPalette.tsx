/** 全局搜索与跳转（Ctrl/⌘ K）。配置项变多后，逐页翻找是最慢的路径。 */
import { useEffect, useMemo, useRef, useState } from "react";
import type { Data } from "../lib/store";
import type { Route } from "../routes";
import { ROUTES, ROUTE_META } from "../routes";
import { IconSearch } from "./Icons";
import { Modal } from "./ui";

interface PaletteItem {
  id: string;
  label: string;
  hint?: string;
  group: string;
  run: () => void;
}

export function CommandPalette({
  open,
  onClose,
  data,
  navigate,
}: {
  open: boolean;
  onClose: () => void;
  data: Data;
  navigate: (route: Route, params?: Record<string, string>) => void;
}) {
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const itemRefs = useRef<(HTMLButtonElement | null)[]>([]);

  const items = useMemo<PaletteItem[]>(() => {
    const list: PaletteItem[] = ROUTES.map((route) => ({
      id: `route-${route}`,
      label: ROUTE_META[route].title,
      hint: ROUTE_META[route].subtitle,
      group: "页面",
      run: () => navigate(route),
    }));
    data.groups.forEach((group) =>
      list.push({
        id: `group-${group.id}`,
        label: group.name,
        hint: "分组配置",
        group: "分组",
        run: () => navigate("groups"),
      }),
    );
    data.accounts.forEach((account) =>
      list.push({
        id: `account-${account.id}`,
        label: account.name,
        hint: account.base_url,
        group: "上游账号",
        run: () => navigate("accounts", { account: account.id }),
      }),
    );
    data.models.forEach((model) =>
      list.push({
        id: `model-${model.id}`,
        label: model.name,
        hint: "对外可用模型",
        group: "模型",
        run: () => navigate("targets"),
      }),
    );
    data.targets.forEach((target) =>
      list.push({
        id: `target-${target.id}`,
        label: target.upstream_model,
        hint: "调度目标",
        group: "调度视图",
        run: () => navigate("targets", { target: target.id }),
      }),
    );
    return list;
  }, [data, navigate]);

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return items.slice(0, 12);
    return items
      .filter(
        (item) =>
          item.label.toLowerCase().includes(needle) ||
          (item.hint ?? "").toLowerCase().includes(needle) ||
          item.group.toLowerCase().includes(needle),
      )
      .slice(0, 20);
  }, [items, query]);

  useEffect(() => {
    if (!open) return;
    setQuery("");
    setActive(0);
    window.setTimeout(() => inputRef.current?.focus(), 30);
  }, [open]);

  useEffect(() => {
    setActive(0);
  }, [query]);

  // 键盘上下移动时把选中项滚进可视区域，长列表也能一路选到底。
  useEffect(() => {
    itemRefs.current[active]?.scrollIntoView({ block: "nearest" });
  }, [active]);

  const choose = (item: PaletteItem | undefined) => {
    if (!item) return;
    item.run();
    onClose();
  };

  return (
    <Modal open={open} onClose={onClose} title="搜索与跳转">
      <div className="command-palette">
        <div className="command-input">
          <IconSearch size={15} />
          <input
            ref={inputRef}
            value={query}
            placeholder="搜索页面、分组、账号、模型、目标…"
            aria-label="搜索与跳转"
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "ArrowDown") {
                event.preventDefault();
                setActive((current) => Math.min(current + 1, filtered.length - 1));
              } else if (event.key === "ArrowUp") {
                event.preventDefault();
                setActive((current) => Math.max(current - 1, 0));
              } else if (event.key === "Enter") {
                event.preventDefault();
                choose(filtered[active]);
              }
            }}
          />
          <kbd className="kbd">Esc</kbd>
        </div>
        <div className="command-results" role="listbox" aria-label="搜索结果">
          {filtered.length === 0 ? (
            <div className="table-empty">没有匹配结果</div>
          ) : (
            filtered.map((item, index) => (
              <button
                key={item.id}
                type="button"
                role="option"
                aria-selected={index === active}
                ref={(element) => {
                  itemRefs.current[index] = element;
                }}
                className={`command-item${index === active ? " is-active" : ""}`}
                onMouseEnter={() => setActive(index)}
                onClick={() => choose(item)}
              >
                <span className="command-item-label">{item.label}</span>
                <span className="command-item-hint">
                  {item.group}
                  {item.hint ? ` · ${item.hint}` : ""}
                </span>
              </button>
            ))
          )}
        </div>
      </div>
    </Modal>
  );
}
