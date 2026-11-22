# x-sage 核心概念拆分草案

状态：待审阅，未进入正式知识图。五个领域概念均拟归 L2，不新增 L0/L1。JSON frontmatter 是合法 YAML。

- [definition-dependency-layering](definition-dependency-layering.md)
- [budgeted-concept-activation](budgeted-concept-activation.md)
- [typed-relation-reasoning](typed-relation-reasoning.md)
- [cross-session-reasoning-state](cross-session-reasoning-state.md)
- [reviewed-knowledge-writeback](reviewed-knowledge-writeback.md)

入库后建议从 x-sage-concept-kb 向这五个概念添加 instantiates 边；不从抽象概念反向依赖 L3 产品。现阶段不提交悬空边。
