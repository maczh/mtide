# AGENT.md

> 本文件面向在 `mtide` 仓库中工作的编码智能体（Codex / Claude Code / CodeBuddy 等）以及人类协作开发者。
> **第一优先级**：产品需求以 `mtide-PRD-v2.0.md` 的“v2.1 评审增补版”内容为唯一事实源（共 169 条需求，分层发布），本文件是「怎么实现」的约束集合。两者冲突时以 PRD 为准，并立刻在 PR 描述里标注 `NEEDS-PM`。
> **模块结构与依赖方向**以 `mtide-架构设计-v1.0.md` 为准（本文件 §3 是其落地清单）；新增 crate 或新增跨 crate 依赖必须先改架构文档并写 ADR。
> **v2.1 相对 v2.0 的关键变化**：新增 UI-19，功能热键以 VSCode/VSCodium 常用键位为基线，并加入 GUI 终端兼容矩阵和终端保留键诊断规则。

---

## 0. 工作协议（每次任务开始前必读）

1. **先定位需求编号**：从 PRD 第七章找到对应的 `FR 编号`（如 `LN-07`），把编号写进 PR 标题与测试名。找不到编号说明需求不存在，先停下来问，不要自行发明功能。
2. **先测试后实现**：写一个会失败的测试（单元或集成），再写实现。测试名格式 `fr_ln07_completion_debounce_150ms`。
3. **每次改动跑全量门禁**：`cargo xtask check`（格式 + clippy + 测试 + 依赖方向校验）。红的不许提交。
4. **不扩大范围**：发现 PRD 没写的功能或没写的边界，记入 `docs/backlog.md` 并在 PR 里提一句，**不要顺手实现**。
5. **产品决策不要猜**：涉及交互、文案、默认值、优先级的选择，写一页 ADR 到 `docs/decisions/NNNN-<slug>.md`，PR 标注 `NEEDS-PM`，先按最保守方案实现（能关掉、不默认开、不自动下载、不静默写盘）。
6. **改动必须可回滚**：一次 PR 只做一件事，禁止「顺手重构」混合在功能 PR 里。
7. **技术债显式化**：允许留 `// TODO(FR-xx):`，必须带编号与原因；禁止无编号 TODO。

---

## 1. 项目概览

`mtide` 是一个运行在终端里的通用开发 IDE：编辑、编译、调试、Git、代码智能、AI CLI 编排、插件，单二进制、无运行时依赖。

**四个不可协商的产品承诺**（破坏任一条的实现一律打回）：

| # | 承诺 | 对应 PRD |
|---|---|---|
| P-1 | 单二进制、零外部运行时依赖（不要求 Node/Python/rg）；Git 与 LSP 服务器为可选外部件，缺失只降级不报错 | 4.1 / NF-06 |
| P-2 | 插件不得直接输出 ANSI，只能返回 Widget 树；AI 不得未经 diff 确认写盘 | 7.9 PL-04 / 7.8 AI-05 |
| P-3 | 任何 UI 操作不同步等待 IO；键位响应 P95 ≤30ms，冷启动 P95 ≤150ms | NF-01 / NF-02 |
| P-4 | 不兼容 VSCode 插件 API；不实现 VSCode 扩展宿主 | 4.2 N-1 |

---

## 2. 技术栈与工具链

| 项 | 选择 | 说明 |
|---|---|---|
| 语言 | Rust（edition 2021，MSRV 1.80） | 单二进制、无 GC、启动快 |
| 异步运行时 | `tokio`（multi-thread），**只在 bin 层创建** | 核心编辑逻辑保持同步，禁止在 `mtide-core` 引入 tokio |
| TUI | **自研** cell buffer + 差量输出；仅用 `crossterm` 或 `termwiz` 做终端后端抽象 | 不引入 ratatui，插件需要自有组件树 |
| 文本 | `ropey` | 大文件 O(log n) 编辑 |
| 语法高亮 | `syntect`（TextMate）+ `tree-sitter`（Top10 语言） | 见 PRD 5.1 混合方案 |
| JSON-RPC | 自研轻量实现（`serde_json` + 行/头解析） | LSP 与 DAP 共用同一套传输层代码 |
| Git | `git` 子进程（porcelain=v2、`-z`） | 不引入 `git2`/libgit2，见 PRD 5.4 |
| 搜索 | `ignore` + `grep-searcher` + `grep-regex` | 与 ripgrep 同源，零外部依赖 |
| PTY | `portable-pty` 或 `nix`（Unix）/ `windows-sys`（ConPTY） | 需自行封装跨平台 |
| 错误 | `thiserror`（库）、`anyhow`（bin 与 xtask） | 库代码禁止 anyhow |
| 日志 | `tracing` + 文件 sink；UI 内可见 error/warn | 禁止 `println!` 于库代码 |

**依赖准入规则**：只允许 MIT / Apache-2.0 / BSD-2/3 / ISC / MPL-2.0；GPL/AGPL 一律拒绝；新增依赖需在 PR 说明体积影响与替代方案。定期 `cargo deny check`。

---

## 3. 仓库结构与依赖方向

