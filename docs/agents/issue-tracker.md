# 事项跟踪：GitHub

仓库：rt-game-440/slint-file-manager。
事项和规格存放在 GitHub Issues，统一使用 gh 操作。

正式产品规格与三阶段交付地图：[Issue #1](https://github.com/rt-game-440/slint-file-manager/issues/1)。该规格已于 2026 年 11 月 7 日由用户整体确认，后续实现与验收以该 Issue 及其关联事项为准。

用户通过的主界面视觉参考：[A2：macOS 灵感双栏](../concepts/file-manager/concept.md)。该文档只收录通过的概念图及采纳边界；功能范围与性能要求仍以 Issue #1 为准。

## 操作约定

- 首次写入前使用 gh repo view --json nameWithOwner 核对目标仓库。
- 创建：gh issue create；多行正文写入临时文件，通过 --body-file 传入。
- 读取：gh issue view <编号> --json number,title,body,labels,comments。
- 列表：gh issue list，按状态、标签筛选。
- 标签：gh issue edit <编号> --add-label 或 --remove-label。
- 评论：gh issue comment；关闭前先发布处理说明，再执行 gh issue close。
- 发布到事项跟踪器意味着创建 GitHub Issue；获取工单意味着读取对应 Issue。
- 子事项优先使用 GitHub 原生关联；不可用时在子事项正文标注 Part of #<父编号>，并在父事项维护任务列表。
- 阻塞关系优先使用原生依赖；不可用时在正文顶部标注 Blocked by: #<编号>。所有阻塞事项关闭后才能开始。

## PR 分诊入口

**PRs as a request surface: no.**

## Wayfinding

- 地图：带 wayfinder:map 标签的 Issue，保存 Notes、Decisions-so-far、Fog。
- 子工单：关联地图，按类型标记 wayfinder:research、wayfinder:prototype、wayfinder:grilling 或 wayfinder:task。
- 选择地图中第一个未关闭、无未解决阻塞且未分配的工单。
- 开始前用 gh issue edit <编号> --add-assignee @me 认领。
- 完成后评论答案、关闭工单，并将结论摘要和链接补充到地图。
