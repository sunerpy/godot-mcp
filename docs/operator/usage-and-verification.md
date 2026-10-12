# 能力实操与验证复现

这一页回答两个问题：**每个能力具体怎么调、什么响应算成功**，以及**怎么把本项目自带的验证门禁在你的机器上重跑一遍、拿到同样的结果**。

安装、客户端接线、配置键、打包校验、故障排查各有专页，本页只做交叉引用，不重复：
[快速开始](quickstart-stdio.md)、[MCP 客户端](mcp-clients.md)、[配置](configuration.md)、[打包](packaging.md)、[故障排查](troubleshooting.md)。

## 30 秒定位

这是一个本地单用户的 MCP 服务器，进程跑在你的 Godot 项目旁边，客户端通过 stdio 与它对话。本页所有数字都是实测值，没测过的地方直接写「未测量」。下面的数字测自首个公开提交（`chore: initial commit`）的源码树经 `make dist` 构建的 Linux 发布归档，由 `scripts/real_client/report_measure.py` 通过官方 MCP Python SDK 1.28.1 驱动归档里的同一个二进制得到，主机为 Linux x86_64：

```console
binary  godot-mcp 0.1.0，godot-mcp-0.1.0-x86_64-unknown-linux-musl.tar.gz 里的 bin/godot-mcp
sha256  ddbeb5072f5587ec0273fb4431d8d4889947c190d9918be5624297c7c11e5457
godot   Godot_v4.7.1-stable_linux.x86_64
rust    1.96.0
```

**一个会让人误判的坑，先说在前面：一次只发一帧，等到响应再发下一帧。** 在同一个会话上同时发出 40 个 `project.info.get`，实测 15 个给出答案，另外 25 个返回（给出答案的个数随请求到达的时序而变，此前各次测量在 8 到 16 个之间）：

```console
operational limit subject_concurrency was exceeded
```

这不是故障，是每主体并发限流器在按设计工作，`health.get` 里报的上限为 8。批量灌帧的客户端会得出「服务器坏了」的错误结论。这一行错误串值得记住，方便日后直接搜索。

要装、要接客户端，去 [快速开始](quickstart-stdio.md) 和 [MCP 客户端](mcp-clients.md)。要知道某个能力能不能用、算不算成功，留在本页。

## 前置条件与安装

* 取一份对应平台的发布归档并校验：见 [打包](packaging.md)。
* 首次 stdio 跑通、`self-check`、`check-config`、`assets install` 的完整步骤：见 [快速开始](quickstart-stdio.md)。
* 客户端的 `command` / `args` / `env` 写法：见 [MCP 客户端](mcp-clients.md)。
* 每个配置键与失败即关闭的不变量：见 [配置](configuration.md)。

读、分析、校验这些能力都不需要安装 Godot。本页的健康诊断一节就是在**找不到 Godot 可执行文件**的条件下测的。

**要用进程面（导入、测试、运行、编辑器会话、导出），`godot-mcp-supervisor` 必须和 `godot-mcp` 放在同一个目录里。** 这是最容易踩的一个安装坑：发布归档的 `bin/` 下是一对可执行文件，只把 `godot-mcp` 单独拷出来用，进程面的每个工具都会拒绝。实测 `project.import.run` 的错误串是

```console
unavailable: the process boundary is not available: the unix_locator boundary is unavailable: containment boundary is unavailable: the bundled `godot-mcp-supervisor` executable is not beside this server
```

这条理由只点出可执行文件的名字，不暴露服务器装在哪个目录。`health.get` 会把 `godot` 这一项报成 `degraded`，codes 里多出 `process_plane_unavailable`，实测消息逐字为

```console
a Godot executable resolved but the bundled `godot-mcp-supervisor` executable is not beside this server, so the full profile's process-plane tools cannot create a contained process until the release archive's `bin/` pair is restored together
```