```
mtide/
├── Cargo.toml                 # workspace
├── AGENT.md                   # 本文件
├── mtide-PRD-v2.0.md           # 需求唯一事实源
├── crates/
│   ├── mtide-api/              # 纯 DTO 与契约类型：Widget 树、RPC 消息、AgentEvent、Patch 结构
│   ├── mtide-proc/             # ★架构 v1.0：子进程 RAII、stdio JSON-RPC、PTY 平台封装、退避重启
│   ├── mtide-tui/              # 终端后端、cell buffer、差量渲染、输入事件、能力探测
│   ├── mtide-vt/               # ★v2.0：VT 缓冲（ANSI 解析、屏幕状态机、备用屏、鼠标协议、resize 透传）
│   ├── mtide-term/             # ★v2.0：PTY 管理、终端面板、AI 交互会话面板（消费 mtide-vt）
│   ├── mtide-widget/           # 布局引擎、组件库（列表/输入/弹窗/表格/进度）、焦点管理
│   ├── mtide-core/             # 文档模型、选区与多光标、撤销树、命令注册表、键位映射、坐标系统
│   ├── mtide-syntax/           # TextMate 高亮、tree-sitter、增量重解析、语言检测
│   ├── mtide-lsp/              # LSP 客户端、传输层、语言配方库、服务器生命周期
│   ├── mtide-dap/              # DAP 客户端、断点模型、会话状态机
│   ├── mtide-task/             # 任务执行、问题匹配器、输出流
│   ├── mtide-git/              # git 子进程封装、状态、补丁、冲突
│   ├── mtide-search/           # ★架构 v1.0：内置全文搜索、模糊文件查找、结果流控
│   ├── mtide-diff/             # ★架构 v1.0：统一 diff 引擎（Git / 比较 / AI 三处共用）
│   ├── mtide-nav/              # ★架构 v1.0：最近文件与位置栈、本地历史、书签、面包屑、大纲、时间线、层级
│   ├── mtide-ai/               # AgentAdapter 契约与内置适配器、diff gate、外发过滤、审计
│   ├── mtide-plugin/           # 插件宿主、RPC 服务端、权限、生命周期
│   ├── mtide-workspace/        # 多根工作区、四层配置、会话快照、文件监听、写盘唯一入口
│   └── mtide/                  # 二进制：CLI 参数、事件循环、模块编排、依赖注入
├── xtask/                     # cargo xtask：check / bench / snapshot / package / syntax-pack
├── docs/
│   ├── decisions/             # ADR
│   ├── 开发启动基线-v1.0.md     # P0/P1 范围、阶段门禁与安全边界
│   └── backlog.md             # 需求外想法收纳
└── tests/                     # 跨 crate 集成测试、终端兼容快照
```

### 3.1 依赖方向（违反即 CI 失败）

```
mtide(bin) ──> {mtide-workspace, mtide-core, mtide-lsp, mtide-dap, mtide-task,
                mtide-git, mtide-search, mtide-diff, mtide-nav, mtide-ai,
                mtide-plugin, mtide-syntax, mtide-term, mtide-widget}
mtide-widget ──> mtide-tui ──> mtide-api
mtide-workspace ──> {mtide-core, mtide-api}
mtide-{lsp,dap,ai,plugin,task,git} ──> mtide-proc ──> mtide-api

mtide-term ──> mtide-vt ──> mtide-api      （VT 缓冲只依赖 api，可脱离终端后端单测）
```

**硬规则**（与 `mtide-架构设计-v1.0.md` §5.3 一致，编号 R-1~R-7）：
- **R-1** `mtide-vt` **不得**依赖 `mtide-tui` / `mtide-widget` / 任何业务 crate：它是纯屏幕状态机，必须能在无终端环境下用固定尺寸网格单测。
- **R-2** `mtide-core` **不得**依赖 `mtide-tui`、`mtide-widget`、`mtide-lsp`、`mtide-git`、`mtide-ai`、`mtide-dap`、`mtide-plugin`。
- **R-3** `mtide-tui` **不得**依赖任何业务 crate（只认 api 的 Widget 与输入事件）。
- **R-4** 业务 crate 之间**禁止**相互依赖；需要协作时通过 `mtide-api` 的 trait 或由 bin 层编排。**唯一例外白名单**：`mtide-git → mtide-diff`、`mtide-ai → mtide-diff`、`mtide-nav → mtide-diff`（`mtide-diff` 是纯函数库，只依赖 api + core）。
- **R-5** `mtide-plugin` 只依赖 `mtide-api` 与 `mtide-workspace` 的只读视图，**不得**直接调用 `mtide-git`/`mtide-ai`；宿主只能拦截经 RPC broker 发起的能力调用，不能把普通子进程伪装成操作系统沙箱。
- **R-6** 只有 `mtide`(bin) 可创建 tokio runtime；库 crate 禁止 `#[tokio::main]` / `Runtime::new`。
- **R-7** 任何 `pub` 的类型变动都要考虑 `mtide-api` 的向后兼容（协议版本号 `PROTOCOL_VERSION`）。

CI 用 `cargo xtask depcheck` 解析 `cargo metadata` 断言上述规则。

---

## 4. 命令

```bash
cargo build                      # 开发构建
cargo run -- path/to/dir         # 启动
cargo xtask check                # fmt + clippy(-D warnings) + test + depcheck（提交前必跑）
cargo xtask bench                # criterion 基准（启动/按键/大文件/内存）
cargo xtask snapshot             # 渲染快照测试（更新用 --update，需人工审阅 diff）
cargo xtask package              # 交叉编译三平台产物
cargo xtask syntax-pack --lang go # 生成语法包
cargo xtask vt-test              # VT 缓冲一致性测试集（输入序列 → 屏幕网格）
cargo xtask plugin-new my-plugin # 生成插件脚手架（PL-09）
cargo deny check                 # 许可与安全检查
mtide --safe-mode                 # 不加载任何插件启动（排障）
mtide --doctor                    # 自检：终端能力、宽字符、git、已装 LSP/DAP、AI CLI 探测
```

