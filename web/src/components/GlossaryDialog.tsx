/** 术语表：把控制台里最容易劝退新用户的领域概念集中解释一遍。 */
import { Modal } from "./ui";

const GROUPS: { title: string; terms: { term: string; desc: string }[] }[] = [
  {
    title: "配置链路",
    terms: [
      {
        term: "分组",
        desc: "调度的硬边界。一个分组拥有一把下游 Key、一套账号/模型/目标，跨分组不会互相调度。",
      },
      {
        term: "下游 Key",
        desc: "客户端调用 Akhub 时使用的密钥，只在本分组内有效；完整值仅在创建或重置时显示一次。",
      },
      {
        term: "上游账号",
        desc: "一套真实的上游凭据（Base URL + API Key）。同一把 Key 要用在两个分组时，请复制成两个账号。",
      },
      {
        term: "逻辑模型",
        desc: "下游看到的模型名。上游模型名不同、但你填了相同“下游模型名”的模型会合并成同一个模型显示；它由账号的模型管理自动维护，不需要单独创建。",
      },
      {
        term: "调度目标",
        desc: "「逻辑模型 + 账号 + 具体上游模型」的组合，由账号模型目录自动生成；它不单独配置，调度视图只展示评分与可用性。",
      },
      { term: "候选过滤", desc: "请求记录里展示本次调度淘汰了哪些目标、原因是什么。" },
    ],
  },
  {
    title: "调度规则",
    terms: [
      {
        term: "账号优先级与层",
        desc: "优先级只配置在账号上（默认 0，越大越优先）。同值的账号同属一层；层内按综合评分与可用性分配流量，跨层严格保底。",
      },
      {
        term: "调度权重",
        desc: "只决定同一层内怎么分流量，不会让低优先级层越过高优先级层。四个维度之和必须是 100。",
      },
      {
        term: "粘性绑定",
        desc: "同一会话的后续请求优先落在上次成功的上游，以提高缓存命中；粘性等待过久仍会切换。",
      },
      {
        term: "能力降级",
        desc: "故障切换时只允许丢弃白名单内能力（thinking 块、协议独有采样参数）；工具、图片与结构化输出永不丢弃。",
      },
      { term: "半开试运行", desc: "目标熔断后先放少量流量探测，成功则恢复，失败则继续冷却。" },
    ],
  },
  {
    title: "成本口径",
    terms: [
      {
        term: "倍率",
        desc: "上游对每次调用的计费折扣系数，不是价格。倍率只能在同一逻辑模型内部比较，跨模型相加会失真。",
      },
      { term: "校准系数", desc: "编码「站 A 的 x1 约等于站 B 的 x0.7」这类跨站点差异；有效倍率 = 上游倍率 × 校准系数。" },
      { term: "分组倍率上限", desc: "绝对红线：有效倍率高于它的目标会被暂停，等于它仍然允许调用。" },
      { term: "宽限期", desc: "自动倍率刷新失败后仍可调度的缓冲时间；超过后倍率未知的目标会被硬停。" },
      { term: "探针", desc: "Akhub 定期访问上游、获取倍率与模型目录的行为。探针故障会被判定为系统性故障。" },
    ],
  },
  {
    title: "运行与计量",
    terms: [
      { term: "RPM / TPM / 并发", desc: "每分钟请求数、每分钟 Token 数与同时在途数；留空表示不限。TPM 按请求体保守估算。" },
      { term: "保留期", desc: "请求元数据落库的天数。设为 0 表示不写历史明细，总览指标只覆盖当天且重启清零。" },
      { term: "在途 / 排队", desc: "在途是已经发出上游、尚未结束的请求；排队是等待调度名额的请求。" },
      { term: "配置版本", desc: "用于发现多个标签页同时修改配置。保存时版本不一致会提示重新加载，避免互相覆盖。" },
    ],
  },
];

export function GlossaryDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal open={open} onClose={onClose} title="术语表" size="lg">
      <div className="glossary">
        {GROUPS.map((group) => (
          <section key={group.title} className="glossary-group">
            <h3 className="glossary-title">{group.title}</h3>
            <dl className="glossary-list">
              {group.terms.map((item) => (
                <div key={item.term} className="glossary-item">
                  <dt>{item.term}</dt>
                  <dd>{item.desc}</dd>
                </div>
              ))}
            </dl>
          </section>
        ))}
      </div>
    </Modal>
  );
}
