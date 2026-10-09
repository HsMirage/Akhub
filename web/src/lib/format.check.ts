import { formatTokenPair, outputUsageNote } from "./format.js";

function equal(actual: unknown, expected: unknown) {
  if (actual !== expected) throw new Error(`expected ${expected}, got ${actual}`);
}

equal(formatTokenPair(123, null), "123 / 未知");
equal(formatTokenPair(null, 5), "未知 / 5");
equal(formatTokenPair(null, null), "未知 / 未知");
equal(formatTokenPair(123, 0), "123 / 0");
equal(outputUsageNote(null, true, "client_gone"), "输出用量未知 · 未完成统计");
equal(outputUsageNote(null, true, "upstream_timeout"), "输出用量未知 · 未完成统计");
equal(outputUsageNote(null, true, null), "输出用量未知 · 上游未上报");
equal(outputUsageNote(0, true, null), null);
