# Changelog

- Completed GT-14 editor diff workflow with HEAD-original capture, current-hunk rollback
  (`Ctrl-Alt-R`), and a read-only original-file viewer from the command palette.
- Removed the remaining production `expect` from task environment-variable expansion so unknown
  variables resolve safely without panicking.
- Added sticky editor context for nested fold regions, toggleable with `Ctrl-Alt-Y` and exposed
  through the command palette.
- Added editor scrollbar click navigation: clicking the scrollbar track or a search/Git marker
  moves the cursor to the corresponding document position.
- Added mouse tab-bar activation with Unicode-safe tab hit testing, keeping keyboard tab switching
  and dirty-tab state preservation intact.
- Added Shift+mouse-wheel horizontal editor scrolling for long lines, preserving normal wheel
  vertical motion.
- Added VSCode Chord handling for `Ctrl+K Ctrl+S` (keyboard help), `Ctrl+K Z` (Zen), with
  1.5-second timeout and Esc cancellation; chords never intercept focused terminal input.

## Unreleased

- PRD2 UI/PL/AI/WS/NV foundation: added independent `mtide-ai`, `mtide-plugin`, and `mtide-nav` contracts with adapter probing/event parsing, sensitive-context filtering, Diff Gate base checks, audit/session metadata, plugin manifest discovery/permission/RPC/widget sanitization, recent-position/bookmark/local-history/outline state, and settings Apply/Cancel staging.
- Added keyboard-accessible UI primitives for semantic accessibility labels, width-safe Markdown preview projection, and terminal-width-gated minimap data.

- 扩展 GT-12 Git 取消链路：blame、fetch/pull/push/sync、merge/rebase、提交详情与 stash 详情均使用可中断进程；新增 `git.cancel` / Ctrl-Shift-G，并在详情面板 Esc 时回收后台 Git 进程。
- TR-01 VT 状态机现在跟踪鼠标报告（1000/1002/1003/1006）与 bracketed paste（2004）；终端聚焦时 TUI 鼠标和粘贴事件按 SGR/括号粘贴协议透传给 PTY。
- GT-14 新增编辑器 Diff 模式（`Ctrl-Alt-Shift-D`）：工作区文件保持可编辑，后台基于当前未保存文本实时重算 HEAD 差异，并在行号槽显示新增行装饰；原有 `Ctrl-Alt-D` 外部变更比较快捷键保持不变。
- 编辑器 Diff 模式补充 Git 滚动条标记与当前 hunk 暂存命令（`Ctrl-Alt-S`），暂存前自动保存未保存编辑。
- 完成 GT-03 逐行取消暂存：Git cached diff、暂存区差异视图与选中行 unstage，保留工作树中的混合修改。
- 补齐 GT-03 区块级取消暂存；diff 面板支持暂存区/工作区切换、可调上下文折叠。
- 补齐 TR-01 VT 的 256 色与真彩 SGR 解析；GT-12 Git 状态/diff 后台任务支持进程级取消与回收。
- 新增 `git.conflict.restore`，可将当前冲突文件恢复到进入解决流程时的原始冲突标记状态，并保留为可撤销编辑。
- UI-04 新增安全的常用 vimrc 映射导入：支持 `nnoremap/map` 与常用 Ctrl/Alt/Shift/F 键及保存、退出、查找、快速打开、撤销/重做动作；未知 Ex 行跳过并报告，导入路径持久化到 workspace。
- UI-02 新增主题预览状态：可在不修改持久化配置的情况下预览主题，Enter 应用、Esc 取消、左右键切换候选主题。
- TR-01 终端 Cell 保留 ANSI 256 色、真彩前景与背景，并在 TUI 输出阶段映射到实际终端颜色；逆色显示同步交换前景/背景。
- GT-06 diff 行增加语法 span 前景色叠加，新增/删除背景色仍保留，统一视图与并排视图均生效。
- BD-04 任务结束交互完善：成功自动收起输出，失败自动展开输出并尝试跳转首个结构化诊断。
- BD-07 任务执行器增加 300 秒默认墙钟超时，超时会终止并回收进程、保留已收集输出，并在任务面板与通知中明确显示。
- ED-18 补齐搜索匹配多光标：Ctrl+D 逐个添加，VSCode Ctrl+K L 一次选择全部，并限制最多 1001 个光标、超限时明确提示。
- ED-16 补齐 Ctrl+左右词移动与段落级上下导航，纵向跳转到短段落时保留期望列位。
- ED-20 为 stdin 未命名缓冲区增加可键盘操作的 Save As 流程，保存前不猜测目标路径。
- ED-01 优化 Rope 编辑撤销快照与边界校验：百万行文件中部编辑从约 438ms 降至约 85µs，并将该场景纳入 `cargo xtask bench` 的 10ms 门禁。

