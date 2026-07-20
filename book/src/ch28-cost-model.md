# 第28章 Cost Modelとアクセスパス選択

`orders`の`customer_id`に索引がある結合が、いつ速くて、いつ遅いか。前章までの`optimize`はこの問いに答えていません。索引があるかどうかだけを見て、常に同じアルゴリズムを選んでいたからです。

```console
minidb> CREATE TABLE customers (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> CREATE TABLE orders (id BIGINT, customer_id BIGINT, item TEXT);
CREATE TABLE
minidb> CREATE INDEX idx_customer_id ON orders (customer_id);
CREATE INDEX
-- customer_idのほぼ全行がcustomersのどれかと一致する、密な結合になるようデータを入れる
minidb> ANALYZE customers;
ANALYZE 1
minidb> ANALYZE orders;
ANALYZE 1
minidb> EXPLAIN SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id;
QUERY PLAN
----------
Projection(customers.name, orders.item) rows=5000
  └─ IndexNestedLoopJoin(INNER JOIN, id = customer_id) rows=5000
    └─ SeqScan(customers) rows=5
    └─ IndexScan(idx_customer_id, customer_id = id) rows=1000
(4 rows)
```

これは第27章時点のコードで実際に実行した結果です。`ANALYZE`は行っていて、`customer_id = id`という一致が平均`1000`行(`orders`5,000行のほぼ全部)に達することまで見積もれています。それでも選ばれているのはIndex Nested Loop Joinのままです。第25章の実測は、この選択がこの場面ではむしろ遅いことを示していました。`m = 32000`の密な結合で、Index Nested Loop JoinはHash Joinより15倍以上遅くなります。それでも`optimize`は、索引があるという構文的な事実だけを見て、迷わずIndex Nested Loop Joinを選び続けます。

## 前章の限界: 統計はあるのに、判断基準は昔のまま

第27章は、この状況に必要な材料をすでに用意していました。`ANALYZE`が集めた行数、NDV、Histogramから、`orders.customer_id = <外側の値>`が平均して何行返すかを見積もれるようになったのです。選択的な結合と密な結合を、数字で区別できるようになりました。

それでも`physical_plan::optimize`のロジックは変わっていません。`choose_access_path`はPoint、Range、SeqScanという優先順位の中から最初に条件を満たしたものを返すだけで、Join方式の選択も「索引があればIndex Nested Loop Join、無ければHash Join」という1本のルールのままです。第27章の本文が明言していたとおり、推定行数は`EXPLAIN`の`rows=`に表示されるだけで、プラン選択には一度も使われていませんでした。

統計を持っていながらそれを使わないというのは奇妙な状態です。せっかく`orders.customer_id = 3`が何行返るかを見積もれるのに、`optimize`はその数字を一顧だにせず、索引の有無という1ビットの情報だけで判断を下します。第25章の実測が示した15倍という差は、この1ビットの判断がどれだけ荒っぽいかを物語っています。

この章がやることは単純です。索引があるかどうかで1つに決め打つのをやめ、**選べる候補をすべて実際に組み立て、それぞれのコストを見積もり、一番安いものを選ぶ**という方式に置き換えます。

## コストの分解: Sequential I/O、Random I/O、CPU per Tuple

「コスト」と一口に言っても、それが実際の秒数を指すなら、ディスクの速度やCPUのクロック、その日のOSのキャッシュ状態まで持ち込まなければ求まりません。この章のコストはそれとは違うものを指します。

候補プランを`SeqScan`、`IndexScan`、`HashJoin`、`IndexNestedLoopJoin`という具体的な演算子の木として組み立てたあと、その木がどれだけの「仕事」をするかを、3種類の重みの合成として見積もります。

このコストモデルは、新規作成する`src/cost_model.rs`に置きます。`src/lib.rs`には`pub mod cost_model;`を追加します。まずは3つの重みを、次のように定義します。

