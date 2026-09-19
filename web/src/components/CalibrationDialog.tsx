/**
 * 校准助手（§6.8）：在账号编辑页内按单模型对账，反算校准系数。
 *
 * 必须按单个模型对账：总用量对账会随"这个月用了哪些模型"漂移，同一个
 * 账号连续两个月能算出两个不同的系数。多账号共享模型时给出近似值并明确
 * 提示，不假装精确。
 */
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import type { Account, CalibrationRecord, CalibrationResult } from "../lib/types";
import { Button, Field, Modal, useToast } from "./ui";

export function CalibrationDialog({
  account,
  models,
  open,
  onClose,
}: {
  account: Account | null;
  /** 全部逻辑模型名，供选择。 */
  models: string[];
  open: boolean;
  onClose: () => void;
}) {
  const toast = useToast();
  const [model, setModel] = useState("");
  /** 对账区间天数（§6.8）。站点改过倍率时，区间太长会把新旧两档混在一起。 */
  const [periodDays, setPeriodDays] = useState(30);
  const [reported, setReported] = useState("");
  const [result, setResult] = useState<CalibrationResult | null>(null);
  const [records, setRecords] = useState<CalibrationRecord[]>([]);
  const [busy, setBusy] = useState(false);

  const accountId = account?.id;

  const loadRecords = useCallback(async () => {
    if (!accountId) return;
    try {
      setRecords((await api.calibrations(accountId)).data);
    } catch {
      // 记录列表失败不阻塞主流程。
    }
  }, [accountId]);

  useEffect(() => {
    if (open) {
      setResult(null);
      setReported("");
      setModel(models[0] ?? "");
      void loadRecords();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, accountId]);

  const submit = async () => {
    if (!accountId || !model || !reported.trim()) return;
    setBusy(true);
    try {
      // 把区间起点算成绝对时间戳传给后端：区间必须由用户明确指定，不能
      // 让"最近多久"这件事藏在服务端默认值里（§6.8）。
      const periodStart = Math.floor(Date.now() / 1000) - periodDays * 86_400;
      const result = await api.calibrate(accountId, model, reported.trim(), periodStart);
      setResult(result);
      toast.success(`校准系数已算出：${result.calibration}`);
      await loadRecords();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "校准失败");
    } finally {
      setBusy(false);
    }
  };

  const applyCalibration = async () => {
    if (!accountId || !result) return;
    setBusy(true);
    try {
      await api.updateAccount(accountId, { calibration: result.calibration });
      toast.success(`校准系数已写入账号：${result.calibration}`);
      onClose();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "写入失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={account ? `校准助手 · ${account.name}` : "校准助手"}
      footer={
        <>
          <Button onClick={onClose}>关闭</Button>
          <Button
            variant="primary"
            onClick={() => void submit()}
            disabled={busy || !model || !reported.trim()}
          >
            {busy ? "计算中…" : "计算校准系数"}
          </Button>
        </>
      }
    >
      <div className="stack" style={{ gap: 12 }}>
        <div className="form-row-2">
          <Field label="选一个模型" hint="必须按单个模型对账，不能用账号总用量。">
            {(id) => (
              <select
                id={id}
                className="select"
                value={model}
                onChange={(e) => setModel(e.target.value)}
              >
                {models.map((name) => (
                  <option key={name} value={name}>
                    {name}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <Field
            label="对账区间"
            hint="站点改过倍率就选短一点，否则新旧两档会被混在一起，算出的系数两边都不对。"
          >
            {(id) => (
              <select
                id={id}
                className="select"
                value={periodDays}
                onChange={(e) => setPeriodDays(Number(e.target.value))}
              >
                <option value={7}>最近 7 天</option>
                <option value={30}>最近 30 天</option>
                <option value={90}>最近 90 天</option>
              </select>
            )}
          </Field>
        </div>
        <div className="form-row-2">
          <Field
            label="站点后台该模型扣费倍率"
            hint="去站点后台看这个模型实际扣了多少倍率，填到这里。"
          >
            {(id) => (
              <input
                id={id}
                className="input mono"
                value={reported}
                placeholder="0.83"
                onChange={(e) => setReported(e.target.value)}
              />
            )}
          </Field>
        </div>

        {result && (
          <div
            className="card-body"
            style={{ border: "1px solid var(--border)", borderRadius: 10 }}
          >
            <div className="row" style={{ gap: 12, flexWrap: "wrap" }}>
              <span>
                校准系数 <b className="mono" style={{ fontSize: 17 }}>{result.calibration}</b>
              </span>
              <span className="text-faint" style={{ fontSize: 12.5 }}>
                站点 {result.reported} ÷ 网关加权均倍率 {result.group_avg_multiplier}
                {" · "}
                对账区间 {result.gateway_requests.toLocaleString()} 次请求
              </span>
            </div>
            <p className="field-hint" style={{ margin: "6px 0 10px" }}>
              {result.notice}
            </p>
            <Button variant="primary" onClick={() => void applyCalibration()} disabled={busy}>
              写入账号校准系数
            </Button>
          </div>
        )}

        {records.length > 0 && (
          <div>
            <div className="text-faint" style={{ fontSize: 11.5, marginBottom: 4 }}>
              最近的对账记录
            </div>
            <div className="table-wrap" style={{ maxHeight: 180, overflowY: "auto" }}>
              <table className="data">
                <thead>
                  <tr>
                    <th>模型</th>
                    <th>站点报值</th>
                    <th>系数</th>
                    <th>请求数</th>
                    <th>时间</th>
                  </tr>
                </thead>
                <tbody>
                  {records.map((record) => (
                    <tr key={record.id}>
                      <td className="mono" style={{ fontSize: 12 }}>
                        {record.logical_model}
                      </td>
                      <td className="mono">{record.reported}</td>
                      <td className="mono cell-strong">{record.calibration}</td>
                      <td className="mono">{record.gateway_requests}</td>
                      <td className="text-faint" style={{ fontSize: 12 }}>
                        {new Date(record.created_at * 1000).toLocaleString()}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        )}
      </div>
    </Modal>
  );
}