- UI now discovers user theme packages from `~/.config/mtide/themes/*.json`, applies custom
  function-key-bar and border settings, cycles them from the theme command alongside Retro and
  Modern, ignores malformed community files, and classifies notification history as success,
  progress, warning, or error with semantic markers.
- Git history now supports async keyword/author filtering and current-file history filtering,
  while preserving the existing HEAD snapshot and pagination checks.
- Terminal panels now render VT cell ANSI styles through semantic TUI colors in both panel and
  fullscreen modes instead of flattening all output to the default style.
- Independent build/test tasks can now run concurrently: the active task remains interactive,
  background tasks are polled and reaped, their output is retained as separate tabs, and the
  status bar reports the background task count.
- Git commit failures now keep the commit dialog and full hook/error text visible, preserving the
  entered message for correction instead of reducing hook output to a transient status line.
- Git status now provides a keyboard-toggleable tree view (`t`) with directory headers, in
  addition to the flat grouped list.
- Added the `workspace.exportDiagnostics` command, producing an atomic compressed, redacted
  diagnostics bundle without source text or raw active-file paths.
- Wired the Vim, Emacs, VSCode, and Borland keymap presets into the application: presets are
  selectable from the command palette, persisted in workspace layout, and route preset-specific
  core shortcuts.
- Added asynchronous workspace file fuzzy search on Ctrl-P / `navigation.quickOpen`, with
  cancellation, `.gitignore`-aware roots, keyboard selection, and direct tab opening.
- Task output now maps common ANSI SGR colours to semantic TUI styles instead of displaying
  escape sequences; the task panel also exposes a default test workflow (Ctrl+Shift+T) with
  Cargo, Go, npm, Python/pytest, and Make recipe detection.
- Completed task output is retained in a bounded history; the output overlay can switch
  between the current task and prior task tabs with Tab/Shift-Tab.
- Git changes now render explicit staged/unstaged/untracked/conflict group headers while
  retaining file-level keyboard selection and actions.
- Git diff now supports line-level staging: press x on changed lines to select them, then s to
  apply only those selected lines to the index; hunk staging remains the default when no lines
  are selected.
- Task completion now waits for both stdout and stderr readers to reach EOF, preventing the last
  output chunk from being lost when a short-lived task exits.
- Added a keyboard-toggleable editor split (Ctrl-\) with a persisted workspace layout setting;
  Ctrl-Shift-\ switches between vertical and horizontal splits, and narrow terminals keep the
  split disabled unless there is enough editor width.
- Git stash view now supports creating stashes including untracked files, opening stash patch
  details, and scrolling commit/stash detail content. Commit messages support multiline input
  and amend mode (Ctrl-A).
- Git history entries now open an asynchronous commit detail view with changed-file stats and
  patch content; large details are bounded and snapshot/repository checks are preserved.
- Added a dedicated task output panel (`Ctrl+Shift+O`) with scrolling, text search, full-output
  copy (`y`), and file-link navigation (`l`) for compiler/test output.
- Git branch checkout now protects dirty worktrees with an explicit choice: stash and switch,
  carry changes, or cancel. Stash/checkout errors leave the current branch untouched.
- F9/Ctrl+F9 now load project tasks from `.mtide/tasks.toml` (also `tasks.toml`), supporting
  build/run entries, command aliases, args, relative working directories, and environment values.
  Declared `depends_on`/`dependsOn` tasks now run in dependency order; a failed or cancelled
  prerequisite clears the queue and prevents dependent tasks from starting. Default language
  recipes remain the fallback when no configured task exists.
- Terminal sessions can now be renamed from the terminal panel with `Ctrl+Shift+R` or the
  command palette; the selected session keeps its PTY and scrollback.
- Git diff now supports toggling whitespace-insensitive comparison (`w`) while retaining the
  original source lines for rendering and selective hunk staging.
- Git CLI commands now drain output concurrently and enforce a 30-second timeout; timed-out
  processes are terminated and reported instead of blocking the workspace indefinitely.