注意这时顶层 `status` 仍然是 `ready`：文件面和分析面确实还能用，只有进程面废了。所以别只看 `status`，要看 `components` 里 `godot` 那一项。每次启动具体落在哪种进程边界里、为什么必须经过 supervisor，见 [进程边界](process-boundaries.md)。

## 启动与握手

实测的启动命令行：

```sh
godot-mcp serve --stdio --project <项目目录> --profile full
```

握手顺序，实测：

1. 发 `initialize`。SDK 1.28.1 协商出的 `protocolVersion` 为 `2025-11-25`。
2. 服务器回 `serverInfo` 为 `{"name":"godot-mcp-server","version":"0.1.0"}`，并声明 `prompts`、`resources`、`tools` 三组能力。
3. 发 `notifications/initialized` 通知。
4. 之后才可以发 `tools/list`、`resources/list`、`tools/call`。

下面这一帧用于说明线格式，由上述握手与调用事实拼出，不是抓包记录：

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"health.get","arguments":{}}}
```

项目归属确立不了时，启动直接拒绝，退出码 69：

```console
godot-mcp serve: startup failed: project ownership could not be established: no project.godot discoverable: no project.godot found at or above <path>
```

从一个还没有项目的空工作区起步，用 `serve --workspace <目录>` 代替 `--project`，只适用于 stdio，见 [配置](configuration.md#what-reads-a-configuration-file-in-this-release)。其余启动期失败症状见 [故障排查](troubleshooting.md)。

## 各 profile 的声明面

| profile | tools | `resources/list` 返回的具体资源 | `resources/templates/list` 返回的模板 |
| --- | --- | --- | --- |
| `core` | 15 | 4 | 0 |
| `editor` | 48 | 4 | 0 |
| `runtime` | 44 | 4 | 1 |
| `full` | 73 | 4 | 1 |

后两列相加，正好等于 [工具与资源清单](../protocol/tool-inventory.md) 里生成的 Resources 列（`core` 与 `editor` 为 4，`runtime` 与 `full` 为 5）：生成清单把资源模板计入总数，而 `resources/list` 不返回模板。那一个模板是 `godot-mcp://artifacts/{artifact_id}`，导出产物与截图都从这里读，见 [Artifacts](artifacts.md)。一个 artifact 只对产生它的那个调用方列出、可读，过期后从列表和读取里一起消失。

**73 个名字，在没有任何参数时分成四档。** 在一台配了真实 Godot 4.7.1、supervisor 就位的主机上，把 `full` 的 73 个工具按名字顺序、逐个只带内核自己拥有的 `idempotencyId` 串行调一遍（`limits.deadline_ms` 为 30000），实测 20 个给出答案，37 个因为缺参数被拒，15 个拒绝执行，1 个撞上执行超时。顺序有影响：排在前面的调用会改变后面调用看到的状态，下面会点出来。

## 20 个无需参数就给出答案的调用

`capabilities.get`、`editor.addon.install`、`editor.addon.remove`、`editor.addon.status`、`editor.session.start`、`editor.status.get`、`export.presets.list`、`gdscript.diagnostics.get`、`health.get`、`project.dependencies.get`、`project.discover`、`project.export.status`、`project.files.list`、`project.import.run`、`project.info.get`、`project.run.start`、`project.run.status`、`project.settings.get`、`project.test`、`project.validate`。

「给出答案」的判据是响应不是错误帧、`isError` 也不为真，而不是「结果一定是成功」，更不是「什么都没发生」：

