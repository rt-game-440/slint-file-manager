# 领域文档

采用 single-context 布局：根目录 GLOSSARY.md 保存领域术语，docs/adr/ 保存架构决策。

探索代码前读取已有 GLOSSARY.md，以及与本次工作相关的 ADR。
这些文件不存在时直接继续；由 domain-modeling 在术语或决策明确后按需创建。

事项标题、方案、假设和测试命名沿用术语表定义，避免换用明确排除的同义词。
发现术语缺口时记录给 domain-modeling。
方案与现有 ADR 冲突时，明确指出 ADR 编号、冲突内容和重新讨论的理由。
