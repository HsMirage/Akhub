/**
 * 请求趋势迷你图（§6.2）：近 24 小时按小时聚合的请求量。
 *
 * 纯 SVG 手写，不引入图表库：这里只需要柱状图 + 悬停信息，一个组件就够，
 * 引入图表库会为这点需求多背几十 KB。
 *
 * 口径：柱高 = 该小时网关收到的请求数；柱子里深色部分 = 2xx 成功，
 * 浅色部分是失败（`requests - success`）。
 */
import type { TrendPoint } from "../lib/types";

function hourLabel(unixSeconds: number): string {
  const date = new Date(unixSeconds * 1000);
  return `${String(date.getHours()).padStart(2, "0")}:00`;
}

export function TrendChart({ points }: { points: TrendPoint[] }) {
  if (points.length === 0) {
    return (
      <div className="trend-empty text-faint">窗口内没有请求数据</div>
    );
  }

  const peak = Math.max(...points.map((point) => point.requests));
  const totalRequests = points.reduce((sum, point) => sum + point.requests, 0);
  const totalSuccess = points.reduce((sum, point) => sum + point.success, 0);
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

  return (
    <div className="trend-wrap">
      <svg
        className="trend-chart"
        viewBox={`0 0 ${width} ${height}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`近 ${points.length} 小时请求趋势，共 ${totalRequests} 次请求、${totalSuccess} 次成功，峰值 ${peak} 次/小时`}
      >
        {/* 每根柱子两层：先画总请求，再叠成功部分（顶部对齐到柱高）。 */}
        {points.map((point, index) => {
          const x = index * 10 + 1;
          const barWidth = 8;
          const totalHeight = point.requests === 0 ? 0 : (point.requests / peak) * height;
          const successHeight =
            point.requests === 0 ? 0 : (point.success / peak) * height;
          const failureHeight = totalHeight - successHeight;
          return (
            <g key={point.bucket_start}>
              <title>
                {`${hourLabel(point.bucket_start)} · ${point.requests} 次请求 / ${point.success} 成功`}
              </title>
              {/* 失败部分（含 4xx/5xx 与非 2xx） */}
              <rect
                x={x}
                y={height - failureHeight}
                width={barWidth}
                height={failureHeight}
                className="trend-bar-failure"
              />
              {/* 成功部分 */}
              <rect
                x={x}
                y={height - totalHeight}
                width={barWidth}
                height={successHeight}
                className="trend-bar-success"
              />
              {/* 零请求的小时留一条极细基线，避免看起来像断档 */}
              {point.requests === 0 && (
                <rect
                  x={x}
                  y={height - 0.5}
                  width={barWidth}
                  height={0.5}
                  className="trend-bar-empty"
                />
              )}
            </g>
          );
        })}
      </svg>
      <div className="trend-foot text-faint">
        <span>
          按小时聚合：共 {totalRequests.toLocaleString()} 次请求，
          {totalSuccess.toLocaleString()} 次成功
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
