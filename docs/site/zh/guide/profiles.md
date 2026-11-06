---
description: 决定服务器能对项目做什么的四种 profile，以及每种答复的含义。
---

# Profile 与答复

profile 是你对「服务器能对项目做什么」的一个明确选择。它不是一个方便用的开关，默认值就是安全的选择。

## 四个 profile

| Profile | 增加的能力 | 适用场景 |
| --- | --- | --- |
| `core` | 只读的发现与分析 | 你想了解项目，什么都不改 |
| `editor` | 编写项目——文件、GDScript、场景、资源、项目设置和资产——依赖引擎的导入，以及编辑器插件 | 你要修改项目 |
| `runtime` | 运行、测试、导出项目并用引擎检查它，托管的游戏与编辑器会话，在已连接的编辑器里运行游戏，以及编写类工具里的读取 | 你要运行并观察项目 |
| `full` | 三者之和 | 以上都要 |

`full` 恰好就是 `core`、`editor` 与 `runtime` 的合集。每个工具以及列出它的 profile，见生成的[工具清单](../../../protocol/tool-inventory.md)。

有三个工具会执行 Agent 自己写的 GDScript：`script.run` 在无头 Godot 里运行，`editor.script.run` 在打开的编辑器里运行，`runtime.script.run` 在运行中的游戏里运行。包含它们的 profile 都会列出它们，但在你用 `--allow-scripts` 启动服务器之前，它们一律返回 `permission_denied`。开关打开后，脚本不受任何限制：它以你的权限运行，做过的事不会被撤销。有合适的类型化工具时，仍然优先用类型化工具：它们写入前会检查改动，编辑器也能撤销。一次运行会答复什么、什么能结束它，见[Profile 与工具结果](../../../operator/profiles-and-outcomes.md#script-execution)。

## 列出不等于可用

一个工具出现在 `tools/list` 里，是因为 profile 列出了它。它需要的东西此刻在不在，是每次调用时单独回答的：没有解析到 Godot 时，`project.test` 答复 `unavailable` 并说明原因；没有游戏在运行时，`runtime.tree.get` 答复 `unavailable` 并指向 `project.run.start`。工具不会从发现结果里消失，也绝不假装已经执行。客户端因此可以信任工具列表，再从答复里了解这台机器的情况。

## 答复的含义

答复要么是结果，要么是带十种错误码之一的结构化错误：

| 错误码 | 含义 |
| --- | --- |
| `invalid_input` | 参数不符合工具的 schema 或规则 |
| `not_found` | 文件、节点、会话或预设不存在 |
| `conflict` | 项目在请求进行时被改动了，或者文件被打开的编辑器占着 |
| `permission_denied` | 路径、调用方或 profile 不被允许 |
| `unsupported` | 项目用到了这个版本处理不了的东西，例如二进制 `.scn` |
| `unavailable` | 工具需要的东西此刻不在；答复说明缺什么以及怎样补上 |
| `timeout` | 工作完成之前截止时间已到 |
| `cancelled` | 请求开始之前就被客户端取消了 |
| `uncertain` | 效果可能发生也可能没有发生，或者分析无法确定 |
| `internal` | 服务器自身的故障 |

`uncertain` 是诚实的答复，而不是更委婉的 `internal`：静态分析无法解析一个动态调用时，会说明这一点并列出所有候选；一次写入的结果无法证实时，会在处理任何其他请求之前由日志核清。重试一次 `uncertain` 的写入之前，先重新读一遍项目。完整的词汇表和每个限制码见[答复分类参考](../../../protocol/error-taxonomy.md)。

## 重复调用

每个会改动东西的调用都带 `idempotencyId`。用相同的 id 和相同的参数重复调用，返回的是记录下来的结果，而不会再执行一次，所以客户端在丢失答复后可以放心重试，不会把改动做两遍。同一个 id 配上不同的参数会以 `conflict` 被拒绝。