- Added asynchronous inline Git blame for the current editor line, showing author and commit
  summary in the status bar without blocking cursor movement.
- Git branch view now supports asynchronous merge (`m`) and rebase (`R`) operations with Git
  conflict/error output surfaced through notifications.
- Added asynchronous Git remote actions: `f` fetch, `l` pull, `u` push, and `y` sync. Git output
  and credential-helper/non-fast-forward errors are surfaced without IDE-side password handling.
- Git branch deletion now requires a second `d` confirmation and rejects current or remote
  branches, preserving a safe path for unmerged-branch errors.
- Git branch view now supports creating (`n`) and renaming (`r`) local branches through a guarded
  keyboard input dialog; remote branches remain read-only.
- Git changes now display explicit conflict/staged/unstaged/untracked group labels, and unresolved
  conflict entries are protected from direct or bulk staging.
- Git diff preview now tracks individual hunks; `Tab` selects a hunk and `s` stages only that
  hunk through the validated patch engine, with multi-hunk rendering coverage.
- Task results now jump to the selected diagnostic rather than always opening the first problem;
  multi-diagnostic navigation is covered by an application-level regression test.
- Added asynchronous Git diff preview from the changes panel; Enter opens a color-coded unified
  diff and Esc/arrow/page keys navigate it without blocking the editor.
- Added HEAD-snapshot-consistent Git history pagination; `PageDown` loads older commits and warns
  instead of silently appending when the repository history changes.
- Added Git stash view with asynchronous listing and keyboard actions to apply, pop, or drop a
  selected stash.
- Added asynchronous Git history browsing in the Git panel with selectable commit summaries and
  detail notifications, alongside the existing changes and branch views.
- Added Git panel commit workflow with guarded non-empty message input, commit failure feedback,
  and `a`/`A` shortcuts for staging or unstaging all visible changes.
- Added Git changes panel branch browsing with local-branch checkout, staged/unstaged toggling,
  asynchronous status refresh, and branch-aware keyboard navigation.
- Added the Rust workspace and the P0/P1 editor foundation.
- Added rope-backed UTF-8/grapheme-safe editing with atomic undo/redo transactions.
- Added atomic workspace persistence, session recovery, configuration layering, file watching,
  encoding and line-ending preservation.
- Added terminal cell diff rendering, responsive layouts, keyboard handling, search, fuzzy file
  lookup, syntax spans, managed-process RAII, and JSON-RPC framing.
- Added GBK detection/round-trip encoding, navigation history, symbol indexing, and keyboard-safe
  tab/tree TUI component models.
- Added atomic gzip diagnostic export with redacted paths.
- Restored session files now re-establish watcher/baseline state, preserve unsaved snapshots as
  dirty, and use fingerprint fallback when polling misses a filesystem event.
- Added actionable external-change handling: reload (`Ctrl-R`), compare summary (`Ctrl-Alt-D`),
  and explicit overwrite (`Ctrl-Shift-S`) instead of leaving conflicts as a dead-end notice.
- Added shebang-aware language detection for extensionless Python, Node, and Deno files.
- Added cancellable chunked file loading with explicit size-limit errors, wired to SIGINT before
  application startup and Rope construction.
- Wired single-cursor grapheme-safe selections into the editor: Shift navigation extends the
  selection, and typing/backspace replaces or removes it as one undoable edit.
- Wired application-level multi-cursor insertion: `Alt-D` adds a same-column cursor on the next
  line, with deduplication, a 1000-cursor cap, and one-step undo.
- Added application clipboard actions (`Ctrl-Shift-C/X/V`) backed by the bounded clipboard history,
  while preserving Ctrl-C as the terminal interrupt-safe quit key.
- Added editor-to-terminal selection transfer (`Ctrl-Alt-T`) with optional `Ctrl-Alt-Shift-T`
  auto-enter behavior.
- Added terminal output link detection and `Ctrl-Shift-L` navigation for file:line:column
  diagnostics, with URL opening delegated to the platform browser.
- Added terminal mouse selection and keyboard copy (`Ctrl-Shift-C`) with newline-preserving
  clipboard history integration.
- Moved Git status and branch refreshes off the TUI thread with non-blocking channel polling and
  repository-root validation for stale results.
- Added `Ctrl+Shift+F9` to cancel a running build/task from the TUI, with visible task-panel state.
- Added configurable timed autosave (default 60s, zero disables); unnamed buffers use the session
  snapshot and autosave pauses on detected external changes.
