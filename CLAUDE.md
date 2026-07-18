# rdbms-in-rust

Rust でリレーショナル・データベースを自作する日本語 Step-by-step 教材(Writing an OS in Rust の RDBMS 版)。本文は mdBook(`book/`)、参照実装はリポジトリ直下の単一クレート `minidb`。

# 執筆規約(本文 Markdown を書く・直すとき必須)

- 執筆・推敲・修正の前に `japanese-tech-writing` と `cognitive-rhythm-writing` の両 skill を読む(理由: 中黒、ダッシュ、一文一行、節末予告などの規範はこの2つに定義されている)
- 地の文は**ですます調**。「〜だが、」のような常体接続も禁止(理由: skill の例文がである調のため、明示しないと執筆エージェントが引きずられて回帰する事故が2回発生)
- 章番号・章タイトルは `docs-local/toc.md`(確定目次)のみを正とする。素材文書(`docs-local/chatgpt_opinion.md` 等)の章番号は旧構成なので本文に持ち込まない(理由: 素材由来の旧章番号が原稿に混入する事故が2回発生)
- 章タイトルは README.md / book/src/SUMMARY.md / docs-local/toc.md / 各章 H1 で一字一句一致させる(toc.md のみ括弧書き補足を持てる)

# 章の完了条件

1. 本文のコード片がリポジトリの実コードと一致している
2. `cargo build` / `cargo test` / `cargo clippy --all-targets -- -D warnings` 全緑
3. `npx --yes markdownlint-cli2@0.22.1` 0 error、`mdbook build book` 成功
4. README.md の目次で該当章をリンク化し、SUMMARY.md のドラフト章 `[タイトル]()` を実ファイルリンクへ変更

# 仕様の変更

`docs-local/toc.md` は確定仕様。実装やレビュー対応の都合で変更しない。変更が必要と考えたら、変更せず理由を添えてユーザーに提案する(理由: レビュー対応中に仕様側を追記して block された事故あり。裁定済みの例外: 第3章のログ出力は原案準拠で維持、一時DBテストヘルパーは第5章以降で導入)。
