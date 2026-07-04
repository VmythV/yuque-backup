# yuque-backup

面向个人及团队空间的语雀本地备份工具。核心目标是尽可能保存原始数据，同时生成便于本地阅读的 Markdown、CSV 和资源文件。

当前实现包括：

- 可配置任意语雀 Host，不在业务代码中固定 `www.yuque.com`
- Cookie 或 Token 认证，凭据只从环境变量读取
- 团队、个人及协作知识库发现
- 团队/知识库多选 TUI
- SQLite 断点状态和跨进程持久化限流
- SQLite 持久化 Bitmap / Roaring Bitmap 恢复索引
- 原始 `appData`、目录、文档 JSON、Lake、HTML 归档
- Markdown、图片、附件本地化
- Lake Sheet 原始压缩数据、解压 JSON 和逐工作表 CSV
- Lake Card 原始数据提取
- 实验性思维导图 / 画板请求探测脚本
- 对疑似思维导图 / 画板文档保存请求层 `diagramData`
- 临时文件原子写入、失败记录和归档检查

## 下载预编译可执行文件

当前 Release 提供 macOS Apple Silicon 版本：

```bash
curl -L -o yuque-backup-v0.1.0-aarch64-apple-darwin.tar.gz \
  https://github.com/VmythV/yuque-backup/releases/download/v0.1.0/yuque-backup-v0.1.0-aarch64-apple-darwin.tar.gz

tar -xzf yuque-backup-v0.1.0-aarch64-apple-darwin.tar.gz
chmod +x yuque-backup
```

可选：移动到 PATH 中：

```bash
sudo mv yuque-backup /usr/local/bin/yuque-backup
```

如果 macOS 提示无法打开未签名程序，可以执行：

```bash
xattr -d com.apple.quarantine ./yuque-backup
```

如果没有移动到 PATH，后续命令用 `./yuque-backup`；如果已经移动到 PATH，直接用 `yuque-backup`。

## 从源码构建

```bash
cargo build --release
```

## 初始化

```bash
yuque-backup init --host https://yuque.com
```

也可以使用其他空间：

```bash
yuque-backup --host https://example.yuque.com tui
```

Host 的优先级为：

1. `--host`
2. `YUQUE_HOST`
3. `yuque-backup.toml` 中的 `host`

Host 必须是完整 origin，例如 `https://yuque.com`，不能包含团队或知识库路径。

## 认证

推荐使用浏览器登录态 Cookie：

```bash
export YUQUE_COOKIE='_yuque_session=你的值'
yuque-backup tui
```

如果企业空间使用不同 Cookie 名，在配置中修改：

```toml
[auth]
cookie_key = "实际的 Cookie 名"
cookie_env = "YUQUE_COOKIE"
```

环境变量也可以只保存 Cookie 值，此时程序会自动加上 `cookie_key=`。若有语雀 Token，可使用 `YUQUE_TOKEN`。

凭据不会写入配置、SQLite、归档或日志。不要把真实 Cookie 放入命令行参数或提交到版本库。

Cookie 的浏览器获取位置、企业空间 Cookie 名确认方法和安全注意事项，请参阅 [获取语雀 Cookie](docs/GET_COOKIE.md)。

## 使用

```bash
# 发现可访问内容
yuque-backup discover

# 打开 TUI，选择团队和知识库，然后同步
yuque-backup tui

# 只保存选择
yuque-backup tui --select-only

# 同步上次选择
yuque-backup sync

# 同步所有发现的知识库
yuque-backup sync --all

# 查看进度
yuque-backup status

# 检查清单和中断的临时文件
yuque-backup verify
```

同步 TUI 会展示：

- 当前团队、知识库、文档和阶段
- 整体知识库/文档进度
- 每个知识库的跳过、下载、失败统计
- API 或资源请求遇到小时限流、服务端 429 时的等待倒计时

