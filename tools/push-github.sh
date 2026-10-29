#!/usr/bin/env bash
# 把本仓库镜像到 GitHub（GitHub 侧的主分支叫 main，本地叫 master）
#
# 用法：
#   export GH_TOKEN=ghp_xxx      # classic token，勾 repo；不配则走系统凭据管理器
#   ./tools/push-github.sh
#
# 为什么单独一个脚本：
#  - 本地分支是 master（Gitee 侧、以及 exe 里嵌的 raw/master 地址都靠这个名字），
#    而 GitHub 新建仓库默认 main，所以要用 refspec 映射，不能直接 `git push github master`
#    （那会在 GitHub 上多出一个 master 分支，两边长期各推各的）。
#  - 代理是通过 HTTP CONNECT 走的，schannel 的证书吊销检查在 CONNECT 后必然失败
#    （CRYPT_E_NO_REVOCATION_CHECK），所以显式关掉。
#
# GitHub 只是**代码镜像**：安装包发布和自动更新仍然只走 Gitee。
# exe 里的检查更新地址是 `include_str!` 编译期嵌进去的，存量版本改不了，
# 仓库里 `src-tauri/update-url.txt` 也绝对不能指向 GitHub。
set -euo pipefail
cd "$(dirname "$0")/.."

git remote get-url github >/dev/null 2>&1 || {
    echo "✗ 没有 github remote，先：git remote add github https://github.com/<你>/lightbox.git"
    exit 1
}

REPO=$(git remote get-url github | sed -E 's#\.git$##' |
    sed -E 's#.*github\.com[:/]([^/]+/[^/]+)$#\1#')
MAIN_BRANCH=${GITHUB_BRANCH:-main}
SRC_BRANCH=$(git rev-parse --abbrev-ref HEAD)

[ "$SRC_BRANCH" = "master" ] || echo "· 当前在 $SRC_BRANCH，本地主分支是 master（exe 的更新地址依赖这个名字）"

echo "镜像仓库 : $REPO"
echo "分支映射 : $SRC_BRANCH → $MAIN_BRANCH"

# 工作区脏也允许推 —— 推的是**已提交**的历史，未提交改动本来就不会进去
if [ -n "$(git status --porcelain)" ]; then
    echo "· 工作区有未提交改动（不会推送，只推已提交的提交）"
fi

PUSH_URL=$(git remote get-url github)
GITOPTS=(-c credential.helper=)
if [ -n "${GH_TOKEN:-}" ]; then
    PUSH_URL=$(printf '%s' "$PUSH_URL" |
        sed -E "s#https://([^@/]+@)?github\.com/#https://x-access-token:${GH_TOKEN}@github.com/#")
    GITOPTS+=(-c http.schannelCheckRevoke=false)
fi

git "${GITOPTS[@]}" push "$PUSH_URL" "refs/heads/${SRC_BRANCH}:refs/heads/${MAIN_BRANCH}"
git "${GITOPTS[@]}" push "$PUSH_URL" --tags

echo "✓ 已同步到 GitHub：https://github.com/$REPO (分支 $MAIN_BRANCH)"
