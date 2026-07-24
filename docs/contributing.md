# Contributing

感谢你对 Atim 的贡献兴趣！

## 构建项目

```bash
git clone https://github.com/zitsen/atim.git
cd atim
cargo build --release
```

### 依赖

- [Rust](https://rustup.rs/) stable（edition 2024）
- [tmux](https://github.com/tmux/tmux)
- [zoxide](https://github.com/ajeetdsouza/zoxide)（可选，用于目录浏览）

## 运行测试

```bash
# 运行所有测试
cargo test

# Clippy 检查（CI 必需）
cargo clippy --all-targets -- -D warnings

# 格式检查
cargo fmt --all --check
```

## 提交规范

使用 [Conventional Commits](https://www.conventionalcommits.org/)：

```
feat: add new feature
fix: resolve bug
docs: update documentation
refactor: restructure code without changing behavior
test: add or update tests
chore: build, CI, deps, tooling
perf: performance improvement
```

## 提交流程

1. Fork 仓库
2. 创建功能分支：`git checkout -b feat/my-feature`
3. 进行修改并添加测试
4. 运行 `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`
5. 提交：`git commit -m "feat: add my feature"`
6. 发起 Pull Request 到 `main`

## 扩展 Atim

### 添加新的 IM 后端

实现 `ImAdapter` trait — 参见 `crates/atim-im/src/` 中的示例。

### 添加新的 Agent

实现 `AgentParser` trait — 参见 `crates/atim-core/src/agent/` 中的示例。

## 许可证

贡献的代码将按照 [MIT 许可证](https://github.com/zitsen/atim/blob/main/LICENSE) 授权。