* **有几个调用不带参数也会真的产生效果。** `editor.addon.install` 把 add-on 写进项目，`editor.addon.remove` 又把它删掉。`editor.session.start` 真的启动一个托管的 headless 编辑器会话，`project.run.start` 真的启动一个托管游戏会话。测量结束前，这两个会话由 `project.run.status` 列出后逐个停止。
* **`project.test` 会真的去找测试框架，然后把找的过程报出来。** 装置项目里没装框架，实测响应的 `framework` 为 `null`，`reason` 把 `addons/gut/plugin.cfg`、`addons/gut/gut_cmdln.gd`、`addons/gdUnit4/plugin.cfg` 等查过的位置逐个列出，并明说不会替你装一个来凑一次运行。这是一个诚实的答案，不是一次拒绝。
* **`project.import.run` 不带参数会真的启动一次 Godot，然后判定为不可判定。** `reason` 说清了为什么不敢报成功：没有提名任何期望产物，而引擎也不输出稳定的导入摘要，所以导入成功这件事观察不到。想要一个可判定的结果，就用 `expected_artifacts` 提名导入后应该出现的产物路径。
* `editor.addon.status` 在 install 与 remove 之后调用，实测 `installed` 为 `false`；`project.validate` 的 `verdict` 为 `no_absent_targets`。

拿不准从哪儿开始，就先调 `health.get`：它是无参调用里信息量最大的一个，见下面的诊断小节。

## 37 个需要参数的调用

缺字段时返回 `-32602`，错误串按缺的字段分组：

| 缺的字段 | 工具 |
| --- | --- |
| `path` | `asset.delete`、`asset.put`、`asset.status`、`file.create`、`file.delete`、`file.get`、`file.patch`、`script.create`、`script.patch`、`resource.batch.apply`、`resource.create`、`scene.batch.apply`、`scene.create` |
| `from` | `file.move` |
| `script` | `gdscript.impact.get`、`gdscript.references.get`、`gdscript.symbols.get`、`script.validate`、`script.check` |
| `scene` | `scene.dependencies.get`、`scene.tree.get` |
| `resource` | `resource.dependencies.get`、`resource.tree.get` |
| `target` | `project.uid.update`、`resource.update` |
| `session` | `editor.session.stop`、`project.run.logs`、`project.run.stop`、`runtime.properties.get`、`runtime.recording.capture`、`runtime.screenshot.capture`、`runtime.signals.get`、`runtime.tree.get`、`runtime.ui.get` |
| `preset` | `project.export` |
| `export` | `project.export.cancel` |
| `expectedSha256` | `project.settings.patch` |

实测的错误串举几例：`` `script` is required and must be a project-relative path ``、`` `session` is required ``、``invalid engine request: missing field `target` ``、`` `expectedSha256` is required and must be a string ``。

这一档只跟参数有关，与主机上有没有 Godot 无关。需要引擎的工具在没有 Godot 的主机上是另一种拒绝：`unavailable`，并说明在常规安装位置和 `PATH` 上都没找到可执行文件。

### 带真实参数的实测结果

实测用的固定装置项目含 `scripts/player.gd`、`scenes/main.tscn`、`scenes/hud.tscn`、`scenes/legacy.scn`、`resources/buff.tres`、`resources/legacy.res`。

| 调用 | 实测响应 |
| --- | --- |
| `script.validate` 传 `{"script":"scripts/player.gd"}` | OK，`verdict` 为 `valid_static`，`diagnostics` 为空，另带 limitation `engine_validation_not_performed` |
| `gdscript.symbols.get` 同参 | OK，`class_name` 为 `"CorePlayer"`，`extends` 为 `"Node"`，4 个符号全部 `exact` |
| `gdscript.references.get` 同参 | OK，`caller_total` 为 0，并带 `call_arguments_not_recorded` 与 `engine_class_registry_unavailable` 两条 limitation |
| `gdscript.impact.get` 同参 | OK，`coherence` 判定为 `coherent`，受影响 3 项：项目、`scenes/main.tscn` 以及其中第 8 行的 `Player` 节点 |
| `scene.tree.get` 传 `{"scene":"scenes/main.tscn"}` | OK，每个节点带 `handle`（`nh1:` 前缀），连接项 `confidence` 为 `"exact"`、`from` 为 `"Player"`、`line` 为 16 |
| `scene.dependencies.get` 同参 | OK，候选按序返回，每项自带 `confidence` |
| `resource.tree.get` 传 `{"resource":"resources/buff.tres"}` | OK，`format_version` 为 3，`root_type` 为 `"Resource"` |
| `resource.tree.get` 传 `{"resource":"resources/legacy.res"}` | `unsupported`，补救建议为 "re-save the document in Godot as a text .tres file..." |
| `scene.tree.get` 传 `{"scene":"scenes/legacy.scn"}` | `unsupported`，同形状的补救建议 |

