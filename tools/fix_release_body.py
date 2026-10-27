#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""修复 Gitee 发行版说明的乱码：用 UTF-8 的 body 覆盖旧的 GBK 字节。

起因：老版本发版时 Python print 走 cp936，中文变成 GBK 字节，
      curl --data-urlencode 原样上传，Gitee 按 UTF-8 存 → 整段变成 �。

用法：
  python tools/fix_release_body.py --dry-run        # 只打印将要写入的内容
  python tools/fix_release_body.py                  # 真正覆盖并回读校验
环境变量 GITEE_TOKEN 必须设置。
"""
import io
import json
import os
import re
import sys
import urllib.parse
import urllib.request

OWNER, REPO = "gathon", "lightbox"
API = f"https://gitee.com/api/v5/repos/{OWNER}/{REPO}"
TARGETS = ["0.9.28", "0.9.27"]

# ---------- 1) 从 docs/version_list.md 抽指定版本的条目 ----------

def extract(version):
    path = os.path.join(os.path.dirname(__file__), "..", "docs", "version_list.md")
    with io.open(path, encoding="utf-8") as f:
        text = f.read()
    m = re.search(r"^## v%s\s*$(.*?)(?=^## v|\Z)" % re.escape(version), text, re.M | re.S)
    if not m:
        return []
    bullets, cur = [], None
    for raw in m.group(1).splitlines():
        line = raw.rstrip()
        if not line.strip() or line.strip().startswith("<small>"):
            continue
        if line.lstrip().startswith("- "):
            if cur:
                bullets.append(cur)
            cur = line.lstrip()[2:].strip()
        elif cur is not None:          # 续行并入上一条
            cur += line.strip()
    if cur:
        bullets.append(cur)
    # 去掉 markdown 粗体标记，保留纯文本
    return [re.sub(r"\*\*(.+?)\*\*", r"\1", b) for b in bullets]


def build_body(version):
    items = extract(version)
    return "\n".join("- " + x for x in items)


# ---------- 2) 找发行版 id ----------

def api(path, method="GET", data=None):
    url = API + path
    if method == "GET":
        url += ("&" if "?" in url else "?") + "access_token=" + TOKEN
        req = urllib.request.Request(url, method="GET")
    else:
        form = dict(data or {})
        form["access_token"] = TOKEN
        payload = urllib.parse.urlencode(form, encoding="utf-8").encode("utf-8")
        req = urllib.request.Request(url, data=payload, method=method)
        req.add_header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8")
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        sys.exit("HTTP %s %s\n%s" % (e.code, req.get_method() + " " + url.split("?")[0],
                                     e.read().decode("utf-8", "replace")[:600]))


TOKEN = os.environ.get("GITEE_TOKEN", "").strip()
if not TOKEN and "--dry-run" not in sys.argv:
    sys.exit("缺少 GITEE_TOKEN 环境变量")

releases = api("/releases?per_page=20") if TOKEN else []
by_tag = {d["tag_name"].lstrip("v"): d for d in releases}

for ver in TARGETS:
    body = build_body(ver)
    print("=" * 60)
    print("v" + ver, "| %d 条 | %d 字符" % (body.count("\n- ") + 1, len(body)))
    print(body)
    if "--dry-run" in sys.argv:
        continue
    rid = by_tag.get(ver, {}).get("id")
    if not rid:
        print("!! 找不到 v%s 的发行版 id，跳过" % ver)
        continue
    rel = by_tag[ver]
    # Gitee 的 PATCH 必须带 tag_name + name，只传 body 会 400
    api("/releases/%d?access_token=%s" % (rid, TOKEN), "PATCH", {
        "tag_name": rel.get("tag_name") or ("v" + ver),
        "name": rel.get("name") or ("LightBox " + ver),
        "body": body,
    })
    back = api("/releases/%d" % rid).get("body", "")
    ok = back.strip() == body.strip()
    print("-> PATCH 完成 id=%s，回读%s" % (rid, "一致 ✓" if ok else "不一致 ✗"))
    if not ok:
        print("回读内容：", back[:200])
