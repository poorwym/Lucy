# Lucy Pitch Narrative

> Working draft, 2026-07-15. The positioning and performance claims below are
> grounded in the current repository and the YIMO-127 benchmark. Reconcile the
> architecture wording with YIMO-84 once the Linear document is available.

## 0. 核心结论

Lucy 最值得讲的不是“一个更快的 3D Tiles 转换器”，而是：

> **Lucy 是空间数据库到实时 3D 世界之间的 serving layer。它让 PostGIS 中持续变化的
> 3D 数据，不经过漫长的离线导出，就能以标准 3D Tiles 动态流向人和机器。**

一句话版本：

> **The serving layer for the spatial world model.**

更具体的产品版本：

> **Lucy turns live PostGIS data into streamable 3D Tiles on demand.**

对投资人的版本：

> GIS、数字孪生和世界模型都在创造越来越多的 3D 空间数据，但数据库与实时 3D
> 应用之间仍靠离线文件流水线连接。Lucy 从这个被忽略的 serving gap 切入，成为
> 3D 空间数据的实时交付基础设施。

对客户的版本：

> 你的 3D 数据已经在 PostGIS 里。Lucy 让它直接进入 Cesium 和其他 3D Tiles 客户端，
> 保留坐标、属性和 feature identity，无需为每次更新重跑整套离线导出。

## 1. Pitch 的主线

整套叙事只讲一条因果链：

1. **世界正在被数据库化和三维化。** GIS、数字孪生、机器人和世界模型都需要可计算、
   可持续更新的空间世界，而不只是一次性的 3D 文件。
2. **渲染端和数据端都已经成熟，中间交付层却停留在批处理时代。** 一端是 PostGIS
   等空间数据库，另一端是 Cesium、游戏引擎和 AI 系统，中间通常仍是“全量导出、
   切片、上传、失效、再导出”。
3. **这不是炫技问题，而是 freshness、成本和产品迭代速度问题。** 每次数据变化都可能
   引发新的离线流水线、重复存储和版本管理；越大的场景越难频繁更新。
4. **Lucy 把文件生成改成数据服务。** 它按请求查询 PostGIS，完成坐标转换、几何裁切、
   LOD、metadata 和 GLB 编码，直接提供标准 3D Tiles 1.1 HTTP 接口。
5. **一个很窄的切口证明了一类更大的基础设施能力。** 当前切口是“把 PostGIS 中的
   建筑 footprint 和原生 3D surface 实时变成 3D Tiles”；长期位置是空间世界模型的
   serving plane，而不是另做一个 GIS 编辑器或可视化应用。
6. **性能证明这条路径不只是架构上优雅，而且已经可用。** 在同一份 57.4 万 feature
   的真实 PostGIS relation 上，Lucy 的系统级全量完成时间是对照工具的 1/2.95，
   同时输出更多、更细的内容，并使用约一半峰值内存。

这里最关键的反差是：

> **大家在争夺谁来构建世界模型，却很少有人解决世界模型如何从权威空间数据库持续、
> 正确、低延迟地被交付。**

## 2. “无人在意的问题”到底是什么

不要说“没人做 3D Tiles”。更准确、也更可信的说法是：

> **市场重视建模、数据库和前端可视化，却低估了从 live spatial database 到
> streamable 3D content 的运行时交付问题。**

典型现状：

```text
PostGIS / source data
        |
        v
offline export -> batch tiling -> object storage -> CDN -> viewer
        ^                                      |
        +--------- data changes, rerun --------+
```

Lucy 的目标状态：

```text
PostGIS / source of truth -> Lucy -> 3D Tiles clients
                                \\-> cache / CDN when useful
```

旧流程的隐性成本：

- **不新鲜**：发布的是某次导出的快照，而不是当前数据库状态。
- **重复数据**：数据库、导出中间件、切片结果和多个版本同时存在。
- **更新时间随数据规模增长**：小改动也可能触发大范围重建。
- **语义容易丢失**：feature id、业务属性和坐标语义在格式链路中被压平。
- **工程责任碎片化**：GIS、数据工程、后端和前端团队共同维护一条脆弱流水线。
- **很难服务机器**：世界模型、仿真和 agent 需要按区域、层级和时间持续取得数据，
  一次性静态文件不是理想的数据接口。