**测试分层**：
- 单元测试与代码同文件 `#[cfg(test)]`
- 集成测试在 `tests/`，允许启动真实子进程（git、mock LSP/DAP/AI CLI）
- 渲染测试用固定尺寸（80×24、120×40、200×60）快照比对
- 性能测试用 `criterion`，阈值写在 `xtask/src/perf.rs`，超阈值 CI 失败

---

## 5. 架构铁律（Architecture Invariants）

| # | 铁律 | 违反后果 |
|---|---|---|
| I-1 | 插件只能返回 `WidgetNode` 树，**禁止**任何 ANSI 转义进入渲染管线 | 屏幕错乱、主题失效、无障碍不可用 |
| I-2 | AI 写盘只能经由 `DiffGate`，且必须校验基线文件 hash | 静默改坏代码，产品承诺崩塌 |
| I-3 | 主线程（渲染 + 输入）不得执行文件、网络、子进程或磁盘阻塞 IO；终端输入泵允许最多 8ms 的可控等待，所有其它 IO 在 tokio 任务中，结果经 channel 回到主循环 | 卡顿，违反 P-3 |
| I-4 | LSP/DAP 的位置一律用「UTF-16 code unit」，内部字符串用 UTF-8 byte，转换只在 `mtide-lsp::conv` 一处发生 | 中文/emoji 场景跳转错位 |
| I-5 | 所有子进程必须绑定生命周期（RAII guard），退出时确保回收，禁止孤儿 | 进程泄漏 |
| I-6 | 快照/配置/会话文件必须原子写（临时文件 + rename） | 崩溃后配置损坏 |
| I-7 | 任何「自动下载/自动安装/自动写盘/自动执行命令」都必须默认关闭且需显式确认 | 企业合规红线 |
| I-8 | 库代码禁止 `unwrap`/`expect`/`panic!`（测试除外）；错误必须可分类并可展示给用户 | 崩溃率超标 |
| I-9 | 不允许 `unsafe`，除非在 `mtide-tui` 的终端后端且附 `// SAFETY:` 说明 | 内存安全风险 |
| I-10 | 面向用户的文案集中在 `mtide/src/i18n.rs`，禁止硬编码在逻辑里 | 无法统一口径与翻译 |

---

## 6. 关键契约草案（Rust）

> 以下为实现骨架，签名可微调，语义不可改。变更语义须写 ADR。

### 6.1 文档与编辑

```rust
pub struct DocId(pub u64);

pub trait DocumentView {
    fn id(&self) -> DocId;
    fn text(&self) -> &str;                   // 已加载内容（大文件为窗口视图）
    fn line_count(&self) -> usize;
    fn line_to_byte(&self, line: usize) -> usize;
    fn byte_to_line(&self, byte: usize) -> usize;
    fn is_dirty(&self) -> bool;
    fn version(&self) -> i32;                 // LSP 用，单调递增
}

pub struct Edit { pub range: Range<usize>, pub insert: String }

pub trait EditEngine {
    fn apply(&mut self, edits: &[Edit]) -> Result<Applied, EditError>; // 原子：全成功或全失败
    fn undo(&mut self) -> Option<Vec<Edit>>;
    fn redo(&mut self) -> Option<Vec<Edit>>;
}
```

### 6.2 命令与键位

```rust
pub struct CommandCtx<'a> {
    pub docs: &'a mut DocSet,
    pub ws: &'a WorkspaceHandle,
    pub ui: &'a mut UiHandle,     // 只能发 Widget/通知，不能直接画屏
    pub args: CommandArgs,
}

pub trait Command: Send + Sync {
    fn id(&self) -> &'static str;             // 形如 "editor.save"
    fn title(&self) -> &str;
    fn when(&self) -> Option<&'static str>;   // 上下文表达式，如 "editorFocus && dirty"
    fn run(&self, ctx: &mut CommandCtx) -> Result<CommandOutcome>;
}

pub enum KeyScope { Editor, Terminal, Global }
pub struct KeyChord { pub mods: Mods, pub key: KeyCode }
```

### 6.3 语言服务

```rust
#[async_trait]
pub trait LanguageClient: Send + Sync {
    fn language_id(&self) -> &str;
    fn state(&self) -> LspState;              // 对应 PRD 9.2 状态机
    async fn initialize(&mut self, root: &Path, caps: ClientCaps) -> Result<ServerCaps>;
    async fn did_open(&self, doc: &dyn DocumentView, text: &str);
    async fn did_change(&self, doc: &dyn DocumentView, edits: &[Edit]);
    async fn completion(&self, doc: DocId, pos: Position) -> Result<CompletionList>;
    async fn hover(&self, doc: DocId, pos: Position) -> Result<Option<Hover>>;
    async fn definition(&self, doc: DocId, pos: Position) -> Result<Vec<Location>>;
    async fn references(&self, doc: DocId, pos: Position) -> Result<Vec<Location>>;
    async fn rename(&self, doc: DocId, pos: Position, new: &str) -> Result<WorkspaceEdit>;
    async fn code_action(&self, doc: DocId, range: Range) -> Result<Vec<CodeAction>>;
    async fn formatting(&self, doc: DocId, range: Option<Range>) -> Result<Vec<Edit>>;
    fn diagnostics(&self) -> &DiagnosticStore; // 由后台事件更新，读不加锁阻塞
}
```

### 6.4 调试