- Added configurable `save_all_on_quit` handling for named dirty buffers, with external-change
  protection preserved during shutdown.
- Added Zen/focus mode (`Ctrl+Alt+Z`) to hide the sidebar and bottom panels while preserving the
  editor layout and keyboard workflow.
- Added multi-tab editing with dirty-state preservation, preview replacement, pinning, CLI
  multi-file opening, `Ctrl+Tab`/PageUp/PageDown switching, close protection, and session restore.
- Wired the command registry into a searchable command palette (`Ctrl+Shift+P`) with keyboard
  filtering, execution, and recent-command prioritization.
- Added an F1 keyboard-help browser showing registered commands and shortcuts, with Esc/F1 close handling.
- Added an editor scrollbar track with search-hit markers and current-line highlighting.
- Added compact ASCII-safe file type icons to editor tabs, including an unknown/untitled fallback.
- Added persistent line bookmarks with F11/Ctrl-F11 controls, gutter markers, and scrollbar markers.
- Added a Ctrl-E recent-files picker with query filtering, missing-file markers, keyboard navigation,
  Enter-to-open, and session persistence.
- Added smart selection expansion/shrinking from identifier to line and buffer with Ctrl-Alt arrows.
- Added configurable UI-12 scrollbar modes (`thin`, `block`, `hidden`) with hidden-mode status summaries.
- Added an F8 keyboard-accessible notification history overlay with bounded recent messages.
- Added editor mouse click定位与拖拽选区、滚轮垂直移动，并修正终端/编辑区鼠标事件分发。
- Added configurable native-terminal foreground colors and transparent/non-transparent rendering options.
- Added `?` command-palette entry into keyboard help plus command/shortcut filtering inside the help browser.
- Added persistent sidebar sizing through workspace layout data and mouse-resizable UI-01 divider handling.
- Made UI-15 file icons opt-in via `file_icons`, with no reserved icon column when disabled.
- Added an F7 Git changes panel with asynchronous status entries, keyboard selection, and Space stage/unstage actions.
- Added an F6 task results/problems panel backed by structured test results and parsed diagnostics.
- Added runtime soft-wrap switching (`Ctrl-Alt-W`) with horizontal-scroll fallback and status-bar
  mode reporting.
- Added indentation inference for files without applicable `.editorconfig`, with conservative
  repeated-sample detection for spaces and tabs.
- Wired current-line duplicate/delete shortcuts (`Ctrl-Shift-D/K`) to atomic core operations with
  undo support.
- Wired `Alt-↑/↓` line movement with logical-column preservation and undo support.
- Added current-search-match replacement from the clipboard (`Ctrl-Shift-R`) with refresh and
  single-step undo.
- Added replace-all for the active literal search (`Ctrl-Alt-R`) as one undoable transaction.
- Added fold-all/unfold-all commands (`Ctrl-Alt-[` / `Ctrl-Alt-]`) with large-file safeguards.
- Wired language-aware current-line comment toggling to `Ctrl-/`, with atomic undo support.
- Added runtime regex-search mode toggle (`Ctrl-Alt-F`) while retaining literal search as default.
- Fixed regex replace-all to use capture-group expansion instead of silently falling back to
  literal replacement.
- Added Unicode-safe Ctrl-←/→ word navigation while preserving UTF-8 boundaries.
- Added M1 snippet expansion for built-in `for`/`fn` templates, placeholders, filename variables,
  and Tab/Shift-Tab stop navigation.
- Added identifier text conversion shortcuts (camel/snake/kebab/Pascal/case variants) with
  numeric-safe tokenization and one-step undo.
- Added selection/current-line surrounding with parentheses or quotes as an atomic edit.
- Added the standalone `mtide-vt` screen state machine with ANSI cursor movement, SGR styles,
  wide-character cells, clearing, and alternate-screen restoration.
- Added `mtide-term` terminal session state with non-blocking pipe fallback output, bounded
  scrollback, named multi-instance focus management, and RAII child cleanup.
- Added a Unix PTY backend with shell input/output, initial window sizing, and resize propagation;
  pipe fallback remains available for platforms without the backend.
- Integrated the terminal panel into the main TUI with focus toggling and common printable,
  control, navigation, and alternate-key input forwarding.
- Added `mtide-task` for non-blocking task execution, streamed output, file/line problem matching,
  cancellation, and RAII process cleanup.