## 3. 为什么是现在

“Why now”需要三个趋势同时成立：

1. **需求侧：3D 空间应用从展示走向运行。** 城市数字孪生、基础设施运维、规划、
   仿真和 world model 不再满足于一年更新一次的演示场景。
2. **标准侧：3D Tiles 1.1 + glTF 已经成为可组合的交付协议。** 产品不必自建客户端
   格式生态，可以专注于数据库到标准内容的运行时系统。
3. **供给侧：空间数据库和计算能力足以支持动态生成。** PostGIS 已是大量机构的
   source of truth；高性能 Rust、空间索引和按 tile 查询使 on-demand serving 成为
   可落地的产品，而不是研究项目。

世界模型是放大器，不是当前收入预测的唯一前提：即使不假设机器人或具身智能爆发，
现有 GIS 和数字孪生客户也已经存在同样的数据交付问题。

## 4. 产品是什么

### 当前产品边界

Lucy 是部署在 PostGIS 和 3D 客户端之间的 HTTP middleware：

- 从明确配置的 PostGIS source 读取数据；
- 支持 2.5D footprint extrusion 和原生 `PolygonZ` / `MultiPolygonZ` surface；
- 按 quadtree tile 查询、裁切和生成内容，不强制预先物化整个数据集；
- 输出 3D Tiles 1.1 implicit tiling、binary subtree 和 GLB；
- 保留 feature id、业务属性、颜色和 picking metadata；
- 显式处理 CRS、vertical datum、ECEF/ENU 和 glTF axis；
- 用稀疏 availability 避免客户端请求空分支；
- 对 overflow、无效 geometry 和坐标契约错误显式失败，而不是静默截断。

### 产品价值，不只是功能列表

| 技术能力 | 客户价值 |
| --- | --- |
| On-demand tile generation | 数据更新不再天然等于全量重导出 |
| Direct PostGIS integration | 数据库继续作为唯一权威源 |
| Standard 3D Tiles / GLB | 不锁定专有 viewer，可进入现有 Cesium 生态 |
| CRS and vertical datum correctness | 降低“模型看起来差不多、实际偏了几十米”的项目风险 |
| Feature identity and metadata | 3D 场景仍可查询、拾取并连接业务系统 |
| Sparse implicit tiling | 大范围稀疏数据也能按需遍历 |
| Deterministic clipping and overflow | 可预测地扩展，而不是靠截断掩盖错误 |

### 长期产品位置

不要把长期故事讲成“支持更多格式”。更大的方向是：

> **把空间数据库变成可被人、仿真器和 AI 按位置、层级、时间和语义调用的实时世界接口。**

可以逐步扩展为：

- 多源 federation 和统一空间目录；
- 增量更新、缓存失效与 edge serving；
- 时间维度和版本化场景；
- 权限、租户隔离和审计；
- 栅格、点云、terrain、BIM 与更多 3D geometry；
- 面向机器的 spatial query / context API；
- 托管 control plane 与可观测性。

## 5. 性能证据应该怎么讲

### 一页 headline

在同一台 Apple M4 Pro、同一个 57.4 万 feature 的 PostGIS relation 上：

| 指标 | pg2b3dm 2.27.0 | Lucy | 可说的话 |
| --- | ---: | ---: | --- |
| 全量端到端时间 | 119.15 s | 40.37 s | **Lucy 以 1/2.95 的系统级完成时间结束** |
| 输出数据量 | 1.25 GB | 2.84 GB | **同时输出 2.27x 数据** |
| 三角形 | 13.76 M | 29.08 M | **同时输出 2.11x 三角形** |
| GLB 数量 | 1,602 | 10,921 | **更细粒度的动态内容** |
| 峰值常驻内存 | 268.6 MiB | 134.1 MiB | **约一半内存** |
| Validator | 0 error / 0 warning | 0 error / 0 warning | **速度没有牺牲标准合法性** |

另一个适合现场 demo 的数字：冷进程中位数 181.7 ms ready，随后完整返回一个
2.26 MB GLB 用时 255.9 ms；从进程启动到响应完成中位数 446.6 ms。

### 推荐话术

