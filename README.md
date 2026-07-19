# rdbms-in-rust

Rustで一から関係データベース管理システム(RDBMS)を作りながら、データベース内部の設計と実装を学ぶStep-by-stepの日本語教材です。
[*Writing an OS in Rust*](https://os.phil-opp.com/) のRDBMS版を目指し、インメモリの最小実装から始め、ディスク永続化、クエリ最適化、トランザクション、クライアント/サーバー化まで、全40章(7部構成)にわたって一つのRustクレートを段階的に育てていきます。

## 目次

現在執筆済みなのは[はじめに](book/src/index.md)と第1〜36章のみで、第37章以降は準備中です。

### 第0部 設計と開発環境

- [第1章 作るRDBMSと作らないRDBMS](book/src/ch01-goals.md)
- [第2章 クエリの一生](book/src/ch02-life-of-a-query.md)
- [第3章 Rustプロジェクトの骨格とテスト基盤](book/src/ch03-project-skeleton.md)

### 第1部 Bare Bones: 最小のインメモリSQLデータベース

- [第4章 関係モデルとSQLサブセット](book/src/ch04-relational-model.md)
- [第5章 `SELECT 1`を実行する](book/src/ch05-select-one.md)
- [第6章 字句解析器](book/src/ch06-lexer.md)
- [第7章 構文解析器とAST](book/src/ch07-parser.md)
- [第8章 型、NULL、式評価](book/src/ch08-expressions.md)
- [第9章 カタログとDDL](book/src/ch09-catalog.md)
- [第10章 インメモリ表とDML](book/src/ch10-inmemory-dml.md)

### 第2部 Storage: ディスクにデータを保存する

- [第11章 データベースファイルとページ](book/src/ch11-database-file.md)
- [第12章 Slotted Page、Tuple、RID](book/src/ch12-slotted-page.md)
- [第13章 Disk ManagerとHeap File](book/src/ch13-disk-manager.md)
- [第14章 Buffer Pool](book/src/ch14-buffer-pool.md)
- [第15章 永続カタログと空き領域管理](book/src/ch15-persistent-catalog.md)
- [第16章 SQL経路の永続化](book/src/ch16-sql-persistence.md)

### 第3部 Query Execution: SQLを実行計画へ変換する

- [第17章 Binderと名前解決](book/src/ch17-binder.md)
- [第18章 関係代数とLogical Plan](book/src/ch18-logical-plan.md)
- [第19章 Physical PlanとVolcano Executor](book/src/ch19-volcano-executor.md)
- [第20章 制約](book/src/ch20-constraints.md)
- [第21章 Sort、Limit、Distinct、Aggregate](book/src/ch21-sort-aggregate.md)
- [第22章 Joinアルゴリズム](book/src/ch22-join.md)
- [第23章 B+Tree I: 検索、挿入、分割](book/src/ch23-btree-1.md)
- [第24章 B+Tree II: 範囲検索、削除、CREATE INDEX](book/src/ch24-btree-2.md)
- [第25章 Index Scanとアクセスパス](book/src/ch25-index-scan.md)

### 第4部 Query Optimizer: よりよい実行計画を選ぶ

- [第26章 ルールベース最適化](book/src/ch26-rule-based-optimization.md)
- [第27章 統計情報とCardinality Estimation](book/src/ch27-statistics.md)
- [第28章 Cost Modelとアクセスパス選択](book/src/ch28-cost-model.md)
- [第29章 Join OrderとPhysical Properties](book/src/ch29-join-order.md)

### 第5部 Transaction: ACIDと並行実行

- [第30章 トランザクション境界とAtomicity](book/src/ch30-transactions.md)
- [第31章 Lock ManagerとStrict 2PL](book/src/ch31-lock-manager.md)
- [第32章 Isolation LevelとDeadlock](book/src/ch32-isolation-deadlock.md)
- [第33章 Write-Ahead Logging](book/src/ch33-wal.md)
- [第34章 Crash RecoveryとCheckpoint](book/src/ch34-crash-recovery.md)
- [第35章 Latchと並行B+Tree](book/src/ch35-latch-concurrent-btree.md)

### 第6部 Server、運用、品質保証

- [第36章 Wire ProtocolとClient/Server](book/src/ch36-wire-protocol.md)
- 第37章 SessionとPrepared Statement
- 第38章 実行制御
- 第39章 System Catalogとメンテナンス
- 第40章 テスト、Fuzzing、Benchmark、リリース

## 読み方とビルド方法

執筆済みの章は上の目次からGitHub上でそのまま読めます。
より読みやすいmdBook形式で読みたい場合は、ローカルで以下を実行してください。

```sh
cargo install mdbook
mdbook serve book
```

`http://localhost:3000` でブラウザから閲覧できます。

## 開発

本書と対になるRustクレートは、リポジトリ直下の`src`および`tests`にあります。

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

ドキュメントはCIと同じ3つの検査をローカルでも実行できます。
順に、Markdown(本文と本README)のlint、日本語文章規範(中黒、ダッシュ、常体接続など)の検査、本文中のMermaid図のレンダリング検証です。

```sh
npx --yes markdownlint-cli2@0.22.1
.github/scripts/check-ja-style.sh
.github/scripts/check-mermaid.sh
```
