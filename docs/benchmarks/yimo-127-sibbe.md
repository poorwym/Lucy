# YIMO-127 `nl_lod12_3d`：pg2b3dm / Lucy 全量比较

## 结论摘要

本报告记录 2026-07-14 在同一台机器、同一 PostGIS 关系
`public.nl_lod12_3d` 上依次执行的两条完整流水线：先运行 pg2b3dm，
再冷启动 Lucy，并通过 HTTP 遍历 implicit tiling availability、把所有可用
subtree 和 GLB 落盘。

| 指标 | pg2b3dm 2.27.0 | Lucy `2057404` | 对比 |
| --- | ---: | ---: | ---: |
| 端到端墙钟时间 | 119.15 s | 40.37 s | Lucy 少 66.12%，pg/Lucy = 2.95x |
| GLB 数 | 1,602 | 10,921 | Lucy 6.82x |
| subtree 数 | 212 | 2,875 | Lucy 13.56x |
| GLB 字节 | 1,248,258,460 | 2,836,327,044 | Lucy 2.27x |
| 全部输出字节 | 1,248,337,446 | 2,837,405,287 | Lucy 2.27x |
| 三角形 | 13,764,915 | 29,077,068 | Lucy 2.11x |
| 最大常驻内存 | 268.6 MiB | 134.1 MiB | Lucy 49.9% |
| 3D Tiles Validator | 0 error / 0 warning | 0 error / 0 warning | 均通过 |

在本次“从数据库到全量落盘结果”的测量口径下，Lucy 用 33.9% 的时间写出
2.27 倍的数据、6.82 倍的 GLB 和 2.11 倍的三角形。这是一个有价值的端到端
结果，但**不是同等切片工作量下的纯生成器微基准**：pg2b3dm 使用 `ADD`、
自适应划分并生成 1,602 个内容；Lucy 使用固定 level 0--7、从 level 3 开始
生成内容、`REPLACE`，还会按 tile 边界裁切三角形，因此文件数、重复元数据行和
三角形数天然不同。

Lucy 的进程冷启动直接内容请求另做了 5 次独立测量。中位数为：服务就绪
181.7 ms、首个 GLB 请求 255.9 ms、从启动到 GLB 完整返回 446.6 ms。

文件名保留 `yimo-127-sibbe.md` 以延续历史 issue，但本次对象是数据库中的完整
`nl_lod12_3d` 关系，不应把结果描述成已冻结的 Sibbe 离线子集。

## 比较口径

两条流水线都读取：

- 同一 PostgreSQL 实例、数据库和 `public.nl_lod12_3d`；
- 几何列 `geom`，EPSG:7415，`MULTIPOLYGON Z`；
- 相同的输出属性 `identificatie`；
- `max_features_per_tile = 1000`；
- 根 geometric error 2,000 m；
- 3D Tiles 1.1 implicit quadtree 和 GLB 内容；
- 双面、不透明材质。

执行顺序严格为 pg2b3dm 后 Lucy。每条正式流水线之前都重启 PostGIS 容器，等待
数据库 ready/healthy，再执行一次精确 `COUNT(*)` 确认输入行数。没有清空宿主机
文件页缓存，因此这里的“冷”指**新的应用进程和空输出目录**，不是物理磁盘或
操作系统页缓存全冷。

pg2b3dm 是静态批量导出器。Lucy 是动态 HTTP 服务；为取得可比较的全量结果，
本次新增的物化器会：启动新的 release 进程、请求 `tileset.json`、遍历全部
subtree availability、以并发 8 请求全部可用 GLB，并将响应写入 APFS。Lucy 的
40.29 s 因而包含服务器启动、数据库查询、几何处理、loopback HTTP、Python
调度和磁盘写入；pg2b3dm 的 119.15 s 包含自身启动、数据库查询、几何处理和磁盘
写入。Lucy 的外部墙钟为 40.37 s，harness 内部从进程启动到物化完成为
40.29 s；Lucy release 编译不计入正式时间。

## 环境与版本