> 我们拿 57.4 万个真实 3D building features 做了从 PostGIS 到完整落盘结果的
> 端到端比较。Lucy 在 40.37 秒完成，对照工具是 119.15 秒。更重要的是，Lucy
> 这个结果不是以更少输出换来的：它生成了 2.27 倍字节、2.11 倍三角形和 6.82 倍 GLB，
> 峰值内存仍只有约一半。两个结果都通过官方 3D Tiles Validator，0 error、
> 0 warning。

### 必须保留的脚注

这不是等切片工作量下的算法微基准。两者使用不同的 tree、refinement 和 clipping
策略；`2.95x` 是两种产品配置下用户实际等待的**系统级全量完成时间比**，不能写成
“核心算法严格快 2.95 倍”。完整条件见
[`docs/benchmarks/yimo-127-sibbe.md`](benchmarks/yimo-127-sibbe.md)。

### 现在不要说

- “Lucy 在所有数据集上都快 3 倍。”
- “Lucy 每个 triangle 的算法性能高 6 倍。”
- “已经证明 Cesium 中完全无视觉问题。”本轮视觉检查尚未完成。
- “这是可完全复现的冻结 Sibbe 数据集。”当前缺原始下载 manifest 和 SHA-256。
- “零存储成本。”动态生成避免强制全量物化，但生产系统仍可能需要 cache/CDN。

## 6. 市场故事：如何把“百亿级”讲扎实

### 不要过早把新类别算成传统 SaaS

Lucy 的商业化路径仍在形成，此时用一个很窄的 ICP、账户数和 ACV 去反推 TAM，会把
一个潜在的基础设施类别过早锚定成“小型 3D Tiles middleware”。更适合融资阶段的
讲法是先定义一个足够大的 **market envelope**，再证明 Lucy 有一个具体、可信的
进入点。

世界模型是一个真实且强烈的资本信号，但目前不是边界稳定的采购分类。World Labs
在 2026 年 2 月宣布获得 10 亿美元新融资，说明资本愿意为 spatial intelligence 的
长期平台价值下注；同一家公司也明确指出，“world model”是当前 AI 领域最重要、也
最被滥用的术语之一，并把产品功能拆为 renderer、simulator 和 planner。这种模糊性
恰恰允许 Lucy 从更广义的 world infrastructure 定义自己的长期市场。

- 融资信号：<https://www.worldlabs.ai/blog/funding-2026>
- 功能分类：<https://www.worldlabs.ai/blog/taxonomy-of-world-models>

Lucy 不需要声称自己是生成式 world model。更有延展性的位置是：

> **World models need a world data plane. Lucy serves authoritative 3D state.**

Lucy 把经过测绘、传感器和业务系统验证的空间状态，转换成可流式访问、保留 geometry
和 semantics 的机器可读世界。无论最终消费者是地图、数字孪生、simulator、renderer、
planner 还是 agent，都需要某种 world-state serving layer。

### 把 Maps、数字孪生和世界模型讲成同一个演进

可以采用一个宽定义：

> **A world model is a persistent, machine-readable representation of the world,
> continuously updated from data and usable for observation, simulation or action.**

在这个定义下：

- Google Maps 是早期的全球 world model：持续更新、地理定位、包含道路、地点、交通
  和越来越丰富的三维语义；
- GIS 是政府和企业维护的 authoritative world model；
- 数字孪生是某个城市、工厂或基础设施的 private world model；
- 游戏和工业仿真是 synthetic / simulated world model；
- 自动驾驶、机器人和 physical AI 正在构建 predictive and actionable world model；
- 新一代生成模型则让世界可以从图像、视频和文本中生成。

它们不是五个互不相关的垂直市场，而是同一件事的不同阶段：**世界正在从给人看的
地图和模型，变成可被机器持续读取、模拟和行动的数据系统。**

### 重叠不是问题，伪精确才是问题

GIS、数字孪生和 world model 本来就在重叠，这正是“市场正在汇合”的证据。可以把
它们的市场规模并列作为 category signals，但不要在台面上写一道加法题，声称得到一个
互斥、可审计的 TAM。更好的讲法是：

