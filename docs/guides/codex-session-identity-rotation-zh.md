# Codex 会话身份轮换与完整历史归档

本功能基于 `v4.0.40-codex-auth-145` 开发，为第三方 Codex 原生 Responses 转发提供到限后的有限重试。完整历史处理规则见 [历史续聊说明](codex-session-history-continuation-zh.md)。

## 开关及适用范围

在 Codex 供应商编辑表单中显示两个开关。续聊开关会同时写入两个底层字段，保留旧配置兼容性：

| 中文名称 | 供应商元数据字段 | 缺省值 |
| --- | --- | --- |
| 解除中转站轮次限制 | `codexSessionIdentityRotation` + `codexSessionHistoryArchive` | `false` |
| 启用会话身份脱敏诊断 | `codexSessionIdentityDiagnostics` | `false` |

每个供应商分别保存。旧配置没有这些字段时视为关闭；旧配置只有身份轮换字段为 `true` 时，“解除中转站轮次限制”开关按开启显示，下一次保存会补齐完整历史归档字段。保存后重新编辑会回显，编辑另一家供应商不会继承上一家的值。诊断可以单独关闭。表单在直连布局中仍可保存设置，但这不表示直连请求会经过本功能。

## 运行模式与旧配置

| 模式 | 配置 | 明确到限后的唯一一次额外重试 |
| --- | --- | --- |
| 完整历史归档续聊 | 新界面开启后，轮换和归档字段均为 `true` | 更换支持的上游身份，将历史模型输出、普通工具调用及结果封装为按原顺序保存的 JSON 文字归档 |
| 旧版严格身份模式 | 仅旧配置或其他直接写配置的路径可出现：轮换为 `true`、归档为 `false` | 更换支持的上游身份，保留历史条目的原生协议形态；在新版表单保存时会规范化为完整历史归档 |

归档模式继续原生发送系统/开发者指令、用户消息、推理密文、压缩密文、动态工具注册及其配对调用；不生成摘要、不截断正文、不修改客户端聊天文件。归档中的 JSON 数组可展开恢复原历史 JSON 值，但 JSON 文本的空白、对象键序和转义写法不保证一致。它改变的是出站历史呈现方式，不能宣称与严格身份模式具有完全相同的协议语义。

两种模式正常首发均不预先改写历史。首次到限并建立映射后，归档模式下的后续请求沿用新身份及归档形态；每次仍从客户端本次发送的完整历史构造，不累计重复嵌套归档。已知媒体条目及其配对工具调用、结果保持原生形式，作为归档分段边界；未知非媒体条目返回明确错误，不发送降级重试。归档可能增加输入量，适用边界和完整说明见 [归档主指南](codex-session-history-continuation-zh.md)。

## 轮换适用条件

只有同时满足以下条件才参与轮换：

- 请求经过 CC Switch 本地代理，包括路由接管和聚合模式。
- 客户端为 Codex，方法为 `POST`，入口路径严格为 `/responses` 或 `/v1/responses`（允许查询参数），最终实际选择的是原生 Responses 上游。
- 当前第三方供应商明确开启了轮换开关。
- 客户端提供了可以识别的原会话或线程标识。

官方 ChatGPT / Codex OAuth、官方直通、绕过本地代理的直连、Chat Completions 转换和 Anthropic 转换不适用。协议按实际转发结果判断，不能只依据表单中的静态协议标签。没有显式身份、`x-codex-turn-metadata` 不是可识别的 JSON 对象、该元数据过大或支持的元数据层级超过 8 层时，不建立自动轮换映射。

`/responses/compact` 等压缩端点不参与；本功能不会自动请求压缩端点。

关闭开关和不适用请求沿用原有发送路径；开启后首次尚未到限的请求保留原有身份和兼容处理，不预先更换 UUID。

## 哪些错误会触发

HTTP 错误必须是 **400**，优先匹配结构化错误中的：

```json
{"error":{"code":"session_turn_limit_exceeded"}}
```

存在其他明确错误码时，即使提示文本相似也不触发。没有错误码时，只接受源码列出的严格整句提示；数字可以是 100、105 或其他正整数，不写死轮数。中文提示形式为：

> 当前会话模型已回复 105 轮，达到上限。Codex 请输入 /compact 压缩上下文后继续，或输入 /new 新开会话。

英文兼容形式为：

```text
This session has reached the limit of 105 model turns. Please use /compact to compact the context and continue, or /new to start a new session.
```

