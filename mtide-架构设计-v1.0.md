# mtide 系统架构设计 v1.0

> **面向**：在 `mtide` 仓库中生成代码的编码智能体（Codex / Claude Code / CodeBuddy）与人类协作开发者。
> **事实源优先级**：`mtide-PRD-v2.0.md`（v2.2，170 条需求，唯一产品事实源） > 本文件（架构与选型） > `AGENT.md`（编码约束与里程碑）。
> 三者冲突时：产品语义以 PRD 为准并标 `NEEDS-PM`；实现约束以 AGENT.md 为准；结构边界以本文件为准。
> **本文件的定位**：回答「系统分成几层、每层边界在哪、为什么这样选、代价是什么、按什么顺序落地」。不重复 PRD 的需求描述，不重复 AGENT.md 的编码规范。

---

## 0. 文档信息

### 0.1 变更记录

| 版本 | 日期 | 变更 |
|---|---|---|
| v1.1 | 2026-11-03 | 评审修订：收敛 workspace 依赖方向；明确插件 RPC broker 不是 OS 沙箱；将 AI 交互会话变更追踪改为尽力而为并引入未知来源；补充原子写耐久性、启动门禁与阶段化开发基线 |

### 0.2 本文依赖的已锁定决策（PRD §1.3）

| 编号 | 决策 | 对架构的直接约束 |
|---|---|---|
| D-01 | 功能覆盖面学 VSCode、交互骨架学 Borland、外观双主题 | 命令模型与键位模型只有一套；主题只换皮，不得出现「Retro 缺功能」 |
| D-02 | 以远程 SSH 主力开发为设计基准 | 首屏不得等待任何子进程；低带宽下差量输出必须可控 |
| D-03 | 不兼容 VSIX | 自建扩展点，无需为兼容层留后门 |
| D-04 | Rust，2–4 人 | 无 GC、单二进制；模块数必须控制在人力可维护范围（≤16 crate） |
| D-05 | AI 双模（结构化 + 交互会话） | 需要 `mtide-vt`；AI 写盘唯一通道是 DiffGate |
| D-12 | 插件宿主：进程 RPC 为主 | 插件是子进程，全部交互走 stdio JSON-RPC |
| D-13 | 与 TTT 差异化 = 调试 + 结构化 AI + 进程级插件 + 双主题 | 调试（DAP）与 AI 可控性架构上必须最强 |

### 0.3 未确认前提（架构假设，变化会影响方案）

| # | 假设 | 若变化的影响 |
|---|---|---|
| AS-1 | 团队 2–4 人，无专职测试 | 若 1 人，P3/P4 阶段需合并，插件与 Minimap 降级为可选 |
| AS-2 | 目标终端列宽 80–200、行高 24–60 | 若需支持 <60 列，布局引擎要再拆一档降级策略 |
| AS-3 | 项目规模量级：≤20 万文件、单文件 ≤256MB | 若需百万文件仓库，文件索引须从「内存哈希」改为「磁盘索引 + mmap」 |
| AS-4 | v1.0 不引入 WASM 插件运行时 | 若必须内置 WASM，二进制体积预算需从 45MB 上调至 70MB+（见 §7.9） |
| AS-5 | 无服务端、无账号、无云同步 | 一旦需要云同步，配置层需增加「冲突合并」语义 |

---

## 1. 架构决策摘要（先结论）

| # | 决策 | 一句话理由 | 主要代价 |
|---|---|---|---|
| **A-01** | **单进程 + 进程外一切重活与不可信代码** | LSP/DAP/AI CLI/插件/git/构建 全部是子进程，宿主只做协议与编排 | 进程启动与 IPC 开销；需要一套统一的子进程监督机制 |
| **A-02** | **三类执行体硬分离**：UI 主线程（零阻塞 IO）/ tokio IO 池 / rayon CPU 池，只经 channel 与不可变快照通信 | 唯一能同时满足「键位 P95 ≤30ms」与「不卡死」的结构 | 禁止跨线程共享 `&mut`，领域对象必须支持快照语义，设计成本上升 |
| **A-03** | **渲染自研**：Cell Buffer + 差量输出，不引入 ratatui | 插件要 Widget 树、低带宽要精确控制重绘、双主题要布局参数化 | 布局引擎与组件库自研，约 +2 人月 |
| **A-04** | **VT 状态机独立 crate（`mtide-vt`），零 UI 依赖** | 出错表现是「屏幕花了」而不是报错，必须能脱离终端用「字节序列 → 期望网格」单测 | 需要自建 VT 一致性测试集（`xtask vt-test`） |
| **A-05** | **一切外部能力走「配方 + 探测 + 显式确认」**，禁止静默下载/安装/写盘 | 企业合规红线（PRD SEC-12 / RK-03） | 首次使用多一次确认，开箱体验略差 |
| **A-06** | **AI 写盘唯一通道 = DiffGate**，编译期（trait 边界）+ 运行时（一次性 token）双重强制 | 把「AI 不得未经确认改代码」从流程约定变成结构约束 | AI 相关写路径必须全部走同一入口，灵活性下降 |
| **A-07** | **插件只返回 `WidgetNode` 树，ANSI 不得进入渲染管线** | 两个插件同时输出转义序列必然打花屏幕，且主题与无障碍全废 | 插件表达能力弱于直接画屏，需组件库持续补全 |
| **A-08** | **几何坐标三套（UTF-8 byte / 显示列 / UTF-16 code unit）转换收敛到单点** | 混用会导致中文注释处跳转错位，且症状是「偶尔错」，不可查 | 所有领域代码必须显式区分坐标类型，命名负担 |
| **A-09** | **本地状态全部文件化 + 原子写 + 快照 LRU**，无数据库 | 单机工具，崩溃可恢复优先于查询能力 | 本地历史与搜索索引需自己做容量治理 |
| **A-10** | **配置四层**（内置默认 < 用户 < 项目 < 企业策略），策略层不可被 UI 绕过 | 企业场景需要强制 `.aiignore`、强制审计、禁用 CLI | 配置解析需支持「只读锁定项」的展示与说明 |
| **A-11** | **首屏不等待任何异步能力**：LSP/git/插件/索引全部后台化，UI 用占位态 | 冷启动 P95 ≤150ms 只能靠这个 | 首秒体验存在「诊断未就绪」的中间态，需要明确的状态指示 |
| **A-12** | **降级是一等公民**：每个外部依赖都有「缺失 → 降级 → 可恢复」路径 | 用户环境不可控（无 git / 无 LSP / 无 CLI） | 每个集成点都要写降级分支与状态展示，代码量增加 |

---

## 2. 质量属性优先级

> 架构不可能同时最大化所有质量属性。本项目排序如下，**后续所有取舍按此顺序裁决**。

| 优先级 | 质量属性 | 为什么排这里 | 为此牺牲了什么 |
|---|---|---|---|
| **1** | **交互确定性**（键位 P95 ≤30ms、冷启动 P95 ≤150ms、滚动 ≥30fps） | 终端 IDE 的全部价值建立在「像本地编辑器一样跟手」上；SSH 场景下任何卡顿都会被直接感知为「不如 vim」 | 放弃全局最优的一次性索引、放弃同步等待诊断、放弃跨模块共享可变状态换来的零拷贝 |
| **2** | **故障隔离与优雅降级**（崩溃隔离率 100%） | 宿主一崩，终端残留状态会让用户连 shell 都用不了，这是最坏的失败形态；且依赖的外部进程全都不可信 | 放弃同进程插件、放弃共享内存、接受 IPC 序列化成本与状态机复杂度 |
| 3 | **可控与合规**（AI 外发、插件权限、审计） | 企业场景一票否决项（PRD RK-03/RK-04，风险值均为 20） | 放弃「AI 全自动改代码」，放弃插件默认网络与命令能力 |
| 4 | 可维护性（2–4 人长期维护 170 条需求） | 模块边界与依赖方向必须能被 CI 机械校验 | 放弃「随手加个 util 包」的便利 |
| 5 | 资源占用（≤180MB 常驻、≤45MB 二进制） | 受限环境（容器/老机器）是目标场景，但排在响应之后 | 允许为性能做内存缓存（如高亮 span 缓存），再用 LRU 治理 |
| 6 | 生态扩展性 | v1.0 只做只读社区注册表，插件市场在 v1.2 | 放弃签名/评分/自动更新 |

**裁决示例**：当「更完整的语义高亮」与「首屏 150ms」冲突时，选择**可视区优先 + 后台补全**；当「插件直接画屏更灵活」与「屏幕不被打花」冲突时，选择 **Widget 树**。

---

## 3. 运行时拓扑

### 3.1 进程与线程边界

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ 终端（xterm / tmux / screen / ConPTY / SSH 会话）                             │
└───────▲────────────────────────────────────────────────────────▲─────────────┘
        │ stdin：键鼠事件 / 能力应答                 stdout：差量 cell + 转义│