> Lucy 位于多个增长市场正在汇合的共同基础设施层；这个 market envelope 用来说明
> category potential，而不是预测某一年可以获得的收入。

### 用 market envelope，而不是假精确的 TAM

融资 deck 可以直接把市场定义为：

> **The $100B+ market for machine-readable worlds and spatial intelligence.**

这个 envelope 包含正在汇合的几类支出：

| 已经存在的市场 | 正在发生的迁移 | 新增的资本方向 |
| --- | --- | --- |
| GIS、mapping、geospatial data | 3D city、digital twin、industrial simulation | world models、spatial AI、physical AI |

这里的 `$100B+` 是宽口径类别规模，不是 Lucy 明年的可服务收入，也不是把几份重叠
报告机械相加后的精确 TAM。正式材料可以用 3-4 个第三方市场数据证明每个组成部分都
足够大，在脚注中写明 categories overlap。主页面只保留一个结论：

> **A hundred-billion-dollar software and infrastructure stack is converging
> around machine-readable representations of the physical world.**

Lucy 的规模不需要用“服务层占整个市场固定百分比”来计算。更好的类比是：Snowflake
不需要拥有 analytics application 市场，Cloudflare 不需要拥有整个互联网应用市场，
它们通过成为多个应用共同依赖的数据或交付层形成独立规模。Lucy 的长期命题同样是：
每一个持续运行的 world model 都需要读取、组织和交付 world state。

### Wedge 不等于市场边界

市场讲宽，切口必须讲窄：

```text
Entry wedge:     PostGIS -> on-demand 3D Tiles
Platform:        live spatial data serving
Category vision: data plane for machine-readable worlds
```

这允许商业模式继续探索。Lucy 最终可能按 runtime、compute、data volume、enterprise
deployment、OEM 或 managed service 收费；融资阶段不需要假装已经知道唯一答案。需要
证明的是：当前 wedge 解决真实问题、技术能力可以沿同一条轴扩展、每个更大的相邻市场
都需要同一个底层 serving primitive。

### Top-down 数据的正确用途

正式 deck 只选 2-3 个最新、可追溯的第三方数据点：

- GIS software / geospatial analytics 支出；
- digital twin platform / software 支出；
- 3D geospatial / reality capture data 的增长；
- 3D Tiles、Cesium、PostGIS 或相关开源生态的采用信号。

它们用于证明 `$100B+` market envelope，而不是精确预测 Lucy 收入。每个数字写明
报告机构、年份、币种、市场定义和 URL；允许类别重叠，但要在脚注中明确这是方向性
口径，不要伪装成互斥、可加总的审计数字。

## 7. ICP、切口与 beachhead

### 最优先 ICP

**已有 PostGIS + 已有 3D geometry + 正在用 Cesium/3D Tiles + 数据会持续更新**的团队。

这些条件非常重要：没有 PostGIS 的客户会拉长集成周期；只有静态展示需求的客户不会
充分感受到动态 serving 的价值。

### 首个高价值 use case

> 3D city / digital twin 团队把大规模建筑和基础设施数据保存在 PostGIS，需要在
> web 端按需浏览、拾取并持续更新，但当前 batch tiling 太慢、太重、太难运维。

选择它的原因：

- 数据量大，性能差异容易显现；
- 更新和版本问题真实存在；
- 3D Tiles 客户端成熟，减少教育成本；
- PostGIS、CRS 和 metadata 都是 Lucy 已有能力；
- 建筑只是起点，后续可自然进入 utility、terrain、point cloud 和 simulation。

### 进入客户的 wedge

不是要求客户重建平台，而是：

1. 给一张真实 PostGIS table 和 bounds；
2. Lucy 在客户环境中启动一个标准 tileset endpoint；
3. 用同一 Cesium viewer 对比现有离线 pipeline；
4. 测量 time-to-first-content、全量时间、内存、更新延迟和运维步骤；
5. 从一个 source 扩展到多 source、生产 SLA 和权限。

## 8. 商业模式

推荐从“open core / source-available data plane + commercial control plane”方向验证，
但在确认开源策略前不要在对外 deck 中承诺具体 license。

可销售的价值层：

