- [はじめに](index.md)

# 第0部 設計と開発環境

- [第1章 作るRDBMSと作らないRDBMS](ch01-goals.md)
- [第2章 クエリの一生](ch02-life-of-a-query.md)
- [第3章 Rustプロジェクトの骨格とテスト基盤](ch03-project-skeleton.md)

# 第1部 Bare Bones: 最小のインメモリSQLデータベース

- [第4章 関係モデルとSQLサブセット](ch04-relational-model.md)
- [第5章 `SELECT 1`を実行する](ch05-select-one.md)
- [第6章 字句解析器](ch06-lexer.md)
- [第7章 構文解析器とAST](ch07-parser.md)
- [第8章 型、NULL、式評価](ch08-expressions.md)
- [第9章 カタログとDDL](ch09-catalog.md)
- [第10章 インメモリ表とDML](ch10-inmemory-dml.md)

# 第2部 Storage: ディスクにデータを保存する

- [第11章 データベースファイルとページ](ch11-database-file.md)
- [第12章 Slotted Page、Tuple、RID](ch12-slotted-page.md)
- [第13章 Disk ManagerとHeap File](ch13-disk-manager.md)
- [第14章 Buffer Pool](ch14-buffer-pool.md)
- [第15章 永続カタログと空き領域管理](ch15-persistent-catalog.md)
- [第16章 SQL経路の永続化](ch16-sql-persistence.md)

# 第3部 Query Execution: SQLを実行計画へ変換する

- [第17章 Binderと名前解決](ch17-binder.md)
- [第18章 関係代数とLogical Plan](ch18-logical-plan.md)
- [第19章 Physical PlanとVolcano Executor](ch19-volcano-executor.md)
- [第20章 制約](ch20-constraints.md)
- [第21章 Sort、Limit、Distinct、Aggregate](ch21-sort-aggregate.md)
- [第22章 Joinアルゴリズム](ch22-join.md)
- [第23章 B+Tree I: 検索、挿入、分割](ch23-btree-1.md)
- [第24章 B+Tree II: 範囲検索、削除、CREATE INDEX](ch24-btree-2.md)
- [第25章 Index Scanとアクセスパス](ch25-index-scan.md)

# 第4部 Query Optimizer: よりよい実行計画を選ぶ

- [第26章 ルールベース最適化](ch26-rule-based-optimization.md)
- [第27章 統計情報とCardinality Estimation](ch27-statistics.md)
- [第28章 Cost Modelとアクセスパス選択](ch28-cost-model.md)
- [第29章 Join OrderとPhysical Properties](ch29-join-order.md)

# 第5部 Transaction: ACIDと並行実行

- [第30章 トランザクション境界とAtomicity]()
- [第31章 Lock ManagerとStrict 2PL]()
- [第32章 Isolation LevelとDeadlock]()
- [第33章 Write-Ahead Logging]()
- [第34章 Crash RecoveryとCheckpoint]()
- [第35章 Latchと並行B+Tree]()

# 第6部 Server、運用、品質保証

- [第36章 Wire ProtocolとClient/Server]()
- [第37章 SessionとPrepared Statement]()
- [第38章 実行制御]()
- [第39章 System Catalogとメンテナンス]()
- [第40章 テスト、Fuzzing、Benchmark、リリース]()
