# Local Console Hub — Agent 工作守则

本项目是 Windows 优先的本地服务/终端统一控制台（Tauri）。产品语义以 docs/ 下的文档为准：
PRODUCT_SPEC / MVP_IMPLEMENTATION_SPEC / EXECUTION_PLAN / UI_STYLE_GUIDE / LOGGING / DECISIONS / ROADMAP / DEVELOPMENT。

## UI 参考图与视觉验收

主力模型自带多模态视觉，可以**直接查看图片**，不需要任何外接视觉服务。

- 服务会话参考图：`assets/ui/ui-v2-service.png`；交互终端参考图：`assets/ui/ui-v2-terminal.png`。
- `docs/UI_STYLE_GUIDE.md` 是规范文本；图文若有冲突，以它的产品语义为准。
  布局 / 配色 / 间距 / 字号等设计细节以参考图为准（`docs/DESIGN_SPEC_EXTRACTED.md` 是它的转译缓存）。
- **UI 开工前**：先读两张参考图，再读 `docs/UI_STYLE_GUIDE.md`，然后才开始写 UI 代码。
- **UI 变更完成后**：对运行中的应用截图，自行与对应参考图逐区域比对
  （布局 / 组件 / 颜色语义 / 间距 / 字号），按差异清单迭代，直到没有 MAJOR 级差异。
  若当前环境确实无法截图，向用户明确说明原因，**不得静默跳过验收**。
- 用户提到或提供任何图片（报错截图、图标、参考素材）时先看图再回答，不要猜测图片内容。

> 2026-09-28：原 `ui-vision` 外接视觉方案（glm-5.3-flash）已随 #18 停用并移除——主力模型
> 已具备原生视觉，那层转译是多余的；`tools/vision-mcp/` 与 `.mcp.json` 均已删除。
> 若将来主力模型换回无视觉能力的模型，重新注册该 server 即可恢复。

## 开发环境注意事项

- **编译 / 测试 / 构建（cargo、npm 等）一律在 instabox 沙箱上跑**，不在本地执行。
  本机只负责改代码和静态阅读；产物验证（`cargo build` / `cargo test` / `npm test` / `npm run build`）
  通过 instabox MCP 工具（`mcp__instabox__*`）在沙箱内完成。
- 开发服务器端口使用 **24120**（或同段不常用端口）。禁止使用 Tauri 默认端口 1420：
  它落在 Windows 的保留端口范围内，可能随机导致启动失败。
  修改端口时 `vite.config.ts` 与 `src-tauri/tauri.conf.json` 必须一起改。

## Agent skills

本仓库的议题以 GitHub Issues 形式追踪（`q956085398-netizen/Local-Console-Hub`）。

- **Issue tracker**：本地 CLI 环境用 `gh` CLI；在 Claude Code 的沙箱 / VM 会话里 `gh` 不存在且无网络出口，
  此时改用 GitHub MCP 工具读写议题与 PR。见 `docs/agents/issue-tracker.md`。
- **Triage labels**：使用默认分诊标签，原样沿用：`needs-triage`、`needs-info`、`ready-for-agent`、`ready-for-human`、`wontfix`。见 `docs/agents/triage-labels.md`。
- **Domain docs**：单一上下文（single-context）：仓库根目录一个 `CONTEXT.md`，加上根目录 `docs/adr/`。见 `docs/agents/domain.md`。