普通 400、上下文超长、鉴权失败、401/403、429、余额不足、配额不足，以及仅包含 `105 model turns` 的零散文字均不触发。不会在任意对话正文中搜索这些词来决定重试。

部分上游返回 HTTP 2xx，却在 JSON 错误信封或 SSE 输出前的 `error` / `response.failed` 事件中报告相同限制。本实现只解析明确的错误结构，再规范化为内部 400，交给相同识别逻辑。

## 身份映射与重试边界

映射以供应商 ID 和客户端原线程身份隔离；有明确线程标识时优先使用线程，否则回退到原会话标识。共享同一客户端进程会话的不同线程不会因此合并。没有硬编码供应商名称、项目名称、模型或 UUID。

第一次明确到限时，生成新上游身份并推进一代。并发请求若都在同一旧代失败，只推进一次并复用同一新代。之后该供应商下的原聊天稳定沿用新身份；新身份以后再次到限，可以在新的客户端请求中再推进一代。

**同一个客户端请求对同一供应商最多增加一次身份轮换重试。** 该预算不会因为其他整流器再次调用转发函数而重置。第二次仍然到限时停止身份轮换重试，将错误交回原有错误分类和故障转移流程。该功能不扩大供应商候选数量，也不单独记一次用户请求；原有请求统计、供应商成功/失败和健康度处理仍由既有流程决定。

映射只保存在进程内存：

| 项目 | 边界 |
| --- | --- |
| 身份映射数量 | 最多 4096 条 |
| 闲置过期时间 | 24 小时 |
| 容量不足 | 按最近使用顺序逐出无活跃请求的条目；全部被活跃请求固定时，安全跳过新映射 |
| 重启后 | 不恢复旧映射，后续请求从原身份重新开始 |

闲置过期或容量逐出同样会使旧映射失效。保存的是开关设置，并非永久保存上游 UUID。

## 最终出站字段与上下文

身份应用发生在实际协议、鉴权、模型映射、请求中间件、本地请求头/正文覆盖等处理完成之后、最终 JSON 序列化之前，因此不会再被这些较早阶段覆盖。

轮换后会统一常见的 `session_id`、`session-id`、`thread-id` 请求头，并处理实际存在的其他支持拼写；同步 JSON `x-codex-turn-metadata`、窗口标识，以及请求根和 `metadata` / `client_metadata` 中支持的身份字段。“CC 03”另补充了对已知元数据容器中 `x-codex-turn-metadata` JSON 字符串的解析、校验和重新编码，包括 `client_metadata["x-codex-turn-metadata"]`，避免外层身份已换新而内嵌字符串仍保留旧身份。该修复属于身份同步，两种模式均使用，不以开启归档为前提。

元数据只沿已知容器边界处理，不把任意字符串当 JSON，也不遍历消息、工具或工具参数。格式无效、超大小或超层级的元数据沿既有保守边界处理，不盲目替换任意 UUID。回合标识、安装标识和其他自定义字段保留原语义。窗口序号在能够识别时保留；无法识别的旧窗口值改为当前新身份的默认窗口。

`prompt_cache_key` 只有与原会话或线程的某个明确身份别名完全相等时才同步更换，包括支持层级内嵌套元数据提供的别名。自定义缓存键保持原值。

自动轮换不会清空、截断或摘要消息，不自动输入 `/compact`，不更改工具定义、工具调用参数、`call_id` 或历史 `turn_id`，也不因轮换删除 `previous_response_id`、`conversation` 等引用。归档模式只在上述保留内容的前提下改变部分历史条目的协议呈现方式。现有实验不能确定上游具体如何计数，不能据此猜测删除引用。

本上传版本原有的第三方兼容层可能在正常请求中移除部分 OpenAI 专有密文字段。为保持正常请求原行为，轮换实现没有对所有请求全局关闭该兼容层：

1. 在原兼容处理前，仅保存需要保留的密文字段、条目定位字段及正文哈希；不保存用于回退的旧对话正文。
2. 首次尚未到限的请求继续原处理。只有明确到限后的自动重试，以及已经建立轮换映射的后续请求，才向最终请求补回能够唯一定位的密文字段。
3. 补回时按条目摘要及可用的条目身份核对，保留最终正文、中间件改写和 PII 脱敏结果；不会为了恢复密文撤销最终正文。显式配置的 `input` 覆盖仍按原有用户配置语义生效。
4. 条目有重复、摘要不匹配或无法唯一定位时不猜测关联。初次请求仍走原发送，但不进行自动身份重试；已经映射的请求则返回明确错误，提示无法安全定位密文条目，避免在已更换身份后静默丢失上下文。