```rust
pub enum DebugState { Idle, Launching, Initialized, Stopped, Running, Terminating, Terminated, Failed }

#[async_trait]
pub trait DebugAdapter: Send + Sync {
    fn adapter_id(&self) -> &str;
    async fn launch(&mut self, cfg: &LaunchConfig) -> Result<()>;
    async fn set_breakpoints(&mut self, file: &Path, bps: &[Breakpoint]) -> Result<Vec<Breakpoint>>;
    async fn continue_(&mut self, thread: i64) -> Result<()>;
    async fn step(&mut self, thread: i64, kind: StepKind) -> Result<()>;
    async fn stack_trace(&mut self, thread: i64) -> Result<Vec<StackFrame>>;
    async fn scopes(&mut self, frame: i64) -> Result<Vec<Scope>>;
    async fn variables(&mut self, r: i64) -> Result<Vec<Variable>>;
    async fn evaluate(&mut self, frame: Option<i64>, expr: &str) -> Result<Variable>;
    async fn terminate(&mut self) -> Result<()>;
}
```

### 6.5 AI 适配层（最关键）

```rust
pub struct AgentCapabilities {
    pub headless: bool,          // 支持非交互调用
    pub streaming: bool,         // 支持增量输出
    pub structured: bool,        // 输出可结构化解析
    pub tool_calls: bool,        // 有工具调用事件
    pub patch_output: bool,      // 能产出标准补丁
}

pub struct AgentRequest<'a> {
    pub prompt: &'a str,
    pub context: &'a AgentContext,   // 已经过 .aiignore 与敏感清单过滤
    pub cwd: &'a Path,
    pub timeout: Duration,
}

pub enum AgentEvent {
    TextDelta(String),
    ReasoningDelta(String),          // 若 CLI 区分思维过程
    ToolCall { name: String, args: serde_json::Value },
    ToolResult { name: String, ok: bool, summary: String },
    PatchProposal { patch: UnifiedPatch, base_hashes: HashMap<PathBuf, String> },
    Usage { input: u64, output: u64 },
    Error { code: String, message: String },
    Done { exit: AgentExit },
}

pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;                    // "codex" / "claude-code" / "codebuddy"
    fn display_name(&self) -> &str;
    fn capabilities(&self) -> AgentCapabilities;
    fn probe(&self) -> ProbeResult;                  // Installed { version } | Missing | TooOld
    fn build_invocation(&self, req: &AgentRequest) -> Result<Invocation>; // 返回 program/args/stdin
    fn parse_event(&self, line: &str) -> Option<AgentEvent>;              // 容错：无法解析返回 None
    fn classify_exit(&self, status: ExitStatus) -> AgentExit;
}
```

**适配器编写要求**：
- 每个适配器配一组 `tests/fixtures/<adapter>/*.jsonl` 真实输出样例，作为契约回归；CLI 升级后样例不全即测试失败。
- `parse_event` 必须容忍未知字段与半行（最后一行可能无换行），不得 panic。
- 新增适配器**不得**修改 `mtide-ai` 核心逻辑，只能新增文件 + 注册。
- 禁止在适配器里拼接 shell 字符串；一律 argv 数组。

### 6.5b 交互式会话（PRD AI-17）

```rust
pub struct SessionSpec {
    pub adapter_id: &'static str,   // "codex" / "claude-code" / 自定义
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

pub trait SessionHost {
    /// 在 PTY 中启动会话，返回可隐藏但不终止的句柄
    fn spawn(&mut self, spec: SessionSpec, size: TerminalSize) -> Result<SessionId>;
    /// 隐藏面板不终止进程；再次 attach 恢复
    fn attach(&mut self, id: SessionId, size: TerminalSize) -> Result<&VtScreen>;
    fn resize(&mut self, id: SessionId, size: TerminalSize) -> Result<()>;
    fn send_input(&mut self, id: SessionId, bytes: &[u8]) -> Result<()>;
    fn terminate(&mut self, id: SessionId) -> Result<ChangeReport>; // 返回 AI-20 的变更清单
}

pub struct ChangeReport {
    pub files: Vec<FileChange>,        // 会话期间被改动且非用户编辑的文件
    pub baseline_hashes: HashMap<PathBuf, String>,
}
```

**实现要求**：会话期间由 `mtide-workspace` 的文件监听提供变更事件，`SessionHost` 只负责按「会话起始时间 + 是否属于编辑器事务」过滤，不自己扫盘。

### 6.6 Diff Gate

```rust
pub struct DiffGate;

impl DiffGate {
    /// 解析补丁并与工作区实际内容比对；任何文件 base hash 不符即整体拒绝
    pub fn stage(&self, patch: &UnifiedPatch, ws: &WorkspaceRead) -> Result<PatchPreview, GateError>;
    /// 只应用被采纳的 hunk；应用失败整体回滚
    pub fn apply(&self, preview: &PatchPreview, accepted: &[HunkId]) -> Result<ApplyReport, GateError>;
    /// 回滚使用的快照由 apply 生成，保存在会话快照目录
    pub fn rollback(&self, report: &ApplyReport) -> Result<(), GateError>;
}
```

**Gate 的不可绕过性**：`mtide-ai` 之外不存在写工作区文件的路径给 AI 用；`mtide-workspace::write` 内部断言调用方不是 AI 通道（除 DiffGate 持有的一次性 token）。

### 6.7 插件 RPC（JSON-RPC over stdio）

| 方法 | 方向 | 说明 |
|---|---|---|
| `plugin/initialize` | host→plugin | 传入宿主版本、插件权限、工作区根 |
| `plugin/contributions` | plugin→host | 返回贡献项（命令/键位/主题/语法/面板） |
| `command/execute` | host→plugin | 执行插件命令，返回 `CommandOutcome` |
| `panel/render` | host→plugin | 请求面板内容，返回 `WidgetNode`（**不得含 ANSI**） |
| `event/notify` | host→plugin | 文档变更、光标移动、Git 状态等（节流后） |
| `log/write` | plugin→host | 结构化日志，进入诊断包 |

