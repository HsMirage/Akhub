// 纯逻辑的行为断言：仓库没有前端测试框架，这里用 tsc + node 直接跑。
declare const process: { exit(code: number): void };

// 注意后缀 `.js`：编译产物的模块解析要求显式扩展名。
import { splitPastedKeys } from "./key-paste.js";

let failed = 0;
const check = (name: string, got: unknown, want: unknown) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) {
    failed++;
    console.log("FAIL", name, "| got", JSON.stringify(got), "| want", JSON.stringify(want));
  } else {
    console.log("ok  ", name);
  }
};
const keys = (d: { api_key: string }[]) => d.map((x) => x.api_key);
// 真实 Key 都是几十个字符；用短串测会把"看起来像凭据"的判据测歪。
const K = (s: string) => "sk-proj-" + s.padEnd(24, "x");

check("十行换行", keys(splitPastedKeys([K("a1"), K("a2"), K("a3")].join("\n"))), [K("a1"), K("a2"), K("a3")]);
check("CRLF", keys(splitPastedKeys(K("b1") + "\r\n" + K("b2"))), [K("b1"), K("b2")]);
check("逗号一行", keys(splitPastedKeys([K("c1"), K("c2")].join(","))), [K("c1"), K("c2")]);
check("混用与空行", keys(splitPastedKeys("\n" + K("d1") + "\n\n , " + K("d2") + " ,\n")), [K("d1"), K("d2")]);
check(
  "空格连起来的多个 Key（单行输入框吃换行的形态）",
  keys(splitPastedKeys([K("e1"), K("e2"), K("e3")].join(" "))),
  [K("e1"), K("e2"), K("e3")],
);
check(
  "标签 + Key",
  splitPastedKeys("站点A " + K("f1")).map((d) => [d.label, d.api_key]),
  [["站点A", K("f1")]],
);
check("两个裸 Key 不误判成标签", keys(splitPastedKeys(K("g1") + " " + K("g2"))), [K("g1"), K("g2")]);
check(
  "标签里带空格则不拆（判据是恰好两个词）",
  keys(splitPastedKeys("站点 A " + K("h1"))),
  ["站点 A " + K("h1")],
);
check(
  "Key 自身带空格时整行保留（宁可让人手工拆）",
  keys(splitPastedKeys("sk short " + K("i1"))),
  ["sk short " + K("i1")],
);
check("单行原样", keys(splitPastedKeys(K("j1"))), [K("j1")]);
check("空输入", keys(splitPastedKeys("   \n  ")), []);
check("不去重", keys(splitPastedKeys(K("k1") + "\n" + K("k1"))), [K("k1"), K("k1")]);
check("逗号两侧空白被丢掉", keys(splitPastedKeys(" " + K("l1") + " , " + K("l2") + " ")), [K("l1"), K("l2")]);
check("新行默认启用且无限额", [splitPastedKeys(K("m1"))[0]?.enabled, splitPastedKeys(K("m1"))[0]?.rpm], [true, ""]);

if (failed > 0) {
  console.log("");
  console.log(failed + " 条失败");
  process.exit(1);
}
console.log("");
console.log("全部通过");