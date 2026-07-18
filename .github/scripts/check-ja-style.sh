#!/usr/bin/env bash
# 日本語文章規範の機械検査。
# コードフェンス(```...```)内の行を除外したうえで、以下を検出する。
#   1. 中黒     : 「・」の直前直後がどちらもカタカナでない(日本語並列での中黒は禁止。
#                 カタカナ固有名詞内部の中黒は許容)
#   2. ダッシュ  : em ダッシュ「—」、horizontal bar「―」を含む行
#   3. 常体接続 : 「だが、」「であるが、」を含む行(ですます調に統一するため)
#   4. である調文末: 行末が「である。」「だ。」「ではない。」のいずれか
#   5. 節末の次章予告: 「次章では」を含む行
# 1件でも違反があれば exit 1。
#
# 使い方:
#   check-ja-style.sh                 README.md と book/src/*.md を検査する(既定動作)。
#   check-ja-style.sh <path> [<path>] 指定したパス(ディレクトリまたはファイル)配下の
#                                     *.md だけを検査する。
# 存在しないパスは黙って読み飛ばす(削除されたファイルを渡されても落ちない)。
set -uo pipefail

# 検査対象のパス。引数があればそれを、なければ README.md と book/src/ を使う。
if [ "$#" -gt 0 ]; then
  targets=("$@")
else
  targets=("README.md" "book/src")
fi

# 実在するパスだけに絞る。
existing=()
for t in "${targets[@]}"; do
  [ -e "$t" ] && existing+=("$t")
done

if [ "${#existing[@]}" -eq 0 ]; then
  echo "検査対象のパスがありません。日本語文章規範検査をスキップします。"
  exit 0
fi

# 対象パスから *.md ファイルの一覧を作る(ファイル直接指定にも対応)。
files=()
for t in "${existing[@]}"; do
  if [ -d "$t" ]; then
    while IFS= read -r f; do
      files+=("$f")
    done < <(find "$t" -type f -name '*.md' | sort)
  elif [ -f "$t" ]; then
    files+=("$t")
  fi
done

if [ "${#files[@]}" -eq 0 ]; then
  echo "検査対象の Markdown ファイルがありません。日本語文章規範検査をスキップします。"
  exit 0
fi

status=0
checked=0
violations=0

for f in "${files[@]}"; do
  checked=$((checked + 1))
  # awk でコードフェンス状態をトグルし、フェンス外の行だけを
  # 「行番号<TAB>行本体」の形式で出力する。
  while IFS=$'\t' read -r lineno line; do
    [ -z "$lineno" ] && continue

    # 1. 中黒: 「・」の前後がどちらもカタカナでなければ違反。
    #    (perl の \p{Katakana} で判定。ー(長音)もカタカナに含める)
    if printf '%s' "$line" | perl -Mutf8 -CSD -ne '
      exit 1 if /(?:^|[^\x{30A1}-\x{30F6}\x{30FC}])・/ || /・(?:$|[^\x{30A1}-\x{30F6}\x{30FC}])/;
      exit 0
    '; then
      :
    else
      status=1
      violations=$((violations + 1))
      echo "::error file=${f},line=${lineno}::[中黒] ${line}"
      echo "${f}:${lineno}: [中黒] ${line}"
    fi

    # 2. ダッシュ: em ダッシュ「—」、horizontal bar「―」。
    if printf '%s' "$line" | grep -qP '[—―]'; then
      status=1
      violations=$((violations + 1))
      echo "::error file=${f},line=${lineno}::[ダッシュ] ${line}"
      echo "${f}:${lineno}: [ダッシュ] ${line}"
    fi

    # 3. 常体接続: 「だが、」「であるが、」。
    if printf '%s' "$line" | grep -qP '(だが、|であるが、)'; then
      status=1
      violations=$((violations + 1))
      echo "::error file=${f},line=${lineno}::[常体接続] ${line}"
      echo "${f}:${lineno}: [常体接続] ${line}"
    fi

    # 4. である調文末: 行末が「である。」「だ。」「ではない。」。
    if printf '%s' "$line" | grep -qP '(である。|だ。|ではない。)$'; then
      status=1
      violations=$((violations + 1))
      echo "::error file=${f},line=${lineno}::[である調文末] ${line}"
      echo "${f}:${lineno}: [である調文末] ${line}"
    fi

    # 5. 節末の次章予告: 「次章では」。
    if printf '%s' "$line" | grep -qP '次章では'; then
      status=1
      violations=$((violations + 1))
      echo "::error file=${f},line=${lineno}::[次章予告] ${line}"
      echo "${f}:${lineno}: [次章予告] ${line}"
    fi
  done < <(awk '
    BEGIN { in_fence = 0 }
    /^```/ { in_fence = !in_fence; next }
    { if (!in_fence) print NR "\t" $0 }
  ' "$f")
done

echo "検査したファイル: ${checked} / 違反: ${violations}"
if [ "$status" -eq 0 ]; then
  echo "すべてのファイルが日本語文章規範に準拠しています。"
fi
exit "$status"
