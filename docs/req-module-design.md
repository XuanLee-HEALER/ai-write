# req module 设计定稿(v0）

> 状态:**已定稿,review 已回(见末尾「review 结论」),依赖版本已锁,未动代码**
> 日期:2026-06-17
> 范围:DeepSeek wrapper 的最底层 —— 无状态 `Client`。单 crate,作为 `mod req`。
> 上游依据:`docs/deepseek-api-research.md`(API 实测调研)、`docs/design.md`(原始思考)。

---

## 0. 决策速查表

| # | 决策 | 选择 | 理由 |
|---|---|---|---|
| D1 | 本层范围 | **只做无状态 `Client`** | 一次调用=一次 HTTP;Session/worker 是下一层 |
| D2 | 同步/异步 | **两形态:sync 基底 + async 作 feature** | — |
| D3 | sync 后端 | **`ureq`** | sync 构建真正不拖 tokio |
| D4 | async 后端 | **`reqwest`**(`async` feature) | 异步生态标配 |
| D5 | 默认 feature | **`default = ["blocking"]`** | 贴合"async 是 feature";不开 async 不拉 tokio |
| D6 | 代码复用 | **纯核心 + 两层薄 IO,手写** | 不用 `maybe_async`(流式两端类型本质不同) |
| D7 | 流式接口 | **sync `Iterator` / async `Stream`,独立方法** | 返回类型分叉,各自干净 |
| D8 | 发送前校验 | **静态白名单 + `validate()` 拦已知非法组合,其余信 422** | 离线可用、零网络开销 |
| D9 | model 类型 | **纯枚举 `#[non_exhaustive] enum Model { V4Flash, V4Pro }`,随版本扩展** | 枚举即白名单;`#[non_exhaustive]` 让加变体兼容;观测侧 id 留 String |
| D10 | finish_reason / 5xx | **只上报类型化结果,不自动续写/重试** | 重试、限速是上层的事 |
| D11 | token 计量 | **只透传服务端 `usage`,不写估算函数** | 记账只信真值 |
| D12 | `Error` 公开类型 | **库无关 + 语义化映射**:`Api`/`Transport`/`Decode`/`InvalidRequest`/`Config`,库错误在 adapter 真实翻译 | 友好、双后端归一、换库不破坏 API(详见 §6) |
| D13 | 价目表 | **不进 wrapper** | 会过期;tokens→¥ 是上层记账策略 |
| D14 | tools | **只放类型 + builder**,执行/回填 loop 不在本层 | 那是 harness |

---

## 1. 范围边界

### 本层做
- 类型化 request / response + builder
- 友好错误模型:HTTP 状态码 → 语义化 `Error` 变体
- SSE 清洗:跳空行 / `: keep-alive` 注释,`[DONE]` 收尾
- `finish_reason` / `usage` 类型化**只上报**
- `list_models()` / `balance()` 工具方法
- `Model` 静态白名单 + 一个小 `validate()`(只拦已知非法参数组合)
- 同步 + 异步两套调用形态

### Park 到下一层(本层不碰)
- `Session` / worker、上下文管理、token 控制与裁剪
- `user_id` 语义与持久化 id generator
- `docs/design.md` 第 8 条的所有权 / scope 内存回收
- 思考开关挂 session、计费聚合、价目表、tools 执行 loop
- 安全、限速、自动重试

### 形态
单 crate,wrapper 落在 `src/req/`,对外 `pub mod req;`。harness(binary / 其它 crate)之后另起,不在本次。

---

## 2. 同步 / 异步双形态

这个 wrapper 的 IO 面极小(就 `chat` / `chat_stream` / `list_models` / `balance` 四个),所以做成 **纯核心 + 两层薄 IO 适配**,共享远大于重复。