```rust
/// 1ページぶんのSequential I/O(順読み)のコスト。PostgreSQLの
/// `seq_page_cost`のデフォルト値(1.0)を採用する。他の重みはすべて
/// この値を1単位とした相対値になる。
pub const SEQ_PAGE_COST: f64 = 1.0;

/// 1ページぶんのRandom I/O(ランダムアクセス)のコスト。PostgreSQLの
/// `random_page_cost`のデフォルト値(4.0)を採用する。回転ディスクを
/// 前提に「ランダムアクセスはシーケンシャルの4倍遅い」という経験則で
/// 決められた値だが、SSDが主流になった現在のPostgreSQLでもデフォルト
/// 値は変わっていない(索引経由のアクセスがキャッシュに乗りにくい、
/// という傾向自体はSSDでも残るため)。第25章の実測(密な結合で
/// Index Nested Loop JoinがHash Joinの15倍以上遅い)も、ランダムな
/// `Storage::get`の積み重ねが`BufferPool`の置き換えを増やすという、
/// 同じ種類の非対称性を示している。
pub const RANDOM_PAGE_COST: f64 = 4.0;

/// 1行を処理する(条件を評価する、ハッシュテーブルへ出し入れする等)
/// CPUコスト。PostgreSQLの`cpu_tuple_cost`のデフォルト値(0.01)を
/// 採用する。I/Oの重み(1.0・4.0)に比べて2桁小さく、「I/Oに比べれば
/// CPU処理は軽い」という一般的な前提を反映している。
pub const CPU_TUPLE_COST: f64 = 0.01;
```

出典はPostgreSQLの`postgresql.conf.sample`が定めるデフォルト値です。`seq_page_cost`を1として、`random_page_cost`はその4倍、`cpu_tuple_cost`は2桁小さいという比率そのものに意味があります。ページを1枚読むだけの`SeqScan`と、`Storage::get`を1行ごとに呼ぶ`IndexScan`(第25章)とでは、同じ1行を取り出すのにまったく違う量の仕事を払うということを、この3つの定数が表現しています。

これらの重みを掛け合わせた値を、この章では`Cost`という専用の型で持ち回ります。

```rust
/// 単位を持たない相対コスト。
///
/// 実行時間の予測値(ミリ秒・マイクロ秒)ではない。[`SEQ_PAGE_COST`]・
/// [`RANDOM_PAGE_COST`]・[`CPU_TUPLE_COST`]という3つの重みを合成した
/// 無次元の値で、同じクエリに対する複数の候補プラン同士を比較する
/// ためだけに使う。`f64`を直接使わず`newtype`にしてあるのは、
/// 「ミリ秒として扱ってしまう」「他の`f64`の値と無自覚に混ぜてしまう」
/// といった誤用を型で防ぐためである。
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Cost(f64);
```

`Cost`が答えるのは「このクエリは何ミリ秒で終わるか」ではなく、「同じ`WHERE`に対する`SeqScan`と`IndexScan`のどちらが、より少ない仕事で済むか」という、候補どうしの大小関係だけです。実際のディスクの速度もCPUのクロックも一切知らないまま、それでも2つの数値の大小を比べれば、たいていの場合はどちらが速いかを言い当てられます。これがコストベース最適化という考え方の要です。実時間を正確に予測する必要はなく、候補の順位さえ間違えなければ十分なのです。

## この教材が扱わないもの: Memory

コストを分解する原案は、Sequential I/O、Random I/O、CPU per Tupleに加えてMemoryという4つ目の要素を挙げています。この章はそれを重み0として明示的に無視します。

理由は2つあります。1つは、`HashJoin`のBuild側(第22章)も`Sort`(第21章)も、このクレートは常にメモリ上の`Vec`や`HashMap`へ全件を保持する実装であり、作業用メモリが不足したときにディスクへ一時ファイルを吐く経路(PostgreSQLが`work_mem`を超えたときに行う外部ソートや外部ハッシュ)がそもそも存在しないことです。存在しない分岐のコストをいくら精密に見積もっても、実際の実行時の挙動とは対応しない架空の数値にしかなりません。

もう1つは、`BufferPool`の固定容量(第14章)が引き起こす影響を、すでにRandom I/O側の重みが引き受けていることです。索引経由のランダムな`Storage::get`がバッファプールの置き換えを増やすという第25章の実測は、「ランダムアクセスは高くつく」という`RANDOM_PAGE_COST`と`SEQ_PAGE_COST`の差そのもので説明できます。バッファプールの容量を独立した第4の要素として持ち込む必要は、この教材の規模では生じません。

## 演算子ごとのコスト式

重みが決まれば、あとは演算子ごとに「何ページ読むか」「何行処理するか」を数え上げるだけです。`SeqScan`は素直な式から始まります。

```rust
/// `SeqScan`のコスト。ページ数ぶんのSequential I/Oと、行数ぶんのCPUコスト
/// (各行を`decode_tuple`する処理、第13章)の和。
pub fn seq_scan_cost(pages: u64, rows: u64) -> Cost {
    Cost(pages as f64 * SEQ_PAGE_COST + rows as f64 * CPU_TUPLE_COST)
}
```

