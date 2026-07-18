# 第2章 クエリの一生 — Lexer→Parser→Binder→Plan→Optimizer→Executor→Storageの概観

次のSQLを1本、実行してみます。

```sql
SELECT name FROM users WHERE id = 42;
```

この短い1文が結果の1行になるまでに、文字列は何段階もの中間表現へ姿を変えます。本章では、その経路を最初から最後まで一度たどります。各段階を実装するのは第4章以降であり、本章ではまだコードを書きません。それでも、経路全体の地図を先に持っておけば、以後の章で「今どの段階を作っているのか」を見失わずに済みます。

## 経路全体

SQL文字列から結果が出るまでの経路は、次の10段階です。

```text
SQL文字列
  ↓
Lexer（字句解析）
  ↓
Parser（構文解析）→ AST
  ↓
Binder（名前解決・型検査）→ Bound AST
  ↓
Logical Plan（関係代数への変換）
  ↓
Optimizer（書き換え・アクセスパス選択）
  ↓
Physical Plan
  ↓
Executor（Volcano型Pull実行）
  ↓
Storage Engine（Heap File / B+Tree）
  ↓
Buffer Pool
  ↓
Database File
```

以下、`SELECT name FROM users WHERE id = 42;` を例に、各段階でこの1文がどんな中間表現に変わるかを見ていきます。

## Lexer: 文字列をTokenに変換する

**Lexer**（字句解析器）は、SQL文字列を意味のある最小単位である**Token**の列に分割します。空白やコメントはここで捨てられ、以後の段階には渡りません。

```text
SELECT   -> Keyword(Select)
name     -> Identifier("name")
FROM     -> Keyword(From)
users    -> Identifier("users")
WHERE    -> Keyword(Where)
id       -> Identifier("id")
=        -> Operator(Eq)
42       -> IntegerLiteral(42)
;        -> Semicolon
```

Lexerがこの段階で判断するのは、字面から一意に決まることだけです。`id` という文字列がテーブル`users`の列かどうかは、Lexerの責務ではありません。それは後段のBinderが、カタログと照合して初めて決まります。

## Parser: Token列を構文木に組み立てる

**Parser**(構文解析器)は、Token列をSQLの文法に従って**AST**(抽象構文木)に組み立てます。`SELECT ... FROM ... WHERE ...` という句の並びや、`id = 42` という二項演算の優先順位は、この段階で構造として確定します。

```text
SelectStmt
 ├─ projections: [ColumnRef("name")]
 ├─ from: TableRef("users")
 └─ where: BinaryOp(Eq,
             ColumnRef("id"),
             Literal(Integer(42)))
```

ASTはSQL構文をそのまま木にしたものであり、`users`というテーブルが実在するか、`id`という列が本当に存在するかをこの時点ではまだ確認していません。構文として正しければASTは作れます。

## Binder: 名前を解決し、型を検査する

**Binder**は、ASTに登場する識別子をカタログと照合し、実体に結びつけます。`users`はテーブルIDへ、`name`と`id`は列インデックスと型へ解決されます。この結果を**Bound AST**と呼びます。

```text
BoundSelect
 ├─ projections: [BoundColumnRef(table=users, index=1, type=Text)]
 ├─ from: BoundTableRef(table_id=TableId(3))
 └─ where: BinaryOp(Eq,
             BoundColumnRef(table=users, index=0, type=BigInt),
             Literal(BigInt(42)))
```

`users`というテーブルが存在しない、あるいは`id`という列がない場合、エラーになるのはこの段階です。`id = 42`の両辺の型が比較可能かどうかの検査も、ここで行われます。

## Logical Plan: 関係代数の木に変換する

Bound ASTは、関係代数の演算子から成る**Logical Plan**へ変換されます。`SELECT`の各句は、対応する演算子ノードになります。

```text
Projection(name)
  └─ Filter(id = 42)
       └─ Scan(users)
```

Logical Planは「何を計算するか」だけを表し、「どう計算するか」はまだ決めていません。`Scan`が全件走査になるのかインデックスを使うのかは、この時点では未定です。

## Optimizer: 計画を書き換え、アクセスパスを選ぶ

**Optimizer**は、Logical Planを2種類の方法で改善します。

- **ルールベースの書き換え**：定数畳み込み、述語のスキャン側への押し下げ、不要な射影の削除など、常に妥当な変換を適用する
- **コストベースの選択**：統計情報から推定した行数とコストをもとに、Scan方式やJoin順序など複数の候補から相対的に安いものを選ぶ

`id = 42`という等値述語に対して`users`テーブルの`id`列にインデックスがあれば、OptimizerはSequential ScanをIndex Scanに置き換えます。今回の例では、Logical Planの構造はそのままに、`Scan(users)`が`IndexScan(users, id = 42)`へ変わる程度の書き換えで済みます。

## Physical Plan: 実行アルゴリズムを確定する

Optimizerの判断は**Physical Plan**として確定します。Logical Planの各ノードが、実際に実行可能な演算子アルゴリズムに置き換わります。

```text
Projection(name)
  └─ IndexScan(users, key: id = 42)
```