### 2.1 纯核心(与 sync/async 无关,永远编译)
- 所有 `types/`(serde request/response)
- `Error` + `(status, body) → Error` 映射
- `Model` + `validate()`
- URL / header / body 组装
- **SSE 行解码** `decode_line(&str) -> LineEvent`:判定一行是 `Data(json)` / `Comment` / `Blank` / `Done`,纯函数,两端共用

### 2.2 各写一份(薄,每端约 4 个函数)
| 调用 | sync(ureq) | async(reqwest) |
|---|---|---|
| `chat` | `.call()` 取 body → 核心解析 | `.send().await` → 核心解析 |
| `list_models` / `balance` | 同上 GET | 同上 GET |
| `chat_stream` | `.into_reader()` → `BufRead::read_line` → `decode_line` → **`impl Iterator`** | `.bytes_stream()` → 缓冲拆行 → `decode_line` → **`impl Stream`** |

两端流式都只是"把字节拆成行,喂给同一个 `decode_line`"。差异仅在字节怎么来(阻塞 `Read` vs 异步 `Bytes` 流)。

### 2.3 为什么不用 `maybe_async`
`maybe_async` 宏能在 `chat`/`list`/`balance` 上消重,但**流式两端类型本质不同**(`Iterator` vs `Stream`、阻塞 `Read` vs 异步字节流),宏桥不过去。强上反而把可读性搞没。手写两层薄壳更符合"简单数据结构 + 简单实现"。

### 2.4 feature / Cargo 草图

```toml
[features]
default = ["blocking"]
blocking = ["dep:ureq"]
async    = ["dep:reqwest", "dep:futures-core", "dep:async-stream"]

[dependencies]
serde        = { version = "1.0.228", features = ["derive"] }
serde_json   = "1.0.150"
thiserror    = "2.0.18"
# sync 后端:ureq 3.x 默认即 rustls、无 tokio,用默认 features
ureq         = { version = "3.3.0", optional = true }
# async 后端:关默认(去 native-tls)走 rustls(0.13 把 feature 从 rustls-tls 改名为 rustls)
reqwest      = { version = "0.13.4", optional = true, default-features = false, features = ["rustls","json","stream"] }
futures-core = { version = "0.3.32", optional = true }
async-stream = { version = "0.3.6", optional = true }
```
> 版本为 2026-06-17 crates.io 最新 stable(toolchain rustc/cargo 1.96)。**落地一律用 `cargo add` 写入,不手改 `Cargo.toml`**;`cargo add` 会自动锁当时最新。
> ureq 3.x / reqwest 0.13 的具体 TLS feature 名与错误变体,scaffold 时对着 docs.rs 核一遍(见 §6.3)。
> `from_env()` 只读环境变量,**不依赖 dotenvy**;加载 `.env` 是调用方(main)的事。

### 2.5 关键认知(已确认)
把 async 设成可选 feature 的真实收益是 **sync 构建不拉 tokio**。这只有 sync 端用 `ureq` 才成立 —— `reqwest::blocking` 内部仍会起 tokio runtime。故 D3 选 `ureq`。

---

## 3. 模块布局(单 crate)

```
src/
  lib.rs                 # pub mod req;
  req/
    mod.rs               # 对外 re-export;feature=async 时导出异步 Client,blocking 始终在
    error.rs             # 纯核心:Error(后端无关) + status->Error 映射
    model.rs             # 纯核心:Model enum + validate()
    protocol.rs          # 纯核心:URL/header/body 组装 + SSE decode_line   ← 重点
    types/
      mod.rs
      request.rs         # ChatRequest + builder、Message、Thinking、ResponseFormat、Tool、ToolChoice
      response.rs        # ChatResponse、Choice、Chunk(delta)、Usage、FinishReason
      common.rs          # ModelInfo、Balance
    blocking.rs          # cfg(feature="blocking"):ureq 实现 blocking::Client
    client_async.rs      # cfg(feature="async"):reqwest 实现异步 Client
```

`async` 是 Rust 关键字不能直接当模块名(要 `r#async`),故异步实现文件叫 `client_async.rs`,对外类型放 `req::Client`;同步对齐 reqwest 习惯放 `req::blocking::Client`。

