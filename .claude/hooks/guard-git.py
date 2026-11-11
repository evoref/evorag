#!/usr/bin/env python3
"""PreToolUse hook: evorag のブランチ運用を守らせる。

長命ブランチは main (リリース専用) と develop (開発の本線) の 2 本だけ。作業ブランチは develop から作り、
PR (squash) で develop に入れる。リリースは develop → main の PR (merge commit、人間) で、タグは人間が署名して付ける。

- main / develop への直接 push・force push・削除を拒否
- force push を拒否。ただし作業ブランチ (feat/ fix/ spec/ chore/ spike/ agent/) への --force-with-lease は許す
  (作業ブランチの中の rebase の後の push。リモートが知っている状態のときだけ上書きする)
- リモートのブランチの削除は、作業ブランチだけ許し、それ以外の名前は確認を求める
- main / develop 上での commit / merge / rebase 等を拒否 (作業ブランチ → PR で入れる)
- リリースタグ (v*) の作成・push を拒否 (署名付きタグは人間が付ける)
- git stash を拒否 (作業ツリーを動かす操作)。reset --hard / clean -f / restore / checkout -- は確認を求める
- gh pr merge は確認を求める (merge するかどうかは人間が決める。実行は指示を受けたエージェント、docs/11 §9.1)

標準入力の JSON から tool_input.command を読み、deny / ask のときだけ JSON を出力する。
"""
import json
import os
import re
import subprocess
import sys

PROTECTED = {"main", "develop"}
TAG_RE = re.compile(r"^(refs/tags/)?v\d")
WORK_BRANCH_RE = re.compile(r"^(feat|fix|spec|chore|spike)/[^/].*$|^agent/[^/]+/[^/].*$")
MUTATING = {"commit", "merge", "rebase", "cherry-pick", "revert", "am", "pull"}
SPLIT_RE = re.compile(r"&&|\|\||[;|\n]")


def decide(kind, reason):
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": kind,
            "permissionDecisionReason": reason,
        }
    }, ensure_ascii=True))
    sys.exit(0)


def current_branch(cwd):
    try:
        out = subprocess.run(
            ["git", "branch", "--show-current"], cwd=cwd or None,
            capture_output=True, text=True, timeout=5)
        return out.stdout.strip()
    except Exception:
        return ""


def resolve(base, path):
    path = path.strip("\"'")
    if re.match(r"^/[a-zA-Z]/", path):  # Git Bash 形式 /c/Users/... -> C:/Users/...
        path = path[1].upper() + ":" + path[2:]
    return os.path.normpath(path if os.path.isabs(path) else os.path.join(base or ".", path))


def git_args(segment):
    """`git [opts] <sub> args...` -> (sub, args, -C で指定されたパス)。git 以外は (None, [], None)。"""
    toks = segment.split()
    for i, t in enumerate(toks):
        if t == "git" or t.endswith("/git") or t.endswith("\\git") or t.lower().endswith("git.exe"):
            rest = toks[i + 1:]
            j = 0
            dash_c = None
            while j < len(rest) and rest[j].startswith("-"):
                if rest[j] == "-C" and j + 1 < len(rest):
                    dash_c = rest[j + 1]
                j += 2 if rest[j] in ("-C", "-c", "--git-dir", "--work-tree") else 1
            if j < len(rest):
                return rest[j], rest[j + 1:], dash_c
            return None, [], dash_c
    return None, [], None


