# rdbms-in-rust

Rustで一から関係データベース管理システム(RDBMS)を作りながら、データベース内部の設計と実装を学ぶStep-by-stepの日本語教材です。
[*Writing an OS in Rust*](https://os.phil-opp.com/) のRDBMS版を目指し、インメモリの最小実装から始め、ディスク永続化、クエリ最適化、トランザクション、クライアント/サーバー化まで、全40章(7部構成)にわたって一つのRustクレートを段階的に育てていきます。

## 目次

現在執筆済みなのは[はじめに](book/src/index.md)と第1〜3章のみで、第4章以降は準備中です。

### 第0部 設計と開発環境

- [第1章 作るRDBMSと作らないRDBMS](book/src/ch01-goals.md)
- [第2章 クエリの一生](book/src/ch02-life-of-a-query.md)
- [第3章 Rustプロジェクトの骨格とテスト基盤](book/src/ch03-project-skeleton.md)

### 第1部 Bare Bones: 最小のインメモリSQLデータベース

- 第4章 関係モデルとSQLサブセット
- 第5章 `SELECT 1`を実行する
- 第6章 字句解析器
- 第7章 構文解析器とAST
- 第8章 型、NULL、式評価
- 第9章 カタログとDDL
- 第10章 インメモリ表とDML

### 第2部 Storage: ディスクにデータを保存する

- 第11章 データベースファイルとページ
- 第12章 Slotted Page、Tuple、RID
- 第13章 Disk ManagerとHeap File
- 第14章 Buffer Pool
- 第15章 永続カタログと空き領域管理
- 第16章 SQL経路の永続化

### 第3部 Query Execution: SQLを実行計画へ変換する

- 第17章 Binderと名前解決
- 第18章 関係代数とLogical Plan
- 第19章 Physical PlanとVolcano Executor
- 第20章 制約
- 第21章 Sort、Limit、Distinct、Aggregate
- 第22章 Joinアルゴリズム
- 第23章 B+Tree I: 検索、挿入、分割
- 第24章 B+Tree II: 範囲検索、削除、CREATE INDEX
- 第25章 Index Scanとアクセスパス

### 第4部 Query Optimizer: よりよい実行計画を選ぶ

- 第26章 ルールベース最適化
- 第27章 統計情報とCardinality Estimation
- 第28章 Cost Modelとアクセスパス選択
- 第29章 Join OrderとPhysical Properties

### 第5部 Transaction: ACIDと並行実行

- 第30章 トランザクション境界とAtomicity
- 第31章 Lock ManagerとStrict 2PL
- 第32章 Isolation LevelとDeadlock
- 第33章 Write-Ahead Logging
- 第34章 Crash RecoveryとCheckpoint
- 第35章 Latchと並行B+Tree

### 第6部 Server、運用、品質保証

- 第36章 Wire ProtocolとClient/Server
- 第37章 SessionとPrepared Statement
- 第38章 実行制御
- 第39章 System Catalogとメンテナンス
- 第40章 テスト、Fuzzing、Benchmark、リリース

## 読み方とビルド方法

執筆済みの章は上の目次からGitHub上でそのまま読めます。
より読みやすいmdBook形式で読みたい場合や、未執筆章も含めた完全な目次(`book/src/SUMMARY.md`)を確認したい場合は、ローカルで以下を実行してください。

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
```

Markdown(本文と本README)のlintと、本文中のMermaid図のレンダリング検証は以下で行います。

```sh
npx --yes markdownlint-cli2@0.22.1
.github/scripts/check-mermaid.sh
```