┌───────┴────────────────────────────────────────────────────────┴─────────────┐
│ mtide 主进程（单进程；无守护进程、无本地服务端口、无常驻 IPC 服务端）          │
│                                                                              │
│  ┌─ [UI 主线程] ─────────────────────────────────────────────────────────┐   │
│  │ 输入泵 → 键位解析 → 命令注册表 → 领域调用(仅提交意图) → 布局 →          │   │
│  │ CellBuffer → 差量计算 → 输出刷写      ★ 此线程不得出现任何阻塞 IO ★     │   │
│  └───────│ mpsc：Command/Intent ────────────────────▲ broadcast：Event ───┘   │
│          ▼                                          │（不可变快照 / 差量）     │
│  ┌─ [tokio IO 运行时]  worker = min(4, ncpu) ────────────────────────────┐    │
│  │ LSP 客户端 · DAP 客户端 · 插件 RPC · AI 结构化任务 · git / 构建子进程 │    │
│  │ 文件监听（notify） · 审计写入 · 会话快照落盘                          │    │
│  └───────────────────────────────────────────────────────────────────────┘    │
│  ┌─ [rayon CPU 池]  n = max(2, ncpu-2) ──────────────────────────────────┐    │
│  │ 语法高亮（syntect）· tree-sitter 解析 · diff 计算 · 全文搜索 · 排序   │    │
│  └───────────────────────────────────────────────────────────────────────┘    │
│  ┌─ [PTY 线程组]  每会话 1 读线程 + 1 writer ────────────────────────────┐    │
│  │ mtide-term ──> mtide-vt（VT 状态机）──> 屏幕快照 ──> UI 合成          │    │
│  └───────────────────────────────────────────────────────────────────────┘    │
│  ┌─ [Supervisor] ────────────────────────────────────────────────────────┐    │
│  │ 子进程 RAII 登记 · 指数退避重启(1s/2s/4s) · 孤儿回收 · 降级决策        │    │
│  └───────────────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────────────┘
        │ fork/exec（全部外部能力一律进程外）
        ▼
 rust-analyzer · gopls · pyright · delve · debugpy · lldb-dap · git · cargo
 · codex · claude · codebuddy · 插件进程（任意语言）
```

**边界说明**：

| 边界 | 规则 |
|---|---|
| UI 主线程 ↔ 其它线程 | 只经 `mpsc`（意图/请求）与 `broadcast`（事件/快照）。**禁止** `Arc<Mutex<Doc>>` 跨线程共享；领域状态变更在领域侧串行化后以快照广播 |
| 领域服务 ↔ 外部进程 | 一律经 `mtide-proc` 的 RAII 句柄 + 协议编解码；进程句柄的生命周期绑定到会话对象，Drop 即回收 |
| PTY ↔ VT | PTY 读线程只做「读字节 → 喂 VT 解析器 → 生成屏幕快照 → 通知 UI」，不做任何渲染决策 |
| 外部进程 ↔ 文件系统 | 普通外部进程按 OS 权限运行；AI 交互会话进入前明示 IDE 无法限制 CLI 读取/写入范围，只提供尽力而为的变更追踪与回滚 |

### 3.2 主循环骨架（伪代码，语义不可改）

```rust
// mtide/src/main.rs —— 唯一允许创建 tokio runtime 的地方
fn main() -> Result<()> {
    let rt  = tokio::runtime::Builder::new_multi_thread()
                  .worker_threads(usize::min(4, num_cpus())).enable_all().build()?;
    let cpu = rayon::ThreadPoolBuilder::new()
                  .num_threads(usize::max(2, num_cpus() - 2)).build()?;
    let _guard = rt.enter();
    let mut app = App::bootstrap(rt.handle(), &cpu)?;   // 首屏：只加载必需项（A-11）
    app.render_first_frame()?;                          // 首帧完成后才允许后台任务入场
    app.spawn_background();                             // LSP/git/插件/索引/监听

    loop {
        // 1. 输入优先：先把队列里的输入事件排空
        while let Some(ev) = app.input.poll(Duration::ZERO)? { app.dispatch(ev)?; }
        // 2. 领域事件：最多处理一帧预算内的事件，超预算留给下一帧
        app.drain_events(Duration::from_millis(4))?;
        // 3. 渲染：脏标记 → 布局 → 差量 → flush
        if app.needs_render() { app.render()?; }
        // 4. 无事件则短暂阻塞等待（不忙轮询）
        app.input.poll(Duration::from_millis(8))?;
        if app.should_exit() { break; }
    }
    app.shutdown()   // 终端状态恢复、子进程回收、快照落盘（有超时兜底）
}
```

三条硬约束：**主线程不做文件/网络/子进程/磁盘阻塞 IO**（I-3）；终端输入泵最多等待 8ms，且不执行其它 IO；**每帧有预算**（见 §8.2）；**退出必恢复终端**（panic hook 兜底）。

---

## 4. 分层设计

### 4.1 分层总览

```
┌───────────────────────────────────────────────────────────────────────────┐
│ L4 交付与工程层   xtask(check/bench/snapshot/vt-test/package) · CI · 打包  │
│                   基准门禁 · 终端兼容矩阵 · 诊断包导出                      │
├───────────────────────────────────────────────────────────────────────────┤
│ L3 交互与编排层   mtide(bin)  CLI、事件循环、面板编排、命令注册表、键位、i18n│
│                   mtide-widget  布局引擎、组件库、焦点管理                  │
├───────────────────────────────────────────────────────────────────────────┤
│ L2 领域服务层     mtide-core 文档/编辑/坐标/撤销/命令模型                   │
│                   mtide-workspace 多根/四层配置/监听/快照/写盘唯一入口      │
│                   mtide-syntax · mtide-lsp · mtide-dap · mtide-task        │
│                   mtide-git · mtide-search · mtide-diff · mtide-nav        │
│                   mtide-ai（含 DiffGate/Guard/审计）· mtide-plugin         │
├───────────────────────────────────────────────────────────────────────────┤
│ L1 渲染与终端层   mtide-tui  cell buffer/差量/输入事件/能力探测             │
│                   mtide-vt   VT 状态机（★零 UI 依赖）                      │
│                   mtide-term PTY 管理、终端面板、AI 交互会话面板            │
├───────────────────────────────────────────────────────────────────────────┤
│ L0 契约与平台层   mtide-api   纯 DTO/trait：WidgetNode、AgentEvent、Patch  │
│                   mtide-proc  子进程 RAII、stdio JSON-RPC、PTY 平台封装     │
└───────────────────────────────────────────────────────────────────────────┘
```

### 4.2 每层职责与禁令

| 层 | 职责 | 明确不做 | 主要 crate |
|---|---|---|---|
| **L0 契约** | 定义跨进程/跨层传输的类型与 trait；保证插件协议与 AI 协议版本化 | 不含任何业务逻辑、不依赖 tokio（除可选 feature）、不读文件系统 | `mtide-api`、`mtide-proc` |
| **L1 渲染** | 把「屏幕应该长什么样」变成终端字节流；把终端字节流变成输入事件；VT 状态机 | 不知道「文件」「LSP」「Git」是什么；不做布局业务决策 | `mtide-tui`、`mtide-vt`、`mtide-term` |
| **L2 领域** | 全部产品能力：编辑、语言、调试、构建、Git、搜索、diff、导航、AI、插件宿主 | 不直接操作终端、不直接画屏（只产出 Widget 树/数据）、不创建 tokio runtime | 见 §5 |
| **L3 编排** | 组合领域能力为交互：面板、命令、键位、焦点、i18n、CLI | 不实现业务规则（命令只是转发与参数解析） | `mtide-widget`、`mtide`(bin) |
| **L4 工程** | 构建、门禁、打包、基准、快照、诊断包 | 不参与运行时 | `xtask` |

### 4.3 层间通信契约

| 方向 | 机制 | 数据形态 |
|---|---|---|
| L3 → L2 | 函数调用（同进程，UI 线程）+ `mpsc` 意图（耗时操作） | `Command` / `Intent` 枚举，参数来自 `mtide-api` |
| L2 → L3 | `broadcast` 领域事件 + 拉取式快照 | 不可变快照（`Arc<T>`）或差量，**禁止 `&mut` 跨线程** |
| L2 → L1 | 布局后产出 `RenderTree`（Widget 树的落地形态） | 语义样式 token，**不得含 ANSI** |
| L1 → 终端 | 差量 cell 流 | 字节缓冲，单帧单次 flush |
| L2 ↔ 外部进程 | `mtide-proc` 句柄 + JSON-RPC / 行协议 | 协议消息由 `mtide-api` 定义 |

---

## 5. 模块（crate）划分与依赖方向

### 5.1 完整 crate 清单

| crate | 层 | 职责 | 对应 PRD 域 |
|---|---|---|---|
| `mtide-api` | L0 | 纯 DTO 与 trait：`WidgetNode`、`AgentEvent`、`UnifiedPatch`、`RPC` 消息、协议版本号 | 全局契约 |
| `mtide-proc` | L0 | 子进程 RAII 句柄、stdio JSON-RPC 传输、PTY 跨平台封装、退避重启原语 | NF-12 / SEC-01 |
| `mtide-tui` | L1 | 终端后端抽象、cell buffer、差量算法、输入事件泵、终端能力探测、panic 恢复 | UI-01/10/11、NF-08 |
| `mtide-vt` | L1 | VT 状态机：ANSI 解析、屏幕网格、备用屏、鼠标协议、resize 重排。**仅依赖 `mtide-api`** | TR-01~07、AI-17、NF-06 |
| `mtide-term` | L1 | 终端面板与 AI 交互会话面板（消费 `mtide-vt`），scrollback 治理 | TR / AI-17/18/20 |
| `mtide-widget` | L3 | 布局引擎（约束式）、组件库（列表/输入/弹窗/表格/树/进度/分割）、菜单栏/下拉/二级菜单、焦点管理 | UI-01/03/06/20 |
| `mtide-core` | L2 | 文档模型（rope）、选区与多光标、撤销树、坐标系统、命令模型、键位映射与 Chord 解析 | ED 全部、UI-04/13/19 |
| `mtide-workspace` | L2 | 多根工作区、四层配置、文件监听、**写盘唯一入口**、会话/崩溃快照、本地历史存储 | WS 全部、NF-11 |
| `mtide-syntax` | L2 | TextMate 高亮、`tree-sitter`（Top10）、语言检测、增量失效、语法包加载 | LN-01/02/04、SR-02 辅助 |
| `mtide-lsp` | L2 | LSP 客户端、能力协商、文档同步、诊断存储、配方库、UTF-16 转换单点 | LN-03/05~19、NV-07/08/11 |
| `mtide-dap` | L2 | DAP 客户端、断点模型、会话状态机、变量/栈/求值 | DB 全部 |
| `mtide-task` | L2 | 任务执行、问题匹配器、输出流、运行配置、覆盖率解析 | BD 全部 |
| `mtide-git` | L2 | git 子进程封装（`--porcelain=v2 -z`）、状态、区块补丁、冲突、历史分页、PR | GT 全部 |
| `mtide-search` | L2 | 内置全文搜索（`ignore`+`grep-searcher`）、模糊文件查找、结果流控 | SR 全部 |
| `mtide-diff` | L2 | 统一 diff 引擎：hunk 计算、逐块采纳、三方合并、可编辑 diff 模型 | GT-06/09/14、NV-10、AI-05 |
| `mtide-nav` | L2 | 最近文件/位置栈、本地历史视图、书签、面包屑、大纲、时间线、层级视图 | NV 全部 |
| `mtide-ai` | L2 | `AgentAdapter` 契约与内置适配器、DiffGate、外发过滤、审计、会话编排 | AI 全部 |
| `mtide-plugin` | L2 | 插件宿主、RPC 服务端、权限与配额、生命周期、安装与注册表 | PL 全部 |
| `mtide` (bin) | L3 | CLI 参数、事件循环、面板编排、依赖注入、i18n、诊断导出 | 全局 |
| `xtask` | L4 | `check` / `bench` / `snapshot` / `vt-test` / `package` / `plugin-new` / `depcheck` | — |

**合计 20 个 crate（含 bin 与 xtask）**，在 2–4 人可维护范围内；P0/P1 只启用其中的最小子集，避免首期并行维护全部 crate。

### 5.2 相对 AGENT.md v2.0 的 crate 增补与理由

| 新增 | 为什么必须有 | 不新增会怎样 |
|---|---|---|
| **`mtide-proc`** | 子进程管理（RAII、退避、孤儿回收）被 LSP/DAP/AI/插件/task/git 六处复用，I-5 铁律要求「无孤儿」 | 六处各写一套，孤儿进程与退避策略必然不一致 |
| **`mtide-search`** | 搜索是 CPU 密集且要流控与可取消（EX-14 类场景），需要独立的性能门禁 | 塞进 workspace 会让工作区快照职责膨胀，且无法单独基准 |
| **`mtide-diff`** | diff 被三处共用：Git（GT-06/09/14）、比较（NV-10）、AI DiffGate（AI-05）。三处若各写一套，逐块采纳的语义必然漂移 | 「AI 补丁的 hunk」和「Git 的 hunk」行为不一致，是高风险缺陷源 |
| **`mtide-nav`** | PRD v2.0 新增 13 条 NV 需求（本地历史、书签、时间线、层级、大纲）在 AGENT.md 中无落点 | 这 13 条会被塞进 core/workspace/lsp，破坏依赖方向 |

> 增补后需同步更新 `AGENT.md §3` 的仓库结构与 `xtask depcheck` 规则（见 §11 任务卡 T-00）。

### 5.3 依赖方向（CI 机械校验）

```
mtide(bin) ──> { mtide-workspace, mtide-core, mtide-lsp, mtide-dap, mtide-task,
                mtide-git, mtide-search, mtide-diff, mtide-nav, mtide-ai,
                mtide-plugin, mtide-syntax, mtide-term, mtide-widget }