`IndexScan`は、B+Treeの根から葉までを降りる部分と、一致した行をHeapから`fetch`する部分に分かれます(第25章の`IndexScanExec::next`)。

```rust
/// `IndexScan`のコスト。Rootから葉までの`height`段はどの段も1ページの
/// Random I/O(第23章、`BTree::height`)で、一致した`matched_rows`件は
/// それぞれ`Storage::get`によるHeapページへのRandom I/Oと、1行ぶんの
/// CPUコストを要する(第25章、`IndexScanExec::next`)。
pub fn index_scan_cost(height: u64, matched_rows: u64) -> Cost {
    let descend = height as f64 * RANDOM_PAGE_COST;
    let fetch = matched_rows as f64 * (RANDOM_PAGE_COST + CPU_TUPLE_COST);
    Cost(descend + fetch)
}
```

`Filter`と`Projection`は、子から受け取った行それぞれに対して式を1回評価するだけの演算子なので、同じ形の式で見積もります。

```rust
/// `Filter`・`Projection`のコスト。どちらも子から受け取った`input_rows`件
/// それぞれに対して式を1回評価するだけなので、同じ式で見積もる。
pub fn cpu_pass_cost(input_rows: u64) -> Cost {
    Cost(input_rows as f64 * CPU_TUPLE_COST)
}
```

`NestedLoopJoin`(第22章)は、`right`をコンストラクタで1回だけ`Vec<Tuple>`へ読み切ってから、`left`の行ごとに`right`の全行と組み合わせを試します。I/Oのコストは`left`と`right`それぞれの子のコストにすでに含まれているので、この演算子自身が追加で払うのは組み合わせの評価だけです。

```rust
/// `NestedLoopJoin`のコスト。`right`は`NestedLoopJoinExec::new`(第22章)が
/// コンストラクタで1回だけ全件読み切って`Vec<Tuple>`へ保持するため、
/// I/Oは`left`・`right`それぞれの子の[`plan_cost`]にすでに含まれる。この
/// 関数が返すのは、`left`の`left_rows`行それぞれについて`right`の
/// `right_rows`行との組み合わせを1つずつ評価するCPUコスト、
/// `|L| × |R|`である。
pub fn nested_loop_join_cost(left_rows: u64, right_rows: u64) -> Cost {
    Cost(left_rows as f64 * right_rows as f64 * CPU_TUPLE_COST)
}
```

`HashJoin`(第22章)は、Build(`right`の全行をハッシュテーブルへ挿入する)とProbe(`left`の行ごとにハッシュテーブルを引く)という2段階を持ちます。どちらも1行あたりの処理はCPUコストだけで、I/Oはやはり子のコストに含まれています。

```rust
/// `HashJoin`のコスト。Build(`right`の`right_rows`行をハッシュテーブルへ
/// 挿入する)とProbe(`left`の`left_rows`行それぞれでハッシュテーブルを
/// 引く)の和(第22章、`HashJoinExec::new`・`next`)。`right`・`left`の
/// 読み取り自体のI/Oは、`HashJoin`・`SeqScan`それぞれの子の[`plan_cost`]に
/// すでに含まれる。
pub fn hash_join_cost(left_rows: u64, right_rows: u64) -> Cost {
    let build = right_rows as f64 * CPU_TUPLE_COST;
    let probe = left_rows as f64 * CPU_TUPLE_COST;
    Cost(build + probe)
}
```

`IndexNestedLoopJoin`(第25章)は、`left`の行ごとに内側テーブルの索引を1回`lookup`します。1回の`lookup`は`index_scan_cost`とまったく同じ形("木を`height`段降りてから一致行をHeapから`fetch`する")のコストを持ちますが、一致行数は`col = 定数`のような特定の値に対するものではなく、「平均的な外側の値が何行と一致するか」という見積もりになります。