| 项目 | 值 |
| --- | --- |
| 日期 / 时区 | 2026-07-14 / Asia/Shanghai |
| 主机 | Apple M4 Pro，14 核，48 GiB |
| 操作系统 | macOS 27.0 build 26A5353q，Darwin arm64 |
| Docker | 29.4.0，server arm64 |
| PostgreSQL | 18.4 |
| PostGIS | 3.6.4 |
| GEOS / PROJ | GEOS 3.13.1 / PROJ 9.6.0，network off |
| 数据库镜像 | `lucy-postgis:18-3.6-rdnaptrans2018` |
| Rust / Cargo | 1.94.1 / 1.94.1 |
| Lucy | commit `2057404d50e1c20f68306e38c00124c99058c8a6`，release build |
| pg2b3dm | `2.27.0+5ddd878c42af2e9ea9359c1c3f8c488a45e07211`，原生 macOS arm64 |
| pg2b3dm 归档 | `pg2b3dm-osx-arm64.zip`，SHA-256 `5a0c7dc399157d1949713a000fb2ce5055065a4bc178a03243db9668b7743a6c` |
| 3D Tiles Validator | `3d-tiles-validator@0.6.1`，Node.js 24.14.0 |

没有使用 pg2b3dm 的 Linux 容器，因为现成镜像为 `linux/amd64`，在 arm64 Mac 上
会引入仿真变量；正式测量使用官方发布页的原生 arm64 二进制。

## 输入数据清单

| 指标 | 值 |
| --- | ---: |
| 行数 / distinct `fid` | 574,164 / 574,164 |
| `fid` 范围 | 1--574,167 |
| distinct `identificatie` | 573,085 |
| 重复 `identificatie` 的额外行 | 1,079 |
| 空 / NULL 几何 | 0 / 0 |
| 几何类型 / 维度 / SRID | `MULTIPOLYGON` / 3 / 7415 |
| polygon / ring | 5,116,198 / 5,116,198 |
| interior ring | 0 |
| 输入顶点 | 28,878,767 |
| 源 CRS XY extent | `BOX(101582.5859375 466871.4375,138142.65625 502763.15625)` |
| 源 Z 范围 | -11.4610004425--175.7949981689 |
| relation / heap / index | 715 MiB / 622 MiB / 81 MiB |

Lucy 的显式 `rdnaptrans2018_epsg_1149` 管线对全部输入点扫描所得精确目标范围是：

```text
west       4.605078758701167
south     52.18871559221645
east       5.139178821360162
north     52.511337538603655
min height 31.479081006349062 m
max height 218.742108682605 m
```

benchmark 配置使用向外安全舍入的
`[4.6050787, 52.1887155, 5.1391789, 52.5113376, 31.47, 218.75]`。
这次范围扫描约遍历 2,888 万点，发生在正式重启和计时之前，不计入任一工具。

### 数据谱系限制

当前数据库中存在 99 行修复审计记录，方法为
`make_valid_horizontal_faces_and_split_touching_holes`，`fid` 范围
15,789--553,938，应用时间为 2026-07-13 11:24:55 UTC。旁系 relation 注释提到
“3DBAG v20250903 benchmark relation; 99 features excluded by full Lucy
native-surface contract scan”，但当前 `nl_lod12_3d` 的原始下载 URL、源文件
SHA-256 和导入命令没有保存在仓库或数据库中。因此：

- 本报告准确描述当前 live relation 的后修复快照；
- 不能仅凭本报告从上游字节级重建完全相同的输入；
- 数据应按 3DBAG 的 CC BY 4.0 条款归属，但后续基准应补齐不可变源文件 manifest；
- 当前数据没有 interior ring，所以本次结果不覆盖带洞多边形。

## 可复现命令

### Lucy release build

```bash
cargo build --release -p lucy-poc
```

构建完成后再开始正式计时。本次构建耗时 5.60 s，不计入下表。

### pg2b3dm

```bash
/usr/bin/time -lp /path/to/pg2b3dm \
  --connection 'Host=localhost;Port=5432;Username=<user>;Database=lucy;CommandTimeOut=0' \
  --column geom \
  --table public.nl_lod12_3d \
  --attributecolumns identificatie \
  --max_features_per_tile 1000 \
  --geometricerror 2000 \
  --output /tmp/lucy-benchmark-20260714/pg2b3dm-full-1
```

