import { api, ApiError, knownConfigVersion } from "./api.js";

function check(value: boolean, message: string): void {
  if (!value) throw new Error(message);
}

function response(version: number, body = "{}", status = 200): Response {
  return new Response(body, { status, headers: { "x-akhub-config-version": String(version) } });
}

let reply: (value: Response) => void = () => {};
globalThis.fetch = async () => response(10);
await api.overview();
globalThis.fetch = () => new Promise<Response>((resolve) => { reply = resolve; });
const oldRead = api.overview();
globalThis.fetch = async () => response(11);
await api.updateGroup("test", { name: "new" });
reply(response(10));
await oldRead;
check(knownConfigVersion() === 11, "旧读取响应不能覆盖成功保存的新版本");

// 服务重启后版本从 1 开始，不能简单使用 Math.max 永远保留旧版本。
globalThis.fetch = async () => response(1);
await api.overview();
check(knownConfigVersion() === 1, "重启后的新读取必须能接受重置的版本");

globalThis.fetch = async () => response(2, "<html>proxy error</html>");
let invalid: unknown;
try { await api.overview(); } catch (error) { invalid = error; }
check(invalid instanceof ApiError, "无效 JSON 不能作为成功的 null 交给页面");

const sentVersions: string[] = [];
globalThis.fetch = async (_url, init) => {
  sentVersions.push(new Headers(init?.headers).get("x-akhub-config-version") ?? "");
  return response(2 + sentVersions.length);
};
await Promise.all([api.updateGroup("test", { name: "a" }), api.updateGroup("test", { name: "b" })]);
check(sentVersions.join(",") === "2,3", "并发保存必须串行携带上次成功响应的版本");

globalThis.fetch = async () => response(99, JSON.stringify({ error: "保存失败" }), 500);
try { await api.updateGroup("test", { name: "failed" }); } catch { /* 失败不能污染版本。 */ }
check(knownConfigVersion() === 4, "失败响应不能推进配置版本");
globalThis.fetch = async () => new Response(null, { status: 204 });
await api.deleteGroup("test");
globalThis.fetch = async () => { throw new TypeError("network"); };
let network: unknown;
try { await api.overview(); } catch (error) { network = error; }
check(network instanceof ApiError && network.status === 0, "网络异常必须转为可读错误");
console.log("通过：响应乱序、重启、无效数据、串行写入、失败版本保护、204、网络异常");