- Added `.mtide` workspace files with portable relative roots, layout/config references, atomic
  save/load, `--workspace` loading, and a workspace-save shortcut.
- Added task variable expansion for workspace/file context, selection, line number, and environment
  variables without shell interpolation.
- Added Git amend commits and snapshot-pinned history pagination with refresh detection when HEAD
  changes between pages.
- Added Git hunk-level staging through validated zero-context patches, preserving unrelated working
  tree changes.
- Added conflict-block resolution shortcuts (`Ctrl+Alt+1/2/3/0`) for ours, theirs, both, and
  neither, with cursor-scoped edits, undo support, marker-aware staging protection, and automatic
  `git add` after saving a fully resolved conflict file.
- Added Git diff split view (`v`) with paired old/new columns, hunk highlighting, unified fallback
  for narrow terminals, and preserved hunk navigation/staging behavior.
- Added command-palette and `Ctrl+Alt+Shift+1/2/3/0` actions to resolve all conflict blocks as one
  undoable transaction.
- Added dynamic workspace-root add/remove actions via the command palette or `Ctrl+Alt-O` /
  `Ctrl+Alt+Shift-O`, with directory validation and `.mtide` persistence.
- Connected the cancellable, ignore-aware workspace search engine to an asynchronous TUI panel
  (`Ctrl+Shift+F`), with multi-root results, selection navigation, and direct file/line jumping.
- Extended workspace search with regex, case-sensitivity, whole-word matching, invalid-pattern
  diagnostics, and a 50 MiB per-file safety limit; toggles are available in the search panel.
- Added workspace replacement mode (`Ctrl+R`) with regex capture expansion, asynchronous batch
  writes, read-only/invalid-file skipping, transactional rollback on failure, and `Ctrl+Z` snapshot
  restoration after a successful replacement.
- Added immediate Retro/Modern theme toggling (`Ctrl+Alt+M`), persisted theme selection in workspace
  layout data, and a Retro function-key chrome bar without changing the editor viewport geometry.
- Added named multi-terminal session controls: create (`Ctrl+Shift+N`), switch (`Ctrl+PageUp/PageDown`),
  close (`Ctrl+Shift+X`), and session index/name display in the terminal panel.
- Integrated terminal fullscreen rendering with live PTY resize propagation and fullscreen-specific
  input routing; `Alt+T` enters/exits fullscreen and `Ctrl+`` restores the editor.
- Integrated task execution into the main TUI: F9/`Ctrl+F9` default build/run recipes, asynchronous
  polling, output panel rendering, duplicate-run protection, and elapsed status reporting.
- Added configurable task-output tail retention so long-running builds do not grow UI memory without
  bound.
- Wired Git repository discovery into the main editor status line with current branch and staged,
  unstaged, untracked, and conflict counts refreshed without blocking the UI loop.
- Fixed task problem matching across split stdout/stderr chunks so diagnostics are not lost when a
  `file:line:column` record crosses a pipe read boundary.
- Tasks now launch in their own Unix process group and cancellation/drop reaps the group to reduce
  orphaned compiler or test subprocesses.
- Added F4 task-diagnostic navigation: relative and absolute `file:line:column` problems now open
  the target file and position the cursor accurately.
- Added configurable `terminal_scrollback` workspace setting (clamped to a safe range) and applied
  it when new terminal sessions are created, including safe-mode defaults.
- Added configurable `terminal_force_keys` parsing for `ctrl`, `alt`, `shift`, `backtick`, and other
  named keys, with invalid entries ignored and default force keys preserved.
- Added Git branch lifecycle operations: create, checkout, rename, and safe/force delete, covered by
  a real temporary repository workflow test.
- Added Git merge/rebase and stash lifecycle APIs with real temporary-repository coverage for stash
  restoration and merge results.
- Expanded task problem matching for Rust compiler arrows/error codes, TypeScript `file(line,col)`
  diagnostics, and Ruff-style error/warning codes.
- Added `mtide-git` repository discovery and Git porcelain-v2 `-z` status parsing with graceful
  missing-git/error handling.
- Added shared `mtide-diff` line hunks with selective application and context-integrity checks;
  Git worktree file diffs now use the shared engine.
- Added Git file staging/unstaging and guarded commit entry points with empty-message rejection.
- Added Git branch listing and bounded commit-history pagination with stable parsed summaries.
- Added `cargo xtask check`, `depcheck`, `snapshot`, and `bench` quality-gate commands.