def check_push(args, branch):
    flags = [a for a in args if a.startswith("-")]
    pos = [a for a in args if not a.startswith("-")]
    if any(f in ("--all", "--mirror", "--tags") for f in flags):
        decide("deny", "push --all / --mirror / --tags は禁止です (保護ブランチとタグを巻き込むため)。")
    # 相手の状態を確かめない上書き (-f / --force / +refspec) と、確かめる上書き (--force-with-lease) を分ける。
    force = any(f in ("-f", "--force") for f in flags)
    lease = any(f.startswith("--force-with-lease") for f in flags)
    delete = any(f in ("--delete", "-d") for f in flags)
    refspecs = pos[1:]  # pos[0] は remote
    targets = []
    for r in refspecs:
        if r.startswith("+"):
            force = True
            r = r[1:]
        dst = r.split(":", 1)[1] if ":" in r else r
        if r.startswith(":"):
            delete = True
        if dst == "HEAD":
            dst = branch
        targets.append(re.sub(r"^refs/heads/", "", dst))
    if not refspecs:
        targets = [branch]
    for t in targets:
        if TAG_RE.match(t):
            decide("deny", "リリースタグ (v*) の push は人間が行います (署名付きタグ)。")
    hit = [t for t in targets if t in PROTECTED]
    if hit:
        decide("deny", f"{hit[0]} への直接 push は禁止です。作業ブランチ → PR で入れてください。")
    if force:
        decide("deny", "--force / -f / +refspec は禁止です。作業ブランチの rebase の後は --force-with-lease を使ってください。")
    if lease:
        others = [t for t in targets if not WORK_BRANCH_RE.match(t)]
        if others:
            decide("deny", f"--force-with-lease は作業ブランチにだけ使えます ({others[0]} は作業ブランチの名前ではありません)。")
    if delete:
        if not refspecs:
            decide("deny", "削除するリモートのブランチを指定してください。")
        others = [t for t in targets if not WORK_BRANCH_RE.match(t)]
        if others:
            decide("ask", f"作業ブランチの名前ではないリモートのブランチ {others[0]} を削除します。よいですか?")


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        return
    cmd = (payload.get("tool_input") or {}).get("command") or ""
    if not re.search(r"\bgit\b|\bgh\b", cmd):
        return
    cwd = payload.get("cwd")
    for seg in SPLIT_RE.split(cmd):
        seg = seg.strip()
        m = re.match(r"^cd\s+(?:/d\s+)?(\S+)", seg)
        if m:  # `cd <path> && git ...` は、以降の git の対象ディレクトリを変える
            cwd = resolve(cwd, m.group(1))
            continue
        if re.match(r"^gh\s+pr\s+merge\b", seg):
            decide("ask", "PR の merge は人間が決めます。この PR を入れてよいか確認してください。")
        sub, args, dash_c = git_args(seg)
        if sub is None:
            continue
        # `git -C <path>` や `cd <path> &&` で別のリポジトリを対象にしているときは、そのブランチで判定する
        branch = current_branch(resolve(cwd, dash_c) if dash_c else cwd)
        if sub == "push":
            check_push(args, branch)
        elif sub == "tag":
            names = [a for a in args if not a.startswith("-")]
            if any(TAG_RE.match(n) for n in names) and not any(a in ("-l", "--list") for a in args):
                decide("deny", "リリースタグ (v*) は人間が署名して付けます。")
        elif sub == "branch":
            if any(a in ("-d", "-D", "--delete", "-m", "-M", "--move") for a in args) and \
                    any(a in PROTECTED for a in args if not a.startswith("-")):
                decide("deny", "main / develop の削除・改名は禁止です。")
        elif sub == "stash" and not (args and args[0] in ("list", "show")):
            decide("deny", "git stash は使いません (作業ツリーを動かし、改行コードや未追跡の変更を壊しやすい)。"
                           "退避が要るときは、作業ブランチにコミットするか、人間に頼んでください。")
        elif sub == "reset" and "--hard" in args:
            if branch in PROTECTED:
                decide("deny", f"{branch} 上の reset --hard は禁止です。")
            decide("ask", "reset --hard は作業ツリーの変更を捨てます。実行してよいか確認してください。")
        elif sub == "clean" and any(a.startswith("-") and "f" in a for a in args):
            decide("ask", "git clean -f は未追跡のファイルを削除します。実行してよいか確認してください。")
        elif sub == "restore" and "--staged" not in args:
            decide("ask", "git restore は作業ツリーの変更を捨てます。実行してよいか確認してください。")
        elif sub == "checkout" and "--" in args:
            decide("ask", "git checkout -- <path> は作業ツリーの変更を捨てます。実行してよいか確認してください。")
        elif sub in MUTATING:
            if branch == "main":
                decide("deny", "main へ直接コミット・マージはしません。main はリリース専用です (develop → main の PR)。")
            if branch == "develop":
                decide("deny", "develop へ直接コミット・マージはしません。作業ブランチを切って PR (squash) で入れてください。")


if __name__ == "__main__":
    main()
