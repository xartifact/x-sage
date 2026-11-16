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