---

## 4. 公开 API 草图

> 下面 sync / async 两端**签名对称**,只差 `async` / `.await` 和 `chat_stream` 的返回类型。

### 4.1 构造
```rust
let client = req::blocking::Client::from_env()?;        // 读 DEEPSEEK_API_KEY / DEEPSEEK_BASE_URL
let client = req::blocking::Client::builder()
    .api_key(k)
    .base_url(url)            // 默认 https://api.deepseek.com
    .timeout(d)              // 建议 >= 10min(服务端 10min 不推理才断)
    .build()?;
// async 端:req::Client,同名方法,带 .await
```

### 4.2 工具方法
```rust
let models:  Vec<ModelInfo> = client.list_models()?;    // GET /models
let balance: Balance        = client.balance()?;        // GET /user/balance;金额是 String 原样给
```

### 4.3 请求 builder(build() 内跑 validate)
```rust
let r = ChatRequest::builder(Model::V4Flash)
    .system("…")
    .user("…")
    .thinking(Thinking::Disabled)                       // Option;None=服务端默认(开)
    .max_tokens(4096)
    .temperature(1.3)
    .response_format(ResponseFormat::JsonObject)
    .stop(["```"])
    .build()?;                                           // 思考+logprobs 等非法组合本地拦
```

### 4.4 非流式
```rust
let resp: ChatResponse = client.chat(&r)?;              // async: client.chat(&r).await?
resp.content();                                         // helper:取首 choice 的 content
resp.reasoning();                                       // helper:取 reasoning_content(Option)
resp.finish_reason();                                   // FinishReason
let _ = resp.usage;                                     // 原始 usage,不加工
```

### 4.5 流式(独立方法,独立返回类型)
```rust
// sync:
let it = client.chat_stream(&r)?;                       // impl Iterator<Item = Result<Chunk, Error>>
for chunk in it { let c = chunk?; /* c.delta_content() … */ }

// async:
let mut s = client.chat_stream(&r).await?;              // impl Stream<Item = Result<Chunk, Error>>
while let Some(chunk) = s.next().await { … }
```
流已清洗:空行 / `: keep-alive` 已跳过,`[DONE]` 自然收尾;开 `stream_options.include_usage` 时 `usage` 落在末帧 Chunk。

---

## 5. 类型设计

### 5.1 Model
```rust
#[non_exhaustive]
pub enum Model { V4Flash, V4Pro }      // 随版本新增变体
// as_str / Display -> "deepseek-v4-flash" / "deepseek-v4-pro"
```
- **纯枚举,无逃生口**:`Model` 就是 lib 维护的白名单;DeepSeek 出新模型 → lib 加变体 + 发版。
- **`#[non_exhaustive]`**:让"加变体"成为**兼容变更**(强制下游写 `_ =>` 兜底臂),正好匹配"随版本扩展";否则加变体会破坏下游的穷尽 match。
- **只用于"请求侧"**(客户端选模型)。"观测侧"从服务端收到的 model id —— `ModelInfo.id`、`ChatResponse.model` —— **保持 `String` 原样**,避免服务端冒出枚举不认识的 id 时反序列化失败。
- 序列化:内部穷尽 match → 字符串;不提供从任意字符串 `FromStr`(没这个需求,观测侧用 String)。