```rust
/// `IndexNestedLoopJoin`のコスト。`left`の`left_rows`行それぞれについて、
/// 内側テーブルの索引を1回`lookup`する(第25章、`IndexNestedLoopJoinExec::next`)。
/// 1回の`lookup`は[`index_scan_cost`]と同じ形("木を`height`段降りてから
/// `avg_matches`件をHeapから`fetch`する")のコストを持つが、`avg_matches`は
/// 定数ではなく「平均的な外側の値が何行と一致するか」という見積もりで
/// あるため`f64`のまま受け取る(呼び出し側は
/// `estimate_rows(IndexNestedLoopJoin) / left_rows`から求める)。
pub fn index_nested_loop_join_cost(left_rows: u64, height: u64, avg_matches: f64) -> Cost {
    let per_probe = height as f64 * RANDOM_PAGE_COST + avg_matches * (RANDOM_PAGE_COST + CPU_TUPLE_COST);
    Cost(left_rows as f64 * per_probe)
}
```

最後に`Sort`(第21章)です。比較回数のオーダーである`n log n`に、比較1回ぶんのCPUコストを掛けます。

```rust
/// `Sort`のコスト。比較回数のオーダーである`n log n`に、比較1回ぶんの
/// CPUコストとして[`CPU_TUPLE_COST`]を掛ける。`rows`が0または1の場合は
/// 並べ替える必要が無いので`Cost::ZERO`。
pub fn sort_cost(rows: u64) -> Cost {
    if rows <= 1 {
        return Cost::ZERO;
    }
    Cost(rows as f64 * (rows as f64).log2() * CPU_TUPLE_COST)
}
```

`Values`、`Limit`、`Insert`、`Update`、`Delete`は、この章では演算子自身の追加コストを0として扱います。`Values`はSQL文の長さぶんの行しか評価しない葉であり、`Limit`は子から早期に`next()`を止めるだけで追加の仕事をしません。`Insert`、`Update`、`Delete`は`Executor`を経由しない一括処理(`crate::physical_plan`冒頭)で、この章ではコストベースの選択対象にもしていないため、`input`の累積コストをそのまま返します。

これらの式に渡す`pages`と`height`は、可能な限り実測値を使います。`Storage::table_page_count`はテーブルが現在使っているデータページ数を`ANALYZE`の有無に関わらず常に返し、`BTree::height`(第23章)は索引の実際の階層数を返します。`Backend::Memory`のようにこれらを参照できない場合や、テーブルがまだ1ページも確保していない場合は、`DEFAULT_ROWS_PER_PAGE`(1ページあたり50行という控えめな仮定)や`DEFAULT_INDEX_HEIGHT`(3)にフォールバックします。

## 候補を列挙してコスト最小を選ぶ

コストの計算式が揃ったところで、`physical_plan::optimize`の中身を書き換えます。第25章までの`optimize`は`storage`と`predicate`から1つの`AccessPath`をルールで決め打っていましたが、この章はまず候補をすべて`PhysicalPlan`として組み立ててから、コストで比較します。

ここからは`src/physical_plan.rs`への追記です。まず、複数の候補から最小コストのものを選ぶ`cheapest`を追加します。

```rust
/// 複数の候補`PhysicalPlan`から、[`crate::cost_model::plan_cost`]が最小になる
/// ものを選ぶ。`candidates`は空であってはならない(呼び出し元が必ず1個以上を
/// 積む)。コストが等しい場合は出現順で最初の候補を選ぶ(`Iterator::min_by`と
/// 同じ「最初に見つかった最小値を残す」規則、第25章までの優先順位に代わる
/// 決定的な同点処理)。
fn cheapest(candidates: Vec<PhysicalPlan>, stats: &dyn StatsLookup, storage: Option<&Storage>) -> PhysicalPlan {
    candidates
        .into_iter()
        .min_by(|a, b| {
            cost_model::plan_cost(a, stats, storage).value().partial_cmp(&cost_model::plan_cost(b, stats, storage).value()).expect(
                "コストはNaN・無限大にならない(行数・ページ数はすべて有限のu64からf64へ変換した値であるため)",
            )
        })
        .expect("candidatesは呼び出し元が必ず1個以上積む")
}
```

`cheapest`は演算子の種類を一切知りません。渡された`PhysicalPlan`のリストのコストを比べて、最小のものを返すだけです。この関数をアクセスパスとJoin方式の両方から呼べば、選択のロジックそのものが1箇所にまとまります。

アクセスパスの選択は、`scan_plan_candidates`(`src/physical_plan.rs`、行数の都合で本文には全文を載せない)が、`WHERE`から取り出せるPoint述語ごと、Range述語ごとに`IndexScan`候補を1つずつ、そして`SeqScan`候補を1つ組み立てます。第25章は最初に見つかった1つを優先順位で選んでいましたが、この章は見つかった候補を**全部**残し、`cheapest`に委ねます。