mtide-widget ──> mtide-tui ──> mtide-api
mtide-workspace ──> { mtide-core, mtide-api }
mtide-{lsp,dap,ai,plugin,task,git} ──> mtide-proc ──> mtide-api

mtide-term  ──> mtide-vt ──> mtide-api        （VT 零 UI 依赖，可脱离终端单测）
mtide-{lsp,dap,ai,plugin,task,git} ──> mtide-proc ──> mtide-api
```

**硬规则（违反即 CI 失败）**：

| # | 规则 | 目的 |
|---|---|---|
| R-1 | `mtide-vt` 不得依赖 `mtide-tui` / `mtide-widget` / 任何业务 crate | 保障 VT 可在无终端环境网格级单测（A-04） |
| R-2 | `mtide-core` 不得依赖 `tui`/`widget`/`lsp`/`git`/`ai`/`dap`/`plugin` | 编辑内核可无终端单测 |
| R-3 | `mtide-tui` 不得依赖任何业务 crate | 渲染层只认 `api` 的 Widget 与输入事件 |
| R-4 | **L2 业务 crate 之间禁止互相依赖**；协作经 `mtide-api` trait 或由 bin 编排 | 防止领域层退化成网状（唯一例外：`mtide-diff` 可被 `git`/`ai`/`nav` 依赖，它是纯函数库且无状态） |
| R-5 | `mtide-plugin` 只依赖 `api` + `workspace` 的**只读视图**，不得直接调 `git`/`ai`；能力必须走宿主 RPC broker | 可审计宿主能力调用；普通子进程不等同于 OS 沙箱 |
| R-6 | 只有 `mtide`(bin) 可创建 tokio runtime；库 crate 不得出现 `#[tokio::main]` / `Runtime::new` | 保证测试可自行构造运行时 |
| R-7 | `mtide-api` 的任何 `pub` 类型变更必须带协议版本号与兼容说明 | 插件/适配器契约稳定性 |

R-4 的例外需显式写白名单：`mtide-git → mtide-diff`、`mtide-ai → mtide-diff`、`mtide-nav → mtide-diff`。`mtide-diff` 自身只依赖 `mtide-api` + `mtide-core`（坐标）。`mtide-workspace` 不再依赖其它 L2 业务 crate；跨域编排统一由 bin 层完成。

---

## 6. 关键机制设计

### 6.1 并发、取消与背压

| 机制 | 设计 | 参数 |
|---|---|---|
| 任务模型 | 每个耗时操作封装为 `Task { cancel: CancellationToken, deadline: Instant }`，由 `Supervisor` 登记 | — |
| 超时默认值 | LSP 请求 5s、Git 命令 30s、AI 结构化任务 300s（可配）、构建无默认超时但可中断 | PRD NF-05 |
| 去抖 | 补全 150ms、诊断合并 100ms、文件监听事件合并 50–100ms、resize 合并 100ms | NF-05 / EX-37 |
| 背压 | 搜索结果每 50ms 批量交付，首屏上限 1000 条并标「还有更多」；插件输出超阈值标注「已截断」 | EX-28 |
| 取消 | 所有 `async fn` 首参接收 `CancellationToken`；取消后**不得**产生部分写入（PRD 9.4 约束） | I-2 |
| 帧预算 | 每帧事件处理预算 4ms；超预算的事件留到下一帧，UI 永不因后台事件饿死 | §8.2 |

**禁止**：在 UI 线程 `block_on`；在 rayon 池内 `await`（会阻塞 CPU 池线程）；跨线程持有 `&mut` 领域对象。

### 6.2 渲染管线

```
命令/事件
   ↓
面板树（L3 mtide-widget：约束求解 → 每个面板拿到矩形区域）
   ↓
Widget 树（领域产出的语义节点，样式是 token 不是 ANSI）       ← 插件也只到这一层
   ↓
布局（换行/宽字符/软换行/裁剪，产出 (row, col, cell) 序列）
   ↓
CellBuffer 写入（双缓冲：back 写、front 读）
   ↓
差量计算（逐行扫描，合并连续变化段为一次移动+写入）
   ↓
终端 flush（单帧一次 write，必要时限流：低带宽模式仅输出变化行）
```

| 环节 | 预算（P95） | 说明 |
|---|---|---|
| 布局 | ≤4ms | 约束求解结果缓存，仅在尺寸/可见性变化时重算 |
| Widget → Cell | ≤3ms | 命中「未脏面板直接复用上一帧 cell」短路 |
| 差量 | ≤2ms | 行级 dirty bitmap |
| flush | ≤3ms | 批处理写；低带宽模式合并 100ms 窗口 |
| **合计** | **≤12ms**，留出 18ms 给命令执行与 IO 调度 | 对应 NF-02 ≤30ms |

**双缓冲与差量的必要性**：SSH 下每帧全屏重绘 200×60 = 12000 cell ≈ 数百 KB，30fps 即 MB/s 级；差量后典型输入帧 <200 字节。

### 6.3 坐标与编码（A-08）

