# DeepSeek API 调研文档(V4 flash / pro）

> 调研日期:2026-06-17
> 来源:官方文档 https://api-docs.deepseek.com/zh-cn/ + 本机真实请求实测验证
> 用途:为本项目（Rust AI 辅助写作）的 DeepSeek wrapper 模块打底
>
> **标注约定**:
> - `【文档】` = 官方文档明确写明
> - `【实测】` = 本次用真实 API key 打请求验证过
> - `【注意】` = 踩坑点 / wrapper 设计需关注

---

## 目录

1. [概览](#1-概览)
2. [模型清单与定价](#2-模型清单与定价)
3. [端点总览](#3-端点总览)
4. [Chat Completions 详解](#4-chat-completions-详解)
5. [思考模式（thinking）专题](#5-思考模式thinking专题)
6. [Function Calling](#6-function-calling)
7. [JSON 输出](#7-json-输出)
8. [前缀续写（Beta）](#8-前缀续写beta)
9. [FIM 补全（Beta）](#9-fim-补全beta)
10. [上下文硬盘缓存](#10-上下文硬盘缓存)
11. [多轮对话拼接](#11-多轮对话拼接)
12. [温度建议](#12-温度建议)
13. [错误码](#13-错误码)
14. [速率限制与 keep-alive](#14-速率限制与-keep-alive)
15. [wrapper 模块设计建议](#15-wrapper-模块设计建议)

---

## 1. 概览

- **Base URL**:`https://api.deepseek.com`(也可用 `https://api.deepseek.com/v1`,纯为兼容 OpenAI SDK,与模型版本无关)
- **Beta Base URL**:`https://api.deepseek.com/beta`(FIM 补全、前缀续写、`strict` function calling 等 Beta 能力必须走这个)
- **鉴权**:HTTP Header `Authorization: Bearer <DEEPSEEK_API_KEY>` 【实测】
- **协议**:与 OpenAI Chat Completions 高度兼容,可直接用 openai 兼容 SDK,但 V4 的 `thinking` 等字段是 DeepSeek 扩展
- **无状态**:`/chat/completions` 不在服务端保存上下文,多轮要自己拼 `messages`【文档】
- **网络**:`*.deepseek.com` 命中本机 sing-box 的 `geosite-cn` 规则,走**直连**(不走 VPS 节点),国内延迟低

---

## 2. 模型清单与定价

实测 `GET /models` 返回(2026-06-17):

```json
{"object":"list","data":[
  {"id":"deepseek-v4-flash","object":"model","owned_by":"deepseek"},
  {"id":"deepseek-v4-pro","object":"model","owned_by":"deepseek"}
]}
```

| 项 | deepseek-v4-flash | deepseek-v4-pro |
|---|---|---|
| 上下文长度 | 1M tokens | 1M tokens |
| 最大输出 | 384K tokens | 384K tokens |
| 思考模式 | 支持，**默认开启** | 支持，**默认开启** |
| 并发上限 | 2500 | 500 |
| 输入·缓存命中 | ¥0.02 / 百万 tokens | ¥0.025 / 百万 tokens |
| 输入·缓存未命中 | ¥1 / 百万 tokens | ¥3 / 百万 tokens |
| 输出 | ¥2 / 百万 tokens | ¥6 / 百万 tokens |

- 货币:人民币（CNY）；文档未提分时段折扣。
- **定位**:`flash` 便宜快、`pro` 更强更贵。flash 输出比 pro 便宜 3 倍、输入未命中便宜 3 倍 → "flash 打底 + pro 攻坚" 的组合策略成立。
- **【注意】思考模式默认开启**:不显式传 `thinking` 时模型会先产出思考链,既增加延迟也消耗 output 费用与 `max_tokens` 预算。对"要快/要省"的场景务必显式 `thinking.type = disabled`。

### 旧模型名弃用

【文档】`deepseek-chat` 与 `deepseek-reasoner` 两个旧模型名将于 **北京时间 2026/07/24 23:59 弃用**,期间作为兼容分别映射到 `deepseek-v4-flash` 的**非思考模式 / 思考模式**。新代码一律直接用 `deepseek-v4-flash` / `deepseek-v4-pro` + `thinking` 控制。

---

## 3. 端点总览

| 端点 | 方法 | 路径 | Base | 说明 |
|---|---|---|---|---|
| Chat 补全 | POST | `/chat/completions` | 正式 | 核心,文本生成/对话/工具调用 |
| FIM 补全 | POST | `/completions` | **Beta** | 填中补全,仅 `deepseek-v4-pro` |
| 列模型 | GET | `/models` | 正式 | 返回可用模型 id |
| 查余额 | GET | `/user/balance` | 正式 | 账户余额 |

### GET /models 【实测】

```
GET https://api.deepseek.com/models
Authorization: Bearer <key>
```
响应:`{object:"list", data:[{id, object:"model", owned_by:"deepseek"}]}`

### GET /user/balance 【实测】

```json
{"is_available": true,
 "balance_infos": [
   {"currency":"CNY","total_balance":"99.41","granted_balance":"0.00","topped_up_balance":"99.41"}
 ]}
```
字段:`is_available`(账户是否可调用)、`balance_infos[]`(`currency` CNY/USD、`total_balance` 总余额、`granted_balance` 未过期赠送额、`topped_up_balance` 充值余额)。注意金额是**字符串**。

---

## 4. Chat Completions 详解

```
POST https://api.deepseek.com/chat/completions
Authorization: Bearer <key>
Content-Type: application/json
```

### 4.1 请求参数

| 参数 | 类型 | 必填 | 默认 | 范围/取值 | 说明 |
|---|---|---|---|---|---|
| `model` | string | 是 | — | `deepseek-v4-flash` / `deepseek-v4-pro` | 模型 id |
| `messages` | object[] | 是 | — | ≥1 条 | 对话消息列表,见 4.2 |
| `thinking` | object | 否 | 开启 | 见 4.3 | 思考模式控制（V4 扩展） |
| `max_tokens` | integer | 否 | — | — | 单次最大输出 token;**思考模式下思考链也算在内**，见第 5 节 |
| `temperature` | number | 否 | 1 | ≤ 2 | 采样温度,越高越随机 |
| `top_p` | number | 否 | 1 | ≤ 1 | 核采样 |
| `stop` | string / string[] | 否 | — | 最多 16 个 | 命中即停 |
| `response_format` | object | 否 | `{type:"text"}` | `text` / `json_object` | 输出格式,见第 7 节 |
| `stream` | boolean | 否 | false | — | SSE 流式 |
| `stream_options` | object | 否 | — | `{include_usage:bool}` | 仅 `stream=true` 时有效;开了则最后一帧带 usage |
| `tools` | object[] | 否 | — | 最多 128 个 | 工具定义,见第 6 节 |
| `tool_choice` | string / object | 否 | auto | `none`/`auto`/`required`/`{type,function}` | 工具调用策略 |
| `logprobs` | boolean | 否 | — | — | 是否返回 token 对数概率 |
| `top_logprobs` | integer | 否 | — | ≤ 20 | 每位置返回前 N 个候选(需 `logprobs=true`) |
| `user_id` | string | 否 | — | `[a-zA-Z0-9-_]`，≤512 | 自定义用户标识，用于内容安全与并发隔离；勿放 PII |
| ~~`frequency_penalty`~~ | — | — | — | — | **已废弃**，无效果 |
| ~~`presence_penalty`~~ | — | — | — | — | **已废弃**，无效果 |

### 4.2 messages 各角色

- **system**:`{role:"system", content:string, name?:string}`
- **user**:`{role:"user", content:string, name?:string}`
- **assistant**:`{role:"assistant", content:string|null, name?, prefix?:bool, reasoning_content?:string|null}`
  - `prefix`（Beta）:配合 `/beta`，强制模型从给定前缀续写，见第 8 节
  - `reasoning_content`（Beta）:思考模式下做前缀续写时回填的思考链
- **tool**:`{role:"tool", content:string, tool_call_id:string}` — 工具执行结果回传

### 4.3 thinking 对象（V4 关键）【实测】

| 字段 | 类型 | 默认 | 取值 | 说明 |
|---|---|---|---|---|
| `thinking.type` | string | `enabled` | `enabled` / `disabled` | 开/关思考模式 |
| `thinking.reasoning_effort` | string | `high` | `high` / `max` | 思考强度;旧值 `low`/`medium`→`high`，`xhigh`→`max` |

实测两种都生效:
- `{"type":"disabled"}` → 直接出 `content`，无 `reasoning_content`
- `{"type":"enabled","reasoning_effort":"high"}` → 出 `reasoning_content`(思考链) + `content`(最终答案)

### 4.4 响应（非流式）【实测】

实测 `deepseek-v4-flash` + `thinking.type=disabled`:

```json
{
  "id": "73b50326-...",
  "object": "chat.completion",
  "created": 1781679415,
  "model": "deepseek-v4-flash",
  "choices": [{
    "index": 0,
    "message": {"role": "assistant", "content": "你好"},
    "logprobs": null,
    "finish_reason": "stop"
  }],
  "usage": {
    "prompt_tokens": 9,
    "completion_tokens": 1,
    "total_tokens": 10,
    "prompt_tokens_details": {"cached_tokens": 0},
    "prompt_cache_hit_tokens": 0,
    "prompt_cache_miss_tokens": 9
  },
  "system_fingerprint": "fp_8b330d02d0_prod0820_fp8_kvcache_20260402"
}
```

思考模式开启时,`message` 多一个 `reasoning_content` 字段,`usage` 多 `completion_tokens_details.reasoning_tokens`。

完整 `message` 可能字段:
- `role`:`"assistant"`
- `content`:string | null（思考模式下若 token 被思考链耗尽，可能为 `""`）
- `reasoning_content`:string | null（仅思考模式）
- `tool_calls`:`[{id, type:"function", function:{name, arguments(JSON 字符串)}}]`

### 4.5 finish_reason 取值【文档】

| 值 | 含义 |
|---|---|
| `stop` | 自然结束或命中 stop 序列 |
| `length` | 触达 `max_tokens` 或上下文上限 |
| `content_filter` | 命中安全策略被过滤 |
| `tool_calls` | 触发工具调用 |
| `insufficient_system_resource` | 后端资源不足中断生成 |

### 4.6 usage 字段全集【实测 + 文档】

| 字段 | 来源 | 说明 |
|---|---|---|
| `prompt_tokens` | 实测 | 输入 token 总数 |
| `completion_tokens` | 实测 | 输出 token 总数（**含思考链**） |
| `total_tokens` | 实测 | 合计 |
| `prompt_cache_hit_tokens` | 实测 | 输入中命中硬盘缓存的 token（按缓存命中价计费） |
| `prompt_cache_miss_tokens` | 实测 | 输入中未命中的 token（按未命中价计费） |
| `prompt_tokens_details.cached_tokens` | 实测 | OpenAI 兼容字段，等价于命中数 |
| `completion_tokens_details.reasoning_tokens` | 实测 | 思考链 token 数（思考模式才有） |

### 4.7 流式响应（SSE）【文档】

`stream=true` 时返回 `text/event-stream`,每帧:

```
data: {"id":..., "object":"chat.completion.chunk", "created":..., "model":..., "system_fingerprint":..., "choices":[{"index":0, "delta":{...}, "finish_reason":null}]}
```

- `delta` 增量字段:首帧带 `role:"assistant"`，之后 `content` / `reasoning_content` 增量
- 结束标志:`data: [DONE]`
- 开了 `stream_options.include_usage=true` 时，`[DONE]` 前最后一帧带完整 `usage`，其余帧 `usage` 为 null
- **【注意】** 高负载下流式会周期性发 `: keep-alive` 注释行，非流式会发空行,解析时要忽略，见第 14 节

---

## 5. 思考模式（thinking）专题

V4 两个模型都内置思考能力,由 `thinking` 对象控制（见 4.3）。以下规则部分源自旧 `deepseek-reasoner` 文档(官方尚未为 V4 全量更新),但行为一致,结合实测整理:

### 5.1 reasoning_content 与 content
- `reasoning_content`:思考链,与 `content` 同级,**先于** `content` 产出
- `content`:最终答案

### 5.2 【注意·最关键】思考 token 吃 max_tokens 预算
实测:`max_tokens:64` + 思考开启 → 64 token 全被思考链吃光,`content` 为 `""`,`finish_reason:"length"`,`usage.completion_tokens_details.reasoning_tokens=64`。

> **wrapper 必须处理**:思考模式下 `max_tokens` 要给足(留出思考 + 正文)。旧 reasoner 默认 `max_tokens` 32K、上限 64K（含思考）。对写作长文场景，建议显式设较大的 `max_tokens`，并在拿到 `finish_reason=length` 且 `content` 为空时判定为"被思考链截断"。

### 5.3 【注意·最关键】多轮对话不要回传 reasoning_content
【文档】下一轮请求前**必须从 messages 里删除上一轮的 `reasoning_content`**,否则报 400。

> wrapper 在把上一轮 assistant 消息塞回历史时，只保留 `content`（和必要的 `tool_calls`），剥掉 `reasoning_content`。唯一例外是 Beta 前缀续写主动回填思考链的场景（第 8 节）。

### 5.4 思考模式下不支持/无效的参数
【文档（reasoner）】`temperature`、`top_p`、`presence_penalty`、`frequency_penalty` **不生效**（前两个+两个废弃项不报错只是无效）;`logprobs`、`top_logprobs` 会**报错**。

### 5.5 能力矩阵
| 能力 | 思考模式 | 非思考模式 |
|---|---|---|
| Chat 补全 | ✅ | ✅ |
| JSON 输出 | ✅ | ✅ |
| 前缀续写(Beta) | ✅ | ✅ |
| Function Calling | ⚠️ 旧 reasoner 不支持;V4 需实测确认 | ✅ |
| FIM(Beta) | ❌ | 仅 pro |

---

## 6. Function Calling

【文档】流程三步:

**1) 定义工具**（`tools` 数组,最多 128 个）:
```json
{"type":"function","function":{
  "name":"get_weather",
  "description":"Get weather of a location, the user should supply a location first.",
  "parameters":{"type":"object","properties":{
    "location":{"type":"string","description":"The city and state, e.g. San Francisco, CA"}
  },"required":["location"]}
}}
```
- `name`:`[a-zA-Z0-9_-]`，≤64 字符
- `strict`（Beta，需 `/beta`）:`true` 强制 schema 合规;支持 object/string/number/integer/boolean/array/enum/anyOf/$ref/$def

**2) 模型返回 tool_calls**:`message.tool_calls = [{id, type:"function", function:{name, arguments}}]`,`arguments` 是 JSON 字符串,`finish_reason:"tool_calls"`。

**3) 回传结果**:追加一条 `{role:"tool", tool_call_id:<对应 id>, content:"24℃"}`,再请求一次拿自然语言回答。

`tool_choice`:`none`(禁用) / `auto`(默认) / `required`(必须调) / `{"type":"function","function":{"name":"xxx"}}`(指定)。

---

## 7. JSON 输出

【文档】
- 设 `response_format = {"type":"json_object"}`
- **prompt（system 或 user）里必须出现 "json" 字样**,并给一个示例 JSON 结构引导
- 【注意】有概率返回空 `content`，可调 prompt 缓解;`max_tokens` 给足避免 JSON 被截断
- 例:system 给 `EXAMPLE JSON OUTPUT: {...}`,模型按结构输出

---

## 8. 前缀续写（Beta）

【文档】对写作 app 很有用——强制模型从你给的开头继续写。
- Base URL 用 `/beta`
- `messages` **最后一条**设为 `{role:"assistant", content:<前缀>, prefix:true}`,模型从该前缀续写
- 配合 `stop` 控制何时停。例:逼模型只产代码块
```python
messages = [
  {"role":"user","content":"Please write quick sort code"},
  {"role":"assistant","content":"```python\n","prefix":True}
]
# stop=["```"]
```
- 思考模式下可再带 `reasoning_content` 回填思考链(这是 5.3 "不回传"规则的唯一例外)

---

## 9. FIM 补全（Beta）

【文档】填中补全(Fill-In-the-Middle),给前缀 `prompt` + 可选后缀 `suffix`,模型生成中间内容,适合代码补全。
- `POST https://api.deepseek.com/beta/completions`
- **仅 `deepseek-v4-pro`**
- 参数:`model`、`prompt`（必填）、`suffix`、`max_tokens`、`temperature`(0–2,默认1)、`top_p`(0–1,默认1)、`stop`(≤16)、`stream`、`stream_options`、`logprobs`(≤20，注意这里是 integer)、`echo`(回显 prompt)
- 响应:`object:"text_completion"`,`choices[].text` + `finish_reason`,`usage` 同 chat
- 【注意】此模式与本写作项目关系不大(偏代码),wrapper 可暂不实现

---

## 10. 上下文硬盘缓存

【文档】**默认对所有用户开启,无需改代码**。命中前缀缓存的输入 token 走"缓存命中价"(flash ¥0.02 / pro ¥0.025，对比未命中 ¥1 / ¥3,**便宜约 50/120 倍**)。
- 缓存以请求边界、公共前缀、长文本固定间隔切分成单元
- 命中数看 `prompt_cache_hit_tokens`,未命中看 `prompt_cache_miss_tokens`
- 未用缓存数小时～数天自动清除;构建耗时秒级
- 仅作用于输入;输出仍按推理算,不影响质量
- **【wrapper 优化点】把稳定不变的内容(system prompt、长设定、已定稿正文)放在 messages 前部,易变内容放后面,最大化前缀命中率,显著省钱**

---

## 11. 多轮对话拼接

【文档】API 无状态,每轮把完整历史传回:
1. 发 `messages=[user1]` → 拿到 `assistant1`
2. 把 `assistant1` 追加进 `messages`,再追加 `user2`,整体再发
3. 依此滚动
- 【注意】追加 assistant 历史时**剥掉 `reasoning_content`**(见 5.3)

---

## 12. 温度建议

【文档】`temperature` 默认 1.0,按场景调:

| 场景 | 建议温度 |
|---|---|
| 代码生成 / 数学解题 | 0.0 |
| 数据抽取 / 分析 | 1.0 |
| 通用对话 | 1.3 |
| 翻译 | 1.3 |
| 创意写作 / 诗歌 | 1.5 |

> 本项目"辅助写作"主场景偏创意,默认可取 1.3～1.5;但**思考模式下 temperature 不生效**(见 5.4),靠 `reasoning_effort` 调质量。

---

## 13. 错误码

【文档】

| HTTP | 含义 | 处理 |
|---|---|---|
| 400 | 请求体格式错误 | 按报错改 body |
| 401 | API key 错误,认证失败 | 检查/换 key |
| 402 | 余额不足 | 充值 |
| 422 | 请求体参数错误 | 按报错调参数 |
| 429 | 速率(TPM/RPM)或并发达上限 | 退避重试/降速 |
| 500 | 服务器内部故障 | 稍后重试,持续则联系支持 |
| 503 | 服务器负载过高 | 稍后重试 |

> wrapper 重试策略:401/402/422/400 不重试(配置/逻辑问题);429/500/503 指数退避重试。

---

## 14. 速率限制与 keep-alive

【文档】
- **并发上限**:flash 2500 / pro 500;超出返回 429。可申请提额。
- 传 `user_id` 时按 user 维度隔离并发(空 id 也算一个独立实体)
- **keep-alive**:等待推理时连接保持——非流式持续发**空行**,流式持续发 `: keep-alive` SSE 注释。解析时都要忽略,不影响 JSON 解析。
- **超时**:10 分钟内未开始推理,服务端关连接。
- 【wrapper】用支持 SSE 注释/空行的解析器；客户端读超时设 ≥10 分钟或自行心跳。

---

## 15. wrapper 模块设计建议

> 此节是为后续 Rust wrapper 模块预先记录的规划,不着急实现。

### 15.1 依赖选型
- HTTP:`reqwest`（带 `rustls-tls`，`json`，`stream` feature）
- 异步:`tokio`
- 序列化:`serde` / `serde_json`
- SSE 流式:`reqwest` bytes stream + 手写行解析,或 `eventsource-stream`（注意要容忍 `: keep-alive` 注释与空行）
- 配置:`dotenvy` 读 `.env` 里的 `DEEPSEEK_API_KEY` / `DEEPSEEK_BASE_URL`
- 错误:`thiserror`

### 15.2 类型骨架（草案）
```rust
enum Model { V4Flash, V4Pro }          // -> "deepseek-v4-flash" / "deepseek-v4-pro"

enum Thinking {
    Disabled,
    Enabled { effort: Effort },        // High | Max
}

struct ChatRequest {
    model: Model,
    messages: Vec<Message>,
    thinking: Option<Thinking>,        // None = 走服务端默认(开启)
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: Option<Vec<String>>,
    response_format: Option<ResponseFormat>,  // Text | JsonObject
    stream: bool,
    tools: Option<Vec<Tool>>,
    tool_choice: Option<ToolChoice>,
    user_id: Option<String>,
}

// Message:序列化时按 role 走,assistant 回历史要能选择性剥离 reasoning_content
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    prompt_cache_hit_tokens: u32,
    prompt_cache_miss_tokens: u32,
    reasoning_tokens: Option<u32>,     // from completion_tokens_details
}
```

### 15.3 必须内建的"坑"处理(对应正文标注)
1. **多轮剥离 `reasoning_content`**:提供 `push_assistant_turn()`,默认丢弃 `reasoning_content`(5.3)。
2. **思考模式 max_tokens 给足**:思考链算进 `completion_tokens` 并吃预算;flash/pro 默认开思考,wrapper 应有合理默认 `max_tokens` 或强制调用方设置;识别"`content` 空 + `finish_reason=length`"为思考截断(5.2)。
3. **思考模式忽略 temperature/top_p**:开思考时不要警告式发这些;`logprobs`/`top_logprobs` 在思考模式会报错,要拦(5.4)。
4. **SSE 容错**:跳过 `: keep-alive` 注释行和空行,识别 `[DONE]`,`include_usage` 末帧取 usage(4.7/14)。
5. **重试策略**:429/500/503 指数退避;400/401/402/422 直接抛错(13)。
6. **缓存友好拼装**:静态内容(system/设定)前置,提升 `prompt_cache_hit_tokens`(10)。
7. **金额是字符串**:`/user/balance` 的余额字段要按字符串解析再转 `Decimal`/`f64`(3)。
8. **Beta 能力切 base url**:FIM、前缀续写、`strict` function calling 需 `/beta`(8/9/6)。

### 15.4 flash + pro 组合策略(给写作 app 的建议)
- **flash 打底**:草稿生成、续写、改写、补全、分类/抽取等高频低难任务,优先 flash + `thinking.disabled` 求快求省。
- **pro 攻坚**:整体结构规划、长文润色定稿、复杂逻辑/一致性校验等,用 pro + `thinking.enabled`。
- **思考开关随任务**:要质量开 `enabled`(必要时 `reasoning_effort:max`),要速度/低成本关 `disabled`。
- 成本直觉:flash 输出 ¥2 vs pro 输出 ¥6(3 倍),输入未命中 ¥1 vs ¥3;高频路径用 flash 能省一个量级。

---

## 附:待补充/待实测项
- V4 思考模式下 Function Calling 是否支持(旧 reasoner 不支持,V4 官方文档未明确,需真机验证)。
- `thinking.reasoning_effort` 各档对延迟/质量/成本的实际影响。
- 流式 + 思考模式下 `reasoning_content` 的分帧细节。
- `response_format` 是否支持更严格的 `json_schema`(目前文档只见 `json_object`)。