```rust
#[derive(Serialize, Deserialize)]
pub enum WidgetNode {
    Text { text: String, style: StyleToken },        // style 是语义 token，不是 ANSI
    List { items: Vec<WidgetNode>, selected: Option<usize> },
    Input { value: String, placeholder: Option<String> },
    Button { label: String, command: String },
    Table { headers: Vec<String>, rows: Vec<Vec<String>> },
    Progress { ratio: f32, label: String },
    VStack(Vec<WidgetNode>),
    HStack(Vec<WidgetNode>),
}
```

---

## 7. 里程碑

> 每个里程碑结束必须：`cargo xtask check` 全绿 + 该里程碑内所有 Must 需求有对应自动化测试 + 更新 `CHANGELOG.md`。
> 「完成」的定义不是「代码写了」，而是「PRD 里那几条验收 GWT 能自动跑通」。

### M0 骨架（约 2 周）

- 目标：能启动、能画出一个带边框的空界面、Ctrl+Q 干净退出、终端状态必被还原。
- 交付：workspace 结构、`mtide-tui` 的 cell buffer 与差量输出、终端能力探测（真彩色/256/16、宽字符）、输入事件泵、panic hook + 终端恢复、日志落盘、`xtask check` 与 depcheck 骨架。
- DoD：80×24 / 120×40 / 200×60 三尺寸快照通过；强制 panic 后终端不乱码；冷启动 ≤60ms（空界面）。
- 禁止：此阶段不做任何业务功能。

### M1 编辑器内核（约 4 周，PRD 7.1）

- 目标：单文件编辑达到「可日常使用」。
- 交付：rope 文档、光标/选区/多光标（ED-02/18）、撤销树（ED-03）、键位系统与命令注册表、查找替换（ED-05）、折叠（ED-07）、行操作（ED-08）、缩进与 editorconfig（ED-09）、保存与外部修改检测（ED-11）、编码换行（ED-13）、大文件模式（ED-12）、只读与二进制（ED-14）、自动保存（ED-17）。
- DoD：100 万行文件打开 ≤3s、插入 <10ms；多光标撤销原子性测试通过；ED 域 18 条 Must/Should 全部有测试。
- 陷阱：本阶段就要做「UTF-8 byte ↔ 显示列 ↔ 宽字符」三套坐标的统一，后面无法补救。

### M2 工作区与 UI（约 4 周，PRD 7.10 / 7.11 / 7.6 / 7.7）

- 目标：像一个 IDE，而不像一个编辑器。
- 交付：**VT 缓冲（`mtide-vt`，ANSI 解析 + 屏幕状态机 + 备用屏 + 鼠标协议 + resize 透传）**、**PTY 与终端面板（TR-01~07）**、布局引擎（UI-01）、双主题（UI-02：Retro 先做）、命令面板（UI-03）、快捷键四套预设与 VSCode/VSCodium 热键兼容（UI-04/UI-19）、状态栏（UI-05）、通用组件（UI-06）、鼠标（UI-07）、字符集降级（UI-10）、滚动条与标记（UI-12）、键位帮助（UI-13）、专注模式（UI-14）、文件图标（UI-15）、多根工作区（WS-01）、四层配置（WS-02）、CLI 参数（WS-04）、文件监听（WS-05）、`.mtide` 工作区文件（WS-09）、文件模糊查找（SR-01）、最近文件（NV-01）、书签（NV-04）、智能扩展选区（NV-09）、比较（NV-10）、分屏同步滚动（NV-13）。
- DoD：80×24 下开启全部面板不崩且编辑区 ≥40 列；Retro 主题下可完成全流程；配置错误给行号；**VT 一致性测试集通过（见第 10 节陷阱 7）**。
- 禁止：Retro 与 Modern 不得有功能差异（UI-02 验收）；主题仅换配色与边框，不改布局模型以外的逻辑。

### M3 语法高亮（约 3 周，PRD 7.2 LN-01/02/04）

- 目标：打开任意文件都有正确颜色。
- 交付：语言检测（LN-01）、TextMate 引擎接入与增量重解析（LN-02）、括号匹配与缩进参考线（LN-04）、内置 20 语言语法打包、tree-sitter Top10 接入用于折叠与结构选择、大文件降级联动。
- DoD：多行结构（模板字符串、heredoc、Rust 原始字符串）不串色；切换文件 2000 行内高亮 <30ms；语法包缺失时降级不崩。

### M4 语言智能（约 6 周，PRD 7.2 LN-03/05~16）

- 目标：补全、诊断、跳转可用，用户不需要手写配置。
- 交付：JSON-RPC 传输层、LSP 客户端与生命周期状态机（LN-06/16）、补全（LN-07）、签名（LN-08）、诊断（LN-09）、跳转与引用（LN-10/11）、重命名（LN-12）、代码动作（LN-13）、格式化（LN-14）、语言配方库与探测安装（LN-15）、语义高亮（LN-03）、inlay hints（LN-05）。
- DoD：用 rust-analyzer + gopls + pyright 三个真实服务器跑通全能力集成测试；服务器崩溃后自动重启并降级；UTF-16 位置转换有专门测试（含 CJK 与 emoji）。
- 陷阱：`textDocument/didChange` 的增量同步必须与本地 edit 顺序严格一致，否则诊断错位——建议先实现全量同步（`TextDocumentSyncKind::Full`）跑通，再开增量。

### M5 构建、运行与搜索（约 3 周，PRD 7.3 / 7.6）

