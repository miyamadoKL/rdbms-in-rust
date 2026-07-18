# 第2章 クエリの一生

次のSQLを1本、実行してみます。

```sql
SELECT name FROM users WHERE id = 42;
```

この短い1文が結果の1行になるまでに、文字列は何段階もの中間表現へ姿を変えます。
Lexer、Parser、Binder……という名前を各章で個別に導入する前に、この1文がどの順で、どんな形へ変わっていくのかをたどっておきます。
各段階を実装するのは第4章以降であり、この章ではまだコードを書きません。

## 経路全体

SQL文字列から結果が出るまでの経路は、次の10段階です。

```text
SQL文字列
  ↓
Lexer（字句解析）
  ↓
Parser（構文解析）→ AST
  ↓
Binder（名前解決と型検査）→ Bound AST
  ↓
Logical Plan（関係代数への変換）
  ↓
Optimizer（書き換えとアクセスパス選択）
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

`SELECT name FROM users WHERE id = 42;` という1文が、各段階でどんな中間表現に変わるかを順に見ていきます。

## Lexer: 文字列をTokenに変換する

**Lexer**(字句解析器)は、SQL文字列を意味のある最小単位である**Token**の列に分割します。
空白やコメントはここで捨てられ、以後の段階には渡りません。

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

Lexerがこの段階で判断するのは、字面から一意に決まることだけです。
`id` という文字列がテーブル`users`の列かどうかは、Lexerの責務ではありません。
それは後段のBinderが、カタログと照合して初めて決まります。

## Parser: Token列を構文木に組み立てる

**Parser**(構文解析器)は、Token列をSQLの文法に従って**AST**(抽象構文木)に組み立てます。
`SELECT ... FROM ... WHERE ...` という句の並びや、`id = 42` という二項演算の優先順位は、この段階で構造として確定します。

```text
SelectStmt
 ├─ projections: [ColumnRef("name")]
 ├─ from: TableRef("users")
 └─ where: BinaryOp(Eq,
             ColumnRef("id"),
             Literal(Integer(42)))
```

ASTはSQL構文をそのまま木にしたものであり、`users`というテーブルが実在するか、`id`という列が本当に存在するかをこの時点ではまだ確認していません。
構文として正しければASTは作れます。

## Binder: 名前を解決し、型を検査する

**Binder**は、ASTに登場する識別子をカタログと照合し、実体に結びつけます。
`users`はテーブルIDへ、`name`と`id`は列インデックスと型へ解決されます。
この結果を**Bound AST**と呼びます。

```text
BoundSelect
 ├─ projections: [BoundColumnRef(table=users, index=1, type=Text)]
 ├─ from: BoundTableRef(table_id=TableId(3))
 └─ where: BinaryOp(Eq,
             BoundColumnRef(table=users, index=0, type=BigInt),
             Literal(BigInt(42)))
```

`users`というテーブルが存在しない、あるいは`id`という列がない場合、エラーになるのはこの段階です。
`id = 42`の両辺の型が比較可能かどうかの検査も、ここで行われます。

## Logical Plan: 関係代数の木に変換する

Bound ASTは、関係代数の演算子から成る**Logical Plan**へ変換されます。
`SELECT`の各句は、対応する演算子ノードになります。

```text
Projection(name)
  └─ Filter(id = 42)
       └─ Scan(users)
```

Logical Planは「何を計算するか」だけを表し、「どう計算するか」はまだ決めていません。
`Scan`が全件走査になるのかインデックスを使うのかは、この時点では未定です。

## Optimizer: 計画を書き換え、アクセスパスを選ぶ

**Optimizer**は、Logical Planを2種類の方法で改善します。

- **ルールベースの書き換え**：定数畳み込み、述語のスキャン側への押し下げ、不要な射影の削除など、常に妥当な変換を適用する
- **コストベースの選択**：Scan方式やJoin順序など複数の実行候補を生成し、統計情報から推定した行数とコストをもとに、そのうち最も安いものを選ぶ

「`id`列にインデックスがあれば、OptimizerはSequential ScanをIndex Scanに置き換える」という単純な規則は成り立ちません。
B+Treeの探索自体は、根から葉へ一度下りるだけで済み、範囲を指定した述語でも葉のページを横につなげて走査するだけです。
コストを左右するのはこの探索そのものではなく、探索で得た各RID(タプルの位置)ごとにHeap Fileのページを取得する側のランダムI/Oです。
一致したRIDごとにHeap Fileへのfetchが必要になるため、`id`の値がテーブルのほとんどの行に一致するような述語では、ページ単位でまとめて読めるSequential Scanより高くつくことがあります。
ただし、これは一致行数の分だけ必ず物理I/Oが増えるという意味ではありません。
複数のRIDが同じページを指していれば1回のfetchで済み、そのページがすでにBuffer Poolに載っていれば物理I/Oは発生しません。
ランダムI/Oが実際にどれだけ増えるかは、テーブル内でのページ局所性とキャッシュの状況次第です。
インデックスの有無は候補を生成するかどうかを決めるだけであり、実際にどちらを選ぶかは推定コストが決めます(コストの見積もり方は第28章で扱います)。

`id = 42`に一致する行数の見積もりは、根拠によって精度が異なります。
`id`列に一意制約があれば、一致する行は高々1行だと確定できます。
一意制約がなくても、統計情報から一致行数が少ないと推定できる場合があります。
この場合の推定件数は1行とは限りませんが、少数であればIndex Scanのコストは低く見積もられます。
説明のため、ここでは統計情報が一致行数を少数と見積もったと仮定します。
この仮定のもとでIndex Scanのコストが低いと推定され、Optimizerはこちらを選びます。
演算子木の親子関係を保ったまま、Physical Planでは`Scan(users)`が`IndexScan(users, id = 42)`になる程度の書き換えで済みます。

## Physical Plan: 実行アルゴリズムを確定する

Optimizerの判断は**Physical Plan**として確定します。
Logical Planの各ノードが、実際に実行可能な演算子アルゴリズムに置き換わります。

```text
Projection(name)
  └─ IndexScan(users, key: id = 42)