两点容易读错，单独点出来：

* **`script.validate` 是 tree-sitter 静态分析，不是 Godot 的 `--check-only`。** 响应自己就把这件事说了，靠的是 `engine_validation_not_performed` 这条 limitation。需要引擎背书时用 `script.check`：它要求广告 `--headless`、`--path`、`--check-only`、`--script` 的 editor build，缺能力时返回结构化 `unsupported`，不会假装已经检查。
* **二进制 `.scn` / `.res` 不解析也不猜**，而是带着可操作的补救建议拒绝。

置信度三档 `exact` / `inferred` / `unresolved` 的语义，见 [Profiles 与结果](profiles-and-outcomes.md)。

## 15 个拒绝执行的调用

每一个都说出是哪个面不在，而不是笼统地说没实现：

* 8 个依赖已连接的编辑器 add-on：`editor.addon.reload`、`editor.redo`、`editor.scene.open`、`editor.scene.tree.get`、`editor.selection.set`、`editor.types.get`、`editor.undo`、`scene.save`，实测均为 `unavailable: the editor boundary is not available`。在 Godot 编辑器里打开装了 add-on 的项目，它们才有对象可操作，见 [编辑器桥](editor-bridge.md)。
* 4 个输入工具 `input.action.send`、`input.key.send`、`input.mouse.send`、`input.sequence.send`：`unavailable: the runtime boundary is not available`。按名字顺序它们排在 `project.run.start` 之前，调用时还没有游戏会话在跑。
* 2 个调试工具 `debug.command`、`debug.session.get`：`unavailable: the lsp_dap boundary is not available`。它们只连托管编辑器会话启动收据里记下的端点。
* `editor.addon.update`：`conflict: the godot_mcp add-on is not installed; use editor.addon.install`。它排在 `editor.addon.remove` 之后，这时 add-on 刚被删掉。

托管会话的生命周期与就绪条件见 [托管会话](managed-sessions.md)。

## 1 个撞上执行超时的调用

`project.run` 不带参数会返回

```console
timeout: the operation exceeded its execution deadline; its effect is not yet proven
```

**这是对的，不是坏了。** `project.run` 是同步调用：不带参数就是「把这个游戏跑起来并等它结束」，而一个跑起来的游戏不会自己退出，于是这次调用只能一直等到 `limits.deadline_ms` 的上限（这次测量为 30000 毫秒）。注意消息的后半句：超时不宣称效果失败，只说效果还没被证明，进程有可能真的起来过。

所以要让游戏跑着、边跑边看，用托管会话 `project.run.start` / `project.run.status` / `project.run.logs` / `project.run.stop`，见 [托管会话](managed-sessions.md)。`project.run` 只适合一个自己会结束的场景。超时后那个进程和它的边界怎么收拾，见 [进程边界](process-boundaries.md)。

## 用 `health.get` 做诊断

`health.get` 是读者手里最强的无参诊断入口：实测响应有 26 个顶层键。在一台找不到 Godot 可执行文件的机器上实测到的值：

| 组 | 实测值 |
| --- | --- |
| readiness | serve `true`、read `true`、mutate `true`、editor `false`、runtime `false`、http `false` |
| codes | `godot_binary_absent`、`editor_not_attached`、`game_not_running`、`http_disabled` |
| components | `project`、`index` 为 `ready`；`godot`、`editor`、`runtime` 为 `unavailable`，各带一句补救说明 |
| journal | block_readable `true`、quarantined 合计 0、unresolved_survivors 合计 0、settings_reconciliation 为 `reconciled` |
| engine | presence 为 `absent`、binary 为 `null`、version 为 `null`、capabilities_probed 为 `false` |
| versions | contract_matched `true`、engine 为 `unknown`、plugin 的 state 为 `not_connected` |
| queue | mode 为 `accepting`、queued 0、running 0、concurrency_limit 8、queue_limit 64 |
| lease | ownership 为 `held_here` |