### 5.2 Message(struct + role 字段)
```rust
pub enum Role { System, User, Assistant, Tool }   // serialize -> "system"/"user"/"assistant"/"tool"

pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")] pub content: Option<String>,            // assistant 纯 tool_calls 时可 None
    #[serde(skip_serializing_if = "Option::is_none")] pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub reasoning_content: Option<String>,  // 仅前缀续写回填
    #[serde(skip_serializing_if = "Option::is_none")] pub tool_calls: Option<Vec<ToolCall>>,  // assistant
    #[serde(skip_serializing_if = "Option::is_none")] pub tool_call_id: Option<String>,       // tool
    #[serde(skip_serializing_if = "Option::is_none")] pub prefix: Option<bool>,               // Beta 前缀续写(assistant)
}
// 便捷构造:Message::system("…") / ::user("…") / ::assistant("…") / ::tool(id, "…")
```
- 扁平 struct(OpenAI 风格):serde 简单,`skip_serializing_if` 不发多余 null。
- 代价:非法字段组合在类型上可表达(如 system 带 tool_calls),靠**构造 helper** 只填合法字段来约束。
- 多轮注意:把上一轮 assistant 塞回历史时**默认剥掉 `reasoning_content`**(否则 400);本层给"干净 clone"helper,不持有历史(那是下一层)。

### 5.3 Thinking
```rust
pub enum Thinking { Disabled, Enabled { effort: Effort } }
pub enum Effort  { High, Max }
// serialize -> {"type":"disabled"} / {"type":"enabled","reasoning_effort":"high"|"max"}
```

### 5.4 输出格式 / 工具(类型 only)
```rust
pub enum ResponseFormat { Text, JsonObject }
pub struct Tool { /* type=function, function: {name, description, parameters(JSON Schema)} */ }
pub enum ToolChoice { None, Auto, Required, Function { name: String } }
```

### 5.5 响应
```rust
pub struct ChatResponse {
    pub id: String, pub model: String, pub created: i64,
    pub system_fingerprint: Option<String>,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}
pub struct Choice { pub index: u32, pub message: RespMessage, pub finish_reason: FinishReason, /* logprobs */ }
pub struct RespMessage { pub role: String, pub content: Option<String>,
                         pub reasoning_content: Option<String>, pub tool_calls: Option<Vec<ToolCall>> }

pub struct Chunk { /* 流式:choices[].delta 增量 + 末帧 usage */ }

pub struct Usage {
    pub prompt_tokens: u32, pub completion_tokens: u32, pub total_tokens: u32,
    pub prompt_cache_hit_tokens: u32, pub prompt_cache_miss_tokens: u32,
    pub reasoning_tokens: Option<u32>,   // 来自 completion_tokens_details.reasoning_tokens
}

pub enum FinishReason { Stop, Length, ContentFilter, ToolCalls, InsufficientSystemResource, Unknown(String) }
```
> `reasoning_tokens` 算进 `completion_tokens` 且吃 `max_tokens` 预算(实测)。本层只如实给数,不解释、不补救。

### 5.6 工具方法返回
```rust
pub struct ModelInfo { pub id: String, pub owned_by: String }
pub struct Balance { pub is_available: bool, pub infos: Vec<BalanceInfo> }
pub struct BalanceInfo { pub currency: String,
                         pub total_balance: String, pub granted_balance: String, pub topped_up_balance: String }
// 金额保持 String,要不要转 Decimal 交上层
```

---

## 6. 错误模型(库无关 + 真实映射）

错误有两类来源:① HTTP 库(ureq/reqwest)抛的**传输层**错;② 服务端返回的**非 2xx 状态**错。两者都归一到一个**语义化、库无关**的公开 `Error`。**粒度在这一层设计**;把 `ureq::Error` / `reqwest::Error` 真正翻译进来是脏活,放在各后端 adapter 里,纯核心只持有目标类型、永不出现库类型名。

### 6.1 公开类型(粒度在此定)

