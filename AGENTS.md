# RULES

## 判断与执行
- 激进范围（全方位重构、清冗余、重划职责、不留兼容）按字面执行，不降级
- 方法论是思路非目录模板：package by feature、契约收敛位置、Capability/Intent/Domain 均按项目裁量，示例路径仅候选
- Greenfield：单文件起步，walking skeleton 先行，此后 thin vertical slice 增量；禁 enterprise FizzBuzz（棕地新增功能同样适用）
- Brownfield：smallest diff，复用已有工具函数，禁非必要新文件/新依赖

## 架构不变量
- Package by feature：一功能一自包含目录，禁按技术层横切；不要求一功能一个 Cargo package
- core/ 仅契约+装配，禁反向 import；依赖方向只能是 feature → core
- 模块单一入口、自注册、依赖显式声明；跨模块仅经 core 契约/事件总线，禁深路径 import
- Deep module：窄接口，复杂度藏内部；信息隐藏，禁 leaky abstraction
- 声明式仅限结构面：schema/装配/注册/路由 一处定义，派生一切结构产物；禁手写重复装配与手工注册清单；业务逻辑保持命令式，禁 inner-platform effect
- SSOT 减少边界内重复，不取消边界校验

## 失效模式（点名禁止）
- YAGNI：speculative generality / premature abstraction / 预留接口，无当前业务价值不引入
- Second-system effect：重写=行为对齐，废弃功能列清单待确认，禁默认移植
- DRY；Rule of three 方许抽象

## 验收
- 删除测试：rm -rf 任一 feature → 编译+测试全绿，其余无感；共享契约和显式依赖除外
- 局部性：功能需求改动限于单个 feature；必要的契约变更除外
- 接入点最小化：新模块只改必要的契约、装配或注册接入点，不因形式上的零接触拆分实现
- 依赖方向：core 不 import feature；跨模块无深路径 import
- 结构面：schema/装配/注册/路由一处定义，无手写重复注册清单

## 输出
- 实现细节（入口文件名、注册机制、命名风格）委托目标语言惯例
- Idiomatic；clarity over cleverness，禁 golf
