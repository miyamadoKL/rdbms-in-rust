- [はじめに](index.md)

# 第0部 設計と開発環境

- [第1章 作るRDBMSと作らないRDBMS — 要件・非目標・ロードマップ](ch01-goals.md)
- [第2章 クエリの一生 — Lexer→Parser→Binder→Plan→Optimizer→Executor→Storageの概観](ch02-life-of-a-query.md)
- [第3章 Rustプロジェクトの骨格とテスト基盤 — エラー型、Newtype、SQL Golden Test基盤、Gitタグ運用](ch03-project-skeleton.md)

# 第1部 Bare Bones: 最小のインメモリSQLデータベース

- [第4章 関係モデルとSQLサブセット — Schema/Tuple/Valueの型設計]()
- [第5章 `SELECT 1`を実行する — REPLと最初の縦切り]()
- [第6章 字句解析器 — Token、Span、位置情報付きエラー]()
- [第7章 構文解析器とAST — Recursive Descent + Pratt Parser]()
- [第8章 型、NULL、式評価 — 三値論理、CAST、Scalar Function]()
- [第9章 カタログとDDL — CREATE TABLE / DROP TABLE]()
- [第10章 インメモリ表とDML — INSERT/SELECT/UPDATE/DELETE、SQLite Differential Test導入]()

# 第2部 Storage: ディスクにデータを保存する

- [第11章 データベースファイルとページ — File Header、Page、Checksum、明示的encode/decode]()
- [第12章 Slotted Page、Tuple、RID — 可変長レコード、Tombstone、コンパクション]()
- [第13章 Disk ManagerとHeap File — ページI/O、テーブル走査]()
- [第14章 Buffer Pool — Page Table、Clock、RAII Guard(`PageReadGuard`/`PageWriteGuard`)]()
- [第15章 永続カタログと空き領域管理 — Metadata Page、Free Space Map、Open/Close]()
- [第16章 SQL経路の永続化 — SeqScan/Insert/Update/DeleteをHeap File実装へ差し替え、再起動テスト]()

# 第3部 Query Execution: SQLを実行計画へ変換する

- [第17章 Binderと名前解決 — Bound AST、型検査、`*`展開]()
- [第18章 関係代数とLogical Plan]()
- [第19章 Physical PlanとVolcano Executor — `Executor::next()`、簡易EXPLAIN]()
- [第20章 制約 — NOT NULL / PRIMARY KEY / UNIQUE(この段階では走査ベース検査)、Statement Rollback]()
- [第21章 Sort、Limit、Distinct、Aggregate — Hash Aggregate、GROUP BY / HAVING]()
- [第22章 Joinアルゴリズム — Nested Loop Join、Hash Join(Inner Equi-Joinに限定)]()
- [第23章 B+Tree I: 検索、挿入、分割]()
- [第24章 B+Tree II: 範囲検索、削除、CREATE INDEX — Index Build、Index Maintenance、インデックスによるPK/UNIQUE検査]()
- [第25章 Index Scanとアクセスパス — Point/Range Index Scan、Index Nested Loop Join、ルールベースのアクセスパス選択]()

# 第4部 Query Optimizer: よりよい実行計画を選ぶ

- [第26章 ルールベース最適化 — Constant Folding、Predicate Pushdown、Projection Pruning]()
- [第27章 統計情報とCardinality Estimation — Histogram、EXPLAIN ANALYZEとの突合]()
- [第28章 Cost Modelとアクセスパス選択 — Seq vs Index、NLJ vs Hash Joinの相対比較]()
- [第29章 Join OrderとPhysical Properties — Left-deep DP、Interesting Order]()

# 第5部 Transaction: ACIDと並行実行

- [第30章 トランザクション境界とAtomicity — BEGIN/COMMIT/ROLLBACK、メモリ上Undo(WAL章での置き換えを予告)、決定的インターリーブテストハーネス導入]()
- [第31章 Lock ManagerとStrict 2PL — S/X Lock、Upgrade、Wait Queue(Table Lock→Tuple Lock)]()
- [第32章 Isolation LevelとDeadlock — 4分離レベル、Wait-for Graph、Victim Selection]()
- [第33章 Write-Ahead Logging — LSN、Before/After Image、WALファースト不変条件]()
- [第34章 Crash RecoveryとCheckpoint — ARIES-lite(Analysis/Redo/Undo)、failpointによるCrash Test]()
- [第35章 Latchと並行B+Tree — Lock vs Latch、Lock Coupling、実スレッド並行テスト解禁]()

# 第6部 Server、運用、品質保証

- [第36章 Wire ProtocolとClient/Server — 長さ付きフレーム、CLIクライアント]()
- [第37章 SessionとPrepared Statement — Parameter Binding]()
- [第38章 実行制御 — Cancellation、Worker Thread Pool、Timeout、メモリ上限、Graceful Shutdown]()
- [第39章 System Catalogとメンテナンス — SHOW/DESCRIBE、ANALYZE、VACUUM、Slow Query Log]()
- [第40章 テスト、Fuzzing、Benchmark、リリース — Fuzzing、Crash Injection、Benchmark、サンプルアプリ(Golden/Differential Testは導入済みの前提で拡充・統合)]()