因此，这项功能保留的是已有请求处理后能够安全对应的完整上下文；它不会重建客户端没有发送的历史，也不会反向撤销用户显式配置的请求改写。

## 路由状态与流式响应

旧身份的 `x-codex-turn-state` 以及支持的元数据路由状态字段不会直接搬到新身份。新身份成功响应中的路由状态仅保存在内存，并受以下范围约束：

- 供应商和原线程对应的映射条目。
- 当前身份代；迟到的旧代响应不能覆盖新代状态。
- 最终出站的明确当前 `turn_id`。请求头、请求根、支持的元数据和 JSON 回合元数据必须一致；重复头冲突、未知格式或缺少回合标识时不缓存、不复用。历史消息和工具参数中的 `turn_id` 不参与当前回合判断。

每个身份最多缓存 8 个回合状态，单条最多 4 KiB，过期时间为 1 小时。缓存不会跨回合、跨线程或跨供应商补用。

流式检查仅在客户端尚未收到响应内容时进行。生命周期事件可以暂存；遇到有效输出或未知事件，即交出已缓冲字节并继续原流。判定时间最多 10 秒，若原首字节超时更短则使用更短值；缓冲读取达到 256 KiB 阈值时也立即交出。交出后不会再回到身份重试循环，因此输出后的限制错误不会造成重放和重复输出。

这些边界意味着：已经提交输出，或超过判定窗口后才出现的错误不会自动轮换重试，而是交给现有流处理逻辑。

## 脱敏诊断如何判读

在需要核对最终身份时打开该供应商的“启用会话身份脱敏诊断”。新增日志使用 `[CodexSessionIdentity]` 前缀，只记录必要的哈希、数量、身份代、阶段、状态码和固定原因标签，不记录 API Key、Token、原会话标识、路由凭据或对话正文。

启用归档时还可出现 `[CodexSessionArchive]` 统计，包含原条目数、已归档条目数、分段数、归档 JSON 字节数和原生推理条目数；不记录归档正文或密文。条目数变化是封装结果，不等于删除历史；应通过逐项展开核对内容。

| 阶段 | 含义 |
| --- | --- |
| `outbound` | 最终第一次出站；第 0 代表示尚未轮换 |
| `limit_retry` | 已明确识别轮数上限，允许本请求对本供应商追加一次重试 |
| `retry_outbound` | 应用新身份后的最终重试出站 |
| `result` | 发送和输出前检查返回的状态码 |
| `failed` | 本次没有资格继续身份轮换重试 |
| `skipped` | 身份/协议不可用，或密文条目无法安全定位 |

在 `outbound` / `retry_outbound` 行中，`key_hash` 用于关联原供应商与聊天，`generation` 表示身份代，`identity_hash` 表示本代新身份。`identity_hash_count` 和 `identity_values` 审计最终请求中受支持的会话/线程身份值；轮换后应只剩同一种身份哈希。第 0 代中客户端原来的会话和线程值可能不同，不能仅因其数量大于 1 就认定异常。窗口值带窗口序号，不计入这一会话/线程哈希集合。

`result` / `failed` 等非出站阶段不重新审计正文，相关字段数量可能为 0，应结合前一条出站日志读取。`result` 的 HTTP 200 也不代表之后整条流必定成功，仍需查看客户端最终结果。诊断使用完毕可关闭；此开关不改变仓库既有其他日志功能的配置。

## 测试覆盖

| 测试位置 | 覆盖内容 |
| --- | --- |
| `tests/components/ProviderForm.codexSessionIdentity.test.tsx` | 旧配置默认关闭、保存、重新编辑回显、供应商切换隔离、直连/路由/聚合布局，以及转换协议提示 |
| `src-tauri/src/provider.rs` | 三个字段的缺省值、序列化和归档对轮换的依赖 |
| `src-tauri/src/database/dao/providers_tests.rs` | 内存数据库保存、重载及不同供应商设置隔离 |
| `src-tauri/src/proxy/codex_session_identity/tests.rs` | 身份与上下文保留、自定义/嵌套缓存键、后续复用、跨供应商和线程、并发、容量、过期、最终回合冲突与路由状态隔离 |
| `src-tauri/src/proxy/codex_history_archive.rs` | 归档逐项恢复、原生指令/动态注册边界、推理和压缩密文保留、重复应用、媒体及工具配对原生保留和未知类型 |
| `src-tauri/src/proxy/forwarder/session_identity_tests.rs` | 真实转发函数配合本机随机端口模拟上游：首次到限后成功、连续到限停止、普通错误不触发、映射复用、再次到限、正常请求兼容、正文中间件不重复执行、2xx 错误及输出后的流式边界 |

