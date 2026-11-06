---
layout: home
markdownStyles: false
title: godot-mcp：面向 Godot 项目的 MCP 服务器
titleTemplate: false
description: godot-mcp 是一个静态 Rust 二进制，把 Godot 项目交给 MCP 客户端：不需要引擎的分析、写入日志的改动，以及它自己运行、操控、截取和调试的游戏与编辑器。

hero:
  name: godot-mcp
  text: 面向 Godot 项目的 MCP 服务器
  tagline: 一个跑在项目旁边的静态二进制。你的 Agent 不用引擎就能读懂 GDScript、场景和资源，通过写入日志的事务修改它们，还能运行、操控、截取和调试游戏与编辑器。
  actions:
    - theme: brand
      text: 安装
      link: /zh/guide/install
    - theme: alt
      text: 快速开始
      link: /zh/guide/quick-start
    - theme: alt
      text: GitHub
      link: https://github.com/sunerpy/godot-mcp

home:
  facts:
    - term: 运行方式
      text: 一个本地进程，默认走 stdio
    - term: Godot
      text: 4.4.0 或更新，用你指定的版本
    - term: 构成
      text: Rust 服务器，外加可选的编辑器插件

  stages:
    steps:
      - Agent 发给运行中游戏的调用
      - godot-mcp 答复的截图
    exchange:
      label: Agent 通过 godot-mcp 操作演示游戏。input.sequence.send 按住 ui_right 2000 毫秒，答复两个步骤都已执行。runtime.wait 等到 score 等于 2，答复条件已满足。runtime.screenshot.capture 答复一个 960 × 540、529325 字节的产物。
      transcript:
        - kind: call
          text: input.sequence.send
        - kind: argument
          text: "action ui_right  press  holdMs 2000"
        - kind: argument
          text: "wait  ms 2000"
        - kind: answer
          text: "steps_run  2"
        - kind: call
          text: runtime.wait
        - kind: argument
          text: "property score  ==  2"
        - kind: answer
          text: "satisfied  true"
        - kind: call
          text: runtime.screenshot.capture
        - kind: answer
          text: "artifact  960 × 540  529325 bytes"
    screen:
      src: /screens/game.webp
      width: 960
      height: 540
      alt: 演示游戏。一个圆形的青色小机器人站在草地上，紧挨着一个木箱；木箱上方和最左侧各浮着一枚硬币，右上角的计数显示 2。
    caption: 画面来自 examples/site-demo 里的演示项目，运行在 Godot 4.7.1 上。这张图就是这次答复里的 PNG，为网页转成了 WebP。

  built:
    title: Rust 服务器，不只是编辑器插件
    intro: 客户端只和一个 Rust 二进制通信。GDScript 只在任务需要引擎的地方于 Godot 内部运行，所以不打开编辑器、不安装 Godot，也能分析项目、修改文件。
    client:
      name: 你的 MCP 客户端
      detail: 把 godot-mcp 当作一个进程启动，通过 stdio 与它通信；打开 HTTP 后，也可以在本机或开发网络里通过 HTTP 通信。
    server:
      name: godot-mcp
      detail: 跑在项目旁边的一个静态 Rust 二进制，不需要安装任何语言运行时。
      parts:
        - 提供 MCP 服务
        - 不用引擎就能读懂 GDScript、场景和资源
        - 每次改动执行前先写入日志
        - 启动并限定它用到的每个 Godot 进程
    targets:
      - name: 项目文件
        detail: GDScript、场景、资源和设置，在没装 Godot 的机器上也能读取和修改。
        status: no-engine
        link: /zh/guide/analysis
      - name: 无界面辅助脚本
        detail: 一个按 SHA-256 固定的 GDScript 文件，服务器修改的每个场景和资源都由 Godot 借助它写出。
        status: engine
        link: /zh/guide/editing#场景与资源
      - name: 运行时探针
        detail: 服务器启动每个游戏时通过命令行交给它的 GDScript。不安装自动加载，也不改任何设置。
        status: engine
        link: /zh/guide/sessions#运行游戏
      - name: godot_mcp 插件
        detail: 装在你正打开的编辑器里的可选插件。它的每次改动都是编辑器里一次可撤销的操作。
        status: engine
        link: /zh/guide/editor
    note: 只做成编辑器插件的工具，只能在编辑器运行时工作，也只能处理编辑器里已有的内容。godot-mcp 在项目旁边工作，需要时再由它的插件把你的编辑器接进来。

  capabilities:
    title: Agent 能用它做什么
    intro: 下面每一项都是 Agent 可以调用的一组工具。标着「无需引擎」的只读项目文件，在没装 Godot 的机器上也能回答；其余的使用你配置的 Godot。
    items:
      - icon: scan-search
        title: 读懂项目
        wide: true
        status: no-engine
        body: 场景树，GDScript 的符号、引用、调用方与影响面，以及依赖图，都从文件中读出。每条关系都标明有多确定。
        evidence:
          kind: code
          text: |-
            Player       CharacterBody2D  player.gd
            Coins/CoinA.collected → ._on_coin_collected  exact
        link: /zh/guide/analysis
      - icon: file-pen-line
        title: 安全地修改
        status: no-engine
        body: 每次写入都对照被替换内容的 SHA-256，并在执行前写入日志。过期的修改会被拒绝，中途崩溃的事务在处理下一个请求之前就会被理清。
        evidence:
          kind: code
          text: "file.patch player.gd  →  conflict"
        link: /zh/guide/editing
      - icon: cpu
        title: 检查、导入与测试
        status: engine
        body: 用你配置的 Godot 检查脚本、运行导入流程，并运行项目自带的 GUT 或 gdUnit4 测试，给出确切计数。
        evidence:
          kind: code
          text: "script.check player.gd  →  passed"
        link: /zh/guide/engine
      - icon: package
        title: 导出
        status: engine
        body: 只用你这个 Godot 版本的导出模板导出预设，作为可以跟踪和取消的托管导出。产物放在服务器自己的存储里，不会写进项目。
        evidence:
          kind: chips
          items: [export.presets.list, project.export, project.export.status, project.export.cancel]
        link: /zh/guide/engine#导出
      - icon: gamepad-2
        title: 运行游戏
        status: engine
        body: 运行主场景或任意场景，带窗口或无窗口。服务器通过命令行交给游戏一个探针，项目里的东西都不会改动。
        evidence:
          kind: code
          text: "project.run.start  →  running, probe connected"
        link: /zh/guide/sessions#运行游戏
      - icon: mouse-pointer-click
        title: 试玩并检查
        status: engine
        body: 按键、鼠标和输入动作结束后一定会被松开；等待和断言写在同一个场景里，报告列出每一步的结果。
        evidence:
          kind: code
          text: "test.scenario.run  →  passed, 5 of 5 steps"
        link: /zh/guide/sessions#测试游戏
      - icon: app-window
        title: 在编辑器里工作
        status: engine
        body: 借助 `godot_mcp` 插件，修改并保存编辑器里的场景、画图块、给动画打关键帧、截取窗口、读取日志。每次修改都是一次可撤销的操作。
        evidence:
          kind: chips
          items: [editor.scene.edit, editor.undo, editor.scene.save, editor.screenshot.capture]
        link: /zh/guide/editor
      - icon: bug
        title: 调试
        status: engine
        body: 无界面的编辑器会话承载 Godot 的语言服务器和调试适配器，用来设断点、看调用栈、作用域和变量，以及取得脚本的诊断。
        evidence:
          kind: chips
          items: [editor.session.start, debug.command, debug.session.get]
        link: /zh/guide/sessions#调试

  showcases:
    - title: 在你的编辑器里修改，随时可以撤销
      intro: "`godot_mcp` 插件把你正打开的 Godot 编辑器接到服务器上。Agent 对已打开场景的改动进入编辑器自己的撤销历史，插件的面板列出编辑器答复过的请求。"
      screen:
        src: /screens/editor-zh-light.webp
        dark: /screens/editor-zh-dark.webp
        width: 1600
        height: 1000
        alt: Godot 4.7.1 编辑器，2D 视图里打开着演示项目的主场景。Player 节点处于选中状态，检查器显示它的脚本里的 Speed 和 Jump Speed。godot_mcp 停靠栏显示 godot-mcp 已连接，底部的 Godot MCP 面板按时间倒序列出 13 个请求，从 editor.scene.save 到 editor.scene.open。
      caption: Godot 4.7.1 与 godot_mcp 插件，打开的是演示项目。
      points:
        - title: 一次可撤销的操作
          body: "`editor.scene.edit` 把一批改动作为场景撤销历史里的一项应用，`editor.undo` 可以把它撤回。"
        - title: 不保存，除非你要求
          body: 改动一直留在编辑器里，直到调用 `editor.scene.save`；编辑器占着的文件，无界面写入会被拒绝并答复 `conflict`。
        - title: 面板上的请求记录
          body: 面板列出编辑器最近答复过的 500 个请求，每个都有方法、涉及的对象和结果，但不显示请求参数。
        - title: 由你掌控的开关
          body: 打开 **Pause agent edits** 后，编辑器拒绝服务器要求的每一次改动，读取仍然可用。
      guide:
        text: 编辑器插件
        link: /zh/guide/editor
    - title: 它能试玩游戏，也能检查结果
      intro: 服务器带着自己的探针启动游戏，向它发送输入，再读回它的状态。`test.scenario.run` 在一次调用里执行输入、等待和断言，并答复一份报告。
      screen:
        src: /screens/game-scenario.webp
        width: 960
        height: 540
        alt: 场景跑完后的演示游戏。机器人回到了最左侧，那里的硬币已被收走，木箱上方还浮着一枚硬币，计数显示 3。
      caption: 这是场景最后一步截下的画面，为网页转成了 WebP。
      points:
        - title: 输入一定会被松开
          body: 每个按键、按钮和输入动作，都会在保持时间结束、序列结束或会话停止时被松开。
        - title: 按游戏自身的状态等待
          body: 可以一直等到某个节点出现或消失、某个属性满足比较条件、某个信号被发出，或某个控件显示指定文字。
        - title: 是报告，而不是日志
          body: 每一步要么通过，要么失败并给出期望值和实际值，要么说明它为什么无法执行。
      exchange:
        label: test.scenario.run 为五个步骤答复的报告。assert_property、向左走的输入、等到 score 等于 3 的等待、对 HUD/Count 的 assert_ui_text 和截图全部通过，结论为 passed，5 个步骤全部通过。
        transcript:
          - kind: call
            text: test.scenario.run
          - kind: argument
            text: "steps: assert_property, input, wait,"
          - kind: argument
            text: "       assert_ui_text, screenshot"
          - kind: answer
            text: "two-coins   assert_property  passed"
          - kind: answer
            text: "walk-back   input            passed"
          - kind: answer
            text: "third-coin  wait             passed"
          - kind: answer
            text: "hud         assert_ui_text   passed"
          - kind: answer
            text: "after       screenshot       passed"
          - kind: answer
            text: "verdict  passed   passed 5  total 5"
      guide:
        text: 测试游戏
        link: /zh/guide/sessions#测试游戏

  clients:
    title: 任何 MCP 客户端，同一条命令
    intro: godot-mcp 是一个 stdio MCP 服务器。每个会启动进程的客户端运行的都是同一条命令，大多数一行就能注册。
    items:
      - name: Claude Code
        command: claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
        link: /zh/guide/clients#claude-code
      - name: Codex
        mark: codex
        command: codex mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
        link: /zh/guide/clients#codex
      - name: OpenCode
        mark: opencode
        note: 项目里或全局配置里 `opencode.json` 的一条 `mcp`。
        link: /zh/guide/clients#opencode
      - name: Pi Agent
        mark: pi
        command: pi mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game --profile full
        link: /zh/guide/clients#pi-agent
      - name: Cursor
        mark: cursor
        note: 同一条命令，写成 `mcpServers` 这段 JSON 里的一项。
        link: /zh/guide/clients#用-json-配置的客户端
    more: Claude Desktop 以及其他任何通过 stdio 启动进程的客户端，运行的也是这条命令，大多数写在一段 JSON 里。
    guide:
      text: MCP 客户端
      link: /zh/guide/clients

  profiles:
    columns: [Profile, 增加的能力, 工具举例]
    rows:
      - cells: [core, 只读，什么都不改, project.info.get scene.tree.get gdscript.references.get]
      - cells: [editor, 写入与编辑器插件, file.patch scene.batch.apply editor.addon.install]
      - cells: [runtime, 运行并观察游戏, project.run.start input.sequence.send runtime.screenshot.capture]
      - cells: [full, 以上全部, core editor runtime]
    code: [0, 2]
    caption: 不在 profile 里的工具不会被列出。已列出的工具如果它需要的引擎、游戏或编辑器不在，就答复 unavailable 并说明缺的是哪一个。

  engine:
    title: 引擎，版本精确
    intro: 项目支持的每个 Godot 版本都是锁文件里的一个确切补丁版本，带官方下载地址和摘要。每次发布都为下面每个平台提供一个归档，并在该平台上针对最新版本冒烟测试。
    versions:
      - version: 4.7.1
        detail: 编辑器检查、导出模板、显示检查和打包冒烟，覆盖 Linux、macOS 和 Windows
      - version: 4.6.1
        detail: 编辑器检查和导出模板，在 Linux 上
      - version: 4.5.1
        detail: 编辑器检查和导出模板，在 Linux 上
      - version: 4.4.1
        detail: 编辑器检查和导出模板，在 Linux 上
    platforms:
      - name: Linux x86_64
        archive: x86_64-unknown-linux-musl
        detail: 静态链接。游戏和编辑器在 X11 或 Wayland 下带窗口运行，或无窗口运行，由进程组、委派 cgroup 或容器适配器限定。
      - name: macOS Apple 芯片
        archive: aarch64-apple-darwin
        detail: 游戏和编辑器在已登录用户的会话里带窗口运行，或无窗口运行，由进程组或容器适配器限定。
      - name: Windows x86_64
        archive: x86_64-pc-windows-msvc
        detail: 静态链接 C 运行时。游戏和编辑器在已登录的桌面会话里带窗口运行，或无窗口运行，由 Job Object 限定。
    install: 安装脚本会用发布里的 SHA256SUMS.txt 核对它下载的归档。
    guide:
      text: 安装指南
      link: /zh/guide/install
    note: 最低支持版本是 4.4.0。Intel 版 macOS、Linux arm64 和 Windows ARM64 没有现成归档，请从源码构建。

  privacy:
    title: 项目数据去了哪里
    intro: godot-mcp 只在你的机器上工作。除了在你配置 OIDC 时去取身份提供方的签名密钥，它自己不发起任何网络请求。
    sendsLabel: 去向
    modes:
      - name: 工具调用
        sends: 你的 MCP 客户端
        detail: 走 stdio 时，客户端与服务器通过两条管道通信。答复里只有这次调用要求的项目数据，不会发往别处。
      - name: HTTP（需要你打开）
        sends: 能连到监听器的客户端
        detail: "`trusted_local` 只监听回环地址，只接受带本次启动密钥的请求；`proxied_oidc` 只接受经过认证的代理转发、并携带某一个用户令牌的请求。`open` 监听你给出的地址，用于机器都属于你的开发网络，不要凭据或只要一个令牌。"
      - name: 引擎
        sends: 你的机器
        detail: Godot 用你配置的版本在本地运行，处在服务器管理的进程边界之内。godot-mcp 从不下载引擎或导出模板。
      - name: 日志与截取
        sends: 本地文件
        detail: 诊断信息写到 stderr 或滚动日志文件，密钥和源码会被遮蔽。截图、录屏和导出结果作为产物留在磁盘上，直到保留期结束。

  faq:
    title: 常见问题
    items:
      - q: 需要打开 Godot 编辑器吗？
        a: 不需要。分析和文件修改直接读写项目文件，不装 Godot 也能用。检查、导入、测试、导出和游戏需要一个 Godot 可执行文件，由服务器自己启动。只有编辑器工具需要你的编辑器，通过 `godot_mcp` 插件连接。
        guide:
          text: 它是怎样组织的
          link: /zh/guide/introduction#它是怎样组织的
      - q: 它能处理 Godot 项目的哪些部分？
        a: 动画、AnimationTree、物理、3D、粒子、导航、音频、着色器、主题、TileMap 和运行中的游戏，每一项都有 Agent 可以调用的工具。`scene.recipe.apply` 按名字把一种常见设置加进场景，例如物理体或烟雾粒子；每个配方同时也是一个 Agent 能找到的 MCP prompt。
        guide:
          text: 覆盖范围
          link: /zh/guide/capabilities
      - q: 它是编辑器插件吗？
        a: 包含一个，就是可选的 `godot_mcp` 编辑器插件，用来把你正打开的编辑器接进来。服务器本身是项目旁边的一个 Rust 二进制，所以不打开编辑器也能读取、修改、运行、测试和导出项目。
        guide:
          text: Rust 服务器，不只是编辑器插件
          link: /zh/guide/introduction#rust-服务器不只是编辑器插件
      - q: 支持哪些 Godot 版本？
        a: Godot 4.4.0 或更新版本。CI 检查 4.4.1、4.5.1、4.6.1 和 4.7.1 这几个确切版本；需要更新版本的工具会说明，例如 `editor.log.get` 需要 4.5.1。
        guide:
          text: Godot 版本
          link: /zh/reference/godot-versions
      - q: 会不会改坏项目？
        a: 每次写入都对照被替换内容的 SHA-256，并在执行前写入日志，所以过期的修改会被拒绝并答复 `conflict`，中途崩溃的事务在处理下一个请求之前就会被理清。它不修改二进制的 `.scn` 和 `.res`，请先把它们另存为文本格式。
        guide:
          text: 修改项目
          link: /zh/guide/editing
      - q: 会运行 Agent 写的 GDScript 吗？
        a: 只有你用 `--allow-scripts` 启动服务器之后才会；类型化工具只调用它们自己那一组固定的引擎方法。加载项目会运行项目自己的代码，所以服务器能限定它启动的进程，却不能让一个不可信的项目变得可以放心打开。
        guide:
          text: 威胁模型
          link: /zh/reference/threat-model
      - q: 另一台机器上的 Agent 能用它吗？
        a: 能，前提是在机器都属于你的开发网络里：`godot-mcp serve --http 0.0.0.0:8765` 通过 HTTP 为网络里的其他机器提供 MCP，不要凭据，或者只要一个令牌。在这样的网络之外，请让 HTTP 只监听回环地址，或在前面放一个经过认证的反向代理。
        guide:
          text: HTTP
          link: /zh/operate/http
      - q: 会把项目发到别处吗？
        a: 不会。除了在你配置 OIDC 时去取身份提供方的签名密钥，它自己不发起任何网络请求，也从不下载 Godot 或导出模板。一个服务器只为一个用户服务一个项目，不是共享服务。
        guide:
          text: HTTP
          link: /zh/operate/http

  closer:
    title: 安装 godot-mcp
    body: 一个二进制，支持 Linux、macOS 和 Windows，采用 MIT 许可证。问题和功能建议请提交到 [GitHub Issues](https://github.com/sunerpy/godot-mcp/issues)。
    actions:
      - text: 安装
        link: /zh/guide/install
        theme: brand
        icon: download
      - text: 快速开始
        link: /zh/guide/quick-start
        theme: alt
        icon: arrow-right
---

<HomeBuilt />

<HomeCapabilities />

<HomeShowcases />

<HomeClients />

<SplitBlock proof="profiles">

## 决定它能做什么

profile 是服务器能对项目做的一切的上限。`core` 只读，什么都不改。`editor` 增加写入，每次写入都写进日志，并核对它替换的内容。`runtime` 增加运行和观察项目。

执行 Agent 自己写的 GDScript 在每个 profile 里都默认关闭，只有用 `--allow-scripts` 启动服务器才会打开。

[Profile 与答复](guide/profiles.md) · [工具与资源](../../protocol/tool-inventory.md)

</SplitBlock>

<HomeEngine>

::: code-group

```sh [Linux 与 macOS]
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell [Windows]
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

```sh [源码]
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp && make dist
```

:::

</HomeEngine>

<HomePrivacy />

<HomeFaq />

<HomeCloser />