pg2b3dm 2.27.0 即使连接串不含密码仍会交互提示密码；本次从环境中的本地开发
密码变量写入 PTY，没有把凭据写入命令或报告。其余采用工具默认值：implicit
tiling、quadtree、GLB、double-sided、`ADD`。

### Lucy 冷进程全量物化

```bash
DATABASE_URL='postgres://<user>:<password>@localhost:5432/lucy' \
/usr/bin/time -lp python3 scripts/benchmarks/materialize_lucy.py \
  --server-binary target/release/lucy-poc \
  --config config/benchmark-nl-lod12.yaml \
  --address 127.0.0.1:18080 \
  --source nl_lod12_3d \
  --output /tmp/lucy-benchmark-20260714/lucy-full-1 \
  --metrics-json /tmp/lucy-benchmark-20260714/lucy-full-1.json \
  --server-log /tmp/lucy-benchmark-20260714/lucy-full-1.server.log \
  --concurrency 8
```

Lucy 参数为 `max_level = 7`、`subtree_levels = 2`、
`content_start_level = 3`、`REPLACE`。并发 8 与 Lucy 数据库连接池上限一致。

### GLB 汇总与标准校验

```bash
python3 scripts/benchmarks/summarize_glbs.py \
  /tmp/lucy-benchmark-20260714/pg2b3dm-full-1 \
  --output /tmp/lucy-benchmark-20260714/pg2b3dm-full-1.summary.json

python3 scripts/benchmarks/summarize_glbs.py \
  /tmp/lucy-benchmark-20260714/lucy-full-1 \
  --output /tmp/lucy-benchmark-20260714/lucy-full-1.summary.json

npx 3d-tiles-validator \
  --tilesetFile /tmp/lucy-benchmark-20260714/pg2b3dm-full-1/tileset.json \
  --reportFile /tmp/lucy-benchmark-20260714/pg2b3dm-full-1.validator.json

npx 3d-tiles-validator \
  --tilesetFile /tmp/lucy-benchmark-20260714/lucy-full-1/tileset.json \
  --reportFile /tmp/lucy-benchmark-20260714/lucy-full-1.validator.json
```

`/tmp/lucy-benchmark-20260714` 中的原始输出、日志和 JSON 是本机临时产物，不是
仓库内的长期 benchmark artifact。

## 详细性能结果

### 全量流水线

| 指标 | pg2b3dm | Lucy |
| --- | ---: | ---: |
| 墙钟 `real` | 119.15 s | 40.37 s |
| 工具内部 / harness 物化 | 118.732 s | 40.067 s |
| Lucy 启动至全量完成 | N/A | 40.289 s |
| `user` | 36.23 s | 25.32 s |
| `sys` | 5.74 s | 9.10 s |
| 最大 RSS | 281,690,112 B | 140,574,720 B |
| tileset 文件 / 字节 | 1 / 890 | 1 / 979 |
| subtree 文件 / 字节 | 212 / 78,096 | 2,875 / 1,077,264 |
| GLB 文件 / 字节 | 1,602 / 1,248,258,460 | 10,921 / 2,836,327,044 |
| 最小 / 最大 GLB | 2,868 / 8,511,316 B | 3,544 / 8,264,820 B |
| 全部输出 | 1,248,337,446 B | 2,837,405,287 B |
| 输出吞吐 | 10.48 MB/s | 70.28 MB/s |
| GLB 产出速率 | 13.45/s | 270.52/s |
| 三角形产出速率 | 115,526/s | 720,264/s |

Lucy 的外部 `/usr/bin/time` 覆盖 Python 物化器及其 Lucy 子进程，所以最大 RSS
是这一进程树测量口径下的值。并发请求的单请求耗时总和不能与墙钟相加：2,875
个 subtree 请求中位数 22.57 ms、最大 254.08 ms；10,921 个内容请求中位数
10.57 ms、最大 292.04 ms。

### Lucy 进程冷启动与首内容

