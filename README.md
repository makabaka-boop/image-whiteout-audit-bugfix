# layer-merge

一个 JSON 命令行工具：把 1～30 个 OCI 风格的镜像层（文件写入、建目录、
whiteout、opaque 目录）合并成最终的文件系统视图，回答三个问题——

1. **最终目录树**长什么样；
2. 每个存活**文件来自哪一层**（被删后重建的同名路径得到新来源）；
3. 曾经存在却被**遮蔽的路径**，各自因为什么、被哪一层遮蔽。

工具是纯过滤器：从 stdin 读 JSON，向 stdout 写 JSON，不触碰文件系统。

## 快速开始

```console
$ cargo build --release
$ ./target/release/layer-merge --pretty < examples/sample.json
```

或用 Compose（`layers` 服务只运行该工具，不挂载任何宿主路径）：

```console
$ docker compose build
$ docker compose run --rm -T layers < examples/sample.json
```

`--pretty` 美化输出；`--help` 打印用法。退出码：

| 退出码 | 含义 |
|---|---|
| 0 | 合并成功 |
| 1 | 层在语义上非法（同层冲突、opaque 目标不存在、文件充当父目录、路径不规范……） |
| 2 | 输入不是合法 JSON / 违反 schema 或规模限制 |

## 输入格式

```json
{
  "layers": [
    {
      "writes":    ["app/main.py"],        // 本层写入的文件
      "mkdirs":    ["data"],               // 本层显式创建的目录
      "whiteouts": ["etc/old.conf"],       // 删除下层同名路径（文件或整棵子树）
      "opaques":   ["data"]                // 隐藏该目录在下层的所有子项
    }
  ]
}
```

- `layers` 必填，**1～30 层**；四个列表均可省略（默认为空），四个列表合计
  **不超过 3000 条记录**；未知字段会被拒绝。
- 路径必须是**规范的相对 ASCII 路径**：可打印 ASCII 段以单个 `/` 连接，
  不含空段、`.`、`..`、首尾斜杠；最长 512 字节。

## 合并语义

层自下而上依次应用。**同一层内**的顺序固定且与书写顺序无关：

1. **whiteout 与 opaque 先作用于下层快照**
   - whiteout：删除下层快照中的同名路径；是目录则整棵子树一并删除。
     下层不存在该路径时是**无操作**（悬挂 whiteout 合法）。
   - opaque：保留下层快照中的同名目录本身，但**隐藏其全部下层子项**。
     opaque 只能标在**下层或本层存在的目录**上（本层显式 `mkdirs` 或写入的
     隐含父目录都算"本层存在"），否则整份输入以 `invalid_opaque` 拒绝。
2. **然后应用本层写入**（先 `mkdirs` 后 `writes`，同类之间结果与顺序无关）
   - 写入会自动创建缺失的父目录（来源记为本层）。
   - 文件被同路径新文件覆盖 → 旧版本记为 `overwritten`，存活文件来源更新为新层。
   - 显式 `mkdir` 落在下层的**文件**上 → 文件被目录取代（`replaced_by_dir`）。
   - 写入落在下层的**目录**上 → 目录及其子树被文件取代
     （`replaced_by_file` / `replaced_ancestor`）。
   - 已存在的目录上重复 `mkdir` 是合并而非重建：目录保留原来源层。

**同层冲突，整份拒绝**（`layer_conflict`）：任一列表内路径重复；同一路径
既是文件又是目录（文件集与"mkdirs ∪ 隐含父目录"集合相交，这也覆盖了
"文件充当同层另一项的父目录"）；同一路径既是 whiteout 又是 opaque。
whiteout 与写入同路径是**合法**的——先删后建，同名路径得到本层的新来源。

**文件不能充当父目录**（`file_as_parent`）：任何写入/建目录的祖先若在下层
快照中是文件，则拒绝——除非该文件被本层 whiteout 先删掉、或被本层显式
`mkdir` 取代。

## 输出格式

```json
{
  "ok": true,
  "tree":  { "kind": "dir", "path": "", "source_layer": null, "children": [ … ] },
  "files": [ { "path": "app/main.py", "source_layer": 1 } ],
  "obscured": [
    { "path": "data/old.csv", "source_layer": 0,
      "reason": "opaque_ancestor", "by_layer": 1, "via": "data" }
  ],
  "stats": { "layers": 3, "files": 5, "dirs": 4, "obscured": 4 }
}
```

- `tree`：最终目录树，子节点按路径排序，每个节点带创建它的 `source_layer`
  （根为 `null`）。
- `files`：每个存活文件及其来源层，按路径排序。
- `obscured`：遮蔽事件日志，按 `(by_layer, path)` 排序。同一路径可多次出现
  （每次被遮蔽一条）；被删后重建的路径，旧版本在此留痕、新版本出现在
  `files` 中。`reason` 取值：

| reason | 含义 |
|---|---|
| `overwritten` | 文件被后续层同路径重写 |
| `whiteout` | 路径本身被 whiteout 点名删除 |
| `whiteout_ancestor` | 祖先目录被 whiteout 删除（`via` 为 whiteout 路径） |
| `opaque_ancestor` | 祖先目录被标 opaque（`via` 为 opaque 目录） |
| `replaced_by_file` | 目录的路径被文件写入取代 |
| `replaced_by_dir` | 文件的路径被显式 mkdir 取代 |
| `replaced_ancestor` | 祖先目录被文件取代（`via` 为被取代的路径） |

失败时输出 `{"ok": false, "error": {"code", "message", "layer"?, "path"?}}`，
`code` ∈ `invalid_json` / `invalid_input` / `invalid_path` / `layer_conflict`
/ `invalid_opaque` / `file_as_parent`。

## 示例

见 [`examples/sample.json`](examples/sample.json)：第 1 层覆写
`app/main.py`、whiteout 掉 `etc/config.yml`、把 `data` 标为 opaque；
第 2 层重建 `etc/config.yml`。结果中 `app/main.py` 来源为层 1、
`etc/config.yml` 来源为层 2（删后重建得到新来源），`data/old_*.csv`
以 `opaque_ancestor` 记录在 `obscured` 中。

## 测试

```console
$ cargo test
```

- `tests/differential.rs` —— **对拍**：主引擎（扁平路径映射实现）对照一个
  独立的、逐层应用的**显式树模型**预言机。混沌生成器制造大量非法输入比对
  错误码；引导生成器基于当前合并状态构造大概率合法、事件密集的层序列，
  比对最终树、文件来源与遮蔽日志，并内置覆盖率断言（七种遮蔽原因、三类
  拒绝都必须被充分触发）。
- `tests/semantics.rs` —— 金样语义测试，重点覆盖**目录整体遮蔽**、
  **同名重建**（新来源层）与**父子类型冲突**（同层冲突 / 跨层
  file-as-parent / 文件⇄目录互相取代）。
- `tests/cli.rs` —— 端到端：stdin→stdout、退出码、`--pretty`。

## 布局

```
src/lib.rs        引擎：解析、校验、合并、输出（库形式，便于测试）
src/main.rs       CLI 外壳：stdin/stdout、参数、退出码
tests/            对拍 + 语义 + 端到端测试
examples/         示例输入
Dockerfile        多阶段构建（构建阶段会先跑完整测试套件）
compose.yaml      layers 服务：只运行工具，read_only、无网络、无特权、无挂载
```
