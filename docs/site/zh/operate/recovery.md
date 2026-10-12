# 恢复

对项目的一次改动要么发生了，要么没有；崩溃之后，服务器必须弄清楚是哪一种。每次启动时，它都会在提供任何服务之前完成这件事。

## 三种结果，没有第四种

| 状态 | 含义 |
| --- | --- |
| `committed` | 已证明改动生效 |
| `compensated` | 改动已从一个被证实的先前状态撤回，项目恢复原样 |
| `quarantined` | 两者都无法证明；在事务被解决之前阻止所有写入 |

不存在「就当它成功了」：服务器无法证明的状态会被隔离，这是一个醒目的、会阻塞的状态，而不是一条很容易被忽略的警告。

## 启动时

服务器获取项目的租约，打开日志，核清前一个进程遗留的每个事务，然后在 stderr 上每种结果打印一行，再打印写入闸门的状态：

```console
godot-mcp serve: startup reconciliation compensated=1
godot-mcp serve: startup reconciliation committed=2
godot-mcp serve: write gate open
```

有事务被隔离时，闸门保持关闭：读取照常工作，写入会被拒绝并给出原因，`health.get` 也会报告。前一个服务器遗留的 Godot 进程——游戏、编辑器、导出——会在同一时间被结束并记录。

## 解决隔离

解决隔离是一个有意的本地操作，只能在这台机器上通过命令完成，任何工具或传输方式都无法触及：

```sh
godot-mcp transaction repair \
  --project /path/to/my-game \
  --transaction-id 00000000-0000-0000-0000-000000000301 \
  --operation reconcile \
  --proof-file ./proof.json \
  --dry-run
```

`reconcile` 报告证明所允许的状态转换，什么都不记录；`compensate`、`terminate` 和 `acknowledge-safe-state` 各记录一次状态转换，前提是服务器自己重新推导出证明并且证明成立。先用 `reconcile`，或者在你打算执行的操作上加 `--dry-run`。每种证明和拒绝情况见[恢复参考](../../../operator/recovery.md)。

## 一个项目一个服务器

一个项目同一时间只由一个服务器持有，依靠的是项目自己目录上的租约。同一项目的第二个服务器会拒绝启动，而不是共用日志；前一个服务器退出后再启动的服务器会接管租约，并核清前一个留下的一切。