其他新增回归断言还核对密文条目的唯一定位和恢复边界。模拟上游只绑定 `127.0.0.1` 随机端口，使用测试供应商数据；不是对真实供应商效果的验证。

源码静态检查、语法解析、类型检查、测试执行、构建，以及补丁与归档核对分别记录在交付验证报告中。静态语法通过不等于 Rust 类型检查、链接或运行测试通过；前端资源构建通过也不等于 Windows 安装包已生成。

## 验证与构建

先按仓库开发指南安装依赖。以下命令在源码根目录执行，测试使用独立目录，不读取或改写正在使用的客户端配置。

```powershell
pnpm install --frozen-lockfile
pnpm typecheck
pnpm exec vitest run tests/components/ProviderForm.codexSessionIdentity.test.tsx tests/config/localeParity.test.ts
pnpm build:renderer

$env:CC_SWITCH_TEST_HOME = Join-Path (Get-Location).Path '.test-data/session-identity'
New-Item -ItemType Directory -Path $env:CC_SWITCH_TEST_HOME -Force | Out-Null
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo test --manifest-path src-tauri/Cargo.toml --locked --release --lib session_identity -- --test-threads=1
cargo test --manifest-path src-tauri/Cargo.toml --locked --release --lib codex_history_archive -- --test-threads=1
cargo test --manifest-path src-tauri/Cargo.toml --locked --release --lib proxy::opaque_state_rectifier:: -- --test-threads=1
cargo test --manifest-path src-tauri/Cargo.toml --locked --release --lib providers::codex_compaction:: -- --test-threads=1
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

Windows 需要 Microsoft C++ Build Tools、Windows SDK 和 WebView2；如未配置编译环境，请先从对应的开发者终端执行。Rust 版本以 `rust-toolchain.toml` 固定的 1.95 为准。上述筛选测试不能替代全量测试，也不证明真实供应商一定接受身份轮换或历史归档。

可选静态检查脚本为 `scripts/validate-session-identity-static.py`，依赖 `tree-sitter`、`tree-sitter-rust` 和 `tree-sitter-typescript`。脚本支持 `--parser-path` 指定独立解析包目录、`--baseline-zip` 核对原始源码包是否缺文件、`--output` 保存检查报告；只验证语法树与源码完整性，不代替编译或运行测试。

本地 Windows 打包可使用覆盖文件关闭更新签名产物生成，并选择中文安装器：

```powershell
& '.\node_modules\.bin\tauri.cmd' build --ci --bundles msi nsis `
  -c src-tauri/tauri.session-identity.windows.json -- --locked
```

该覆盖文件不修改默认发布配置，不需要发布私钥。产物默认位于 `src-tauri/target/release/bundle`；设置 `CARGO_TARGET_DIR` 后以对应目录为准。构建不会自动安装或启动应用，测试目录变量也不隔离正式应用的运行配置。

## 真实客户端验证

1. 在隔离环境准备第三方原生 Responses 供应商，确认开关默认关闭，保存后重新打开能够正确回显，切换供应商不会继承其他供应商的设置。
2. 开启“解除中转站轮次限制”和诊断，经本地代理继续测试会话，不手动清空历史或更换聊天。
3. 等上游明确返回轮次限制后，核对一次 `limit_retry` 和推进一代后的 `retry_outbound`，确认重试成功才出现身份切换提示。
4. 检查旧历史的具体信息是否仍能被回答，并完成新工具调用及结果往返；不能只依据 HTTP 200 判断成功。
5. 发送后续消息确认复用新身份，其他线程和供应商不串用。混合图片、音频或文件历史需要另外验证实际客户端和上游的支持情况。
6. 连续到限时确认没有无限重试，保存脱敏错误信息并关闭诊断。身份轮换不增加模型上下文窗口，也不能恢复客户端已经压缩且未再发送的原文。

源码测试、构建、安装、真实上游请求和客户端展示分别验证，不能以其中一项代替另一项。
