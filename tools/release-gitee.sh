#!/usr/bin/env bash
# 一键发版：打 tag → push → 建 Gitee 发行版 → 上传 exe 附件
#
# 用法：
#   export GITEE_TOKEN=<私人令牌>        # 只需配一次（Gitee → 设置 → 私人令牌，勾 projects）
#   ./tools/release-gitee.sh             # 版本号自动读 src-tauri/Cargo.toml
#   ./tools/release-gitee.sh --dry-run   # 只打印将要做什么，不联网
#
# 前置：dist-app/lightbox.exe 已是最新（先跑 npm run build:exe）
# 说明：exe 不进 git 历史，只作为发行版附件存 1 GB 配额，每版约 3.85 MB。
set -euo pipefail
cd "$(dirname "$0")/.."

# 中文版本说明必须以 UTF-8 字节上传，否则 Gitee 会按 UTF-8 解析 GBK 字节，
# 整段变成一片 "�"（v0.9.27 / v0.9.28 就这么坏的，v0.9.29 才正常）。
# Python 在中文 Windows 下 stdout 默认 cp936，必须强制 UTF-8 模式。
export PYTHONUTF8=1
export PYTHONIOENCODING=utf-8

DRY=0
[ "${1:-}" = "--dry-run" ] && DRY=1

VERSION=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' src-tauri/Cargo.toml | head -1)
TAG="v$VERSION"
EXE="dist-app/lightbox.exe"

die() { echo "✗ $*" >&2; exit 1; }

[ -n "$VERSION" ] || die "读不到 src-tauri/Cargo.toml 里的 version"
[ -f "$EXE" ] || die "找不到 $EXE —— 先跑 npm run build:exe"

# 从 origin 解析 owner/repo，避免写死
REMOTE=$(git remote get-url origin 2>/dev/null) || die "没有 origin，先 git remote add origin <仓库地址>"
# 先去掉可能的 .git 后缀，再取出 owner/repo
CLEAN=$(printf '%s' "$REMOTE" | sed -E 's#\.git$##')
SLUG=$(printf '%s' "$CLEAN" | sed -E 's#.*gitee\.com[:/]([^/]+/[^/]+)$#\1#')
OWNER=${SLUG%%/*}
REPO=${SLUG##*/}
[ "$OWNER" != "$SLUG" ] || die "origin 不是 Gitee 地址：$REMOTE（当前 $SLUG）"

echo "仓库   : $OWNER/$REPO"
echo "版本   : $VERSION  (tag $TAG)"
echo "附件   : $EXE ($(du -h "$EXE" | cut -f1))"

# 工作区必须干净，否则打出来的 tag 和源码对不上
if [ -n "$(git status --porcelain)" ]; then
    git status --short
    die "工作区有未提交改动，先 commit 再发版"
fi

# 版本说明：优先取站点清单里的 notes，退化成一行默认文案
NOTES=$(python - "$TAG" <<'PY' 2>/dev/null || true
import json, sys
tag = sys.argv[1].lstrip("v")
try:
    v = json.load(open("update-site/version.json", encoding="utf-8"))
except Exception:
    sys.exit(0)
if str(v.get("latest", "")) != tag:
    sys.exit(0)
lines = v.get("notes") or []
print("\n".join(f"- {x}" for x in lines))
PY
)
[ -n "$NOTES" ] || NOTES="- 版本 $VERSION"

if [ "$DRY" = 0 ] && [ -z "${GITEE_TOKEN:-}" ]; then
    die "缺少 GITEE_TOKEN（Gitee → 设置 → 私人令牌，勾选 projects 权限）"
fi

if [ "$DRY" = 1 ]; then
    echo "--- dry-run ---"
    echo "git tag $TAG && git push origin master --tags"
    echo "create release $TAG, body:"
    echo "$NOTES"
    echo "upload $EXE"
    exit 0
fi

# 1) tag + push
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    echo "· tag $TAG 已存在，跳过"
else
    git tag -a "$TAG" -m "LightBox $VERSION"
    echo "· 已建 tag $TAG"
fi
git push origin HEAD --tags
echo "· 已推送"

API="https://gitee.com/api/v5/repos/$OWNER/$REPO"

# 2) 复用已有发行版，没有才创建（重复执行不会报错）
RID=$(curl -sS "$API/releases/tags/$TAG?access_token=$GITEE_TOKEN" |
    python -c "import json,sys;d=json.load(sys.stdin);print(d.get('id',''))" 2>/dev/null || true)

if [ -z "$RID" ]; then
    RID=$(curl -sS -X POST "$API/releases" \
        --data-urlencode "access_token=$GITEE_TOKEN" \
        --data-urlencode "tag_name=$TAG" \
        --data-urlencode "name=LightBox $VERSION" \
        --data-urlencode "body@-" \
        --data-urlencode "target_commitish=master" <<<"$NOTES" |
        python -c "import json,sys;d=json.load(sys.stdin);print(d.get('id',''))" 2>/dev/null || true)
    [ -n "$RID" ] || die "创建发行版失败（令牌权限不够？需要 projects）"
    echo "· 已创建发行版 id=$RID"
else
    echo "· 复用已有发行版 id=$RID"
fi

# 3) 上传附件
NAME="lightbox-$VERSION.exe"
UP=$(curl -sS -X POST "$API/releases/$RID/attach_files" \
    -F "access_token=$GITEE_TOKEN" \
    -F "file=@$EXE;filename=$NAME")
URL=$(printf '%s' "$UP" | python -c "import json,sys;print(json.load(sys.stdin).get('browser_download_url',''))" 2>/dev/null || true)
[ -n "$URL" ] || { echo "$UP"; die "上传失败（单文件需 <100 MB）"; }

# 4) 回读校验：说明正文不该出现 U+FFFD（编码坏掉的标志）
BAD=$(curl -sS "$API/releases/tags/$TAG?access_token=$GITEE_TOKEN" |
    python -c "import json,sys;print((json.load(sys.stdin).get('body') or '').count('\ufffd'))" 2>/dev/null || echo 0)
if [ "$BAD" != "0" ]; then
    echo "⚠ 发行版说明里有 $BAD 个乱码字符（编码问题），跑 tools/fix_release_body.py 修"
else
    echo "· 说明正文编码正常"
fi

echo
echo "✓ 发版完成"
echo "  发行版页面 : https://gitee.com/$OWNER/$REPO/releases/tag/$TAG"
echo "  直链       : $URL"
echo
echo "（可选）把直链填进 update-site/version.json 的 download 字段，或直接改站点首页按钮"
