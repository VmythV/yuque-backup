# 设计约束

## Host

所有页面和 API URL 必须通过 `AppConfig::url` 从配置 Host 生成。业务模块不得直接使用固定语雀域名。SQLite 中所有选择、文档和限流记录都以 Host 为隔离键。

## 归档层次

1. 原始层：接口 JSON、Lake、HTML、Sheet 压缩数据、Lake Card。
2. 可读层：Markdown、CSV，后续包含 HTML/SVG/PNG。
3. 状态层：SQLite，仅保存同步状态、哈希、错误和限流时间，不保存认证信息。

原始层不得依赖可读层生成，转换失败不能导致原始数据丢失。

## 状态机

```text
pending -> metadata_saved -> content_saved -> assets_downloading
        -> rendered -> verified -> complete
                            \-> failed
```

文件使用同目录 `.part` 临时文件，完成写入并 `sync_all` 后原子改名。只有文件提交成功后才能推进数据库状态。

## 限流

API 和资源使用独立桶。API 小时窗口持久化到 SQLite，默认保留 15% 配额。API 正文请求保持串行；资源下载允许有限并行，且认证 Cookie 只发送给配置 Host。

## 删除和重命名

文档以远端 ID/UUID 为身份，不以标题或路径为身份。远端缺失只记录 tombstone，不自动删除本地数据。文件名冲突通过稳定远端 ID 区分。