```rust
pub enum Error {
    Api(ApiError),                 // 服务端非 2xx,按状态码语义化
    Transport(TransportError),     // 连不上 / 断了 / 超时,与库无关
    Decode { context: &'static str, source: serde_json::Error }, // 解析失败;context="chat"/"models"/"balance"/"sse"
    InvalidRequest(String),        // 发送前 validate() 拦下
    Config(String),                // 缺 key、base_url 非法等
}

pub struct ApiError { pub status: u16, pub kind: ApiErrorKind, pub message: String }
pub enum ApiErrorKind {
    BadRequest,           // 400
    Unauthorized,         // 401
    InsufficientBalance,  // 402
    InvalidParams,        // 422
    RateLimited,          // 429
    ServerError,          // 500
    Overloaded,           // 503
    Other,                // 其它非 2xx
}

pub enum TransportError {
    Timeout,   // 连接或读取超时
    Connect,   // 建连失败:DNS、连接被拒、网络不可达
    Tls,       // TLS 握手 / 证书错误
    Closed,    // 连接中途断开 / 读 body 时断
    Other(Box<dyn std::error::Error + Send + Sync>), // 兜底,仍不泄漏库类型
}
```

设计点:
- `ApiError` 同时留 `status`(原始真值,日志 / 未枚举状态用)和 `kind`(给上层 match 的语义)。
- `TransportError` 把上层重试逻辑真正关心的 4 种语义拎出来(超时 / 建连 / TLS / 断开),其余进 `Other(Box)` —— **藏掉"哪个库的错",保留"哪种错"**。
- `Decode` 保留 `serde_json::Error`(基础稳定依赖、且已通过 serde 公开),并带 `context` 标明哪个响应解析失败。
- 往 enum 加变体是兼容变更:以后细分(如 `Timeout` 拆 connect / read)不破坏下游。

### 6.2 状态码 → ApiError(纯核心,两端共用)

```rust
fn api_error_from_status(status: u16, body: &str) -> ApiError {
    let message = extract_api_message(body); // 试抠 {"error":{"message":…}} 或 {"message":…};抠不到给 body 截断
    let kind = match status {
        400 => ApiErrorKind::BadRequest,
        401 => ApiErrorKind::Unauthorized,
        402 => ApiErrorKind::InsufficientBalance,
        422 => ApiErrorKind::InvalidParams,
        429 => ApiErrorKind::RateLimited,
        500 => ApiErrorKind::ServerError,
        503 => ApiErrorKind::Overloaded,
        _   => ApiErrorKind::Other,
    };
    ApiError { status, kind, message }
}
```
两个后端走不同的路到这里(见 6.3),但**全局只有这一个状态映射**,不重复。

### 6.3 库错误 → Error(脏活,各 adapter 一份)

两个库形态差很多,adapter 的职责就是把差异抹平。三个最硬的归一点:

**① 非 2xx 是怎么来的不一样**
- `reqwest`:非 2xx 是 `Ok(resp)` —— 要**自己**查 `resp.status()`、读 body,再喂 `api_error_from_status`。
- `ureq`:非 2xx 默认直接当 `Err`(2.x `Error::Status(code, resp)` / 3.x `Error::StatusCode`),从里面抠 code + body,再喂**同一个**函数。
- 结果:两条路都汇进 `Error::Api`,公开层看不出差别。

**② 超时检测的位置不一样**
- `reqwest`:`err.is_timeout()` → `TransportError::Timeout`。
- `ureq` 2.x:藏在 `Transport(kind = Io)` 的 `io::Error`(kind `TimedOut` / `WouldBlock`),要 downcast 判;3.x:有显式 `Timeout` 变体,直接映。

**③ TLS 不一样**
- `reqwest`:无 `is_tls()`,得顺 `source()` 链找 rustls 错误类型;找不到退到 `Connect` / `Other`。
- `ureq`:有 `Tls` / `InsecureRequestHttpsOnly` 这类 kind,直接映 `TransportError::Tls`。

