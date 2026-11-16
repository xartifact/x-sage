# x-sage

面向大模型的概念知识库。概念以 Markdown 为唯一真源，服务端读取时计算传递闭包与对称边，
客户端（CLI、浏览器）通过 REST 使用它。

生产：<https://x-sage.ts.xartifact.dev>

该域名解析到 Tailscale 私网地址（`100.80.110.125`），**只在 tailnet 内可达**；离开 tailnet
会连接失败，此时 CLI 报 `cannot reach`，不是服务异常。公网入口尚未建立。

## 形态

标准 C/S：服务端是唯一读得到知识库的组件。CLI 与可视化都是它的 REST 客户端，
**客户机不携带任何 KB 文件**，也没有本地模式。

```
apps/server    HTTP 适配器（鉴权、压缩、上限、静态托管）
apps/cli       REST 客户端：一次命令一次请求
apps/viz       浏览器端知识图谱
packages/service  用例与路由（不依赖 Hono）
packages/core     解析、图算法、schema、提案与合并
kb/<id>/          知识库数据
```

## 安装 CLI 与 Skill（联网，无需克隆）

```bash
curl -fsSL https://raw.githubusercontent.com/xartifact/x-sage/main/scripts/install-cli.sh | bash
```

只依赖 [Bun](https://bun.sh) 与 `curl`。安装器会先把分支/tag 解析成一个明确的 40 位 commit，
再按该 commit 下载 CLI 与 Skill，所以一次安装内部不会混版。

```
~/.local/bin/x-sage                        wrapper
~/.local/lib/x-sage/cli/src/*.ts           CLI 源码（index / config / build-info）
~/.local/lib/x-sage/skills/x-sage-kb/      Skill
~/.local/lib/x-sage/build-manifest.json    `x-sage --version` 读取的版本身份
~/.local/lib/x-sage.previous/              上一个安装
~/.agents/skills/x-sage-kb/SKILL.md        安装后的 Skill
```

`@x-sage/service` 在客户端只有 `import type`，编译期即被擦除，所以装几个源文件就够，
不需要克隆仓库或安装 workspace。

| 环境变量 | 作用 |
|---|---|
| `X_SAGE_REF` | 分支、tag 或完整 commit，默认 `main`；要可复现就传 commit |
| `X_SAGE_PREFIX` | 安装前缀，默认 `$HOME/.local` |
| `X_SAGE_BASE` | 源地址前缀，默认 GitHub raw；内网镜像用 |
| `X_SAGE_API` | 解析 commit 用的 API 前缀，默认 GitHub API |
| `X_SAGE_SKILLS_DIR` | Skill 目录，默认 `$HOME/.agents/skills` |

固定到某个 commit（可复现安装）。变量必须在 `bash` 一侧：`VAR=x curl ... | bash` 只给
`curl` 赋值，管道右侧的 `bash` 收不到。

```bash
curl -fsSL https://raw.githubusercontent.com/xartifact/x-sage/main/scripts/install-cli.sh \
  | X_SAGE_REF=<完整40位commit> bash
```

`X_SAGE_REF` 接受分支、tag 或完整 commit；传 commit 时安装器跳过解析，也不再依赖 GitHub API。
传 commit 必须是包含 `apps/cli/src/build-info.ts` 与 `skills/x-sage-kb/SKILL.md` 的那一版，
否则安装会在下载阶段失败（旧版本保留）。

### 失败与回滚

安装是「先解析 commit → 下载到 staging → 冒烟通过 → 才切换」。任何一步失败都保留旧版本，
并打印 `nothing was installed`：

- commit 解析失败（分支不存在、无网络、API 限流）
- 任一文件缺失或返回 HTML（代理/门户劫持）
- 暂存 CLI 无法启动，或 `--version` 报出的 commit 与预期不符（混版）
- Skill 缺少 `name: x-sage-kb` 或未覆盖当前命令

每个 commit 装在 `~/.local/lib/x-sage/versions/<commit>/` 下，`current` 是指向其中一个的软链接，
安装只改这个链接。回滚有两种方式：

```bash
# 1) 切回上一个版本（安装器会在输出里打印确切的 previous 路径）
ln -sfn ~/.local/lib/x-sage/versions/<old-commit> ~/.local/lib/x-sage/current.new
bun -e 'require("node:fs").renameSync(process.argv[1], process.argv[2])' \
  ~/.local/lib/x-sage/current.new ~/.local/lib/x-sage/current
```

不要用 `mv` 替换这个链接：目标是指向目录的软链接时，BSD 与 GNU 的 `mv` 都会把源移进该目录，
而不是替换链接本身。`renameSync` 才是「替换这个目录项」。

安装中途失败时，脚本自己执行同样的回滚，并恢复原来的 `bin/x-sage` 入口。

Skill 已存在且内容不同（或本身是软链接）时，旧目录被移到
`~/.agents/skills/x-sage-kb.backup-<UTC 时间>/`，不会静默丢弃你的修改，也不会写穿软链接指向的仓库。

**配置文件从不被覆盖**：`~/.config/x-sage/config.toml` 存在时安装器原样保留。

### PATH 上的旧入口

安装器只保证 `~/.local/bin/x-sage` 是新的，**不保证 `x-sage` 命令实际运行它**：如果 PATH 上
更靠前的目录里还有一个旧入口（例如从源码 `bun link` 装到 `~/.bun/bin/x-sage`，那是指向
工作树的软链接），`x-sage` 仍然跑旧的那个。安装器检测到这种情况会打印警告并指出先命中的路径。

处理方式：把 `~/.local/bin` 放到 PATH 前面，或删掉旧入口。**不要直接编辑那个旧软链接指向的文件**
——它就是仓库源码。

```bash
command -v -a x-sage      # 看全部命中项；第一个才是实际执行的
x-sage --version          # 确认报出的 commit 是刚装的那个
```

### 配置

```toml
# ~/.config/x-sage/config.toml（XDG_CONFIG_HOME 优先）
server = "https://x-sage.ts.xartifact.dev"
# 写操作才需要；读操作可以留空
token = "<write token>"
```

优先级：`--server`/`--token` > `X_SAGE_SERVER`/`X_SAGE_TOKEN` > 配置文件。

```bash
x-sage --version               # 本地输出 version + commit + ref，不访问服务端
x-sage validate --graph        # 验证连通与知识库完整性
x-sage activate decision       # 加载一个概念及其前置
```

## 从源码运行

```bash
bun install
bun run server                 # http://127.0.0.1:3001
bun run viz                    # 前端 dev server（代理 /api）
x-sage --server http://127.0.0.1:3001 activate decision
```

## 常用命令

| 命令 | 作用 |
|---|---|
| `manifest` | 全局原子索引（AI 开卷第一步） |
| `search <kw>` / `activate <id>` / `edges <id> --dir in` / `path a b` | 检索与遍历 |
| `validate --graph` | 文件 schema + 图完整性 + 产物是否落后 |
| `concept body <id>` | 概念正文 + `revision` |
| `refine-body <id> --file - --expected-revision <rev> --reason "…"` | 提交正文提案 |
| `refine-atom` / `propose-edge` | 提交 atom / 边提案 |
| `proposals list \| show <file> \| merge <file>` | 审阅与应用提案 |
| `frontier list \| add` | 未决概念 |
| `checkpoint create \| read \| save -` | 跨会话快照 |
| `graph generate --manifest` | 由概念文件重建 `graph.json` 与 `MANIFEST.md` |

写操作走提案：先写入 `proposals/`，审阅后再合并，期间用 `revision` 检测并发修改。

## Skill

`skills/x-sage-kb/SKILL.md` 是 agent 侧的接入说明，**由安装器与 CLI 从同一 commit 安装**，
所以两者不会各说各话：

```bash
curl -fsSL https://raw.githubusercontent.com/xartifact/x-sage/main/scripts/install-cli.sh | bash
```

安装器没有 skill-only 模式：它总是同时装 CLI 和 Skill，这正是两者不漂移的原因。
如果确实只要 Skill，用下面的手工方式——不要单独 `curl` 一份 SKILL.md 覆盖旧目录，
那会既混版又丢掉你的修改。

手工安装（仅在无法运行安装器时）必须自己固定 commit 并先备份已有的 Skill：

```bash
COMMIT=<完整40位commit>
[ -d ~/.agents/skills/x-sage-kb ] && \
  mv ~/.agents/skills/x-sage-kb "~/.agents/skills/x-sage-kb.backup-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p ~/.agents/skills/x-sage-kb
curl -fsSL "https://raw.githubusercontent.com/xartifact/x-sage/$COMMIT/skills/x-sage-kb/SKILL.md" \
  -o ~/.agents/skills/x-sage-kb/SKILL.md
```

## 文档

| 文档 | 内容 |
|---|---|
| [docs/architecture.md](docs/architecture.md) | 架构、概念分层、REST API、写入约束 |
| [docs/deployment.md](docs/deployment.md) | 镜像、CI、x99 上线、故障排查 |
| [docs/edge-semantics.md](docs/edge-semantics.md) | 边类型的关系代数 |
| [docs/concept-layers.md](docs/concept-layers.md) | L0–L3 判定标准 |

## 验证

```bash
./scripts/ci.sh      # typecheck + 契约测试 + 全部测试 + KB 校验
bun run typecheck
bun test
```

## 网页知识库维护

网页顶部可选择服务端注册的知识库；所有读取、提案、快照与写入均限定在所选库内。
图谱在前台每 3 秒刷新，窗口重新获得焦点时也刷新；网页写入成功后立即刷新。
后台同步失败会显示错误并保留已加载图谱，恢复连接后继续同步。切换库时丢弃旧请求结果。
这属于近实时轮询，不是服务端推送。

“维护知识库”提供新增概念、正文/摘要/关联提案、提案审阅合并、未决问题、会话快照、
搜索、路径查询、校验和图/索引重建。提案提交后仍待审，不会直接改变正式概念。
新增概念直接创建正式节点，校验 ID、元数据与关联；重复 ID 返回冲突。正文编辑保留版本基线。
写入凭据仅保留在当前面板内，关闭面板即丢弃，不写入浏览器持久存储。

服务默认使用仓库 `registry.json`，也可显式指定：

```bash
X_SAGE_REGISTRY=/data/registry.json X_SAGE_KB=/data/kb/default bun run server
```

注册表沿用 `knowledgeBases: [{ id, name, path }]`；`path` 相对注册表所在目录解析。
只设置 `X_SAGE_KB` 时保留旧的单库模式。Docker 使用多库时，需挂载注册表和其中引用的所有库，
并设置 `X_SAGE_REGISTRY`；仅替换前端文件不能让旧服务支持切库。注册表为空时网页显示空态。
新增库目前仍通过服务端注册表和 KB 目录配置，不提供网页创建/删除知识库。

创建概念与提案合并共用 `.write.lock`。创建过程不会按锁年龄强行抢锁；异常退出后如遗留锁，
必须先确认没有活动写入再由运维清理。创建成功但派生图/索引更新失败时会返回提示，
可在网页执行“重建图与索引”；不要为此重复创建概念。

### 前端技术栈与组件规范

`apps/viz` 使用 React 19、TypeScript、Vite 6、Tailwind CSS 4、shadcn/ui（Radix）和 `cn`，
D3 7 继续负责图谱布局。shadcn 的官方 new-york-v4 组件源码位于 `src/components/ui/`，
配置位于 `components.json`；统一通过 `src/lib/utils.ts` 导出 `cn` 类名合并函数。
组件源码来源：https://ui.shadcn.com/r/styles/new-york-v4/ 。

视觉变量统一来自 `src/xartifact-tokens.css`，支持鸢尾紫、海洋蓝、松石绿、陶土橙四套配色，
以及浅色、深色、跟随系统。偏好保存在浏览器本地，凭据仍只保留于当前维护弹窗。

浏览页由概念目录、可自适应的图谱和上下文详情组成；Cmd/Ctrl+K 聚焦搜索。
详情的关系目标可直接跳转，编辑入口会带入当前概念。维护弹窗使用受控表单与未保存确认，
会话快照提供结构化编辑，不再要求手写 JSON。图谱在后台标签页也先绘制初始节点，
窄屏自动适应范围，避免等待动画帧后才出现内容。