- 目标：F9 能干活。
- 交付：任务系统（BD-01）、配方默认命令（BD-02）、问题匹配器（BD-03，先做 cargo/go/tsc/gcc 四种）、F9 与 Ctrl+F9（BD-04）、输出面板（BD-05）、测试运行（BD-06）、并发与中断（BD-07）、全文搜索（SR-02）、工作区替换（SR-03）、结果面板（SR-04）、搜索健壮性（SR-05）。
- DoD：cargo 项目构建失败时问题面板可跳转到准确行列；搜索排除 `node_modules` 且软链环不死循环；F9 重复按不并发启动。

### M6 Git（约 4 周，PRD 7.5）

- 目标：不离开 IDE 完成日常 Git。
- 交付：仓库发现（GT-01）、状态（GT-02）、暂存含区块级（GT-03）、提交含钩子输出（GT-04）、分支（GT-05）、diff 双模式（GT-06）、内联 blame（GT-07）、历史（GT-08）、冲突解决（GT-09）、远程操作（GT-10）、stash（GT-11）、大仓库性能（GT-12）。
- DoD：在含 submodule 与 worktree 的临时仓库上集成测试通过；钩子失败不丢提交消息；冲突全部解决后自动 add；所有解析走 `-z` 与 porcelain=v2，禁止解析人类可读输出。

### M7 调试（约 5 周，PRD 7.4）

- 目标：能真的断点调试三种语言。
- 交付：DAP 客户端与状态机（DB-01/03）、断点模型（DB-02）、调用栈（DB-04）、变量（DB-05）、监视（DB-06）、REPL（DB-07）、内联值（DB-08）、配置（DB-09）、适配器配方与健康检查（DB-10）、调试 UI（DB-11）、会话清理（DB-12）。
- DoD：delve（Go）、debugpy（Python）、lldb-dap（Rust/C）三语言真实断点命中；强制退出 IDE 后无孤儿进程；变量面板惰性展开 1000 元素不卡。
- 陷阱：DAP 的初始化顺序（initialize → initialized 事件 → setBreakpoints → configurationDone）与 LSP 不同，不要复用同一套会话状态机。

### M8 AI 编排（约 5 周，PRD 7.8）

- 目标：把 Codex / Claude Code / CodeBuddy 变成 IDE 内可控的一等公民。
- 交付：适配层契约与注册表（AI-01）、三个内置适配器（AI-02）、**无头流式（AI-03）**、**交互式会话（AI-17，复用 M2 的 `mtide-vt`）**、**会话多标签与持久化（AI-18）**、**安装引导需确认（AI-19）**、**会话期间变更追踪（AI-20）**、**自定义适配器（AI-21）**、上下文收集（AI-04）、diff gate（AI-05）、会话管理（AI-06）、中断超时（AI-07）、三层解析（AI-08）、探测健康检查（AI-09）、外发管控（AI-10）、审计（AI-11）、任务模板（AI-12）、结果落点（AI-16）。
- DoD：三 CLI 各 20 次结构化任务成功率 ≥95%；**各 5 次交互会话：隐藏面板后重新选中仍存活、尺寸变化正确重排、可观测期间变更全部进入清单，监听丢事件时标记未知来源**；未确认时文件 hash 不变；`.env` 内容不出现在任何请求（mock CLI 抓 stdin 断言）；杀进程后 5s 内清理。
- 禁止：不做 PTY **分屏**内嵌的复杂交互增强；不做自动执行 AI 建议的命令；不做静默安装；**交互会话模式下不得声称 IDE 能限制 CLI 的读取范围**（EX-05/SEC-05）。

### M9 插件（约 4 周，PRD 7.9）

- 目标：第三方能在不改核心的前提下加能力。
- 交付：清单与注册（PL-01）、进程宿主与 RPC（PL-02）、权限（PL-03）、Widget 树渲染（PL-04）、**六类扩展点（PL-05，含侧栏面板与右键菜单）**、生命周期与崩溃隔离（PL-06）、资源限制（PL-07）、安装管理（PL-08）、五个官方示例 + `mtide plugin new` 脚手架（PL-09）、**社区注册表与 URL 安装（PL-11）**、**热重载与开发模式（PL-12）**。
- DoD：插件崩溃 100% 不影响宿主；含 ANSI 的文本被转义显示；越权调用被拒并留痕；五个示例用文档步骤可构建运行；**脚手架生成的插件可直接安装并出现在面板**。

### M10 打磨与发布（约 4 周，PRD 7.11 / 第八章）

- 交付：会话恢复（WS-03）、设置界面（WS-06）、诊断导出（WS-07）、更新提示（WS-08）、**本地历史（NV-03）**、**文件时间线（NV-05）**、**最近编辑位置（NV-02）**、**结构化搜索（NV-12）**、**覆盖率展示（BD-08）**、**变更列表与搁置（GT-17/18）**、**图片预览（UI-16）**、**Markdown 渲染（UI-17）**、**Minimap（UI-18）**、无障碍（UI-09）、Modern 主题补齐、低带宽模式、性能门禁全绿、三平台打包、终端兼容矩阵测试。
- DoD：NF-01~NF-16 全部有自动化或可复现验证；崩溃后恢复内容与崩溃前 diff 为空；**本地历史容量超限按 LRU 清理且恢复动作本身可撤销**；三平台安装包可运行。

### M0–M10 之外：v2.0 新增 48 条需求的落点速查