| 坐标 | 用途 | 类型（命名即类型，禁止裸 `usize`） |
|---|---|---|
| `ByteOffset` | rope 内部、编辑、搜索 | UTF-8 字节 |
| `LineIdx` / `Col` | 显示、光标、渲染 | 行号 + 显示列（宽字符 = 2 列） |
| `LspPos` | LSP/DAP 协议 | **UTF-16 code unit**（0-based） |
| `CharIdx` | 树/解析器 | Unicode 标量 |

规则：
1. 转换只在两处发生：`mtide-core::coords`（byte ↔ 行/列/字符，含宽字符与组合字符表）与 `mtide-lsp::conv`（内部 ↔ UTF-16）。
2. 领域函数签名必须写具体类型，禁止 `usize` 三义混用；CI 用 clippy 自定义 lint 拦截 `fn f(line: usize, col: usize)` 这类签名。
3. 宽字符表随 Unicode 版本升级需回归；Emoji ZWJ 序列按 grapheme cluster 处理（NF-10）。

### 6.4 文本、高亮与解析

| 主题 | 设计 |
|---|---|
| 文本结构 | `ropey::Rope`；大文件（>20MB）进入「窗口模式」：mmap + 行索引 + 可视区 rope 片段（ED-12、NF-03） |
| 高亮 | `syntect`（TextMate）为主；按「可视区 + 前后 500 行」增量计算，结果进 LRU span 缓存；编辑后仅失效受影响行到语法块边界 |
| 结构化 | `tree-sitter` 仅对 Top10 语言内置；用于折叠（LN-17）、智能选区（NV-09）、结构化搜索（NV-12）、缩进 |
| 语义高亮 / Inlay hints | 由 LSP 提供，作为**叠加层**渲染，与 TextMate 结果按优先级合并（语义优先） |
| 后台化 | 高亮与解析在 rayon 池；结果未就绪时用上一帧结果 + 「计算中」标记，**绝不阻塞输入** |

### 6.5 语言服务与诊断聚合

```
打开文件 → 语言检测(mtide-syntax) → 查配方(mtide-lsp::recipe)
   → 探测二进制（PATH/版本）→ 已装：spawn(JSON-RPC over stdio, mtide-proc)
                             → 未装：状态栏提示 + 安装命令（需确认，禁止静默）
   → initialize/initialized/能力协商 → didOpen → Ready
诊断汇聚：LSP(publishDiagnostics) + linter CLI(mtide-task) + 语法错误
   → DiagnosticStore（按文件聚合、100ms 合并、来源可筛）→ UI 与滚动条标记
```

| 要点 | 设计 |
|---|---|
| 状态机 | 严格按 PRD 9.2：Discovered → Spawning → Initializing → Ready ⇄ Degraded → Crashed → Restarting(≤3) → Disabled |
| 同步顺序 | 本地 edit 顺序必须与 `didChange` 发送顺序严格一致；**v1.0 先用 Full 同步跑通，再开增量**（AGENT.md 陷阱 2） |
| 多诊断源（LN-19） | 统一 `Diagnostic { source, range, severity, message, code }`；来源可筛选、可按来源禁用；同位置合并展示 |
| 请求治理 | 补全 150ms 去抖 + 取消前序请求；请求超时 5s；连续超时进 Degraded 并降频 |
| 多根 | 每个工作区根独立服务器实例或按服务器能力传入 workspaceFolders；GT-16 要求状态栏分支跟随「当前文件所属根」 |

### 6.6 调试（DAP）

```
选运行配置(.mtide/launch.toml) → 探测适配器(delve/debugpy/lldb-dap/node)
   → spawn → initialize → 【等 initialized 事件】→ setBreakpoints → configurationDone
   → launch/attach → 事件驱动：stopped/continued/terminated/output
   → Stopped: 取 stackTrace → scopes → variables（懒加载、分页）
```

**与 LSP 的三个差异（不可复用同一会话状态机）**：① 必须先等 `initialized` 事件再发断点；② 断点是「先注册后启动」；③ 线程/帧/变量是有状态句柄（`variablesReference`），需缓存与失效管理。

| 要点 | 设计 |
|---|---|
| 断点模型 | 文件 + 行 + 条件 + 命中次数 + 日志点；文件未加载时保留断点，加载后补注册 |
| 变量 | 按 `variablesReference` 懒加载，展开时才请求；帧切换即失效 |
| 求值 | REPL/监视表达式走 `evaluate`，结果不可信时降级为文本；禁止在 UI 线程等待 |
| 异常 | 适配器缺失/版本不符不启动空会话（EX-15）；端口占用显示 PID 不自动杀（EX-16）；断连保留变量快照（EX-17） |

### 6.7 AI 编排（双模）

```
                 ┌──────────── A 模式：结构化无头任务（可审计主路径）────────────┐
用户发起任务 ───→ │ 上下文收集（选区/当前文件/诊断/相关文件）                     │
                 │   ↓ 三层过滤：.aiignore → 内置敏感清单 → 企业策略             │
                 │ 展示「将发送 N 个文件」→ 用户确认 → spawn CLI（argv 数组）    │
                 │ 流式输出 → AgentAdapter::parse_event → AgentEvent 流           │
                 │ 解析出补丁 → DiffGate::stage（校验基线 hash）→ 逐 hunk 确认    │
                 │ 采纳 → 落盘 + 生成回滚快照；拒绝 → 丢弃（文件 hash 不变）      │
                 │ 全过程写只追加审计 JSONL（不含代码内容）                       │
                 └───────────────────────────────────────────────────────────────┘
                 ┌──────────── B 模式：交互会话（保真，靠 C 补审计）────────────┐
选择 provider ──→│ 探测 → 缺失则引导（展示命令、需确认）→ PTY spawn              │
                 │ mtide-vt 还原界面 → 嵌入面板；按键透传（force keys 除外）      │
                 │ 隐藏面板不终止进程；resize 透传 SIGWINCH；崩溃退避重启 ≤3      │
                 │ 【C：会话期间变更追踪】文件监听 → 排除编辑器内事务 → 变更清单  │
                 │ 会话结束 → 生成待审阅清单（可 diff、可回滚）                   │
                 └───────────────────────────────────────────────────────────────┘
```

| 要点 | 设计 |
|---|---|
| 适配器契约 | `AgentAdapter { capabilities, probe, build_invocation, parse_event, classify_exit }`；三内置 + 自定义（配置驱动）；**新增适配器只加文件 + 注册，不改核心** |
| 契约回归 | 每个适配器一组真实输出样例 `tests/fixtures/<adapter>/*.jsonl`；解析失败返回 `None` 而非 panic |
| DiffGate | `stage` 校验每个文件基线 hash，任一不符整体拒绝；`apply` 只应用被采纳 hunk，失败整体回滚；`rollback` 用 apply 生成的快照 |
| 不可绕过性 | `mtide-ai` 之外不存在给 AI 的写盘路径；`mtide-workspace::write` 断言调用方持有 DiffGate 一次性 token |
| 外发管控 | 三层过滤 + 发送前预览 + 企业策略可禁用指定 CLI；审计只追加，策略开启时不可被用户配置关闭 |
| 交互模式限制 | 进入前必须明示「IDE 无法约束 CLI 读取范围」（SEC-05）；只提供尽力而为的变更追踪与回滚，来源分为 AI/用户/未知，不宣称完整审核 |

### 6.8 插件进程模型

```
mtide-plugin（宿主侧）
  ├─ 安装：本地目录 / URL / Git → 展示来源与权限 → 确认才落盘（SEC-11）
  ├─ 清单：plugin.toml（贡献项 + 权限声明）
  ├─ 启动：mtide-proc spawn（stdio JSON-RPC），权限集合注入 initialize
  ├─ 运行：command/execute（同步，超时）、panel/render（异步，超时用上一帧）
  │        event/notify（节流）、log/write（进诊断包）
  ├─ 权限：默认仅工作区读；命令/网络/剪贴板/写文件需安装时逐项授权，可撤销（仅约束 broker）
  ├─ 配额：内存 ≤256MB（可配）、CPU 时间片、输出速率；越权 3 次自动禁用（EX-27）
  └─ 崩溃：Supervisor 退避重启 ≤3 → Disabled；在故障注入矩阵中宿主不受影响（NF-12）
```

> 安全边界：普通插件进程仍继承操作系统用户权限，v1.0 不提供 OS 级沙箱。宿主可以拒绝 RPC broker 的越权调用，但无法可靠阻止插件自行使用其语言运行时访问文件、网络或启动子进程；企业环境需通过容器、沙箱或操作系统策略补充隔离。

**扩展点六类**：命令、键位、主题、语法包、面板（Widget 树）、语言配方。
**生态补偿**（RK-07）：`mtide plugin new` 脚手架 + 5 个可复制示例 + 只读社区注册表 + 热重载（PL-11/12）。

### 6.9 存储、崩溃恢复与版本兼容

| 数据 | 位置 | 写入方式 | 治理 |
|---|---|---|---|
| 配置（四层） | `~/.config/mtide/*`、`<ws>/.mtide/*`、企业策略路径 | 同目录临时文件 + flush/sync + rename；跨设备或 Windows rename 失败时保留临时文件并报错 | 策略层只读，UI 展示锁定项与来源 |
| 会话 | `~/.local/share/mtide/sessions/<id>.json` | 原子写，间隔 ≤2s（NF-11） | 损坏则跳过该项，其余正常恢复（EX-33） |
| 崩溃快照 | `snapshots/<hash>.snap` | 原子写 | 启动提示恢复，可导出诊断包 |
| 本地历史 | `history/<ws-hash>/<file-hash>/` | 追加 + LRU 清理 | 超限清理最旧且不删正在查看的（EX-34） |
| AI 审计 | `ai/audit/YYYY-MM.jsonl` | 只追加（O_APPEND） | 企业策略下不可关闭（SEC-07） |
| 日志 | `logs/mtide-YYYY-MM-DD.log` | `tracing` 文件 sink | 分级、轮转；诊断包脱敏（不含源码/路径原文） |

