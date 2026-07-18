#!/usr/bin/env bash
# ドキュメント内の ```mermaid 図がすべてレンダリング可能かを検査する。
# GitHub の rich display と同じ Mermaid エンジン(mermaid-cli)でパースし、
# パースエラーになる図があれば、その図を含むファイルとエラーを表示して失敗する。
# mmdc があればそれを、なければ npx 経由で mermaid-cli を使う(ローカル実行用)。
#
# 使い方:
#   check-mermaid.sh                  リポジトリ全体の *.md を検査する(従来動作)。
#   check-mermaid.sh <path> [<path>]  指定したパス(ディレクトリまたはファイル)配下の
#                                     *.md だけを検査する。ローカルで1分冊だけ確認したり、
#                                     CI で変更のあったディレクトリだけに絞るために使う。
# 存在しないパスは黙って読み飛ばす(削除されたディレクトリを渡されても落ちない)。
set -uo pipefail

if command -v mmdc >/dev/null 2>&1; then
  MMDC=(mmdc)
else
  MMDC=(npx --yes @mermaid-js/mermaid-cli@11)
fi

# 検査対象のパス。引数があればそれを、なければリポジトリ全体(.)を使う。
if [ "$#" -gt 0 ]; then
  targets=("$@")
else
  targets=(".")
fi

# 実在するパスだけに絞る。
existing=()
for t in "${targets[@]}"; do
  [ -e "$t" ] && existing+=("$t")
done

if [ "${#existing[@]}" -eq 0 ]; then
  echo "検査対象のパスがありません。Mermaid 検査をスキップします。"
  exit 0
fi

# Chromium をサンドボックスなしで起動する(CI/root 環境向け)。
PP="$(mktemp)"
printf '{"args":["--no-sandbox","--disable-gpu"]}\n' > "$PP"
WORK="$(mktemp -d)"
trap 'rm -rf "$PP" "$WORK"' EXIT

status=0
checked=0
failed=0

while IFS= read -r f; do
  checked=$((checked + 1))
  if ! "${MMDC[@]}" -p "$PP" -i "$f" -o "$WORK/out.md" >"$WORK/log" 2>&1; then
    status=1
    failed=$((failed + 1))
    echo "::error file=${f}::Mermaid 図のレンダリングに失敗しました"
    echo "----- ${f} -----"
    grep -iE "parse error|expecting|got '|error:" "$WORK/log" | head -6
  fi
done < <(grep -rl --include='*.md' '```mermaid' "${existing[@]}" | sort -u)

echo "検査した Mermaid 含有ファイル: ${checked} / 失敗: ${failed}"
if [ "$status" -eq 0 ]; then
  echo "すべての Mermaid 図が正常にレンダリングできました。"
fi
exit "$status"