| 里程碑 | 承接的 v2.0 新增需求 |
|---|---|
| M1 | ED-19~ED-25（退出保存全部、stdin 缓冲区、软换行、代码片段、文本转换、包围、粘滞滚动） |
| M2 | TR-06/07、UI-11~15、WS-09、NV-01/04/09/10/13、**`mtide-vt` VT 缓冲** |
| M3 | NV-12（结构化搜索，依赖 tree-sitter） |
| M4 | LN-17/18/19、NV-06/07/08（面包屑、调用/类型层级、符号大纲 NV-11） |
| M5 | BD-08/09 |
| M6 | GT-14/15/16/17/18（含编辑器内可编辑 diff） |
| M8 | AI-17~AI-21（交互会话、会话持久化、安装引导、变更追踪、自定义适配器） |
| M9 | PL-11/12 |
| M10 | NV-02/03/05、UI-09/16/17/18 |

---

## 8. 编码规范

| 项 | 规则 |
|---|---|
| 格式 | `cargo fmt`，提交前必跑 |
| Lint | `cargo clippy -- -D warnings`；允许的例外必须在行内注明理由 |
| 命名 | 模块 snake_case、类型 CamelCase、trait 用能力名（`AgentAdapter` 而非 `IAgent`）；命令 id 用 `域.动作`（`editor.save`） |
| 文件规模 | 单文件 ≤600 行，超了拆模块 |
| 错误 | 库返回 `Result<T, E>` 且 `E` 为 `thiserror` 枚举；面向用户的错误必须带 `user_message()` 与 `recovery_hint()` |
| 并发 | 共享状态优先消息传递（channel）；必须共享时用 `RwLock` 且临界区不得含 IO |
| 日志 | `tracing`，字段化（`lang=`, `path_hash=`, `doc_id=`）；**禁止记录源码内容与绝对路径原文** |
| 注释 | 只解释「为什么」，不解释「做了什么」；公开 API 写 `///` |
| 测试命名 | `fr_<域><编号>_<行为>`，如 `fr_ed02_multi_cursor_undo_atomic` |
| 提交粒度 | 一个提交只做一件事；提交信息 `type(scope): subject` |

---

## 9. 性能门禁（CI 阻断项）

| 指标 | 阈值 | 测法 |
|---|---|---|
| 冷启动 P95 | ≤150ms | 20 万行合成项目，10 次取 P95，`criterion` |
| 按键到渲染 P95 | ≤30ms | 10 万行文件滚动中注入按键，测量帧时间 |
| 大文件打开 | 50MB ≤3s | 合成日志文件 |
| 常驻内存 | ≤150MB | 10 万行 + 2 mock LSP + 1 终端，RSS 采样 |
| 渲染输出字节数 | 单帧 ≤ 屏面积 × 4 字节 | 差量渲染断言（防全屏重绘退化） |

**任何 PR 使上述任一指标退化 >10% 即阻断**，需在 PR 说明原因并获得批准。

---

## 10. 常见陷阱（踩过就别再踩）

1. **三套坐标**：UTF-8 byte offset / 显示列（含宽字符）/ 行号。三者转换集中在 `mtide-core::coords`，禁止在业务里手算列宽。
2. **宽字符**：用 `unicode-width` + 自定义 emoji/ZWJ 簇处理；测试必须含中文、日文、Emoji 家庭组合、组合重音。
3. **LSP 位置是 UTF-16**：内部用 byte，转换只在 `mtide-lsp::conv`。混用会在中文注释处跳错行。
4. **resize 抖动**：100ms 内合并，只重排一次；否则 tmux 分屏拖拽会疯狂闪烁。
5. **差量渲染必须包含光标与 IME 预编辑区**，否则中文输入法候选窗位置错乱。
6. **子进程 SIGPIPE**：向已退出进程写 stdin 会收到 EPIPE，必须当作正常结束处理，不能 panic。
7. **PTY 必须设置 `TERM`、初始窗口尺寸，并分离读写线程**；否则子进程 `vi` 会显示 80×24 错位。
8. **git 输出解析**：只用 `--porcelain=v2 -z`；不要解析 `git status` 的人类可读输出；路径含空格/中文必须走 `-z`。
9. **AI CLI 输出是行缓冲**：最后一行可能没有换行；`parse_event` 要能处理半行并在 EOF 时兜底 flush。
10. **补丁应用**：先校验 base hash，再做 3-way；不做模糊匹配强写；失败整体回滚而不是部分写入。
11. **快照原子写**：临时文件 + `rename`；进程被杀在写一半时会留下损坏文件，加载时须校验并可跳过。
12. **Windows ConPTY**：`\r\n` 与 `\n` 混用、Ctrl 键语义、路径分隔符三处差异；条件编译隔离，不要在主逻辑里塞 `cfg!(windows)` 分支。
13. **undo 与 LSP 同步**：撤销后必须发送 `didChange`，否则服务器状态与本地漂移（诊断会指向不存在的位置）。
14. **插件渲染别在主线程同步等待**：`panel/render` 必须异步请求，超时就用上一帧 + 「加载中」占位。
15. **VT 缓冲（`mtide-vt`）的正确性靠测试集，不靠肉眼**：建立「输入字节序列 → 期望屏幕网格」的用例库（光标移动、滚动区、备用屏、插入/删除行、宽字符、SGR 组合、鼠标协议），任何改动都必须跑。出错表现是「屏幕花了」而不是报错，肉眼调试会付出数倍代价。
16. **交互会话的 resize 要合并**：面板尺寸变化时合并 100ms 内事件，只向 PTY 发一次 SIGWINCH 并重排一次，否则 tmux 拖拽会疯狂闪烁。
17. **变更追踪要区分来源**：AI-20 判定「AI 改动」时必须排除「用户在编辑器内的编辑」（后者有明确的事务记录），否则用户手改会被误判为 AI 改动并进入待审阅清单。
18. **本地历史快照必须原子写 + LRU 清理**：恢复动作执行前先为当前状态建一个快照，保证「恢复」本身可撤销；清理不得删除用户正在查看的快照。