同步结束、失败或任务异常结束后，TUI 会停留在最终状态，按 `Enter`、`q` 或 `Esc` 返回。

## 限流

默认只使用配置额度的 85%，API 并发为 1。每次请求均记录在 `backup/.state/state.sqlite3`，重新启动不会重置小时窗口。服务端返回 429 时会遵循 `Retry-After`。

```toml
[rate_limit]
api_requests_per_hour = 600
api_concurrency = 1
minimum_interval_ms = 1500
asset_concurrency = 3
asset_minimum_interval_ms = 200
reserve_ratio = 0.15
```

在未确认空间实际额度前，不建议提高默认值。

如果刚完成大量同步后重新运行 `tui`/`discover`/`sync` 看起来没有进入界面，通常是本地持久化限流窗口已满。程序会等待最近一小时内最早的请求过期后继续执行；新版启动时会打印已用额度和预计等待时间。

重新同步时，已完成且远端更新时间未变化的文档会批量跳过，不再逐篇刷新 TUI。

## 恢复执行索引

断点恢复仍以 `documents` 表为事实来源；额外的 Bitmap / Roaring Bitmap 只作为本地加速索引，不依赖 Redis。

每个知识库会按当前目录顺序给文档分配 ordinal，并在 SQLite 中保存：

```text
resume_snapshots  当前目录和远端更新时间的快照
resume_items      ordinal -> doc_id/slug/title 映射
resume_bitmaps    planned/done/skipped/downloaded/failed 的 BLOB
```

恢复时会先根据目录和远端更新时间计算 `snapshot_hash`：

- 命中：直接加载 Bitmap，使用 `planned - done` 找到下一个待处理 ordinal。
- 未命中：从 `documents` 表批量重建 Bitmap。
- 中断导致 Bitmap 落后：下次会用 `documents` 表合并修正。

Dense Bitmap 用于密集集合，Roaring Bitmap 用于稀疏或大规模集合。同步过程中每处理一批文档会刷新 Bitmap；同一快照下不会反复重写全量 `resume_items`。

## 数据完整性

Markdown 是派生格式，不是唯一备份。每个知识库还会保存：

```text
raw/app-data.json
raw/repository.json
raw/toc.json
raw/docs/<doc-id>.json
raw/docs/<doc-id>.lake
raw/docs/<doc-id>.html
tables/<doc-id>/sheet.zlib
tables/<doc-id>/sheet.json
tables/<doc-id>/*.csv
diagrams/<doc-id>/lake-cards.json
diagrams/<doc-id>/request-default.raw.json
diagrams/<doc-id>/diagram.raw.json
diagrams/<doc-id>/diagram.normalized.json
diagrams/<doc-id>/diagram-report.json
```

画板、思维导图或动态卡片无法无损转换时，原始 Lake/Card 数据仍会保留。

如果语雀页面里的思维导图是折叠状态，静态预览图本身无法还原隐藏节点。同步时会对疑似思维导图 / 画板文档额外请求一次无 `mode` 的默认 API：独立 `Board/lakeboard` 会提取 `content.diagramData`，普通 `Doc/lake` 会解码 `<card name="board" value="data:...">` 中的 `diagramData`。如果只发现 `image` card，则会在 `diagram-report.json` 中标记为不可恢复。探测脚本用法见 [语雀思维导图 / 画板请求探测方案](docs/MINDMAP_REQUEST_PROBE.md)。

如果本地已有完成状态但缺少 `diagram-report.json`，重新运行同步时会把疑似文档重新加入待处理队列，用于回填 `diagramData`。

## 当前边界

- 首次在具体企业空间使用时，需要根据真实响应校验发现接口和 Cookie 名。
- 文档资源识别目前覆盖 Markdown 图片和语雀附件；HTML/Lake 内部资源会在后续版本继续扩展。
- `verify` 会检查清单格式、中断文件，并对已登记的正文和资源执行 SHA-256 审计。