```

Physical Planは、後段のExecutorがそのまま実行できる形をしています。
「JoinをNested LoopでやるかHash Joinでやるか」のような選択は、この段階で既に一つに決まっています。

## Executor: Physical Planを実際に動かす

**Executor**は、Physical Planの各ノードを、共通のインターフェースを持つ演算子として実行します。

```rust
trait Executor {
    fn next(&mut self) -> Result<Option<Tuple>>;
}
```

`next()`の呼び出しは、木の根であるルート演算子(この例では`Projection`)から始まり、子へ子へと下りていきます。
`Projection`は自分の`next()`が呼ばれると、まず子である`IndexScan`の`next()`を呼びます。
`IndexScan`は自分の子を持たず、Storage Engineに対してB+Treeの検索を要求し、タプルを1件返します。

タプル自体は、この呼び出しの流れと逆に、葉から根へ向かって返っていきます。
`IndexScan`が返したタプルを`Projection`が受け取り、そこから`name`列だけを取り出して、自分の呼び出し元へ返します。
「呼び出しは根から葉へ下り、タプルは葉から根へ返る」というこの実行方式を、**Volcano型**のPull実行と呼びます。

## Storage Engine、Buffer Pool、Database File: データを取り出す

`IndexScan`が`next()`を呼ばれると、Storage EngineはB+Treeを`id = 42`で検索し、該当するタプルの位置(Record ID)を得てから、Heap Fileの該当ページを読みに行きます。

ページの読み込みは、常にディスクへ直接アクセスするわけではありません。
**Buffer Pool**がページをメモリ上にキャッシュしており、既に読み込み済みのページであればディスクI/Oは発生しません。
キャッシュに無ければ、Buffer PoolがDatabase Fileから該当ページを読み込み、キャッシュに載せてから返します。

Executorが最終的に受け取るのは、こうしてページから取り出され、Slotted Page形式からデコードされた1件のタプルです。
それが`name`列だけに射影され、クエリの結果として返ります。

## 実在DBMSでの対応

この経路は本教材独自の設計ではなく、既存のRDBMSにも同様の分離があります。

**SQLite**は、TokenizerとParserでSQLをASTに変換し、Code GeneratorがVirtual Machine向けのバイトコードを生成します。
VMがそのバイトコードを実行し、B-TreeモジュールとPage Cacheがディスク上のデータへのアクセスを担います[^sqlite-arch]。

**PostgreSQL**は、ParserがASTを生成し、Transformationがカタログ情報と結びつけた形へ変換し、PlannerがLogical PlanからPhysical Planを組み立て、ExecutorがPlanを実行します[^pg-arch]。

いずれの実装も、字句解析、構文解析、名前解決、計画生成、実行、永続化という責務を、別々のモジュールに分離しています。
本教材が全40章かけて作るモジュール群も、この分離に沿っています。

## モジュールと実装章の対応

各段階を、本教材のどの部のどの章で実装するかは次の通りです。

| 段階 | モジュール | 実装章 |
| --- | --- | --- |
| SQL文字列 → Token列 | Lexer | 第6章 字句解析器 |
| Token列 → AST | Parser | 第7章 構文解析器とAST |
| AST → Bound AST | Binder | 第17章 Binderと名前解決 |
| Bound AST → Logical Plan | 関係代数への変換 | 第18章 関係代数とLogical Plan |
| Logical Planの書き換え | ルールベースOptimizer | 第26章 ルールベース最適化 |
| アクセスパスとJoin順序の選択 | 統計情報に基づくコストベースOptimizer | 第27章 統計情報とCardinality Estimation、第28章 Cost Modelとアクセスパス選択、第29章 Join OrderとPhysical Properties |
| Logical Plan → Physical Plan、実行 | Volcano Executor | 第19章 Physical PlanとVolcano Executor |
| インデックスを使った実行 | Index Scan | 第25章 Index Scanとアクセスパス |
| キーとRIDの検索 | B+Tree | 第23〜24章 B+Tree I / II |
| テーブルデータの読み書き | Heap File、Disk Manager | 第13章 Disk ManagerとHeap File |
| ページのキャッシュ | Buffer Pool | 第14章 Buffer Pool |
| ページ形式 | Database File、Page | 第11章 データベースファイルとページ、第12章 Slotted Page、Tuple、RID |
| SQL実行経路をディスク実装へ接続 | SeqScan/DMLのHeap File化 | 第16章 SQL経路の永続化 |

第1部では、この経路のうちStorage Engineより下(Buffer Pool、Database File)を実装せず、まずインメモリのVec上でテーブルを表現します。
ディスクへの永続化は第2部から着手し、第16章でSQL実行経路をディスク実装へ接続し直します。
B+Treeは第3部後半(第23〜24章)に置いており、Join(第22章)を実装した直後に取り組む配置です。

Lexer、Parser、Binder、それぞれの内部で何を判断し、どんなデータ構造を持つかは、まだ何も決めていません。

[^sqlite-arch]: SQLiteのアーキテクチャについては <https://sqlite.org/arch.html> を参照。
[^pg-arch]: PostgreSQLのクエリ処理経路(Parser、Transformation、Planner、Executor)については、PostgreSQL公式ドキュメントのクエリ処理の章を参照。
