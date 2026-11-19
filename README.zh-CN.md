# yourfs

[English](README.md) | **简体中文**

一个用 Rust 编写的**只读、可按文件隔离的 NFS 文件系统**。

`yourfs` 通过纯 Rust 的 NFS 客户端（[nfs-rs]，NFSv3）挂载一个远端 NFS 导出目录，向用户展示的是一棵完全由数据库中
**白名单**（支持 SQLite3、PostgreSQL、Redis）定义的**虚拟目录树**。白名单里没有的文件
对用户完全不可见；被列出的文件还可以用与服务端不同的名字展示。最终得到的是一个只读视图，
你能逐文件精确控制"暴露什么、以什么名字暴露"。

> 本项目是原 C 版 [TechWander/yourfs](https://github.com/TechWander/yourfs) 的 Rust
> 重写，保留了原版行为。

---

## 工作原理

虚拟目录树由一张三元组表描述：

| 字段       | 含义                                          |
|------------|-----------------------------------------------|
| `parent`   | 条目所在的虚拟目录（`/` 为根）                |
| `name`     | 展示给用户的名字                              |
| `realpath` | 该文件在 NFS 导出中的真实路径                 |

```
          用户看到            白名单行              经 NFS 读取
   /mnt ─── a.txt  ──────►  (/, a.txt, /a.txt) ──────►  nfs://host/export/a.txt
            （b.txt 不在表中，因此在这里根本不存在）
```

* **`readdir`** 列出某目录的子项（`WHERE parent = ? ORDER BY name`）。
* **`lookup`/`open`/`getattr`** 把虚拟路径解析为 `realpath`，再经 NFS 读取；无匹配行返回 `ENOENT`。
* 挂载为**只读**：任何写操作返回 `EROFS`。

产出两个可执行文件：

* **`yourfs`** —— FUSE 文件系统本体，挂载白名单定义的 NFS 视图。
* **`yourfs-tool`** —— 构建白名单：扫描本地目录树生成文本，再写入 SQLite3 / PostgreSQL / Redis。

## 后端与 `-X` 连接串

| 后端       | `-F` 取值   | `-X` 格式                                                     |
|------------|-------------|---------------------------------------------------------------|
| SQLite3    | `sqlite3`   | `/path/to/data.db`                                            |
| PostgreSQL | `postgres`  | `ip,port,user,passwd,dbname`                                  |
| Redis      | `redis`     | `redis://host:port/db` · `ip,port[,db[,passwd]]` · `host:port` |

Redis 存储方式：每个目录用两个键——有序集合 `{table}|d|{parent}`（成员为子项名，天然按字节序，
服务 `readdir`）与哈希 `{table}|h|{parent}`（`name → realpath`，`O(1)` 解析）。

## 构建

```bash
cargo build --release
```

生成 `target/release/yourfs` 与 `target/release/yourfs-tool`。FUSE 与 NFS 两层均为
**纯 Rust**（`fuser` + `nfs-rs`），构建**完全不链接 `libfuse`/`libnfs`**——也就无需
安装它们的任何包；gnu 版剩下的动态依赖只有每个 Linux 都自带的 glibc 基础库。

### musl 全静态版（可跑在 musl/Alpine 上）

```bash
./scripts/build-musl-static.sh
# -> target/x86_64-unknown-linux-musl/release/yourfs{,-tool}
```

`yourfs` 本就不含 C 库，脚本因此只需 Rust 的 musl target 加一套 musl C 工具链（用于
编译 `rusqlite` 内嵌的 SQLite），把一切交叉编译进**零动态依赖**的单个 ELF（`ldd`
显示 *statically linked*，且无 `INTERP` 段；`file` 对这种 static-PIE 会误报
"dynamically linked"，以 `ldd`/`readelf` 为准）。目标机上 FUSE 本身的运行前提见
[部署与运行前提](#部署与运行前提)。

### 预编译二进制

GitHub Release 提供两个 x86_64 Linux 压缩包及 `SHA256SUMS.txt`：

* `yourfs-<ver>-linux-x86_64-gnu.tar.gz` —— glibc 版（需 glibc ≥ 2.30）；
* `yourfs-<ver>-linux-x86_64-musl.tar.gz` —— 全静态版，无任何共享库依赖。

包内均为 `yourfs` 与 `yourfs-tool`。

## 快速上手

```bash
# 1. 扫描导出目录，生成白名单文本（每行 parent name realpath）
yourfs-tool -s /srv/nfs/export -o list.txt

# 2. 写入 SQLite3 数据库，表名 data
yourfs-tool -F sqlite3 -X /srv/data.db -Z data -i list.txt

# 3. 挂载只读、隔离的 NFS 视图
mkdir -p /mnt/yourfs
yourfs -n nfs://127.0.0.1/srv/nfs/export -m /mnt/yourfs \
       -F sqlite3 -X /srv/data.db -Z data

ls /mnt/yourfs        # 只有白名单内的条目可见
umount /mnt/yourfs    # 或 fusermount -u /mnt/yourfs
```

**`-n` URL 格式。** 导出以 `nfs://host[:端口]/导出路径` 的形式寻址。`yourfs` 讲
**NFSv3**，当你省略相关参数时会自动为 URL 追加 `?version=3&noresvport=true`：version 3
对应只读挂载所用的“按 file handle + 偏移无状态 READ”路径；`noresvport` 让非 root 挂载者
无需绑定特权（< 1024）源端口——相应地导出需允许非安全端口（`insecure` 选项）。底层
`nfs-rs` 客户端认识的查询参数仍可显式指定（`version`、`uid`、`gid`、`mountport`、
`nfsport`、`noresvport`），显式值不会被默认值覆盖；`version` 接受 `3`、`4.0`、`4.1`、
`4.2`（裸 `4` 会被拒绝），但本项目仅实测 NFSv3。UID/GID 在挂载时一次性固定并对每个
请求沿用——见[设计说明](#设计说明)。

`-X`/`-Z` 指向的数据库必须已经建好（上面第 1–2 步）：`yourfs` 启动时会校验表，
失败即带着明确原因退出，而不是挂出一个每次访问都报 `EIO` 的视图。

## 完整案例

只发布导出目录的一部分、换个名字展示，并且事后随时改主意：

```
NFS 服务端导出 /srv/data                  用户看到 /mnt/yourfs
├── public/welcome.txt          ────►      ├── welcome.txt
├── internal/2025/report.pdf               └── reports/
└── secret/keys.txt（未列入白名单）              └── report.pdf
```

**1. 描述虚拟树**——每条目一行，以空白分隔的 `parent name realpath`
（路径相对导出根目录、以 `/` 开头；`secret/keys.txt` 没有对应行，于是在视图里
彻底不存在）：

```
/          welcome.txt    /public/welcome.txt
/          reports        /internal/2025
/reports   report.pdf     /internal/2025/report.pdf
```

目录也是一行（`/ reports /internal/2025`），子项靠 `parent` 指向它——注意
`parent` 是**虚拟**路径，所以 `/reports` 可以是服务端 `/internal/2025` 的别名
（已实测：`ls` 里 `reports` 显示为目录，`cat` 能直接读到文件）。

**2. 生成白名单**（整棵树的场景先用扫描代替手写第 1 步，再手工裁剪）：

```bash
yourfs-tool -s /srv/data -o list.txt     # 也可以直接从手写文件开始
yourfs-tool -F sqlite3 -X /srv/data.db -Z data -i list.txt
```

> 导入是**整表替换**：先 `DROP TABLE` 再全量重建（Redis 则是 `SCAN`+`DEL
> {table}|*` 后重写）。导入之后在数据库里手工加改的行，下次导入时会被清掉。

**3. 挂载读取**：

```bash
yourfs -n nfs://<server>/srv/data -m /mnt/yourfs -F sqlite3 -X /srv/data.db -Z data
ls /mnt/yourfs        # reports  welcome.txt
```

### 挂载期间修改虚拟目录

每次操作都读实时数据库，所以改表后**下一次 `ls`/打开立即生效**，无需重新挂载
（除非挂载时开了 `-w`：缓存会继续供旧答案，需重启 `yourfs`）。

```sql
-- 立即多发布一个文件
INSERT INTO data(parent,name,realpath) VALUES ('/','keys.txt','/secret/keys.txt');
-- 改用户看到的名字（指向的还是同一个文件）
UPDATE data SET name='welcome-v2.txt' WHERE name='welcome.txt';
-- 下架一个文件
DELETE FROM data WHERE name='keys.txt';
```

Redis 则用：发布 = `ZADD data|d|/ <name>` **加** `HSET data|h|/ <name>
<realpath>`；下架 = 对应的 `ZREM` + `HDEL`。PostgreSQL 同上走 `psql`——但注意
它的 `CHAR` 列比较时会忽略尾部空格，名字/路径末尾不要带空格。

若要大改结构，更稳的做法是改列表文件后重新导入，并选在挂载空载时进行——
导入会先删表，这个窗口期内落到的读操作会报错。

## 命令行选项

**`yourfs`**

| 选项 | 含义 |
|------|------|
| `-n <url>` | 要挂载的 NFS 导出：`nfs://host/path`（**必填**；主机名后必须带路径——根导出写 `nfs://host/`；此外结尾斜杠可省） |
| `-m <path>` | 本地挂载点（**必填**，隐含只读） |
| `-F <type>` | 白名单后端：`sqlite3` \| `postgres` \| `redis` |
| `-X <db>` | 后端连接串（见上表）；sqlite3 时文件必须已存在——`yourfs` 从不建库 |
| `-Z <table>` | 白名单表名 / 键前缀 |
| `-M <MB>` | SQLite 页缓存大小（默认 10） |
| `-w` | 将白名单缓存在内存中（可选开启；仅当挂载期间白名单不会变化时启用） |
| `-V <N>` | FUSE 事件循环线程数（默认 4，范围 1–64）。`nfs-rs` 把所有 RPC 复用到单条连接上，此项不再控制 NFS 连接池 |
| `-?` | 用法 |

另有一组为与 C 版命令行兼容而保留的开关。有内核/`fuser` 语义的会被采纳：`-a`/`-o`
allow_other、`-r` allow_root、`-p` 切换 default_permissions、`-t` 切换多线程、`-f`
fsname、`-s` subtype。旧的 libfuse I/O 开关**接受但忽略**——它们所选的缓存改由文件系统
内部实现（`FOPEN_KEEP_CACHE` + 很短的属性 TTL），不再经 libfuse：`-d` direct_io、
`-k` kernel_cache、`-c` auto_cache、`-A`/`-S` 异步/同步读、`-Y` nonempty、`-u`/`-g`
uid/gid 及各类超时。逐调用者凭证开关 `-U`/`-G` 接受但无实际作用（固定凭证模型，见
[设计说明](#设计说明)）。

**`yourfs-tool`**

| 选项 | 含义 |
|------|------|
| `-s <path>` | 扫描该目录树并写入 `-o` 输出文件；省略则本工具改为写数据库 |
| `-o <file>` | 扫描时的输出文本文件（默认 `output.txt`） |
| `-i <file>` | 写数据库时的输入文本文件（默认 `input.txt`） |
| `-F`/`-X`/`-Z`/`-M` | 同上（写入目标） |
| `-?` | 用法 |

## 部署与运行前提

纯 Rust 栈本就不链接 `libnfs`/`libfuse`，但运行**任何** FUSE 文件系统仍需目标机具备：

* 已加载 **`fuse` 内核模块**、存在 **`/dev/fuse`** 设备；
* 来自 `fuse`/`fuse3` 包的 **`fusermount`**（或 `fusermount3`）辅助程序。`fuser` 在有
  权限时直接经 `mount(2)` 挂载，否则回退到 fork/exec `fusermount`；无论哪种，它都是独立
  的 setuid 程序，无法打包进 `yourfs` 二进制。

此外：

* 默认（`x86_64-unknown-linux-gnu`）二进制需要 **glibc ≥ 2.30**，可跑在主流 glibc 发行版；
* **musl** 二进制为全静态，可额外跑在 musl/Alpine 上；
* 运行期需能通过网络访问 NFS 服务端。

## 测试说明

`yourfs` 涉及 FUSE、内核与真实 NFS 服务，仅靠单元测试说明不了问题。白名单隔离、符号链接
解析、写入返回 `EROFS`、`statfs`、断连恢复、干净卸载等端到端行为，已在真实本地 NFS
导出上，对三个后端（SQLite3、PostgreSQL、Redis——后两者经 docker 部署）、glibc
**以及**全静态 musl 的 release 二进制验证通过。

## 设计说明

* **纯 Rust，无 C 库。** FUSE 层用 [`fuser`][fuser]（其 `libfuse` feature 保持关闭，
  走默认纯 Rust 挂载路径），NFS 层用 [`nfs-rs`][nfs-rs]。此前手写的 libfuse/libnfs FFI
  绑定（`fuse2.rs`、`libnfs.rs`）与链接它们的 `build.rs` 已删除，最终既不链接 `libfuse`
  也不链接 `libnfs`（gnu 与静态 musl 两版均经 `ldd`/`readelf -d` 验证）。
* **inode 制、白名单驱动。** `fuser` 高层 API 以 inode 为单位，故 `src/fs.rs` 维护一张
  `inode ⇄ 虚拟路径` 映射表（libfuse 路径以名字为单位）。`readdir` 完全由数据库白名单
  驱动——每个子项各自 resolve 再 `getattr` 取其类型与 inode——因此列目录不依赖 NFS
  `READDIR`，无匹配行的路径直接不可见（`ENOENT`）。
* **挂载期固定凭证。** `nfs-rs` 的 AUTH_UNIX 凭证在挂载时一次性固定，取自 URL 的
  `?uid=&gid=`（缺省则为挂载者的真实 uid/gid），不提供按请求切换的入口。故 yourfs 对每次
  操作转发的是挂载用户的身份，而非各个 FUSE 调用者各自的身份。白名单仍逐行隔离可见性，
  所以这只放宽服务端对被导出文件本身的权限校验，不改变隔离模型。
* **NFSv3、无状态读。** 否则 `nfs-rs` 会去协商 4.1，故 URL 默认补 `version=3`。NFSv3 的
  `READ` 按 file handle + 偏移寻址，只读挂载无需有状态的 NFS `OPEN`：打开文件只是解析出
  句柄。客户端在单条连接上多路复用并发 RPC，故不再有逐调用者连接池，`-V <N>` 改用于设定
  FUSE 事件循环线程数。
* **一致地启用缓存。** libfuse 的 `auto_cache` 在 `fuser` 侧没有对应挂载选项（它不属于
  内核 FUSE ABI），故读缓存直接在文件系统内实现：`open` 设置 `FOPEN_KEEP_CACHE`，让内核
  跨 open 保留文件页缓存，配合很短（约 1 秒）的 entry/attr TTL。因每次操作仍实时解析
  白名单，撤销某行会让文件在 TTL 内即消失，即便其页曾被缓存——与 `auto_cache` 同等的
  安全性；256 MB 文件实测冷读约 500 MB/s、热读约 3.9 GB/s。
* **并发但不牺牲一致性。** `fuser` 会在多个 worker 线程上分发回调；各后端（SQLite3 /
  PostgreSQL / Redis）都从一个固定容量的小连接池中借出一条连接，而非在单一互斥锁上排队。
  默认不缓存任何东西：每次操作都读实时数据库，外部新增 / 修改 / 撤销的条目立即生效。
* **可选的白名单缓存（`-w`）。** 开启后缓存已解析的路径与目录列表，重复操作可省掉数据库
  往返——对网络后端是实打实的延迟收益。它**默认关闭**，因为被撤销的行或被改动的 `realpath`
  会继续被缓存命中而失效；仅在能确保白名单在整个挂载期间不变时才开启。
* **自行守护化。** `fuser` 不像 libfuse 的 `fuse_main` 那样自行 fork，故 `bin/yourfs.rs`
  显式守护化。NFS 运行时与连接无法跨越那次 `fork`，所以父进程只在一条一次性连接上
  *预检* 导出（把坏 URL 报在终端上），真正的会话由守护子进程建立——预检刻意不写入
  进程级静态量，好让子进程那次唯一的一次性 init 能填充它们。

## 许可证

GPL-2.0-or-later（与原 C 项目一致）。

[nfs-rs]: https://github.com/NetAppLabs/nfs-rs
[fuser]: https://github.com/cberner/fuser
