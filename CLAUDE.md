# CLAUDE.md

完整工作手册见 [AGENTS.md](AGENTS.md)。

## 快速提示

- **分层**：`packages/core`（纯函数）→ `packages/service`（用例 + 分发器 + 渲染）→ `apps/{cli,server,viz}`（传输/展示）
- **一个分发器**：路由、参数校验、文本渲染都在 `packages/service`。CLI 与 HTTP 只是它的两个适配器，不要在前端各写一份。
- **没有本地模式**：CLI 只有 `--server <url>`（或 `X_SAGE_SERVER`）。客户机不读 KB、不带图算法，只发 REST 请求。
- **知识库位置**：`kb/<kb-id>/`，概念 markdown 是唯一真源
- **启动**：`bun run server`（API + 生产静态）；`bun run viz`（开发，/api 代理到 3001）
- **会话开始**：加载 `MANIFEST.md` 和 `frontier.md`
- **写回**：`x-sage --server <url> propose-edge ...` 经 API 写入 `proposals/`，不要直接改概念文件
- **改完必重建**：`graph generate` → `validate --graph`（或直接 `bun run ci`）
- **写保护**：`X_SAGE_WRITE_TOKEN` 设置后，所有 POST/PUT 需要 `Authorization: Bearer <token>`