**崩溃恢复链**：`panic hook`（恢复终端 + 尝试写快照）→ 下次启动检测 → 校验快照校验和 → 提示恢复。快照写入采用同目录临时文件、flush/sync、rename；若在 sync/rename 前进程被杀，启动时跳过不完整文件并保留诊断信息。所有写盘经 `mtide-workspace` 原子写入口（I-6）。

### 6.10 安全与权限（架构视角）

| 边界 | 机制 |
|---|---|
| 宿主 ↔ 插件 | 进程隔离 + 权限清单 + 配额 + 审计；敏感能力必须走宿主 RPC broker（R-5）；不把普通进程隔离表述为 OS 沙箱 |
| 宿主 ↔ AI | 结构化模式：三层过滤 + 预览 + 确认 + DiffGate + 审计；交互模式：明示 + 变更追踪 + 回滚 |
| 宿主 ↔ 文件系统 | 仅 `mtide-workspace::write` 一个写入口，带调用方身份（AI 通道需 DiffGate token） |
| 供应链 | 不内置任何 LSP/DAP/AI 二进制（SEC-10）；安装命令全文展示 + 确认（SEC-12）；依赖许可白名单 + `cargo deny` |
| 数据外发 | 遥测默认关闭；审计与日志永不包含源码、提示词、绝对路径原文 |

---

## 7. 关键技术选型

> 每张表按 **方案 / 优点 / 代价 / 适用条件** 四列给出，最后写明选定与理由。代价一列不得为空。

### 7.1 实现语言与运行时

| 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|
| **A. Rust（选定）** | 单二进制、无运行时依赖、启动快、无 GC 停顿、内存可控、FFI 到 tree-sitter 自然 | 学习曲线与编译时间；异步生态心智负担 | 需要 ≤150ms 启动、≤180MB 内存、单文件分发 |
| B. Go | 开发快、单二进制、协程简单 | GC 停顿（大文件/高亮场景帧抖动）、rope 与语法库生态弱、体积与内存偏大 | 对帧稳定性不敏感的工具 |
| C. Zig / C++ | 极致性能与体积 | 生态与人力成本、内存安全靠人 | 单人长期投入、追求极限 |

**选定 A**（PRD D-04 已锁）。edition 2021，MSRV 1.80。

### 7.2 异步运行时与并发模型

| 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|
| **A. tokio（multi-thread，worker=min(4,ncpu)）+ rayon CPU 池 + 单 UI 线程（选定）** | IO 与 CPU 分离，UI 绝不被 IO 或高亮卡住；CPU 池可用 `spawn` 取消语义外的并行 | 三类执行体的边界需要纪律；领域对象要支持快照语义 | 有明确帧预算与响应指标 |
| B. 全 tokio（含 `spawn_blocking`） | 单一心智模型 | 阻塞任务挤占 worker 会拖慢 IO；优先级不可控 | 无严格帧预算 |
| C. 全同步 + 手写事件循环 | 简单、无异步传染 | 子进程 IO、PTY、多 LSP 并发手写状态机成本极高 | 依赖极少的工具 |

**选定 A**。补充参数：`tokio` worker `min(4, ncpu)`（终端工具无需高 IO 并发）；rayon 池 `max(2, ncpu-2)`，与 tokio worker 错开，避免 CPU 争抢导致帧抖动。

### 7.3 终端渲染

| 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|
| **A. 自研 Cell Buffer + 差量输出，后端抽象用 `crossterm`（选定）** | 精确控制重绘（低带宽/SSH）、Widget 树可托管插件、双主题可参数化、可复用给 VT 合成 | 布局引擎与组件库自研（约 2 人月） | 需要插件 Widget 树与低带宽优化 |
| B. 采用 ratatui | 组件与布局现成，上手快 | 组件树与插件 Widget 语义冲突（宿主组件 vs 插件声明树两套模型）；差量粒度不可控 | 无插件需求、无低带宽约束 |
| C. termwiz 全套（含 widgets） | 功能全（含图像协议） | 抽象偏重，且与自研 Widget 树重复 | 不需要自定义 Widget 契约 |

**选定 A**。`crossterm` 仅作后端（raw mode、事件读取、能力查询、Windows ConPTY）；`mtide-tui` 不暴露后端类型给上层。

### 7.4 VT 缓冲（终端面板与 AI 交互会话共用）

| 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|
| **A. 自研 `mtide-vt`（选定）** | 可脱离终端网格级单测；能嵌入面板（尺寸任意、可被布局裁剪）；与渲染管线统一 | 需实现 ANSI 子集 + 备用屏 + 鼠标协议 + resize 重排；约 3–4 人周 | 必须把全屏 TUI 嵌进面板 |
| B. 复用现有 Rust VT 库 | 省实现 | 现有库面向「全屏终端模拟器」，不支持嵌入任意矩形与差量合成 | 只做全屏接管（PRD v1.1 AI-14） |
| C. 不内嵌，AI 会话全屏接管 | 零 VT 成本 | 对标（TTT）已有内嵌面板，缺失即落后；且不能与编辑器同屏 | 降级方案（RK-08 触发时启用） |

**选定 A**，配套 `xtask vt-test`：用例为「输入字节序列 → 期望屏幕网格」，覆盖光标移动、滚动区域、备用屏、SGR、宽字符、resize。

### 7.5 PTY

| 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|
| **A. `portable-pty`（选定）** | 跨平台统一（Unix forkpty / Windows ConPTY）、维护活跃 | 需自行封装 resize/SIGWINCH 与进程组清理 | 三平台一级支持（NF-09） |
| B. 直接用 `nix` + `windows-sys` 各写一套 | 可控性最强 | 双份代码与双份缺陷；ConPTY 细节易错 | 有专职平台工程师 |

**选定 A**。Windows 差异用条件编译隔离在 `mtide-proc::pty`，并设专项兼容测试（RK-13）。

### 7.6 文本结构与高亮

| 项 | 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|---|
| 文本 | **`ropey`（选定）** vs 自研 gap buffer vs piece table | O(log n) 编辑、成熟、可分片 | 内存约为原文 1.2–1.5 倍 | 需要 100 万行文件可编辑 |
| 高亮 | **`syntect`（TextMate）（选定）** vs tree-sitter 高亮 vs 自研正则 | 与 VS Code 同源、100+ 语法是纯数据、插件可分发语法 | 正则回溯与增量边界复杂 | 覆盖率优先 |
| 结构化 | **`tree-sitter` Top10（选定）** vs 全量编译 | 折叠/选区/结构化搜索准确 | 每语言一个解析器二进制体积，约 0.3–1MB/语言 | 只在 Top10 用 |
| 宽字符 | **`unicode-width` + 自建 grapheme 表** | 对齐正确（NF-10） | 需随 Unicode 版本回归 | 有 CJK 用户 |

### 7.7 LSP / DAP 传输

| 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|
| **A. 自研轻量 JSON-RPC（`serde_json` + 头/行解析），LSP 与 DAP 共用（选定）** | 传输层一份代码；可注入取消与超时；便于 mock 与录制回放 | 需处理分块、半包、乱序 id、通知/请求区分 | 只需协议子集 |
| B. 用 `tower-lsp` / 现成 DAP 库 | 省实现 | 两个库模型不一致、取消与超时难统一、版本绑定 | 只做 LSP |

**选定 A**。**注意**：LSP 与 DAP 的**会话状态机不可共用**（初始化顺序与事件语义不同），只有传输层共用。

### 7.8 Git / 搜索 / diff

| 项 | 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|---|
| Git | **`git` 子进程 `--porcelain=v2 -z`（选定）** vs `git2`/libgit2 | hooks、LFS、credential helper、worktree、submodule、签名提交全部免费正确 | 进程开销（用异步 + 缓存抵消）；需解析 porcelain | 用户环境普遍有 git |
| 搜索 | **内置 `ignore` + `grep-searcher` + `grep-regex`（选定）** vs 外挂 `rg` | 零外部依赖（P-1），与 rg 同源结果一致 | 需自己做流控与取消 | 单二进制承诺 |
| diff | **自研 `mtide-diff`（Myers + 逐 hunk 采纳）（选定）** vs `similar` 库 + 自研采纳层 vs `diffy` | 三处共用同一 hunk 语义（Git/比较/AI），逐块采纳行为一致 | 约 1.5 人周；三方合并需另写 | 三处共用 diff |

### 7.9 AI 接入与插件宿主