资源 `godot-mcp://resources/health` 返回同一套事实。

怎么读它：

* readiness 里 mutate 为 `true` 而 codes 里同时有 `godot_binary_absent`，这是对的。文件面的写入不需要 Godot。
* editor 与 runtime 的 `false` 不是缺功能：`editor` 一项说的是还没有 add-on 连上，`runtime` 一项说的是还没有托管游戏会话在跑，补救说明分别指向 `editor.addon.install` 与 `project.run.start`。
* engine 的 version 为 `null`、capabilities_probed 为 `false` 同样是刻意的：健康检查路径从不启动 Godot，因此它也报不出探测过的版本。
* queue 里的 concurrency_limit 为 8，正是本页开头那条限流错误的来源。

序列化后的 `health.get` 响应里没有绝对路径、没有形似凭据的字符串。按 `/config/`、`/tmp/`、`/home/`、`.gd"` 以及 secret、token、bearer、password 扫过：零命中。缺 supervisor 时的 `godot` 组件消息与 `project.import.run` 拒绝理由，按 `/config/`、`/tmp/`、`/home/`、`/Users/`、`/var/`、`/opt/`、`/usr/` 扫过：同样零命中。

## 复现验证

### 工作区门禁

```sh
cargo +1.96.0 fmt --all --check
cargo +1.96.0 clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +1.96.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.96.0 test --workspace --all-features --locked
cargo +1.96.0 build --workspace --all-features --locked
cargo +1.96.0 deny check bans licenses sources
```

预期：fmt、两遍 clippy、test、build 退出 0；deny 打印 `bans ok, licenses ok, sources ok`。suite、通过和忽略的精确计数属于候选快照，不在这份持续维护的复现指南中声明。

委派 cgroup 的那一组测试只有在主机能起一个委派的 user scope 时才会真正执行，否则跳过并打印 `the delegated-cgroup check did not run`。要强制执行、让缺委派变成失败，在 `systemd --user` 可用的 Linux 主机上设 `QA_REQUIRE_DELEGATED_CGROUP=1` 再跑 `cargo test`。

### 真实客户端冒烟与端到端

这些脚本用官方 MCP Python SDK 驱动构建出的 `godot-mcp`（`godot-mcp-supervisor` 要在它旁边），对一个确切的 Godot 构建跑：

```sh
python3 -I -B scripts/real_client/m5_smoke.py --binary target/debug/godot-mcp --godot <Godot editor>
python3 -I -B scripts/real_client/e2e_complete.py --binary target/debug/godot-mcp \
  --godot <Godot editor> --gut <与该 Godot 小版本匹配的 GUT 归档>
```

`m0_smoke.py` 到 `m6_smoke.py` 各对应一个里程碑。窗口化的游戏需要显示：Linux 上设 `DISPLAY` 指向一个 Xvfb 即可；macOS 上服务器要跑在已登录用户的图形会话（Aqua）里；Windows 上要跑在已登录用户的桌面会话里，服务和容器会话都不行。导出需要该构建的确切模板，放在 Godot 自己找的位置（Linux 的 `XDG_DATA_HOME`、macOS 的 `~/Library/Application Support/Godot`、Windows 的 `%APPDATA%\Godot`）；`m6_smoke.py` 带 `--require-templates` 时，没有模板直接判失败，而不是只验证拒绝分支。设 `GODOT_MCP_SMOKE_BOUNDARY=cgroup` 时，服务器在一个委派的 user scope 里启动，每次启动都进入它自己的 cgroup。

要对本机平台的每个确切版本一次跑完全部检查，用 `platform_matrix.py`：

```sh
python3 -I -B scripts/real_client/platform_matrix.py --binary target/debug/godot-mcp \
  --out <结果目录>
```