```rust
fn choose_scan_plan(storage: &Storage, scan: logical_plan::ScanNode, predicate: BoundExpr, stats: &dyn StatsLookup) -> PhysicalPlan {
    let candidates = scan_plan_candidates(storage, &scan, predicate);
    cheapest(candidates, stats, Some(storage))
}
```

Join方式の選択も同じ形です。等値結合の鍵が取り出せれば`HashJoin`は常に候補になり、内側テーブルの結合列に索引があれば`IndexNestedLoopJoin`も候補に加わります。

```rust
/// 等値結合の鍵`keys`が取り出せた場合の候補を組み立て、
/// [`crate::cost_model::plan_cost`]が最小のものを選ぶ(第28章)。
///
/// `HashJoin`は常に候補になる(`right`を索引に頼らず全件読み切れる、
/// 第22章)。[`index_scan_target`]が内側テーブルの索引を見つけられた場合は
/// `IndexNestedLoopJoin`も候補に加わる。第25章まではIndex Nested Loop Joinを
/// 見つかり次第無条件に選んでいたが、第25章末の実測が示すとおり、密な結合
/// (内側テーブルのほぼ全行が一致する)ではHash Joinの方が15倍以上速い場合が
/// ある。ここでは2つの候補を実際にコストで比較し、統計情報(第27章)から
/// 見積もった一致行数に応じてどちらが有利かを判断する。
fn choose_join_plan(
    storage: Option<&Storage>,
    stats: &dyn StatsLookup,
    left: PhysicalPlan,
    right: PhysicalPlan,
    kind: JoinKind,
    condition: BoundExpr,
    keys: Vec<(BoundExpr, BoundExpr)>,
) -> PhysicalPlan {
    let index_target = index_scan_target(storage, &right, &keys);

    let mut candidates = Vec::new();
    if let Some(target) = &index_target {
        candidates.push(PhysicalPlan::IndexNestedLoopJoin(IndexNestedLoopJoinNode {
            left: Box::new(left.clone()),
            kind,
            condition: condition.clone(),
            outer_key: target.outer_key.clone(),
            table_id: target.table_id,
            table_name: target.table_name.clone(),
            schema: target.schema.clone(),
            index_name: target.index_name.clone(),
            column_name: target.column_name.clone(),
        }));
    }
    candidates.push(PhysicalPlan::HashJoin(HashJoinNode { left: Box::new(left), right: Box::new(right), kind, keys, condition }));

    cheapest(candidates, stats, storage)
}
```

`NestedLoopJoin`だけは、この章でもコストで他候補と比較しません。等値条件が1つも取り出せない結合(`ON true`のような実質的な直積や、`a.x < b.y`のような不等号条件)は、`HashJoin`と`IndexNestedLoopJoin`のどちらの実行アルゴリズムにも要求する「等値の鍵」を持たないため、比較する候補がそもそも`NestedLoopJoin`しかありません。

これらを束ねる`optimize`本体は、`storage`に加えて`stats: &dyn StatsLookup`(第27章)を受け取るようになりました。行数の推定にはこの`stats`を使います。