| 项 | 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|---|
| AI | **双模：无头结构化（主）+ PTY 交互会话（辅）+ 变更追踪（补）** | 同时拿到「保真」与「可控」；对 TTT 形成反超 | VT 自研成本；两套代码路径 | 需要可审计 + 原生体验 |
| 插件宿主 | **进程 RPC（stdio JSON-RPC）（选定）** vs Lua VM vs 内嵌 JS | 任意语言、崩溃隔离最强、可直接封装现有 CLI | 上手成本高（需脚手架与示例补偿）；IPC 开销 | 安全与语言自由度优先 |
| 插件 WASM | **v1.0 不引入，feature gate 预留（建议）** vs `wasmtime` vs `wasmi` | 纯数据类插件（主题/语法/片段）可用 WASM 获得更强沙箱 | `wasmtime` 体积增量与 NF-07（≤45MB）冲突；`wasmi` 性能较低 | 若体积预算上调到 70MB+ 可引入 |

> WASM 取舍需人类确认（见 §12.2 Q-3）。默认按「不引入」实现，`mtide-plugin` 预留 `wasm` feature 位。

### 7.10 存储、配置与序列化

| 项 | 方案 | 优点 | 代价 | 适用条件 |
|---|---|---|---|---|
| 配置格式 | **TOML（选定）** vs JSON vs YAML | 注释友好（用户手改）、生态成熟 | 嵌套表达略啰嗦 | 人手编辑为主 |
| 主题/语法/插件清单 | **JSON（选定）** | 与 VS Code 主题/语法生态数据兼容，可直接复用纯数据 | 无注释（主题不需手改） | 需复用现成数据包 |
| 审计/事件 | **JSONL 只追加（选定）** | 崩溃安全、易追加易归档、可用外部工具分析 | 需自行轮转与清理 | 审计合规 |
| 会话/快照 | **JSON + 原子 rename（选定）** vs SQLite vs bincode | 无依赖、可读可排查、崩溃安全 | 大会话序列化开销（<1MB，可接受） | 单机、数据量小 |
| 本地历史 | **内容寻址文件 + LRU（选定）** vs SQLite | 零依赖、可 diff、易清理 | 需自己管容量 | ≤万级快照 |

> SQLite 在 v1.0 不引入：会增加体积与依赖许可审查面，且当前数据量（会话 <1MB、历史 ≤万条）用文件足够。若未来需要「跨工作区全文检索历史」再评估。

### 7.11 工程与质量基建

| 项 | 选择 | 说明 |
|---|---|---|
| 错误 | `thiserror`（库）/ `anyhow`（bin、xtask） | 库代码禁 anyhow |
| 日志 | `tracing` + 文件 sink | UI 内可见 error/warn |
| 基准 | `criterion` | 阈值写死在 `xtask/src/perf.rs`，超阈值 CI 失败 |
| 快照测试 | 固定尺寸（80×24 / 120×40 / 200×60）渲染快照 | `--update` 需人工审阅 |
| 许可与安全 | `cargo deny` | MIT/Apache-2.0/BSD/ISC/MPL-2.0 白名单 |
| 打包 | `cargo xtask package` + 三平台交叉编译 | Linux/macOS/Windows 一级 |
| 依赖方向校验 | `xtask depcheck`（解析 `cargo metadata`） | 违反 §5.3 规则即失败 |

---

## 8. 性能与容量预算

> 以下为**量级估算与推导**，非实测结论；M0 起用 `xtask bench` 逐步替换为实测值。

### 8.1 冷启动预算（目标 P95 ≤150ms，NF-01）

| 阶段 | 预算 | 说明 |
|---|---|---|
| 进程启动 + 二进制 mmap | ~20ms | 静态链接 + 按需分页 |
| CLI 解析 + 四层配置加载 | ~10ms | 配置小、解析一次 |
| 终端能力探测 | ~15ms | **必须带超时（默认 100ms，可用 `MTIDE_TERM` 覆盖）**，SSH 下无应答不得卡住 |
| 主题 + 当前语言语法包加载 | ~25ms | 懒加载：只装当前打开文件的语言 |
| 工作区根打开 + 首文件读取 | ~20ms | 只列当前目录，不全量索引 |
| 首帧渲染 | ~10ms | 布局 + 差量 |
| **小计** | **~100ms** | **余量 50ms** |
| 后台（不计入首屏） | — | 文件索引、LSP 启动、git 状态、插件加载、AI 探测（A-11） |

**架构含义**：任何「首屏等待 LSP/git/插件/索引」的写法都会直接破坏 NF-01，必须走占位态 + 后台就绪通知。

### 8.2 按键响应预算（目标 P95 ≤30ms，NF-02）

| 阶段 | 预算 |
|---|---|
| 输入读取 + 键位解析 | ≤1ms |
| 命令执行 + rope 编辑（100 万行） | ≤3ms |
| 高亮失效判定（只标脏，不重算全量） | ≤2ms |
| 布局 + Widget → Cell | ≤7ms |
| 差量 + flush | ≤5ms |
| **小计** | **≤18ms（余量 12ms）** |

后台重算（全量高亮、诊断、搜索）不允许进入此链路；结果就绪后以事件形式触发下一帧。

### 8.3 VT 帧预算（目标 ≤8ms，NF-06）

| 阶段 | 预算 |
|---|---|
| PTY 读取（批量 ≤64KB） | ≤2ms |
| ANSI 解析 + 网格状态更新 | ≤3ms |
| 合成进 CellBuffer（裁剪/滚动） | ≤2ms |
| 输入透传（按键 → PTY 写） | ≤1ms（端到端 ≤10ms 含终端往返） |

### 8.4 内存预算（目标常驻 ≤180MB，NF-04）

场景：10 万行文件 + 2 个 LSP + 1 个终端 + 1 个 AI 会话 + 4 个插件。

| 项目 | 估算 | 推导依据 | 可调参数 |
|---|---|---|---|
| 二进制映射（代码段 + 只读数据） | ~25MB RSS | 45MB 二进制，常驻代码页通常占 40–60% | 语法包外置可降到 ~15MB |
| 文本 rope（10 万行 ≈ 4MB 源码） | ~6MB | ropey 约为原文 1.2–1.5 倍 | 大文件模式只对窗口建 rope |
| 行索引 + 显示列缓存 | ~2MB | 每行 ≈ 20B | — |
| 高亮 span 缓存（可视区 + 前后 500 行） | ~4MB | 1000 行 × ~4KB | 缓存行数可配（默认 500） |
| tree-sitter 树（单语言） | ~5MB | 大文件单棵树典型 3–8MB | 仅 Top10 语言加载 |
| CellBuffer（200×60 双缓冲） | ~0.5MB | 12k cell × ~16B × 2 | — |
| **VT 缓冲（2 终端 + 1 AI）** | **~10MB** | scrollback **默认 1000 行** × 200 列 × 16B ≈ 3.2MB/会话 | **scrollback 行数是最有效的削减旋钮** |
| LSP 客户端（2 个） | ~10MB | 传输缓冲 + 文档镜像 + 诊断存储 | 文档镜像可在超阈值时只留元信息 |
| Git 状态缓存（2 万文件） | ~8MB | 每文件约 300–400B | 大仓库改为分片懒加载 |
| 插件（4 个 × RPC 缓冲） | ~16MB | 每插件 ≤4MB 用户态缓冲（配额上限 256MB 是进程侧） | 插件数上限可配 |
| tokio/rayon 线程栈与运行时 | ~12MB | 约 10 线程 × 1MB 栈（含 rayon） | worker 数已限 4 |
| 日志/审计/会话缓冲 | ~5MB | 环形缓冲 | — |
| 其它（UI 状态、命令注册表、i18n） | ~5MB | — | — |
| **合计** | **≈108MB** | 余量约 40% | — |

**若实测超标的削减顺序**（按性价比）：`scrollback 1000→500`（省 ~5MB）→ 插件数上限 4→2（省 8MB）→ Git 缓存分片懒加载（省 5MB）→ 高亮缓存 500→200 行（省 2MB）→ 语法包外置（省 10MB）。**不要**先砍 rope 与 VT 正确性。

### 8.5 二进制体积预算（≤45MB，NF-07）

| 组成 | 估算 |
|---|---|
| Rust 代码（release + strip + LTO） | ~12MB |
| TextMate 语法包（内置 20 语言） | ~8MB（可压缩到 ~4MB） |
| tree-sitter Top10 解析器 | ~6MB |
| 主题与图标数据 | ~1MB |
| 依赖与运行时 | ~5MB |
| **合计** | **~32MB**（余量 13MB，供语法包扩展） |

> 若引入 `wasmtime`，此项至少 +20MB，将击穿 45MB 预算——这是 §7.9 建议 v1.0 不引入 WASM 的量化依据。

---

## 9. 依赖就绪度与架构风险对策

### 9.1 依赖就绪度（架构视角）

| 依赖 | 就绪 | 分级 | 架构上的兜底 |
|---|---|---|---|
| Rust 生态（crossterm/ropey/syntect/tree-sitter/ignore/grep-searcher/portable-pty） | 已就绪 | 并行 | 全部经本仓 trait 包装，可替换；渲染与 VT 自研不受影响 |
| **VT 缓冲（自研）** | 未开始 | **阻塞**（TR-01、AI-17） | 延期则 AI 交互会话降级为全屏接管（PRD v1.1 AI-14），终端面板降级为「外部终端切换」 |
| LSP 服务器二进制 | 用户环境不定 | 阻塞（开箱承诺） | 配方 + 探测 + 确认安装 + 离线包；对外口径「已装即用」 |
| DAP 适配器 | 用户环境不定 | 并行 | 缺失则该语言调试不可用并明示；先做 delve/debugpy/lldb-dap |
| AI CLI | **无稳定契约** | 阻塞（AI 承诺） | 适配器版本化 + 样例契约回归 + 单 CLI 隔离失效；最坏降级全屏接管 |
| git 二进制 | 普遍预装 | 可绕过 | 缺失则 Git 面板禁用，其余功能不受影响 |
| TextMate 语法包（20 种外） | 已就绪 | 可绕过 | 按需下载或降级纯文本 |

