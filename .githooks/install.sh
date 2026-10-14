#!/usr/bin/env bash
# 启用仓库 hooks（clone 后执行一次）：git config core.hooksPath .githooks
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
echo "已启用 .githooks（pre-commit: fmt+clippy；pre-push: test + Windows 交叉检查）"