- **Enterprise runtime**：生产部署、更多 connector、cache、HA、auth、audit；
- **Managed Lucy**：按 source、compute、egress 或生成量计费；
- **Control plane**：source catalog、deployment、observability、policy、版本和失效管理；
- **Enterprise support**：SLA、升级、数据契约审计和性能 tuning；
- **OEM / embedded**：卖给数字孪生平台和 GIS integrator，按部署或终端客户收费。

早期定价应围绕替代成本验证，而不是 CPU 成本：客户当前维护 batch pipeline 的人力、
更新等待、重复存储和项目延期，通常比单次生成的 compute 更贵。

## 9. GTM

### Phase 1: design partners

找 3-5 个具备真实大数据集的团队，每家只承诺一个可量化 outcome：

- 从每日/每周 batch 更新变成分钟级或按请求可见；
- 删除一条自建 exporter + tiler pipeline；
- 在相同硬件上降低生成时间和内存；
- 保留 feature identity 和业务 metadata；
- 把一个新的 PostGIS source 上线时间从若干天降到若干小时。

### Phase 2: ecosystem distribution

- 做好 PostGIS + Cesium 的 reference deployment；
- 发布可复现 benchmark corpus，而不是只发一张快 3 倍的图；
- 与 3D city data provider、Cesium integrator、BIM/GIS consultancy 合作；
- 用开源 data plane 进入工程团队，用 enterprise features 完成采购；
- 优先兼容客户已有 cloud、Kubernetes、database 和 object storage。

### Phase 3: platform expansion

在同一个客户内部从 building source 扩到 utilities、terrain、point cloud、time series
和 machine-facing spatial context，提升 ACV 和替换成本。

## 10. 护城河

“Rust 所以快”不是护城河。真正可积累的壁垒是：

1. **空间正确性 corpus**：CRS、vertical datum、极端经度、deep tile precision、
   vertical surface、holes、invalid topology 和边界 ownership 的真实测试集。
2. **数据库到 mesh 的 query/geometry co-design**：哪些工作交给 PostGIS broad phase，
   哪些必须在 3D core 中精确完成，直接决定正确性和性能。
3. **运行时调度和缓存数据**：真实 workload 下的 hot tile、source 更新、失效策略和
   cost model 会持续优化 serving engine。
4. **标准与生态兼容**：3D Tiles、glTF metadata、Cesium 和主流空间数据库的长期兼容。
5. **部署信任**：政府、城市、utility 和防务数据往往不能离开客户环境；可审计、
   deterministic、self-hosted 的 data plane 是重要门槛。
6. **迁移成本来自接口成为基础设施**：一旦多个 viewer、业务系统和模型依赖同一个
   spatial serving endpoint，Lucy 就不再是一次性转换工具。

## 11. 竞争框架

不要只做 feature checklist。用产品范式区分：

| 路径 | 优势 | 结构性限制 | Lucy 的位置 |
| --- | --- | --- | --- |
| Offline exporter / tiler | 简单、成熟、适合静态发布 | 更新需重跑，产生副本，运行时不可查询源数据 | 把生成变成在线服务 |
| GIS server extension | 与既有 GIS 产品集成 | 可能受专有栈、格式或许可约束 | 开放标准、PostGIS-first middleware |
| Digital twin platform | 完整应用和 UI | 重、贵、替换现有系统 | 不抢应用层，成为其 data plane |
| Custom in-house pipeline | 完全贴合单个项目 | 重复开发、难维护、性能和正确性依赖个人 | 产品化共同基础设施 |
| Object storage + CDN | 静态分发高效 | 不解决源数据更新和生成 | 可作为 Lucy 后面的 cache/distribution 层 |

核心竞争句：

> **Lucy 不替代 PostGIS、Cesium 或 CDN；它把三者连接成一条实时 3D 数据路径。**

## 12. 建议 deck：12 页

### 1. Title

**Lucy — The serving layer for the spatial world model**

副标题：Live PostGIS data in, streamable 3D Tiles out.

### 2. The shift

世界从“地图和 3D 文件”走向“持续更新、可被人和机器查询的空间世界模型”。用一张
数据库、城市、viewer/robot 的图表达，不要先堆市场数字。

### 3. The missing layer

展示 batch export 链路和更新循环。标题：

**The 3D world is live. Its delivery pipeline is still offline.**

