# Sidecar 资源约定(G14,对齐内核 §7)

> 依据:`docs/ai-write-kernel.html` §7。内核对根抽象的一处诚实让步:引入富文本 / 配图(为转 PDF)后,单个文件装不下全部资源,article 实为「一个正文文件 + 一套 sidecar 资源约定」。本文定义该约定的最小可实现形态。
> 范围:**只做资源定位约定 + 安全解析**,不做富文本 / PDF 渲染。

## 约定

文章 `topic/name.md` 的关联资源(图片、图表等二进制 / 富媒体)放在与正文**同级**的 sidecar 目录:

```
topic/
  index.json
  name.md                  # 正文(纯文本,事实源仍是单文件)
  name.md.assets/          # sidecar 目录
    cover.png
    figures/
      fig1.png
```

规则:

- **后缀拼在完整文件名后**:`name.md` → `name.md.assets`,不是 `name.assets`。这样 `a.md` 与 `a.txt` 不会撞同一个资源目录,也无需解析扩展名。后缀常量 `ASSETS_DIR_SUFFIX = ".assets"`。
- **目录可嵌套**:资源用「相对 sidecar 目录」的路径命名,允许子目录(`figures/fig1.png`)。
- **正文不变**:「文章 = 一个正文文件」的内核不变;sidecar 只是正文旁边一个有约定名字的目录,index/manifest 不记录资源清单(资源由目录本身枚举)。
- **沙箱内**:所有资源路径走 `Workspace::resolve` 同一套沙箱(词法层禁 `..` / 绝对路径,符号层禁 symlink 逃逸),不可越出工作区。

## API(`Workspace`,kernel §7)

| 方法 | 作用 |
|---|---|
| `asset_dir(theme, file_name) -> PathBuf` | 计算文章的 sidecar 目录(主题相对路径),不创建。 |
| `resolve_asset(theme, file_name, asset_path) -> PathBuf` | 把单个资源(相对 sidecar 目录)解析为沙箱内绝对路径;`..` / 绝对路径 / symlink 逃逸一律拒。 |
| `list_assets(theme, file_name) -> Vec<PathBuf>` | 递归列出 sidecar 目录下的常规文件(相对路径、已排序);目录不存在则返回空。symlink 与非常规项跳过。 |

`list_assets` 返回的每个路径可直接回喂 `resolve_asset`。

## 不在本任务范围

- 富文本 / Markdown → PDF 渲染。
- 资源的写入 / 删除工具(模型侧 tool);本任务只落地定位与安全解析,写入路径后续随渲染需求再加。
- 把资源纳入 coordinator 事务的声明锁集(资源目前不参与 commit 粒度讨论)。