它按锁文件取本机平台的编辑器构建，缓存里摘要对得上就复用，否则下载后校验 SHA-256。只要选了导出类检查，就按锁文件把该构建的导出模板装进 `--data-home`（缺省为 `<work>/data-home`），并让 M6 带 `--require-templates` 运行，所以导出检查不会只靠拒绝分支通过；端到端所需的 GUT 按脚本里钉住的摘要获取。每项检查是一个独立进程，结果写进 `<结果目录>/summary.json`，有任何一项没通过就以 1 退出；起不来的检查记为失败，不记为跳过。

`e2e_complete.py` 是最终验收的那条路径：空 workspace、创建项目、写脚本与设置与资产、创建场景并挂脚本、导入与引擎检查、启动游戏、查看运行状态、发送输入、截图、停止、测试、导出，然后重启服务器并核对最新的一致修订。

### 文档门禁

```sh
cargo +1.96.0 run --locked -q -p godot-mcp-cli --features test-hooks --bin docs-check -- --workspace-root .
```

预期输出 `docs-check: clean`。规则清单与写法约束见 [文档规则](../contributor/documentation.md)。

### 方案的逐 todo 门禁

```sh
RUSTUP_TOOLCHAIN=1.96.0 make qa-task TASK=N
```

这一组属于 0.1 方案，在本源码树上未测量。

### 包自检

不需要 Godot：

```sh
godot-mcp self-check
```

按它自己的帮助文本：每个 payload 文件的摘要与大小都对得上、notices / SBOM / 示例配置齐备、没有 payload 文件携带凭据标记、`bin/` 里恰好只有声明的可执行文件时，退出 0；上述任一条不满足时退出 1；不是从解包目录里运行时退出 2。在本源码树上未测量，校验语义细节见 [打包](packaging.md)。

### 测试端口固定装置

`crates/godot-mcp-server/tests/evidence_redaction.rs` 里的 `DeterministicFixture::new` 传入 41000，这个值只是端口游标的起点，不是要求必须拿到的端口号。`DeterministicPorts::allocate()` 从游标处开始探测 loopback，返回第一个能绑上的端口：已被其他进程占用的端口（包括上一个套接字还停在 `TIME_WAIT` 里的那种，绑定时都表现为 `AddrInUse`）会被跳过，游标随后严格向前推进，所以同一个固定装置发出的端口单调递增且不重复。

扫描是有界的，一次 `allocate()` 最多探测 `DeterministicPorts::SCAN_WINDOW` 也就是 64 个连续端口；这 64 个全被占用时它返回 `AddrNotAvailable`，错误信息里带上探测过的区间，而不是继续往整个端口空间走下去。

因此这个套件不再预期出现 `AddrInUse` 失败。真出现了就是真实失败，请照实排查，不要重跑掩盖。

## Godot 与平台

版本锁文件位于

```console
tests/fixtures/godot-releases.lock.json
```

其中 `minimum_supported_version` 为 `4.4.0`；钉住的版本为 `4.4.1`、`4.5.1`、`4.6.1`、`4.7.1`，每个各 4 个 artifact；`required_platforms` 为 `linux-x86_64`、`macos-universal`、`windows-x86_64`、`all-platforms`。矩阵本身见 [兼容性矩阵](version-matrix.md)。

真正执行过的范围（四个确切版本上 M0 到 M6 的真实客户端冒烟加完整的端到端，每个版本都装了它自己的导出模板；驱动的是本源码树为该平台构建的发布归档里的可执行文件）：

* **Linux x86_64**：在弱定位器、委派 cgroup 和参考命名空间适配器三种边界下都跑过并通过（适配器下是不受信项目），工作区测试也在这里。
* **macOS（Apple silicon，macOS 26.2）**：工作区测试通过；矩阵在已登录用户的图形会话（Aqua）里经弱定位器全部通过，包括窗口会话、输入、截图和停在断点的调试运行。后台会话（例如 CI 构建所在的会话）没有窗口服务器，窗口会话在那里被拒绝。
* **Windows（Windows Server 2022）**：工作区测试在容器里通过；矩阵在已登录用户的桌面会话里经 Job Object 全部通过，Godot 通过 ANGLE（Direct3D 11）渲染。Server Core 容器缺 `AVRT.dll`，Godot 在那里根本起不来。

