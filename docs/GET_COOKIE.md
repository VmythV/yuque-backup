# 获取语雀 Cookie

本项目默认连接 `https://yuque.com`，使用浏览器现有登录态访问团队和私有知识库。Cookie 属于账号凭据，请只在自己的设备上操作，不要发送给他人，也不要写入项目配置或提交到版本库。

## 方法一：从浏览器存储中获取

以下步骤以 Chrome 和 Edge 为例：

1. 使用需要备份的账号登录 [yuque.com](https://yuque.com/)，确认可以正常打开目标团队和知识库。
2. 在语雀页面按 `F12`；macOS 也可以按 `⌥⌘I` 打开开发者工具。
3. 打开 `Application` 面板。如果没有显示，点击顶部的 `»` 查找。
4. 在左侧依次展开 `Storage`、`Cookies`，选择 `https://yuque.com`。
5. 在 Cookie 列表中查找 `_yuque_session`。
6. 双击该行的 `Value`，只复制完整值，不要复制引号、空格、`Domain` 或其他列。

某些企业空间的会话 Cookie 名可能不是 `_yuque_session`。如果列表中没有该名称，使用下面的网络请求方法确认实际名称。

## 方法二：从网络请求中确认

1. 保持开发者工具打开，切换到 `Network` 面板。
2. 刷新语雀页面。
3. 选择一个发往 `yuque.com` 的请求，例如页面请求或 `/api/mine/...` 请求。
4. 打开 `Headers`，找到 `Request Headers` 中的 `Cookie`。
5. 从中找到会话项，复制对应的 `名称=值`。不要复制无关 Cookie。

如果 Cookie 一栏被浏览器隐藏，优先使用方法一。

## 提供给程序

只在当前终端会话中设置最安全：

```bash
export YUQUE_COOKIE='_yuque_session=这里替换为完整值'
```

程序也接受只有值的形式，此时会使用配置文件中的 `auth.cookie_key`：

```bash
export YUQUE_COOKIE='这里替换为完整值'
```

如果实际 Cookie 名不同，修改 `yuque-backup.toml`：

```toml
[auth]
cookie_key = "实际的 Cookie 名"
cookie_env = "YUQUE_COOKIE"
token_env = "YUQUE_TOKEN"
```

然后验证登录态并查看可访问的团队和知识库：

```bash
./target/release/yuque-backup discover
```

验证成功后启动选择和下载界面：

```bash
./target/release/yuque-backup tui
```

## 常见问题

### 提示登录态失效

Cookie 可能已经过期、被退出登录操作撤销，或者复制不完整。重新登录语雀，再按上述步骤获取新值。

### 能发现知识库但附件下载失败

确认浏览器账号本身有附件访问权限。如果附件位于其他语雀域名，程序不会自动向不同域名发送当前 Host 的 Cookie，以避免凭据泄漏；失败链接会保留在 Markdown 和日志中。

### 终端关闭后变量消失

这是预期行为，可以避免凭据长期以明文保存在磁盘。再次运行时重新执行 `export` 即可。不建议把 Cookie 写入 `.zshrc`、`.bashrc`、`yuque-backup.toml` 或脚本。

### 使用完成后清除变量

```bash
unset YUQUE_COOKIE
```

Cookie 可能允许直接访问账号内的私有数据。如果怀疑泄漏，应立即退出语雀登录或在账号安全设置中撤销相关会话。