```rust
pub fn optimize(plan: LogicalPlan, storage: Option<&Storage>, stats: &dyn StatsLookup) -> PhysicalPlan {
    match plan {
        LogicalPlan::Scan(scan) => PhysicalPlan::SeqScan(SeqScanNode {
            table_id: scan.table_id,
            table_name: scan.table_name,
            schema: scan.schema,
        }),
        LogicalPlan::Values(values) => PhysicalPlan::Values(ValuesNode { schema: values.schema, rows: values.rows }),
        LogicalPlan::Filter(filter) => match (storage, *filter.input) {
            (Some(storage), LogicalPlan::Scan(scan)) => choose_scan_plan(storage, scan, filter.predicate, stats),
            (_, input) => PhysicalPlan::Filter(FilterNode {
                input: Box::new(optimize(input, storage, stats)),
                predicate: filter.predicate,
            }),
        },
        LogicalPlan::Join(join) => {
            let left = optimize(*join.left, storage, stats);
            let right = optimize(*join.right, storage, stats);
            let left_len = left.output_schema().len();
            match split_equi_join_keys(&join.condition, left_len) {
                Some(keys) => {
                    let keys: Vec<(BoundExpr, BoundExpr)> = keys
                        .into_iter()
                        .map(|(left_key, right_key)| (left_key, shift_column_index(&right_key, left_len)))
                        .collect();
                    choose_join_plan(storage, stats, left, right, join.kind, join.condition, keys)
                }
                None => PhysicalPlan::NestedLoopJoin(NestedLoopJoinNode {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind: join.kind,
                    condition: join.condition,
                }),
            }
        }
        LogicalPlan::Aggregate(aggregate) => PhysicalPlan::Aggregate(AggregateNode {
            input: Box::new(optimize(*aggregate.input, storage, stats)),
            group_by: aggregate.group_by,
            calls: aggregate.calls,
            schema: aggregate.schema,
        }),
        LogicalPlan::Projection(projection) => PhysicalPlan::Projection(ProjectionNode {
            input: Box::new(optimize(*projection.input, storage, stats)),
            projection: projection.projection,
        }),
        LogicalPlan::Distinct(distinct) => {
            PhysicalPlan::Distinct(DistinctNode { input: Box::new(optimize(*distinct.input, storage, stats)) })
        }
        LogicalPlan::Sort(sort) => {
            PhysicalPlan::Sort(SortNode { input: Box::new(optimize(*sort.input, storage, stats)), keys: sort.keys })
        }
        LogicalPlan::Limit(limit) => PhysicalPlan::Limit(LimitNode {
            input: Box::new(optimize(*limit.input, storage, stats)),
            limit: limit.limit,
            offset: limit.offset,
        }),
        LogicalPlan::Insert(insert) => PhysicalPlan::Insert(InsertNode {
            table_id: insert.table_id,
            table_name: insert.table_name,
            schema: insert.schema,
            columns: insert.columns,
            input: Box::new(optimize(*insert.input, storage, stats)),
        }),
        LogicalPlan::Update(update) => PhysicalPlan::Update(UpdateNode {
            table_id: update.table_id,
            table_name: update.table_name,
            schema: update.schema,
            assignments: update.assignments,
            predicate: update.predicate,
            input: Box::new(optimize(*update.input, storage, stats)),
        }),
        LogicalPlan::Delete(delete) => PhysicalPlan::Delete(DeleteNode {
            table_id: delete.table_id,
            table_name: delete.table_name,
            schema: delete.schema,
            predicate: delete.predicate,
            input: Box::new(optimize(*delete.input, storage, stats)),
        }),
    }
}
```

`Database::execute_select`と`execute_explain`は、`self`(`StatsLookup`を実装する`Database`自身、第27章)をそのまま`stats`として渡します。`ANALYZE`していないテーブルは`estimator::DEFAULT_ROW_COUNT_ESTIMATE`(1000)にフォールバックするので、統計が無くても`optimize`はクラッシュせず、単に粗い見積もりのままコストを比べます。

## 測って確認する: 第25章の逆転ケースの回収

`EXPLAIN`は、選ばれたプランのコストも表示するようになりました。`rows=`の後ろに`cost=<推定コスト>`(小数点以下2桁)が付きます。

```console
minidb> EXPLAIN SELECT id, amount, name FROM orders WHERE id = 42;
QUERY PLAN
----------
Projection(id, amount, name) rows=1 cost=12.02
  └─ IndexScan(idx_id, id = 42) rows=1 cost=12.01
(2 rows)
```

これは`ANALYZE`済みの1,000行のテーブルに対するPoint述語です。`id`列はDistinct値が1,000あるため、`id = 42`の一致行数はほぼ1行と見積もられ、Index Scanの`cost=12.01`が、全ページを読む`SeqScan`のコストを大きく下回ります。

本題である第25章の逆転ケースを、実際に再現します。`customers`(5行)と`orders`(5,000行)を等値結合し、`customer_id`の分布だけを変えます。**選択的な結合**では、`customer_id`を`customers`の総数よりずっと広い範囲(`5 × 1000`)に散らします。

```console
minidb> CREATE TABLE customers (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> CREATE TABLE orders (id BIGINT, customer_id BIGINT, item TEXT);
CREATE TABLE
minidb> CREATE INDEX idx_customer_id ON orders (customer_id);
CREATE INDEX
minidb> -- customer_idを0〜4999の範囲に散らし、customersの5件とごく一部しか一致しない
minidb> ANALYZE customers;
ANALYZE 1
minidb> ANALYZE orders;
ANALYZE 1
minidb> EXPLAIN SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id;
QUERY PLAN
----------
Projection(customers.name, orders.item) rows=5 cost=101.15
  └─ IndexNestedLoopJoin(INNER JOIN, id = customer_id) rows=5 cost=101.10
    └─ SeqScan(customers) rows=5 cost=1.05
    └─ IndexScan(idx_customer_id, customer_id = id) rows=1 cost=20.01
(4 rows)
```