### 4. Pain

只放四个结果：stale data、duplicated storage、slow iteration、lost semantics。最好
配一个真实客户 workflow 和等待时间，而不是抽象 icon。

### 5. Solution

PostGIS -> Lucy -> any 3D Tiles client。展示一次请求内部的 query、transform、clip、
LOD、encode，但不要把架构图画成十几个 box。

### 6. Demo / product

同一个 live table：修改一条 feature，再由客户端取得更新内容。现场展示 feature
picking 和 metadata，证明这不是一张 baked mesh。

### 7. Proof

用 benchmark 的 40.37 s vs 119.15 s、2.27x output、50% memory、0 validator errors。
脚注写明 system-level comparison 和不同 tiling strategy。

### 8. Beachhead

ICP：PostGIS + 3D data + Cesium + frequent updates。列 3D city、utility、mapping data
provider、integrator 四类，不要一口气覆盖所有具身智能公司。

### 9. Market

把 GIS、Maps、digital twin、simulation 和 physical AI 画成一条向 machine-readable
world 收敛的演进，而不是几个孤立的圆。headline 写：**The $100B+ market for
machine-readable worlds and spatial intelligence.** 脚注注明这是包含重叠相邻类别的
market envelope。World Labs 的 10 亿美元融资用于证明 timing；PostGIS -> 3D Tiles
用于证明 Lucy 有一个具体 wedge；不在这一页承诺尚未验证的 ACV 或五年 SOM。

### 10. Business and GTM

Land with one source, expand across data types and deployments。收入来自 enterprise
runtime、managed control plane、support 和 OEM。

### 11. Moat and roadmap

护城河只保留三项：spatial correctness corpus、query/geometry engine、deployment and
workload data。路线从 buildings -> multi-source -> temporal/machine APIs。

### 12. Team and ask

团队为什么能同时做 database、geometry、graphics 和 infra。Ask 要具体：融资额度、
可支撑月份、要拿下的 design partners、产品里程碑和收入验证目标。

## 13. 30 秒话术

> 今天的 GIS、数字孪生和世界模型都需要持续更新的 3D 空间数据，但数据库到 3D
> 应用之间仍然靠离线导出、切片和上传。Lucy 是这个缺失的 serving layer：它直接
> 连接 PostGIS，按需生成标准 3D Tiles，并保留正确坐标和业务语义。在 57.4 万个
> 真实 3D building features 上，Lucy 的端到端完成时间是现有对照工具的 1/2.95，
> 同时生成 2.27 倍数据、只用约一半内存。我们先服务 3D city 和 digital twin 团队，
> 长期成为空间世界模型的数据交付基础设施。

## 14. 2 分钟话术

> 世界正在被三维化。城市、基础设施、机器人和 AI 都在构建自己的 world model。
> 但一个很基础的问题被忽略了：这些 3D 数据通常已经存在 PostGIS 等空间数据库里，
> 真正进入 Cesium、仿真器或机器系统时，却仍要先全量导出、离线切片、上传到对象
> 存储。数据一更新，流水线就重来一次。结果是场景永远落后于 source of truth，
> 团队还要维护多份数据和一条脆弱的 ETL。
>
> Lucy 把这条离线流水线变成一个实时服务。它直接连接 PostGIS，根据客户端请求的
> 区域和层级完成空间查询、坐标转换、3D 几何裁切、LOD、metadata 和 GLB 编码，
> 输出开放的 3D Tiles 1.1。客户不需要更换数据库、viewer 或业务应用，只是在中间
> 增加一个高性能 serving layer。
>
> 这件事最难的也不只是编码速度。真实数据包含垂直基准、复杂 CRS、跨 tile surface、
> 深层级浮点精度和 feature identity。Lucy 已经把这些数据库、几何和图形问题放在
> 一个确定性的 pipeline 里。在同一份 57.4 万 feature 的真实 PostGIS 数据上，Lucy
> 40.37 秒完成全量系统级输出，对照工具需要 119.15 秒；Lucy 同时输出 2.27 倍数据、
> 2.11 倍三角形，峰值内存约一半，并通过 3D Tiles Validator 的 0 error、0 warning
> 校验。
>
> 我们从一个非常具体的客户切口进入：已经用 PostGIS 和 Cesium、数据持续更新的
> 3D city 与 digital twin 团队。这个切口本身可以形成企业基础设施生意；随着空间
> 数据成为人和 AI 的共同上下文，Lucy 可以继续扩展成 world model 的 serving plane。