Physical Planは、後段のExecutorがそのまま実行できる形をしています。「Joinを Nested LoopでやるかHash Joinでやるか」のような選択は、この段階で既に一つに決まっています。

## Executor: Physical Planを実際に動かす

**Executor**は、Physical Planの各ノードを、共通のインターフェースを持つ演算子として実行します。

```rust
trait Executor {
    fn next(&mut self) -> Result<Option<Tuple>>;
}
```

`Projection`は自分の子である`IndexScan`の`next()`を呼び、返ってきたタプルから`name`列だけを取り出して返します。`IndexScan`は自分の子を持たず、Storage Engineに対してB+Treeの検索を要求します。木の末端から`next()`の呼び出しが親へ伝播していくこの実行方式を、**Volcano型**のPull実行と呼びます。

## Storage Engine、Buffer Pool、Database File: データを取り出す

`IndexScan`が`next()`を呼ばれると、Storage EngineはB+Treeを`id = 42`で検索し、該当するタプルの位置(Record ID)を得てから、Heap Fileの該当ページを読みに行きます。

ページの読み込みは、常にディスクへ直接アクセスするわけではありません。**Buffer Pool**がページをメモリ上にキャッシュしており、既に読み込み済みのページであればディスクI/Oは発生しません。キャッシュに無ければ、Buffer PoolがDatabase Fileから該当ページを読み込み、キャッシュに載せてから返します。

Executorが最終的に受け取るのは、こうしてページから取り出され、Slotted Page形式からデコードされた1件のタプルです。それが`name`列だけに射影され、クエリの結果として返ります。

## 実在DBMSでの対応

この経路は本教材独自の設計ではなく、既存のRDBMSにも同様の分離があります。

**SQLite**は、TokenizerとParserでSQLをASTに変換し、Code GeneratorがVirtual Machine向けのバイトコードを生成します。VMがそのバイトコードを実行し、B-TreeモジュールとPage Cacheがディスク上のデータへのアクセスを担います[^sqlite-arch]。

**PostgreSQL**は、ParserがASTを生成し、Transformationがカタログ情報と結びつけた形へ変換し、PlannerがLogical PlanからPhysical Planを組み立て、ExecutorがPlanを実行します[^pg-arch]。

いずれの実装も、字句・構文解析、名前解決、計画生成、実行、永続化という責務を、別々のモジュールに分離しています。本教材が全40章かけて作るモジュール群も、この分離に沿っています。

## モジュールと実装章の対応

各段階を、本教材のどの部・どの章で実装するかは次の通りです。

| 段階 | モジュール | 実装章 |
| --- | --- | --- |
| SQL文字列 → Token列 | Lexer | 第6章 字句解析器 |
| Token列 → AST | Parser | 第7章 構文解析器とAST |
| AST → Bound AST | Binder | 第17章 Binderと名前解決 |
| Bound AST → Logical Plan | 関係代数への変換 | 第18章 関係代数とLogical Plan |
| Logical Planの書き換え | ルールベースOptimizer | 第26章 ルールベース最適化 |
| アクセスパス・Join順序の選択 | 統計・コストベースOptimizer | 第27章 統計情報とCardinality Estimation、第28章 Cost Modelとアクセスパス選択、第29章 Join OrderとPhysical Properties |
| Logical Plan → Physical Plan、実行 | Volcano Executor | 第19章 Physical PlanとVolcano Executor |
| インデックスを使った実行 | Index Scan | 第25章 Index Scanとアクセスパス |
| キーとRIDの検索 | B+Tree | 第23章・第24章 B+Tree I・II |
| テーブルデータの読み書き | Heap File、Disk Manager | 第13章 Disk ManagerとHeap File |
| ページのキャッシュ | Buffer Pool | 第14章 Buffer Pool |
| ページ形式 | Database File、Page | 第11章 データベースファイルとページ、第12章 Slotted Page、Tuple、RID |
| SQL実行経路をディスク実装へ接続 | SeqScan/DMLのHeap File化 | 第16章 SQL経路の永続化 |

第1部では、この経路のうちStorage Engineより下(Buffer Pool、Database File)を実装せず、まずインメモリのVec上でテーブルを表現します。ディスクへの永続化は第2部から着手し、第16章でSQL実行経路をディスク実装へ接続し直します。B+Treeは第3部後半(第23〜24章)に置いており、Join(第22章)を実装した直後に取り組む配置です。

## 次章から手を動かす

本章で示したのは、地図です。Lexer、Parser、Binder、それぞれの内部で何を判断し、どんなデータ構造を持つかは、まだ何も決めていません。

次章では、この経路を実装していくためのRustプロジェクトの骨格と、各章の変更を検証するためのテスト基盤を整えます。手を動かすのはそこからです。

[^sqlite-arch]: SQLiteのアーキテクチャについては <https://sqlite.org/arch.html> を参照。
[^pg-arch]: PostgreSQLのクエリ処理経路(Parser、Transformation、Planner、Executor)については、PostgreSQL公式ドキュメントのクエリ処理の章を参照。