`customers`の5行ぶんの`lookup`しか払わないIndex Nested Loop Joinが選ばれ、コストは`101.10`です。同じテーブル定義のまま、`customer_id`の値域だけを`customers`の総数(5)に絞り、**密な結合**に変えます。

```console
minidb> -- customer_idを0〜4の範囲に絞り、ほぼ全行がcustomersのどれかと一致する
minidb> ANALYZE customers;
ANALYZE 1
minidb> ANALYZE orders;
ANALYZE 1
minidb> EXPLAIN SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id;
QUERY PLAN
----------
Projection(customers.name, orders.item) rows=5000 cost=197.10
  └─ HashJoin(INNER JOIN, id = customer_id) rows=5000 cost=147.10
    └─ SeqScan(customers) rows=5 cost=1.05
    └─ SeqScan(orders) rows=5000 cost=96.00
(4 rows)
```

`customer_id`の分布を変えただけで、選ばれる演算子がIndex Nested Loop JoinからHash Joinへ切り替わりました。索引は`idx_customer_id`としてずっと存在しているのに、密な結合ではその索引を経由するIndex Nested Loop Joinを、`cheapest`が自分から避けています。第25章が実測でしか示せなかった「索引があるかどうかでは選択の正しさを決められない」という事実に、この章のコストベース最適化はついに数値で答えを返せるようになりました。

## この章の限界: Bitmap ScanがまだIndex Range Scanを補っていない

コストベースの選択には副作用もあります。`amount`が2つの値(`1000`と`2000`)にだけ集中し、残りが`[0, 899]`と`[2001, 4000]`へ均等に散らばる5,000行のテーブルを作り、その間([1400, 1600])を範囲述語で問い合わせます。

```console
minidb> CREATE INDEX idx_amount ON orders (amount);
CREATE INDEX
minidb> -- amountの大半(4,500行)は[0, 899]と[2001, 4000]に均等分布し、
minidb> -- 500行だけが1000(250行)と2000(250行)の2値に集中する
minidb> ANALYZE orders;
ANALYZE 1
minidb> EXPLAIN ANALYZE SELECT id FROM orders WHERE amount >= 1400 AND amount <= 1600;
QUERY PLAN
----------
Projection(id) rows=1288 cost=158.88 actual=0
  └─ Filter(amount >= 1400 AND amount <= 1600) rows=1288 cost=146.00 actual=0
    └─ SeqScan(orders) rows=5000 cost=96.00 actual=5000
(3 rows)
```

`amount`が`1000`と`2000`しか取らない以上、`[1400, 1600]`に実際に一致する行は`actual=0`件です。それでも見積もりは`rows=1288`(全体の26%)まで膨らみ、`idx_amount`という索引が存在するのに選ばれたのはSeqScanでした。

原因は、第4部レビュー対応で`bucket_overlap_fraction`(第27章、`crate::estimator`)がBIGINTに対して行うようになった線形補間の前提にあります。この補間は「バケツの`[lower, upper]`区間の中で値が一様に分布している」と仮定して区間内の位置から按分します。`1000`と`2000`という2つの値だけが、ソート順で連続する1つのバケツ(`[1000, 2000]`、500行)に押し込まれているこの分布では、区間の中身は実際には両端に偏っており、中央付近(`1400`〜`1600`)にはそもそも1行もありません。バケツの境界(`lower`と`upper`)だけを見て「区間内は一様」と仮定する限り、この偏りは見積もりに反映しようがありません。

`index_scan_cost`は一致行数1件ごとに`RANDOM_PAGE_COST`(Heapページへの`Storage::get`)を払う式である以上、26%程度の一致行数を見積もられたIndex Range Scanは、`SeqScan`の1ページあたりのコストに到底かないません。これは見積もりの粗さだけの問題ではなく、この教材の実装が抱える正直な限界です。実務のRDBMSは、一致する`RecordId`を先にページ順へソートしてからHeapを読む**Bitmap Index Scan**(PostgreSQLにもある方式)を持ち、ランダムアクセスの回数そのものを減らします。この章はBitmap Scanを実装しないため、範囲述語はよほど選択的でない限りSeqScanのままになります。

## 到達点