### 9.2 架构层风险与对策

| 编号 | 风险（对应 PRD） | 架构对策 | 监控信号 |
|---|---|---|---|
| AR-01 | **VT/输入序列差异表现为「快捷键失效或屏幕花了」而非报错**（RK-08） | `mtide-vt` 零 UI 依赖 + `xtask vt-test` 网格用例集 + GUI 终端兼容矩阵自动化输入/截图对比；记录 A/B/C 兼容等级与终端保留键 | VT 用例通过率、各终端热键失败数、终端保留键诊断覆盖率 |
| AR-02 | **AI CLI 契约不稳定导致适配层返工**（RK-01） | 适配器只做「注册 + 文件」，核心零改动；每适配器真实样例契约回归；`capabilities()` 声明式降级 | 解析失败率（按 CLI 分列）、样例陈旧告警 |
| AR-03 | **插件提权与供应链**（RK-04） | 进程隔离 + 权限清单 + 配额 + 审计 + 无自动更新；`mtide-plugin` 不直连 git/ai（R-5） | 越权拦截次数、插件崩溃次数 |
| AR-04 | **多引擎叠加击穿性能目标**（RK-10） | 帧预算（§8.2）+ 每帧事件预算 4ms + 后台化 + CI 基准门禁 | 启动/按键/帧 P95、常驻内存 |
| AR-05 | **帧抖动来自 GC 式争抢**（本项目特有） | tokio worker 限 4、rayon 池与 UI 线程错开、CPU 密集全部进 rayon | 帧时间方差、rayon 队列等待 |
| AR-06 | **模块边界腐化**（2–4 人长期维护） | `xtask depcheck` 机械校验 7 条规则（§5.3），CI 阻断 | depcheck 失败数、crate 间边数 |
| AR-07 | **首屏被后台任务拖慢**（A-11 反模式） | 首帧后才 `spawn_background`；能力探测带超时 | 冷启动 P95、首帧时间 |
| AR-08 | **Windows ConPTY 差异**（RK-13） | 平台差异隔离在 `mtide-proc::pty` + 条件编译 + 专项矩阵测试 | Windows 终端用例失败数 |
| AR-09 | **交互会话不可控引发合规争议**（RK-03） | 进入前明示 + 尽力而为变更追踪 + 未知来源标记 + 回滚 + 审计；口径写死在 PRD 15.3 | 未知来源变更数、监听丢事件数、待审阅清单覆盖率 |
| AR-10 | **二进制体积增长击穿 45MB** | 语法包可外置、WASM 默认不引入、`cargo deny` + 体积门禁 | 产物体积趋势 |

---

## 10. 演进路线

> 阶段划分与 AGENT.md 的 M0–M10 一一对应；每阶段给出**交付物 / 验收 / 回滚方式**。

| 阶段 | 里程碑 | 目标 | 关键交付 | 验收（可自动执行） | 回滚方式 |
|---|---|---|---|---|---|
| **P0 骨架** | M0 | 能启动、能画空界面、干净退出 | workspace、`mtide-api`、`mtide-proc`、`mtide-tui`（cell buffer/差量/能力探测）、panic hook、`xtask check/depcheck` | 三尺寸渲染快照通过；强制 panic 后终端不乱码；冷启动 ≤60ms | 无用户数据，直接 revert |
| **P1 内核** | M1–M3 | 「可日常编辑」 | `mtide-core`（rope/多光标/撤销/坐标）、`mtide-widget`、`mtide-workspace`（配置/监听/原子写）、`mtide-syntax`（TextMate）、`mtide-search`、文件查找 | 100 万行打开 ≤3s、插入 <10ms；ED 域 Must/Should 全有测试；启动 ≤150ms | 功能未外发，revert 分支 |
| **P2 工作台** | M2、M5、M6 | 「编辑 + 构建 + Git + 终端」闭环 | 命令注册表/键位/面板编排、`mtide-term`+`mtide-vt`、`mtide-task`（问题匹配器/F9）、`mtide-git`、`mtide-diff` | 核心场景 S-1 自动化跑通；终端在 tmux/ConPTY 下无错位；Git 在 submodule/worktree 仓库通过 | 面板可 `--safe-mode` 关闭；Git 面板可禁用 |
| **P3 智能与调试** | M4、M7、M8 | 「IDE 能力成型 + 差异化成立」 | `mtide-lsp`（配方/诊断聚合/层级/大纲）、`mtide-dap`、`mtide-ai`（双模 + DiffGate + 审计）、`mtide-nav`（本地历史/书签/时间线） | LSP 首诊断 ≤2.5s；调试成功率 ≥95%；AI 结构化成功率 ≥95%（按 CLI 分列）；未确认时文件 hash 不变（自动化断言） | `--no-lsp` / `--no-ai` 开关；适配器可逐个禁用 |
| **P4 生态与发布** | M9、M10 | 「可推广」 | `mtide-plugin`（进程 RPC/权限/配额/注册表/热重载）、Modern 主题、会话恢复、更新提示、三平台打包、性能门禁全绿 | 在故障注入矩阵中插件崩溃不影响宿主；崩溃恢复 diff 为空；NF-01~NF-16 全绿 | 插件可全禁用（`--safe-mode`）；主题可切回 Retro |

**阶段内原则**：每阶段出口必须 `cargo xtask check` 全绿 + 该阶段 Must 需求有自动化测试；任何阶段不得把「未完成的后台能力」放进首屏路径。

**与 PRD 版本的对应**：P0–P1 ≈ alpha 基础；P2 ≈ alpha；P3 ≈ beta；P4 ≈ v1.0。v1.1（AI 全屏接管增强、多调试会话、结构化搜索）与 v1.2（插件市场、结构化 Agent 协议）在 P4 之后。

---

## 11. 面向 Codex 的开工顺序与任务卡

> 任务卡可直接作为 PR 描述派发。格式：`编号 · 标题 · 涉及 crate · 依赖 · 交付 · 验收命令 · PRD 编号`。

### 11.1 第 0 批（奠基，必须按序）

| 编号 | 标题 | crate | 依赖 | 交付与验收 |
|---|---|---|---|---|
| **T-00** | 更新仓库结构与 depcheck 规则（本文件 §5 的 4 个新 crate） | 全部 | — | 建 `mtide-proc`/`mtide-search`/`mtide-diff`/`mtide-nav` 空壳 + `Cargo.toml`；`cargo xtask depcheck` 校验 R-1~R-7 全通过；同步更新 `AGENT.md §3` |
| **T-01** | 终端后端与 cell buffer | `mtide-tui` | T-00 | raw mode、能力探测（带超时）、输入事件泵、双缓冲与差量算法；`xtask snapshot` 三尺寸通过；panic 后终端恢复 |
| **T-02** | 子进程与协议基础设施 | `mtide-proc` | T-00 | RAII 句柄（Drop 必回收）、stdio JSON-RPC（半包/乱序/通知）、PTY 封装、退避重启原语；孤儿进程测试、断管测试 |
| **T-03** | 坐标与文档模型 | `mtide-core` | T-00 | `ByteOffset/LineIdx/Col/LspPos` 类型化；rope 封装、编辑原子性；中文 + Emoji ZWJ 用例 |
| **T-04** | 布局引擎与组件库骨架 | `mtide-widget` | T-01 | 约束式布局（三尺寸）、基础组件（列表/输入/弹窗/表格）、焦点管理；80×24 下不崩且编辑区 ≥40 列 |

### 11.2 第 1 批（内核闭环，可并行）

| 编号 | 标题 | crate | 依赖 | 交付与验收 |
|---|---|---|---|---|
| **T-05** | 编辑命令与键位系统 | `mtide-core` + bin | T-03/T-04 | 命令注册表、`when` 上下文、Retro/Modern/VSCode-VSCodium 预设、Chord 解析、终端保留键诊断；多光标原子撤销；F9 不被占用 |
| **T-06** | 工作区、配置与原子写 | `mtide-workspace` | T-02/T-03 | 四层配置（策略层只读）、文件监听（合并 100ms）、原子写、会话快照 ≤2s |
| **T-07** | 语法高亮与语言检测 | `mtide-syntax` | T-03 | syntect 集成、可视区增量 + LRU、语言检测；冷启动语法加载 ≤25ms |
| **T-08** | 内置搜索与文件查找 | `mtide-search` | T-06 | `ignore`+`grep-searcher`、流控/取消、模糊查找；50MB 文件搜索不阻塞 UI |
| **T-09** | VT 状态机 | `mtide-vt` | T-00 | ANSI 子集、备用屏、鼠标协议、resize 重排；`xtask vt-test` 覆盖 100+ 用例 |
| **T-10** | 终端面板 | `mtide-term` | T-02/T-09 | 多实例、全屏、force keys、scrollback 默认 1000 行可配；VT 帧 ≤8ms |

### 11.3 第 2 批（IDE 能力，可并行）