每次启动全新的 release Lucy 进程，等 `/health`，随后直接请求固定内容
`content/3/4/1.glb`，读取完整的 2,264,236 字节响应后退出。数据库在这 5 次
探针中保持健康，未清空数据库或 OS cache。

| 次数 | 服务就绪 | GLB 请求 | 启动到响应完成 | 字节 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 209.222 ms | 258.029 ms | 474.990 ms | 2,264,236 |
| 2 | 181.399 ms | 255.863 ms | 446.291 ms | 2,264,236 |
| 3 | 178.864 ms | 255.848 ms | 442.764 ms | 2,264,236 |
| 4 | 181.742 ms | 256.581 ms | 446.553 ms | 2,264,236 |
| 5 | 183.573 ms | 255.694 ms | 447.123 ms | 2,264,236 |
| **中位数** | **181.742 ms** | **255.863 ms** | **446.553 ms** | **2,264,236** |

全量物化器记录的“启动至首次内容”是 11.506 s，但它先发现**所有** subtree，
再并发下载内容；这个数字反映 harness 调度顺序，不代表 Lucy 的真实首内容冷延迟，
因此不作为主要首包指标。

## 输出结构与语义

| 项目 | pg2b3dm | Lucy |
| --- | --- | --- |
| refinement | `ADD` | `REPLACE` |
| content 层级 | 文件名 level 3--7，五个内容深度 | level 3--7 |
| `subtreeLevels` | 4 | 2 |
| `availableLevels` | 5 | 8 |
| root region | `[4.60036325, 52.18738382, 5.14068404, 52.51229001, 31.1998, 219.0996]` 约值 | `[4.6050787, 52.1887155, 5.1391789, 52.5113376, 31.47, 218.75]` |
| 放置 | root ECEF translation + 所有 GLB 相同轴转换 matrix | root ENU-to-ECEF matrix + 每 tile 独立 node matrix |

pg2b3dm 的 root region 更宽，不应直接解释为模型平移错误。对 2.27.0 源码的核查
显示，它用 `ST_Transform(ST_3DExtent(geom), 4979)` 得到保守包围范围，并对经纬度
再扩 1e-6；Lucy 的范围则来自显式 RDNAPTRANS2018 + EPSG:1149 点级管线。
pg2b3dm 的 root translation 为
`[3890115.25, 331484.59375, 5026675]`；Lucy root translation 为
`[3890108.4481771584, 331593.78849051497, 5026712.956889126]`，同时在同一
root matrix 中包含完整局部坐标轴旋转。

pg2b3dm 的 1,602 个 GLB 都使用相同 node 轴转换 matrix；Lucy 的 10,921 个 GLB
具有 10,921 个不同的 tile-local node matrix。两者都是可表达的放置方式，但本次
自动校验只能证明结构合法，不能替代 Cesium 中的视觉高度和位置检查。

### 几何、法线与材质

| 指标 | pg2b3dm | Lucy |
| --- | ---: | ---: |
| mesh / primitive | 1,602 / 1,602 | 10,921 / 10,921 |
| triangles | 13,764,915 | 29,077,068 |
| positions / normals | 40,474,749 / 40,474,749 | 54,696,866 / 54,696,866 |
| primitive attributes | `POSITION`, `NORMAL`, `_FEATURE_ID_0` | `POSITION`, `NORMAL`, `COLOR_0`, `_FEATURE_ID_0` |
| sampled normals | 4,806 | 32,763 |
| sampled normal length | 0.999999877--1.000000117 | 0.999999961--1.000000040 |
| material | white, OPAQUE, double-sided | white base factor + `COLOR_0`, OPAQUE, double-sided |

汇总器对每个 NORMAL accessor 取首、中、尾三个样本；最大单位长度误差分别为
`1.23e-7` 和 `4.01e-8`。这证明抽样法线归一化良好，但不是逐顶点法线扫描。

Lucy 输出更多三角形的主要原因是固定多层 `REPLACE` 内容与 tile 边界裁切；
pg2b3dm 的 `ADD` 内容和自适应树不是同一份 mesh 分块。因而三角形数量不要求相等，
也不能用单一“每三角形耗时”得出算法优劣结论。