---

## 11. PR 规范

```
标题：feat(lsp): completion debounce and live filtering  [FR-LN-07]
正文必须包含：
1. 需求编号与 PRD 章节
2. 改动摘要（≤5 条）
3. 测试说明（新增/修改了哪些测试，如何跑）
4. 性能影响（是否触及门禁，数据）
5. 风险与回滚方式
6. 截图/录屏（涉及 UI 时必填，80×24 与 120×40 各一张）
```

**自检清单（提交前逐项确认）**：

- [ ] `cargo xtask check` 全绿
- [ ] 新增/修改行为都有测试
- [ ] 没有无编号 TODO
- [ ] 没有 `unwrap`/`panic` 进入库代码
- [ ] 没有新增 GPL 依赖
- [ ] 没有硬编码用户文案（走 i18n）
- [ ] 日志不含源码内容与绝对路径原文
- [ ] 未确认的自动行为（下载/安装/写盘/执行）都默认关闭
- [ ] 依赖方向未被破坏（depcheck 通过）
- [ ] CHANGELOG 已更新

---

## 12. 禁止事项（Hard No）

| # | 禁止 | 原因 |
|---|---|---|
| HN-1 | 实现 VSCode 扩展 API 兼容层 | 产品明确不做，成本无底 |
| HN-2 | 让插件输出 ANSI 或直接操作屏幕 | 破坏渲染管线与主题 |
| HN-3 | 绕过 DiffGate 写工作区文件（尤其 AI 通道） | 产品核心承诺 |
| HN-4 | 静默下载/安装任何二进制（LSP、DAP、插件） | 企业合规红线 |
| HN-5 | 默认开启遥测，或上报代码内容、提示词、绝对路径原文 | 隐私底线 |
| HN-6 | 引入 GPL/AGPL 依赖，或内置语言服务器二进制 | 许可风险 |
| HN-7 | 在主线程做阻塞 IO 或同步等待子进程 | 性能承诺 |
| HN-8 | 用 `unsafe` 绕过借用检查 | 内存安全 |
| HN-9 | 自动 `git push --force`、自动丢弃用户改动、自动杀端口占用进程 | 不可逆操作 |
| HN-10 | 把 `unwrap` 当错误处理，或吞掉错误只打日志不告知用户 | 可诊断性 |
| HN-11 | 未经确认执行 AI 建议的 shell 命令 | 安全 |
| HN-12 | 在宣传或代码注释中使用他方商标指代本产品 | 合规 |
| HN-13 | 交互会话模式下暗示或声称「IDE 会审核/限制 AI 的读取与写入」 | 事实不符，PRD SEC-05 要求明示不可控，只提供变更追踪与回滚 |
| HN-14 | 自动执行第三方安装脚本（AI CLI install、插件 install 钩子）而不经用户确认 | 供应链与合规红线 |
| HN-15 | 让 `mtide-vt` 依赖 `mtide-tui` / `mtide-widget`，或把屏幕状态机写进渲染层 | 破坏 VT 测试集可脱离终端运行的前提 |

---

## 13. 排障与可观测

- `mtide --log-level=debug` 输出详细日志到 `~/.local/share/mtide/logs/`
- `mtide --no-lsp` / `--no-plugins` / `--safe-mode` 用于定位「是不是扩展引起的」
- `mtide --doctor` 自检：终端能力、宽字符对齐、git 版本、已装 LSP/DAP、AI CLI 探测结果、配置校验
- 内部错误统一 `ErrorReport { code, module, recoverable, user_message, hint }`；code 形如 `LSP-003`
- 诊断包 `mtide --export-diag` 生成 zip：版本、平台、终端能力、脱敏配置、插件列表、近 3 天日志；**必须过滤源码与提示词**

---

## 14. 需要人类决策的待办（不要自行决定）

| 编号 | 问题 | 影响 |
|---|---|---|
| Q-01 | 正式产品名与 crate 前缀是否沿用 `mtide` | 影响包名、配置目录、文档，改名成本随时间上升 |
| Q-02 | 插件清单格式（TOML vs JSON）与扩展点版本策略 | 影响插件生态兼容 |
| Q-03 | 是否内置 tree-sitter Top10 之外的语法，还是全走下载 | 影响二进制体积与首启体验 |
| Q-04 | 遥测是否提供、由谁运营、隐私政策文本 | 影响合规与信任 |
| Q-05 | 企业策略文件的字段与优先级最终稿 | 影响 v1.0 企业可用性 |
| Q-06 | 三平台中 Windows 是否首发即一级支持（ConPTY 风险） | 影响 M10 工期 |
| Q-07 | 插件市场是否 v1.2 立项（涉及签名与分发基础设施） | 影响路线图 |

---

## 15. 附：给智能体的任务卡模板

```md
## 任务：<一句话>
- 需求编号：FR-XX-NN（PRD 7.x）
- 里程碑：M?
- 输入：相关文件路径、现有测试、已知约束
- 期望行为：<可观测的行为描述>
- 验收：<可直接跑的测试断言>
- 不要做：<明确的边界，防止扩大范围>
- 完成后：跑 cargo xtask check；更新 CHANGELOG；PR 附测试说明与性能影响
```

> 一句话总结给智能体：**先找编号，先写测试，不猜产品，不越边界，不破坏那四条承诺。**
