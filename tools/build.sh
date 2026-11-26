#!/usr/bin/env bash
# 编译 LightBox（脱敏 release）并把产物拷到两处常用位置。
#
# 用法（Git Bash，任意目录都行）：
#     bash tools/build.sh
#
# 做三件事：
#   1. 带着 --remap-path-prefix 编译，exe 里不留 C:\Users\<你的名字>（发版要求）；
#   2. 校验脱敏结果 + 打印版本号、体积、md5；
#   3. 覆盖 dist-app\lightbox.exe 和 E:\software\LightBox\lightbox.exe
#      （后者被正在运行的旧实例占用时，改装成 lightbox-v<版本>.exe 并提示改名）。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

# 脱敏：把编译目录前缀换成 cargo（改这个值会让缓存全量失效一次，之后同值命中）
WINHOME="$(cygpath -w "$HOME")"
export RUSTFLAGS="--remap-path-prefix=${WINHOME}=cargo"

echo "== 编译 =="
if ! cargo build --release --manifest-path "$ROOT/native/Cargo.toml"; then
    echo "!! 编译失败，看上面的报错" >&2
    exit 1
fi

EXE="$ROOT/native/target/release/lightbox.exe"
[ -f "$EXE" ] || { echo "!! 没找到 $EXE" >&2; exit 1; }

echo
echo "== 校验 =="
leak="$(strings -a "$EXE" | grep -c 'C:\\Users' || true)"
echo "脱敏检查（必须为 0）：$leak"
echo "版本：$("$EXE" --version 2>&1)"
echo "体积：$(stat -c %s "$EXE") B"
echo "md5 ：$(md5sum "$EXE" | cut -d' ' -f1)"

echo
echo "== 部署 =="
cp "$EXE" "$ROOT/dist-app/lightbox.exe" && echo "已更新 dist-app/lightbox.exe"

DEST="/e/software/LightBox/lightbox.exe"
if [ -d "$(dirname "$DEST")" ]; then
    if cp "$EXE" "$DEST" 2>/dev/null; then
        echo "已更新 E:\\software\\LightBox\\lightbox.exe"
    else
        ver="$("$EXE" --version 2>&1 | tr -d '\r' | awk '{print $NF}')"
        alt="/e/software/LightBox/lightbox-v${ver}.exe"
        cp "$EXE" "$alt" && {
            echo "旧 exe 正被运行，已另存为 $(basename "$alt")"
            echo "→ 先在托盘退出 LightBox，再把它改名成 lightbox.exe"
        }
    fi
else
    echo "（没找到 E:\\software\\LightBox，跳过这一处）"
fi

echo
echo "完成。"