adapter 接缝(各在对应后端文件内,核心不可见):
```rust
// client_async.rs   (feature = "async")
fn map_reqwest(e: reqwest::Error) -> Error {
    if e.is_timeout() { return Error::Transport(TransportError::Timeout); }
    if e.is_connect() { return Error::Transport(TransportError::Connect); }
    // body/解码、source 链找 TLS …… 其余兜底:
    Error::Transport(TransportError::Other(Box::new(e)))
}

// blocking.rs       (feature = "blocking")
fn map_ureq(e: ureq::Error) -> Error {
    match e {
        // ureq 把 4xx/5xx 当 Err —— 其实是 API 语义错,不是传输错:
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            Error::Api(api_error_from_status(code, &body))
        }
        ureq::Error::Transport(t) => Error::Transport(match t.kind() {
            // Dns / ConnectionFailed → Connect;Io+TimedOut → Timeout;Tls → Tls;
            // 其余 → Other(Box::new(t))
            _ => TransportError::Other(Box::new(t)),
        }),
    }
}
```

> 已锁 **ureq 3.3 / reqwest 0.13**;具体谓词 / 变体名以这两版为准,coding 时对着 docs.rs 钉死(ureq 3.x 错误枚举与 2.x 不同,上面 Status 那段以 3.x 为准重写)。这里定的是**目标语义 + 归一规则**,与版本无关。映射兜底一律进 `TransportError::Other(Box)` —— 绝不 panic、绝不丢信息。

### 6.4 validate() 本地拦截(D8)
最小集,只拦"必 422 / 必报错"的组合,避免一次空往返:
- 思考模式开启 + (`logprobs` 或 `top_logprobs`) → `InvalidRequest`(实测会报错)
- `top_logprobs` 设了但 `logprobs != true` → `InvalidRequest`
- `messages` 为空 → `InvalidRequest`
- model 由枚举保证合法,无需运行时校验(请求侧不可能传出枚举外的值)

> 思考模式下 `temperature` / `top_p` 是**静默失效**不是报错,本层**不拦**,只在文档注明。

### 6.5 重试 / 续写(本层不做)
本层**不做**。`Api`(含 429/500/503)与 `Transport` 如实返回、`finish_reason` 如实返回枚举,是否退避重试 / `Length` 续写全交上层。

> 可选便捷:在 `Error`(或 `ApiErrorKind` / `TransportError`)上挂**分类谓词**(如 `is_transient()`)——只**告知**不**行动**,方便上层写重试。**已确认补**(只告知不行动)。

---

## 7. SSE / 传输层清洗

- `decode_line(line)`:`data: {…}` → `Data(json)`;`: keep-alive`(冒号开头注释) → `Comment`(丢弃);空行 → `Blank`(丢弃);`data: [DONE]` → `Done`(收尾)。
- async 端按 `\n` 缓冲拆行(行可能跨网络帧),再逐行 `decode_line`;sync 端 `read_line` 逐行喂。
- 非流式:高负载下 body 前可能有空行,`serde_json` 容忍前导空白,基本免费;稳妥起见先 `trim_start` 再 parse。
- 超时:builder 默认读超时建议 ≥ 10min(服务端 10min 不开始推理才断连)。

---

## 8. 留给下一层(明确不在本次)

`Session`/worker、上下文与 token 控制、`user_id` 语义与持久化、所有权 scope 内存回收、思考开关挂 session、计费聚合与价目表、tools 执行 loop、自动重试、限速、安全。本层把"无状态、类型化、错误友好、双形态、SSE 清洗"做扎实,给上面这些留干净接口。

---

## review 结论(已定)
1. **Message**:用 **struct + `role` 字段**(扁平,见 §5.2),非法组合靠构造 helper 约束。
2. **validate()**:先做**入门集**(§6.4 当前几条),后续按需加。
3. **helper**:**最小集**(`content()` / `reasoning()` / `finish_reason()` + 干净 clone),不铺开。
4. **命名**:`req::Client`(async)+ `req::blocking::Client`(sync),确认。
5. **错误粒度**:`TransportError` 4 桶 + `Other` 确认;**补分类谓词** `is_transient()`(只告知不行动,§6.5)。
6. **依赖 / 工具链**:§2.4 已锁版本;cargo 工具链 + just 任务运行器,操作走 `cargo add` / `cargo init`(见 `CLAUDE.md`)。