## 15. 常见反对意见

### “为什么不提前切好放 CDN？”

静态、低频更新的数据应该继续预生成并使用 CDN。Lucy 的价值在于数据大、变化频繁、
source 多或语义查询重要的场景。长期产品也可以把动态生成与 cache/CDN 组合，而不是
二选一。

### “PostGIS / Cesium 以后自己做怎么办？”

Lucy 的机会来自跨层复杂性：数据库只负责空间数据，不会自然拥有完整的 3D Tiles、
glTF、LOD、metadata 和部署产品；viewer 也不应绑定某一种数据库。Lucy 保持开放标准
接口，并通过多 source、多 client 和生产运维能力扩大中间层价值。

### “这是不是一个很小的 converter？”

如果产品停在 CLI 全量导出，就是 converter。Lucy 的产品边界是长期运行的 HTTP
serving layer：按请求访问 live database、管理 availability、geometry、metadata、
cache、policy 和 observability。收入和护城河也必须围绕 runtime，而不是一次性转换。

### “动态生成会不会太贵或延迟太高？”

不是所有 tile 都必须每次重算。正确架构是 source-aware invalidation + memory/object
cache + CDN；冷路径动态生成，热路径直接命中。当前 benchmark 已证明冷生成具备可用
性能，下一阶段要用真实访问分布证明 cache hit rate、P95 latency 和单位成本。

### “大厂也能做。”

能做不等于会优先做。这个问题横跨 spatial database、computational geometry、
graphics standards 和 infrastructure，单个项目往往不值得重复投入；专门产品可以把
真实数据 correctness、benchmark 和部署经验持续复用。需要用客户 adoption 建立事实，
不能只靠技术复杂度声称永久壁垒。

## 16. 下一步需要补齐的证据

按融资材料的优先级排序：

1. **客户证据**：20-30 个 ICP 访谈，记录现有 pipeline、更新频率、等待时间、团队
   人数、预算 owner 和采购路径。
2. **Design partner**：至少 3 家，用真实 source 给出 before/after 指标和 testimonial。
3. **严格性能实验**：补同范围、同层级、同 feature、等价 metadata/material 的共同
   工作量 microbenchmark，和现有 system benchmark 并列。
4. **视觉与 correctness**：完成 Cesium 自动化/手工验收，加入冻结输入 manifest、
   holes、更多 CRS、时间变化和恶意 geometry corpus。
5. **生产指标**：P50/P95/P99 tile latency、并发扩展、cache hit rate、数据库负载、
   单 GB / 单 million triangles 成本、更新到可见的 latency。
6. **市场数据**：为 target account count 和 ACV 找公开来源，并用访谈验证，不叠加
   重复的行业 TAM。
7. **商业验证**：至少一个付费 pilot，证明客户愿意为 freshness、简化运维和 SLA
   付费，而不只是认可 benchmark。

## 17. Pitch 语言纪律

始终区分三层表述：

- **已证明**：当前代码、测试或 benchmark 能直接支持；
- **合理推断**：从产品能力推导，但还没有客户或生产数据；
- **长期愿景**：world model serving plane，不冒充当前产品收入。

推荐用词：

- live spatial data
- on-demand 3D serving
- database remains the source of truth
- open-standard delivery layer
- system-level end-to-end result
- spatial correctness, not just rendering

避免用词：

- “颠覆所有 GIS”
- “没有竞争对手”
- “世界最快”
- “零 ETL / 零存储 / 无限扩展”
- “world model platform”而不解释当前具体产品
- 把多个重叠市场报告简单相加

最终要让听众记住三件事：

1. **一个被忽略的基础设施断层**：live database 到 streamable 3D world；
2. **一个极具体的产品切口**：PostGIS -> on-demand 3D Tiles；
3. **一组可信的技术证据**：57.4 万 features，1/2.95 完成时间，2.27x 输出，约一半
   内存，0 validator errors/warnings。