`choose_scan_plan`と`choose_join_plan`は、第25章の固定優先順位を、候補をすべて構築してからコスト最小のものを選ぶ方式へ置き換えました。`cost_model::plan_cost`は`SeqScan`、`IndexScan`、`Filter`、`Projection`、`NestedLoopJoin`、`HashJoin`、`IndexNestedLoopJoin`、`Sort`のそれぞれに、Sequential I/O、Random I/O、CPU per Tupleという3つの重みから組み立てたコストを与え、`EXPLAIN`はそのコストを`cost=`として表示します。

第25章が実測でしか示せなかった「索引があるかどうかだけでは選択の正しさを決められない」という限界に、この章はようやく数値で応えました。選択的な結合ではIndex Nested Loop Joinが、密な結合ではHash Joinが、同じ`optimize`から自動的に選ばれます。一方で、Histogramの粒度に起因するRange Index Scanの弱さのように、コストベースの選択自体が新しく明らかにする限界もあります。

複数テーブルのJoin順序をどう決めるかは、まだ手つかずのままです。第29章は、この章のコストモデルを使って、3つ以上のテーブルを結合するときにどの順番で結合すれば全体のコストが最小になるかを探索します。

## 演習問題

### 必須課題

1. `cost_model::DEFAULT_ROWS_PER_PAGE`と`DEFAULT_INDEX_HEIGHT`は、`Backend::Memory`や統計の無いテーブルに対して使われるフォールバック定数です。`Database::memory`で作ったテーブルに対して`EXPLAIN`を実行し、`cost=`がこのフォールバック値からどう計算されているかを、`seq_scan_cost`と`index_scan_cost`の式を手で辿って確認してください。
2. 本文の「密な結合」の例は`customer_id`の値域を`customers`の総数に絞ることで作りました。値域を`customers`の総数の2倍、5倍、10倍…と徐々に広げていき、`EXPLAIN`が選ぶプランがHash JoinからIndex Nested Loop Joinへ切り替わる境界がどのあたりにあるかを実測してください。
3. `cost_model.rs`の`plan_cost`は`Limit`のコストを常に0(子のコストをそのまま返す)として扱っています。これは`LIMIT 5`のような句が、実際には`SeqScan`の全行を読み切る前に止まる場合があることを無視した単純化です。`Limit`の子が`SeqScan`のときに限り、`limit`件を読むために必要な推定ページ数だけのコストにする(全ページを読む前提のコストより小さくする)よう`plan_cost`を改良し、その前後で`EXPLAIN`の`cost=`がどう変わるかをテストで確認してください。
4. 「この章の限界」で見たとおり、線形補間は「バケツの区間内で値が一様に分布している」という仮定に立っており、この仮定が崩れる分布(1つのバケツの中身が両端に偏っているなど)では過大評価が残ります。`HISTOGRAM_BUCKET_COUNT`(第27章)を増やしてバケツを細かくすると、この種の偏りに対する見積もり誤差がどこまで縮むかを実測してください。

### 発展課題

1. この章はBitmap Index Scanを実装しません。索引から得た`RecordId`をあらかじめHeapページ順にソートしてからHeapを読む`BitmapIndexScanNode`を設計し、そのコスト式(一致行数ではなく、一致行が散らばっている異なるページ数に比例するコスト)を`cost_model.rs`に追加してください。「この章の限界」で見た`amount >= 1400 AND amount <= 1600`のような範囲述語で、Bitmap Index ScanがSeqScanより安くなる場合があるかを確認してください。
2. `choose_join_plan`は等値結合の鍵が取り出せた場合、`HashJoin`と`IndexNestedLoopJoin`だけを候補にし、`NestedLoopJoin`を候補から外しています。実務のRDBMSでは、内側テーブルが極端に小さい場合(数行程度)、`NestedLoopJoin`が索引の`lookup`コストすら不要な分だけ有利になることがあります。`NestedLoopJoin`も候補に加え、3つの候補からコスト最小を選ぶよう`choose_join_plan`を拡張し、内側テーブルの行数を変えながらどちらが選ばれるかを観察してください。
3. `cost_model::Cost`は`PartialOrd`だけを持ち、`Ord`は実装していません(`f64`がNaNを持ちうるため)。`cheapest`は`partial_cmp`の`None`を`expect`でpanicに倒すことでこの問題を回避していますが、より安全な設計として、コストが不正な値(NaN、無限大)になりえないことを型で保証する`Cost`の代替実装(例えば固定小数点、あるいは`NonNan`のような検証済みラッパー)を検討し、実装してください。
