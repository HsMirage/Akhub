/**
 * 批量粘贴 API Key 的切分规则（§4.2.1）。
 *
 * 单独成文件的原因只有一个：**这是产品行为，必须能被测试钉住**，而不是散在
 * 一个带 React 依赖的组件里。手上有十把 Key 的人不会愿意点十次"添加"，所以
 * "粘进去就自动变成十行"是主路径，这条路径的边界情况必须可回归。
 *
 * 这里不 import 组件里的 `KeyDraft`：那会把 JSX 依赖拖进来，纯逻辑就没法
 * 单独编译与测试了。返回值是这个更小的结构，组件里的草稿类型在它之上加
 * 展示字段。
 */
export interface PastedKeyDraft {
  api_key: string;
  label: string;
  enabled: boolean;
  rpm: string;
  tpm: string;
  max_concurrency: string;
}

/** 一行里"像标签而非凭据"的样子：短、没有空白，也不是 `a=b` 那种键值对。 */
function looksLikeLabel(text: string): boolean {
  return text.length > 0 && text.length <= 32 && !/\s/.test(text) && !/=/.test(text);
}

/** 一行里"像凭据"的样子：没有空格，且至少有 8 个字符。 */
function looksLikeKey(text: string): boolean {
  return text.length >= 8 && !/\s/.test(text);
}

/**
 * 把粘贴进来的一段文本切成要并入池子的 Key 行。
 *
 * 规则：
 *
 * 1. 换行与逗号都是分隔符。很多站点导出的 Key 列表就是逗号分隔的一行，
 *    只认换行会让那整行变成一个"看起来很长"的 Key——那是最糟的结果。
 * 2. **空格连起来的多个 Key 也要拆。** 单行输入框在粘贴时会把换行吃成空格
 *    （HTML 的既有规定），只认换行的话，十把 Key 粘进第一行就会变成一把
 *    "sk-a sk-b sk-c…" 的超长凭据——提交给上游必然鉴权失败，而且很难看出
 *    错在哪里。判据是**每一个词都像凭据**；只要有一个不像，就整段当成一个值。
 * 3. `sk-key1` 这种"标签 + 空格 + Key"的行也照顾：恰好两个词、左边像标签、
 *    右边像凭据时拆成标签 + Key。
 * 4. 空行跳过，行内首尾空白丢掉。
 *
 * 这里**不去重、不排序**：同一次粘贴里出现两把一样的 Key 是用户的决定，
 * 静默吞掉一行会让人对着账对不上。
 */
export function splitPastedKeys(text: string): PastedKeyDraft[] {
  const drafts: PastedKeyDraft[] = [];
  const push = (apiKey: string, label = "") => {
    drafts.push({
      api_key: apiKey,
      label,
      enabled: true,
      rpm: "",
      tpm: "",
      max_concurrency: "",
    });
  };
  for (const raw of text.split(/[\r\n,]+/)) {
    const line = raw.trim();
    if (!line) continue;
    const words = line.split(/\s+/);
    // 整行都是凭据词（≥2 个）：每个词一把。这是"粘贴十把"最常见的样子。
    if (words.length > 1 && words.every(looksLikeKey)) {
      for (const word of words) push(word);
      continue;
    }
    const [first, second] = words;
    if (
      words.length === 2 &&
      first !== undefined &&
      second !== undefined &&
      looksLikeLabel(first) &&
      looksLikeKey(second)
    ) {
      push(second, first);
      continue;
    }
    push(line);
  }
  return drafts;
}