| 编号 | 标题 | crate | 依赖 | 交付与验收 |
|---|---|---|---|---|
| **T-11** | 统一 diff 引擎 | `mtide-diff` | T-03 | hunk 计算、逐块采纳、三方合并；Git/比较/AI 三处共用同一语义；失败整体回滚 |
| **T-12** | LSP 客户端与配方库 | `mtide-lsp` | T-02/T-03/T-06 | 状态机（PRD 9.2）、Full 同步优先、UTF-16 转换单点、配方探测 + 确认安装；mock LSP + 真实服务器两套测试 |
| **T-13** | 任务系统与问题匹配器 | `mtide-task` | T-02/T-06 | 输出流、匹配器、F9、可中断；构建失败不得卡死 UI |
| **T-14** | Git 集成 | `mtide-git` | T-02/T-06/T-11 | porcelain=v2 -z 解析、区块暂存、历史分页、冲突；无 git 时面板降级 |

### 11.4 第 3 批（差异化，风险最高，须在前序稳定后启动）

| 编号 | 标题 | crate | 依赖 | 交付与验收 |
|---|---|---|---|---|
| **T-15** | DAP 客户端 | `mtide-dap` | T-02/T-03 | 与 LSP 独立的状态机（initialized 后再注册断点）；delve/debugpy/lldb-dap 三个配方；mock DAP 测试 |
| **T-16** | AI 结构化通道 | `mtide-ai` | T-02/T-11 | `AgentAdapter` + 三内置适配器 + 样例契约回归 + 三层过滤 + DiffGate + 审计；未确认 hash 不变断言 |
| **T-17** | AI 交互会话与变更追踪 | `mtide-ai` + `mtide-term` | T-09/T-16 | PTY 会话（隐藏不终止、resize 透传、退避重启）+ 会话期间变更清单（排除编辑器事务） |
| **T-18** | 导航与本地历史 | `mtide-nav` | T-06/T-11/T-12 | 最近文件/位置栈/书签/面包屑/大纲/时间线/层级；本地历史 LRU 与容量治理 |
| **T-19** | 插件宿主 | `mtide-plugin` | T-02/T-06 | RPC 六方法、权限、配额、生命周期、注册表、热重载、脚手架；插件崩溃不影响宿主 |

### 11.5 派发纪律

1. 一次只派发一个任务卡；PR 标题必须带任务号与 PRD 需求号（如 `T-12 / LN-07 补全去抖`）。
2. 每个任务卡先写失败测试再实现；跨 crate 改动必须同时更新 `depcheck` 白名单（若确需例外）。
3. 任何「本文件未定义的模块/依赖方向/存储位置」都视为需要 ADR，不得自行发明。

---

## 12. ADR 摘要与待人类决策

### 12.1 已成型决策（建议直接登记为 ADR）

| ADR | 主题 | 结论 |
|---|---|---|
| ADR-001 | 渲染底座 | 自研 cell buffer + 差量，不引入 ratatui |
| ADR-002 | 并发模型 | UI 主线程 / tokio IO 池 / rayon CPU 池三分离，只经 channel 与快照 |
| ADR-003 | 外部能力接入 | 一律进程外 + 配方 + 探测 + 显式确认，禁止静默下载安装 |
| ADR-004 | VT 缓冲 | 自研并独立为零依赖 crate，配网格级测试集 |
| ADR-005 | AI 写盘 | 唯一通道 DiffGate，编译期 trait 边界 + 运行时一次性 token |
| ADR-006 | 插件宿主 | 进程 RPC + Widget 树；v1.0 不引入 WASM 运行时 |
| ADR-007 | Git 后端 | git 子进程 porcelain=v2 -z，不引入 libgit2 |
| ADR-008 | 存储 | 无数据库；文件 + 原子写 + LRU + 只追加审计 |
| ADR-009 | diff 引擎 | 统一 `mtide-diff`，Git/比较/AI 三处共用 |
| ADR-010 | 首屏策略 | 首屏不等待任何异步能力，全部占位 + 后台就绪 |

### 12.2 待人类拍板（架构无法自行决定）

| # | 问题 | 建议默认值 | 影响 |
|---|---|---|---|
| Q-1 | 团队实际投入人数（2 还是 4） | 4 | 2 人则 P3/P4 合并，插件与 Minimap 降为 v1.1 |
| Q-2 | scrollback 默认行数 | 1000（可配） | 直接决定 VT 内存占用（§8.4 最大旋钮） |
| Q-3 | v1.0 是否引入 WASM 插件运行时 | **不引入** | 引入则二进制预算需上调至 70MB+ |
| Q-4 | 低带宽模式（合并输出窗口）在哪个阶段做 | P2（随终端一起） | 若放 P4，SSH 场景体验在 beta 前都偏弱 |
| Q-5 | 企业策略文件的分发方式（本地文件 / 环境变量 / 系统级路径） | 系统级只读文件 + 环境变量覆盖 | 影响配置层解析与 UI 锁定项展示 |
| Q-6 | 本地历史保留策略（天数 + 每文件快照数 + 总容量） | 7 天 / 50 份 / 每工作区 512MB | 影响存储治理与 EX-34 行为 |

---

## 13. 附录

### 13.1 目标目录结构

```
mtide/
├── Cargo.toml                  # workspace
├── AGENT.md                    # 编码约束与里程碑
├── mtide-PRD-v2.0.md           # 产品事实源
├── mtide-架构设计-v1.0.md      # 本文件
├── crates/
│   ├── mtide-api/  mtide-proc/                       # L0
│   ├── mtide-tui/  mtide-vt/  mtide-term/            # L1
│   ├── mtide-core/ mtide-workspace/ mtide-syntax/    # L2
│   │   mtide-lsp/  mtide-dap/       mtide-task/
│   │   mtide-git/  mtide-search/    mtide-diff/
│   │   mtide-nav/  mtide-ai/        mtide-plugin/
│   ├── mtide-widget/                                  # L3
│   └── mtide/                                         # bin
├── xtask/                      # check/bench/snapshot/vt-test/package/plugin-new/depcheck
├── docs/
│   ├── decisions/              # ADR（NNNN-slug.md）
│   └── backlog.md
├── tests/                      # 跨 crate 集成、终端兼容快照
└── fixtures/                   # LSP/DAP/AI 契约样例、VT 用例
```

### 13.2 命名与结构约定

| 项 | 约定 |
|---|---|
| crate 名 | `mtide-<模块>`，全部小写连字符 |
| 协议版本 | `mtide-api` 顶层常量 `PROTOCOL_VERSION`，插件与适配器握手时校验 |
| 错误 | 库用 `thiserror`，错误枚举按领域命名（`EditError`/`GateError`/`LspError`），均可展示给用户 |
| 坐标类型 | 禁止裸 `usize`，一律 `ByteOffset`/`LineIdx`/`Col`/`LspPos`（§6.3） |
| 命令 id | `域.动作`，如 `editor.save`、`git.stage.hunk`、`ai.task.dispatch` |
| 事件 | 领域事件为不可变快照，命名 `<Domain>Changed`，如 `DiagnosticsChanged` |
| 测试命名 | `fr_<小写需求号>_<行为>_<预期>`，如 `fr_ln07_completion_debounce_150ms` |
| 配置键 | 与 PRD 10.2 示例一致；新增键必须同步更新示例与 `--doctor` 自检 |

### 13.3 PRD 需求域 → 模块映射

| PRD 域 | 条数 | 主模块 | 协作模块 |
|---|---|---|---|
| ED 编辑内核 | 25 | `mtide-core` | `mtide-syntax`（折叠/片段）、`mtide-widget` |
| LN 语言智能 | 19 | `mtide-lsp` | `mtide-syntax`、`mtide-task`（linter 诊断源） |
| BD 构建运行 | 9 | `mtide-task` | `mtide-diff`（覆盖率着色复用） |
| DB 调试 | 12 | `mtide-dap` | `mtide-core`（断点装饰） |
| GT Git | 18 | `mtide-git` | `mtide-diff`、`mtide-task`（PR 依赖 `gh`） |
| SR 搜索 | 5 | `mtide-search` | `mtide-workspace`（忽略规则） |
| TR 终端 | 7 | `mtide-term` | `mtide-vt`、`mtide-proc` |
| AI 编排 | 21 | `mtide-ai` | `mtide-diff`、`mtide-term`（交互会话）、`mtide-workspace`（变更追踪） |
| PL 插件 | 12 | `mtide-plugin` | `mtide-api`、`mtide-proc` |
| UI 界面 | 20 | `mtide-widget` | `mtide-tui`、bin（面板编排）、`mtide-core`（键位解析） |
| WS 工作区 | 9 | `mtide-workspace` | `mtide-proc`（监听） |
| NV 导航历史 | 13 | `mtide-nav` | `mtide-lsp`（层级/大纲）、`mtide-diff`（比较）、`mtide-workspace`（本地历史存储） |

### 13.4 合规提示

> 本架构仅基于 PRD 与开源通用方案设计。ttt 的 UI-20 仅作为交互规格参考；若复用其 MIT 许可代码或配置片段，必须保留原版权声明与许可证文本，不得复制其品牌、图标、主题素材和非必要文案。涉及第三方语言服务器、调试适配器与 AI CLI 的调用与分发，须逐一核对其许可证与服务条款；依赖引入须过 `cargo deny` 白名单。文中所有性能与容量数字为量级估算，须由 `xtask bench` 实测替换后方可作为对外承诺。

---



