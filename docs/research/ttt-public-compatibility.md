# TTT public compatibility surface

Research date: 2026-11-07. This is an inventory of user-visible contracts for the Wayfinder question in [maczh/mtide#3](https://github.com/maczh/mtide/issues/3). It is an inventory, not a parity verdict.

## Baseline and evidence boundary

The locally pinned TTT checkout is currently:

- clean upstream reference: `origin/main` = `afa48edc0b35539423062bafef747dca610fbd5b`;
- local `main`: `3eb1ca669e3554ded66836dbf2efbbf14cfa493b`, four commits ahead of that reference;
- working tree: dirty (`internal/oracle/script.go`) plus untracked `.tmp-goside/` and `cmd/scopecheck/`;
- nearest local tag: `v1.9.2-AI`; no clean baseline pin is recorded in mtide.

The local product docs call out v1.10.0, but GitHub now has v1.11.0 (`d47fbcf27de59c820ae39130aa5ba0f5486a5e64`, released 2026-11-06). GitHub's compare API reports the local `origin/main` and v1.11.0 are diverged (125 commits ahead, 11 behind). Therefore the first compatibility decision is still open: freeze a clean commit/tag, and decide whether v1.11.0 is in scope. This note uses the local source for the detailed inventory and records v1.11.0 as a release delta to reconcile, not as silently substituted source.

Primary evidence is the local TTT source/docs/tests and mtide's domain/product docs. Local source links below use the observed commit for reproducibility; the local checkout's dirty files are not treated as release contracts.

## User-visible contracts

### 1. Launch, CLI, target precedence, and workspace files

The executable name is `ttt`. Its CLI accepts files, directories, `file:line[:column]`, GitHub PR URLs, and stdin. `-` explicitly selects stdin; piped stdin is consumed automatically only when no other target was requested. Options are `--help/-h`, `--version/-v`, `--workspace FILE`, `--welcome`, `--config FILE`, `--exec SCRIPT`, `--exec-split-on SEP`, `--plugin FILE`, `--size WxH`, `--debug`, and `--listen`. `--listen` requires a real TTY and serves `POST /exec` on `127.0.0.1:4242`; `--exec` uses a simulation screen and exits non-zero after invalid/failed actions. These rules are implemented in [`cmd/ttt/main.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/cmd/ttt/main.go#L49-L220) and startup wiring at lines 359–421.

Multiple folder arguments create a multi-root workspace. `.ttt` files serialize folder paths and are loaded with `--workspace`; the active file determines the owning folder for Explorer, Search, Git, and status-bar branch context. The file format and path interpretation are documented in [`internal/workspace/workspace.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/workspace/workspace.go#L10-L151) and the [workspace guide](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/workspaces.md#L24-L64).

`TTT_CONFIG_DIR` redirects settings, keybindings, themes, plugins, the plugin registry, and logs. `--config` selects an explicit settings file. `--plugin FILE` loads a Lua file at startup with full permissions, which is a distinct, higher-trust path from manifest plugins.

Compatibility obligations: preserve argument precedence, target classification, stdin safety, line/column indexing, PR failure behavior when `gh` is absent, `.ttt` schema/path behavior, exit codes, and config isolation.

### 2. Menus, command IDs, keybindings, focus, and mouse routing

The top menu bar is ordered File, Edit, Selection, View, Options, Help. File exposes New/Save/Save All/Save As, folder/workspace open/add/save/close, PR diff, and Quit. Edit exposes Undo/Redo/Cut/Copy/Paste/Find/Replace. Selection exposes Select All and multi-cursor actions. View exposes Command Palette, Quick Open, Explorer/Search/Replace/Changes/Outline, Sidebar, each configured Agent, Terminal, Theme, Keybindings, and Settings. Help exposes About; Options is generated from settings. These labels, separators, command bindings, dynamic shortcut labels, editor context menu, diff context menu, terminal context menu, and Changes context menus are in [`internal/app/menus.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/app/menus.go#L12-L180).

The command registry is a second public surface: current source contains 230 `ID` entries across built-in editor, file, workspace, explorer, search, changes/Git, diff, tab, terminal, LSP, settings/options, plugin, Agent, PR, welcome, debug, and help namespaces. The authoritative registration sites are [`commands_editor.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/app/commands_editor.go), [`commands_git.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/app/commands_git.go), [`commands_options.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/app/commands_options.go), [`commands_plugin.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/app/commands_plugin.go), and their sibling `commands_*.go` files. Hidden commands and option selector IDs are included in that count; plugin-visible command listing must not accidentally expose or rename the wrong subset.

The shipped JSON contains 89 default keybindings. It is the complete default mapping and supports VS Code-style chords and user overrides: [`config/keybindings.json`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/config/keybindings.json#L1-L89). Its groups are:

- global/file/workspace: sidebar toggle/focus, palette, quick open, folder/workspace open, new/save/save-all/save-as, quit, tabs and pinning;
- editor: select/copy/cut/paste, undo/redo, line and word movement/deletion, line insert/move/duplicate/delete/join, comments, transforms, matching brackets, file start/end, indentation and folding;
- search and panels: find/replace/next/previous, Explorer/Search/Replace/Changes, panel resize, Sidebar resize, terminal toggle/new/fullscreen;
- LSP: autocomplete, hover, rename, definition/implementation/type definition, references, format document/selection/external, organize imports, fix all;
- multi-cursor: next occurrence, all occurrences, undo cursor;
- menus/help: File/Edit/View/Options/Help menus, menu-bar toggle, and keybinding viewer.

Key dispatch is focus-sensitive. Terminal focus sends most keys directly to the PTY except force keys; overlays capture input and restore the prior focus; menus resolve current shortcuts dynamically. These are behavior contracts, not merely UI details. The product description calls out the same rule in the [terminal guide](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/terminal.md#L8-L32); dispatch and force-key ownership live in `internal/ui/root.go`, `internal/app/keys.go`, and `internal/app/commands.go`.

### 3. Settings, themes, schema compatibility, and persisted state

Public files are `settings.json`, `keybindings.json`, `themes/*.json`, and `plugins.ttt.json`. Settings include theme and appearance, editor behavior, diff rendering, search debounce, Explorer filtering/reveal, sidebar/panel layout, Git file view, terminal shell/args/scrollback, Agent providers, LSP servers and save-on-rename/code-actions, autocomplete, optional AI completion, Markdown, images, plugins, welcome/favorites, and extension-to-formatter mappings. The JSON structs, defaults, unknown-field preservation, normalization, and legacy Agent migration are in [`internal/config/settings.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/config/settings.go#L33-L590); the shipped defaults are [`config/settings.json`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/config/settings.json#L1-L107).

Most settings have a form with Apply/Cancel; LSP and external formatter details are JSON-only. Theme JSON covers editor, syntax, diff, sidebar, tabs, status, borders, terminal ANSI palette, semantic colors, and optional TextMate token colors. Theme switching has a live-preview/reload path. References: [settings reference](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/reference/settings.md#L1-L260) and [theme guide](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/themes.md#L56-L310).

Compatibility obligations: preserve field names/types/defaults, unknown fields, legacy aliases, workspace/config path roots, Apply/Cancel and restart semantics, and theme fallback behavior. A schema change needs migration or an explicit compatibility decision.

### 4. Editor, tabs, files, and text semantics

The documented editor contract includes TextMate highlighting, bracket matching, Find/Replace, Go to Line, Unicode editing, system/OSC52 clipboard, undo/redo, indentation and `.editorconfig`, folding, line operations, multi-cursor editing, mouse positioning, diagnostics, formatting, Git gutter/blame, and diff tabs ([README editor feature list](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/README.md#L105-L126), [PRD FR-02/FR-03](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/PRD.md#L43-L78)).

The high-risk semantics to freeze are rune/byte/display-column conversion, East Asian and emoji width, soft-wrap and viewport mapping, selection direction and copy order, multi-cursor overlap and one-step undo grouping, save-point/dirty computation, external file changes, Save As overwrite protection, read-only/diff buffers, comment syntax by language, and overlay focus restoration. These are represented by focused core/UI tests such as `internal/core/{buffer,cursor,selection,multicursor,undo}`, `internal/ui/editor_*`, `editor_widget_*`, `diff_editor_*`, `watch_*`, and `internal/config/editorconfig_*`.

### 5. Explorer, search, and workspace navigation

Explorer supports multi-root trees, hidden/Git-ignored filtering, sorting, expand/collapse, refresh, create file/folder, rename, delete, reveal, copy absolute/relative path, and Git status colors. Search has file/workspace modes, smart-case and regex toggles, include/exclude globs, grouped results, result navigation, replace-one/all, and jump-to-file/line. Quick Open and the command palette are searchable entry points, and the active file/workspace root is shared by Explorer, Search, and Changes. Primary references are [`commands_explorer.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/app/commands_explorer.go), [`commands_search.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/app/commands_search.go), the [search guide](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/search.md#L1-L70), and the Explorer/Search test files.

Compatibility obligations: preserve path-root selection, symlink/traversal boundaries, sort/filter behavior, debounce/cancellation and result generation, selected-result activation, current-buffer search synchronization, and all destructive-action confirmations/empty/error states.

### 6. Git, diffs, history, blame, and PR review

Git is exposed through status, staged/unstaged/untracked file trees, stage/unstage/discard, commit, branch creation/switching, fetch/pull/push/sync, Git gutter, blame, history, commit details, file-at-commit views, and editable working-tree diffs. Diff tabs expose split/unified, changes-only/full-file, wrapping, context expansion and hunk navigation. PR URLs open GitHub review surfaces; `gh` is an external prerequisite. The product scope is documented in [README Git](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/README.md#L192-L234), [Git guide](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/git.md#L1-L110), and `internal/app/commands_git.go`/`changes_*`/`pr.go`.

Compatibility obligations: preserve repository-root selection in multi-root workspaces, porcelain/path parsing, staged-vs-unstaged selection, diff hunk semantics, context/wrap/view settings, async cancellation, credential prompts and non-persistence, conflict/error states, and PR/comment navigation.

### 7. LSP and formatter boundary

TTT starts configured language servers over stdio/JSON-RPC and exposes completion, signature help, hover, diagnostics, definition, implementation, type definition, references, rename, document/selection formatting, format-on-save, code actions, and server restart/status. Servers are configured under `lsp.servers`; external formatters are configured separately. Language-server binaries and versions are external dependencies. References: [LSP feature reference](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/reference/lsp-features.md#L1-L55), [LSP guide](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/lsp.md#L230-L350), and `internal/lsp` plus `internal/app/app_lsp*.go`.

Compatibility obligations: preserve process launch/cwd/env, JSON-RPC framing, UTF-16/rune position conversion, full-document sync, request cancellation/stale-result handling, diagnostics merge/clear, snippet/commit-character insertion, workspace-edit undo grouping, failure/restart status, and configuration precedence.

### 8. Integrated terminal, PTY, VT, clipboard, and pointer behavior

The integrated terminal has multiple tabs, configurable shell/args/scrollback, VT parsing, 24-bit/256-color output, ANSI theme colors, alternate-screen behavior, terminal links, scrollback, OSC title/clipboard handling, and focus-sensitive PTY input. Host force keys remain active while the PTY is focused. The [terminal guide](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/terminal.md#L1-L45), `internal/terminal`, and `internal/ui/terminal_*` define the boundary.

Compatibility obligations: preserve VT/control-sequence output, application cursor/keypad modes, resize ordering, PTY close/exit idempotence, raw modifier encoding, shell cwd/env, terminal selection/copy/paste, OSC52 policy, links, alternate-screen restoration, scrollback, and terminal-dependent mouse/pointer behavior.

### 9. Lua plugins, manifests, permissions, and registry ecosystem

The plugin contract is broader than “run a Lua file.” A plugin has `plugin.ttt.json` metadata/API version, entrypoint, permissions, commands, keybindings, sidebar/bottom/drawer/editor panels, file handlers, themes/grammars, and lifecycle hooks. Plugins may be installed from the community registry, URL, or local/workspace locations; users can approve, enable/disable, reload, update, uninstall, or disable the plugin system. The [plugin guide](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/plugins.md#L16-L150) and [authoring reference](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/plugin-authoring.md#L1-L282) define the manifest, lifecycle, registration shapes, and permissions.

The documented API surface includes the `ttt` module and `ttt.json`, `ttt.regex`, `ttt.command_line`, `ttt.terminal`, `ttt.editor`, `ttt.events`, `ttt.diagnostics`, `ttt.decorations`, `ttt.bookmarks`, `ttt.fs`, `ttt.system`, `ttt.net`, `ttt.rpc`, `ttt.settings`, `ttt.storage`, `ttt.crypto`, `ttt.automation`, plus widget/raw-cell APIs. User-visible semantics include callback/event names and payloads, 1-based Lua offsets, Unicode width, widget selection/focus/mouse behavior, redrawing, tabs/diff/readonly views, timers, storage, subprocess/network/RPC completion order, and permission-denied errors. The module index begins at [`plugin-authoring.md#ttt-module-functions`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/plugin-authoring.md#L282-L1031), with editor/filesystem/system/network/RPC/automation references through lines 1865–2630.

Permission scope is part of the contract: panel/editor/terminal/screen/clipboard/settings/storage/system/network/filesystem/RPC capabilities are separately gated; filesystem paths and executable/host/endpoint allowlists are bounded. Compatibility obligations include manifest parsing, permission prompts/persistence/intersection, sandbox behavior, discovery precedence, registry schema, API return/error shapes, lifecycle cleanup, and main-thread callback ordering. TTT's plugin API is an ecosystem compatibility target, not interchangeable with an mtide-native API.

### 10. Agent sessions and automation

The View menu and command palette expose configured persistent right-side CLI Agent sessions for Codex, Claude Code, CodeBuddy, GitHub Copilot, Cursor, Kimi, and OpenCode. Each provider has executable, args, cwd/install and persistence behavior; missing executables are surfaced with an install command and should not be silently installed. Agent PTYs are distinct from the ordinary terminal. Sources: [`config/settings.json`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/config/settings.json#L34-L61), [`internal/app/menus.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/internal/app/menus.go#L67-L76), and [AI Agents guide](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/docs-web/src/content/docs/guides/ai-agents.md#L14-L84).

The `--exec`/`--listen` script protocol is a public automation API: `click`, `key`, `type`, `exec`, `screenshot`, `debug`, `wait`, `wait-for`, `quit`, and `shutdown`. Actions are acknowledged after main-thread handling/redraw; invalid commands, missing panels, capture failures, and timeouts are errors. This protocol is documented in [README Debug & Testing CLI](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/README.md#L395-L425) and implemented through `internal/app/exec_script.go`/`listen.go`.

### 11. Errors, notifications, lifecycle, and observability

Failures are user-visible through status notifications, dialogs, panel empty/error rows, LSP/plugin output, stderr, exit codes, and `crash.log`. A panic writes a timestamp/versioned crash report under the config directory, prints the stack to stderr, finalizes the terminal, and exits 1 ([`cmd/ttt/main.go`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/cmd/ttt/main.go#L49-L64)). Startup and shutdown close LSP, plugins, PTYs, listeners, image resources, and the screen. Async work returns to the main event loop; cancellation and stale-generation handling prevent old results from changing newer UI state.

Compatibility obligations: preserve error classification and text where scripts/users depend on it, notification severity, focus restoration after dismissal, cleanup/idempotence, cancellation, crash artifact location/format, and no-secret logging guarantees.

### 12. Distribution, package, and release surfaces

The public install surface lists Homebrew, AUR, Nix, Scoop, Go install, source builds, direct release downloads, the `install.sh` installer, and Herdr integration ([README installation](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/README.md#L9-L104)). `install.sh` selects Linux/macOS/FreeBSD/Android Termux by OS/architecture, downloads a named GitHub release asset, accepts an optional version and `INSTALL_DIR`, and installs an executable named `ttt` ([`install.sh`](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/install.sh#L1-L95)). The Makefile packages Linux amd64/arm64, macOS amd64/arm64, FreeBSD amd64/arm64, Android arm64, and Windows amd64/arm64; the release workflow builds those binaries and `checksums.txt` on `v*` tags ([Makefile](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/Makefile#L1-L46), [release workflow](https://github.com/eugenioenko/ttt/blob/3eb1ca669e3554ded66836dbf2efbbf14cfa493b/.github/workflows/release.yml#L1-L59)). Docs publish to `tttedit.dev`.

The current upstream release is [v1.11.0](https://github.com/eugenioenko/ttt/releases/tag/v1.11.0). Its release notes add or clarify public behavior not present in the local v1.9.2-derived checkout: selected-text find/replace prefill and Find/Replace switching, manual language mode, add cursor above/below, Ctrl-link navigation, context-aware pointers, Alt-wheel horizontal scrolling, running-window folder forwarding, plugin additions, UTF-16 positions, save-point preservation, external-delete warning, and Go 1.25 minimum. These must be either included by choosing v1.11.0 or explicitly excluded by pinning an older clean baseline.

Compatibility obligations: settle whether mtide preserves the `ttt` binary name or deliberately ships `mtide`, the config namespace, release asset names, version reporting, supported platform matrix, installer behavior, checksums, package-manager metadata, docs URL, and upgrade/migration story.

## Tests as executable contract evidence

The TTT test surface is deliberately layered:

- Go unit tests cover core buffer/cursor/selection/multicursor/undo/diff, config/schema/theme/editorconfig, LSP/JSON-RPC, plugin manifest/permissions/Lua APIs/widgets, Markdown, terminal and search.
- `tests/e2e/` composes the full App with `SimScreen` and tests rendered UI, commands, panels, Git, LSP, terminal, plugins and lifecycle.
- `tests/functional/` drives the real binary through `--exec`, asserting screen text, screenshots, state dumps, file effects, startup/exit, editing, Unicode, search/replace, tabs, panels, and plugins.
- `tests/integration/` drives real PTYs and external processes for LSP, external file changes, settings roundtrips and bracketed paste.
- `tests/chaos/` fuzzes keys, mouse, resize, clipboard and command-palette actions in Docker and records deterministic replay JSON.
- Release/pre-release/CI workflows run unit, functional, integration, LSP, docs and chaos gates; `make` also exposes editor/diff/log/code benchmarks.

These tests prove selected behavior, not a complete compatibility matrix. A real mtide compatibility plan still needs differential scenarios that drive both the chosen clean TTT pin and mtide and compare semantic UI snapshots, files, settings, subprocesses, errors, and exit results.

## Explicit unknowns and decisions required

1. **Baseline:** choose and record one clean commit/tag. The local checkout is dirty and differs from both its `origin/main` and current upstream v1.11.0; no mtide pin file exists.
2. **Release scope:** decide whether v1.11.0 behavior is in the first release baseline or whether compatibility freezes at an older release/local commit.
3. **Canonical command list:** source registration includes hidden, dynamic, and option-selector IDs; generate a checked catalog from the chosen pin rather than relying on README prose.
4. **Full plugin parity:** the authoring guide is the completeness checklist, but this inventory does not reproduce every function signature, widget field, event payload, return value, error, or permission edge case symbol-by-symbol.
5. **External dependencies:** exact compatibility depends on Git, `gh`, ripgrep, shells, PTYs, terminal emulators, clipboard tools, LSP servers, formatter binaries, Agent CLIs, tmux, package managers, OS path rules, and network services; their supported versions and failure matrices are not fixed here.
6. **Packaging ownership:** Homebrew/AUR/Scoop metadata and Herdr/plugin integrations are external repositories or services; this repo does not establish whether mtide must preserve, fork, or deprecate each one.
7. **Terminal matrix:** key delivery and pointer/VT behavior varies by terminal emulator and OS; a terminal compatibility matrix is still needed.
8. **Behavioral parity:** no mtide-vs-TTT differential test run was performed, so this document does not claim that current mtide behavior is compatible.

## Source pointers

- Local TTT snapshot: `/home/macro/Work/go/src/github.com/maczh/ttt`, observed `3eb1ca669e3554ded66836dbf2efbbf14cfa493b` plus the dirty-state note above.
- Current upstream release commit: `d47fbcf27de59c820ae39130aa5ba0f5486a5e64`, [v1.11.0 release](https://github.com/eugenioenko/ttt/releases/tag/v1.11.0).
- mtide domain vocabulary: [`GLOSSARY.md`](../../GLOSSARY.md); compatibility principles: [`docs/00-index.md`](../00-index.md), [`docs/01-product-requirements.md`](../01-product-requirements.md), [`docs/05-cli-config.md`](../05-cli-config.md), [`docs/06-editor-core.md`](../06-editor-core.md), [`docs/08-plugin-and-agent.md`](../08-plugin-and-agent.md), [`docs/09-testing.md`](../09-testing.md), [`docs/13-upstream-research.md`](../13-upstream-research.md), [`docs/14-implementation-status.md`](../14-implementation-status.md).