### Feature ID 与结构化元数据

| 指标 | pg2b3dm | Lucy |
| --- | ---: | ---: |
| `EXT_mesh_features` GLB | 1,602 | 10,921 |
| `EXT_structural_metadata` GLB | 1,602 | 10,921 |
| property table rows | 584,322 | 1,170,701 |
| 属性 | `identificatie` | `featureId`, `identificatie` |
| unique `identificatie` | 573,085 | 573,085 |
| unique `featureId` | 未输出 | 574,164 |

两个结果的 distinct `identificatie` 都与源表的 573,085 完全一致。Lucy 额外保留
`fid` 为 `featureId`，其 574,164 个唯一值与源表行数完全一致，因此可以证明每个
输入 feature 至少在输出中出现一次。pg2b3dm 只输出非唯一的 `identificatie`；
虽然 distinct 值覆盖一致，但在没有 `fid` 的情况下，不能仅凭它证明 1,079 条
重复标识行逐条覆盖。property table 总行数大于源 feature 数是跨 tile、跨层级
重复的结果，不是输入行数。

## 标准校验

官方 `3d-tiles-validator@0.6.1` 完整遍历了两个 tileset：

| 结果 | pg2b3dm | Lucy |
| --- | ---: | ---: |
| errors | 0 | 0 |
| warnings | 0 | 0 |
| infos | 1,602 | 10,921 |

每个 GLB 都产生一条 info，因为该版本内嵌的 glTF validator 尚不支持
`EXT_structural_metadata` 与 `EXT_mesh_features`，并把由扩展引用的部分
bufferView 视为“可能未使用”。这些是能力提示，不是 3D Tiles error 或 warning。
自建汇总器还验证了全部 GLB header/chunk 长度，两个结果的 invalid GLB 均为 0。

### Cesium 视觉检查状态

本次尝试通过 Codex 内置浏览器打开本地 Cesium demo，但浏览器运行时初始化连续
失败，错误为 `Cannot redefine property: process`。按浏览器控制流程的约束没有切换
到另一套自动化实现。因此位置、高度、比例、渐进 refinement 和颜色的 Cesium
视觉检查状态是**未执行**，不是通过。正式发布前仍需手工或在可用浏览器会话中：

1. 分别加载两个 `tileset.json`，以同一相机位姿截图；
2. 核对荷兰区域水平位置、NAP 高度和建筑比例；
3. 检查 Lucy `REPLACE` 与 pg2b3dm `ADD` 在缩放时是否有闪烁、空洞或重复表面；
4. 抽查 feature picking，特别是重复 `identificatie` 的记录。

## 结果解释与后续建议

本轮足以支持以下结论：

- 两个工具都完成了同一 live relation 的全量生成，且 3D Tiles 标准校验无错误或
  警告；
- Lucy 的 `featureId` 唯一值完整覆盖 574,164 个源 feature，两个结果的
  `identificatie` distinct 集合都完整；
- 在这里定义的全量落盘口径中，Lucy 明显更快，同时生成了更多、更细的内容；
- 因切片/refinement/裁切策略不同，不能把 2.95x 直接宣传为同等工作量的算法
  加速比；它是系统级完成时间比；
- 视觉验证和不可变输入 manifest 是当前报告的两个主要缺口。

下一轮若要得到严格的算法吞吐对比，应冻结带 SHA-256 的原始 3DBAG 子集，补上
带洞样本，再增加一个共同工作量实验：固定同一空间范围、同一层级、同一 feature
集合和等价的 metadata/material，分别只生成对应单层内容。现有全量实验仍应保留，
因为它代表两种产品默认策略下用户实际等待的系统级时间。

## 参考资料

- pg2b3dm getting started：<https://geodan.github.io/pg2b3dm/getting_started.html>
- pg2b3dm v2.27.0：<https://github.com/Geodan/pg2b3dm/releases/tag/v2.27.0>
- Cesium 3D Tiles Validator：<https://github.com/CesiumGS/3d-tiles-validator>