每个平台跑过什么、在哪种边界下跑的，见 [进程边界](process-boundaries.md#what-has-been-executed-where)。

发布目标三个：`x86_64-unknown-linux-musl`、`aarch64-apple-darwin`、`x86_64-pc-windows-msvc`。各目标实际构建到什么程度，见 [打包](packaging.md) 的平台覆盖表，本页不复述。

## 已知缺口

不打折扣地列出来：

* 窗口化会话需要真实的桌面会话：Linux 上要有 `DISPLAY` 或 `WAYLAND_DISPLAY`，macOS 上要在已登录用户的 Aqua 会话里，Windows 上本进程的 window station 要是已登录会话的可见那个。否则 `display` 为 `auto` 时落到 headless，要求 `window` 时被拒绝。
* Windows Server Core 容器起不了 Godot（缺 `AVRT.dll`），在那里只能跑服务器自己的测试。
* 导出支持 Linux、Windows Desktop 和 macOS 三种预设，三种主机都能导出；冒烟运行只运行本机能运行的那一种：Linux 上的 x86_64 Linux 构建、Windows 上的 x86_64 Windows 构建、Mac 上从 zip 解包的 macOS app。macOS app 里的符号链接不解包，含符号链接的导出照常注册为产物，但不做冒烟运行。
* Windows 上 Job Object 是 kill-on-close：服务器被杀时会话一起结束，游戏的探针来不及释放它按住的输入；下一台服务器把这些会话记为 `reconciled`。
* `godot-mcp transaction repair` 只能为弱定位器下的 `process_launch` 重新推导证明；委派 cgroup、容器适配器和 Windows Job Object 下被隔离的启动由下一台服务器启动时经边界本身对账，见 [恢复](recovery.md)。
* `project.test` 只运行项目已经提交的 GUT 或 gdUnit4，不安装测试框架；GUT 的版本要与 Godot 的小版本匹配。没有框架或同时存在两套框架时返回 `unsupported`，零退出但没有可解析汇总时返回 `inconclusive`。
* `script.validate` 是静态分析而非引擎背书，响应自带 `engine_validation_not_performed`；需要真实编译检查时使用 `script.check`。
* 引擎版本与能力探测按设计不在健康路径上报：健康路径不启动 Godot，所以 version 为 `null`、capabilities_probed 为 `false`。
* stdio 通道上 HTTP 就绪状态是 disabled，codes 里对应 `http_disabled`。需要第二个本地进程接同一个服务器时才考虑 [HTTP: `trusted_local`](http-trusted-local.md)。
* 可选的 gdext 原生后端不存在：ADR 0010 的 spike 没有满足激活门槛，见 [ADR 0010](../contributor/adr/0010-complete-development-bridge-strategy.md)。

## 参见

* [快速开始](quickstart-stdio.md) — 首次 stdio 跑通
* [MCP 客户端](mcp-clients.md) — 客户端接线
* [配置](configuration.md) — 每个配置键与不变量
* [Profiles 与结果](profiles-and-outcomes.md) — 成员资格、可用性、不确定性
* [托管会话](managed-sessions.md) — 游戏与编辑器会话、输入、截图
* [导出](exports.md) — 确切模板、产物与取消
* [进程边界](process-boundaries.md) — 每次启动落在哪种边界里
* [故障排查](troubleshooting.md) — 症状优先的诊断
* [打包](packaging.md) — 归档内容与校验
* [兼容性矩阵](version-matrix.md) — 锁定的确切版本
* [工具与资源清单](../protocol/tool-inventory.md) — 生成的完整清单
* [结果分类](../protocol/error-taxonomy.md) — 生成的错误词表
