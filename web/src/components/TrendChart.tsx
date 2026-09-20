/**
 * 请求趋势迷你图（§6.2）：近 24 小时按小时聚合的请求量。
 *
 * 纯 SVG 手写，不引入图表库：这里只需要柱状图 + 悬停/点击信息，一个组件就够，
 * 引入图表库会为这点需求多背几十 KB。
 *
 * 口径：柱高 = 该小时网关收到的请求数；柱子里深色部分 = 2xx 成功，
 * 浅色部分是失败（`requests - success`）。
 *
 * 可读性约定：图例与数值必须直接可见，不能只藏在 title 里——触屏和键盘用户
 * 看不到 title（§可访问性）。
 */
import { useState } from "react";
import type { TrendPoint } from "../lib/types";

function hourLabel(unixSeconds: number): string {
  const date = new Date(unixSeconds * 1000);
  return `${String(date.getHours()).padStart(2, "0")}:00`;
}

export function TrendChart({ points }: { points: TrendPoint[] }) {
  const [active, setActive] = useState<number | null>(null);

  if (points.length === 0) {
    return <div className="trend-empty text-faint">窗口内没有请求数据</div>;
  }

  const peak = Math.max(...points.map((point) => point.requests));
  const totalRequests = points.reduce((sum, point) => sum + point.requests, 0);
  const totalSuccess = points.reduce((sum, point) => sum + point.success, 0);
  const totalFailure = totalRequests - totalSuccess;
  const peakPoint = points.find((point) => point.requests === peak);
  const width = points.length * 10;
  const height = 40;

  if (peak === 0) {
    return (
      <div className="trend-empty text-faint">
        近 {points.length} 小时没有请求进来
      </div>
    );
  }

  const activePoint = active === null ? null : points[active];

  return (
    <div className="trend-wrap">
      <div className="trend-legend">
        <span className="trend-legend-item">
          <i className="trend-swatch is-success" aria-hidden="true" />
          成功 <span className="mono">{totalSuccess.toLocaleString()}</span>
        </span>
        <span className="trend-legend-item">
          <i className="trend-swatch is-failure" aria-hidden="true" />
          失败 <span className="mono">{totalFailure.toLocaleString()}</span>
        </span>
        <span className="spacer" />
        <span className="text-faint">峰值 {peak} 次/小时</span>
      </div>

      <div className="trend-canvas">
        <span className="trend-axis-max mono" aria-hidden="true">
          {peak}
        </span>
        <svg
          className="trend-chart"
          viewBox={`0 0 ${width} ${height}`}
          preserveAspectRatio="none"
          role="img"
          aria-label={`近 ${points.length} 小时请求趋势，共 ${totalRequests} 次请求、${totalSuccess} 次成功、${totalFailure} 次失败，峰值 ${peak} 次/小时`}
        >
          {/* 每根柱子两层：先画总请求，再叠成功部分（顶部对齐到柱高）。 */}
          {points.map((point, index) => {
            const x = index * 10 + 1;
            const barWidth = 8;
            const totalHeight =
              point.requests === 0 ? 0 : (point.requests / peak) * height;
            const successHeight =
              point.requests === 0 ? 0 : (point.success / peak) * height;
            const failureHeight = totalHeight - successHeight;
            const label = `${hourLabel(point.bucket_start)} · ${point.requests} 次请求 / ${point.success} 成功`;
            return (
              <g key={point.bucket_start}>
                <title>{label}</title>
                <rect
                  x={x}
                  y={height - failureHeight}
                  width={barWidth}
                  height={failureHeight}
                  className="trend-bar-failure"
                />
                <rect
                  x={x}
                  y={height - totalHeight}
                  width={barWidth}
                  height={successHeight}
                  className="trend-bar-success"
                />
                {point.requests === 0 && (
                  <rect
                    x={x}
                    y={height - 0.5}
                    width={barWidth}
                    height={0.5}
                    className="trend-bar-empty"
                  />
                )}
                {/* 透明命中区：鼠标悬停、键盘聚焦和触屏点按都能看到同一个数据。 */}
                <rect
                  x={index * 10}
                  y={0}
                  width={10}
                  height={height}
                  fill="transparent"
                  tabIndex={0}
                  role="img"
                  aria-label={label}
                  onMouseEnter={() => setActive(index)}
                  onMouseLeave={() => setActive((current) => (current === index ? null : current))}
                  onFocus={() => setActive(index)}
                  onBlur={() => setActive((current) => (current === index ? null : current))}
                  onClick={() => setActive(index)}
                />
              </g>
            );
          })}
        </svg>
        <span className="trend-axis-zero mono" aria-hidden="true">
          0
        </span>
        {activePoint && (
          <div
            className="trend-tooltip"
            style={{ left: `${((active! + 0.5) / points.length) * 100}%` }}
            role="status"
          >
            <div className="trend-tooltip-title mono">
              {hourLabel(activePoint.bucket_start)}
            </div>
            <div>
              请求 <span className="mono">{activePoint.requests}</span> · 成功{" "}
              <span className="mono">{activePoint.success}</span> · 失败{" "}
              <span className="mono">{activePoint.requests - activePoint.success}</span>
            </div>
          </div>
        )}
      </div>

      <div className="trend-foot text-faint">
        <span>
          按小时聚合：共 {totalRequests.toLocaleString()} 次请求，
          {totalSuccess.toLocaleString()} 次成功，{totalFailure.toLocaleString()} 次失败
        </span>
        {peakPoint && (
          <span>
            峰值 {peak} 次/小时 @ {hourLabel(peakPoint.bucket_start)}
          </span>
        )}
      </div>
    </div>
  );
}
