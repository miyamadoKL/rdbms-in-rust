# 第29章 Join OrderとPhysical Properties

`customers`、`orders`、`shipments`という3つのテーブルを結合するとき、どの2つを先に結合するかによって、実行コストはどれだけ変わるでしょうか。

```console
minidb> CREATE TABLE customers (id BIGINT NOT NULL, name TEXT);
minidb> CREATE TABLE orders (id BIGINT NOT NULL, customer_id BIGINT, country BIGINT);
minidb> CREATE TABLE shipments (id BIGINT NOT NULL, country BIGINT);
-- customersは5件。ordersのcustomer_idは0〜399へ広く散らし、customersの5件とだけ
-- 選択的に一致させる。ordersとshipmentsのcountryはどちらも0〜9の低カーディナリティ
-- (2,000件ずつ)。
minidb> ANALYZE customers;
minidb> ANALYZE orders;
minidb> ANALYZE shipments;
minidb> EXPLAIN SELECT customers.name, orders.id FROM shipments JOIN orders ON shipments.country = orders.country JOIN customers ON orders.customer_id = customers.id;
```

`FROM`には`shipments`、`orders`、`customers`の順に書きました。
第22章の`build_from`は、`JOIN`をそのまま左深い木にする、と決めています。
`(shipments JOIN orders) JOIN customers`という木がそのまま実行計画になるなら、まず`shipments`と`orders`を`country`という列で結合することになります。

`country`は`shipments`にも`orders`にもある、0から9までの10種類しか値を持たない列です。
2,000行の`shipments`と2,000行の`orders`を`country`で結合すると、一致する組み合わせは`(2000 × 2000) / 10 = 400,000`件規模に膨らみます。
その後で`customers`を結合しても、すでに膨らんだ40万件規模の中間結果を1件ずつ処理する仕事は消えません。
一方、`orders`と`customers`を先に`customer_id`で結合すれば、`customers`側は5件しかないので中間結果はごく小さく抑えられ、`shipments`との結合はその小さな中間結果を相手にするだけで済みます。

同じ3テーブル、同じ`WHERE`無しの`JOIN`条件でも、結合する順序によって払う仕事の量がまったく違います。

## 前章の限界: SQLに書いた順序がそのまま実行順序になる

第28章までの`physical_plan::optimize`は、`LogicalPlan::Join`を見つけるたびに`left`と`right`をそれぞれ再帰的に`optimize`し、`choose_join_plan`でHash JoinとIndex Nested Loop Joinをコストで比較していました。
比較していたのは、あくまで**1つのJoinノードの実行アルゴリズム**です。
`(shipments JOIN orders) JOIN customers`という木の形そのもの、つまり「どの2つを先に結合するか」は、`build_from`が`FROM`の記述順から機械的に組み立てた形のまま、一度も疑われていませんでした。

この章の`combine_in_syntactic_order`(このあとフォールバック経路として実装に残す関数)に、`FROM`の記述順のまま3テーブルを結合させると、次の計画になります。

```text
HashJoin(INNER JOIN, customer_id = id)     cost=201.10
  └─ HashJoin(INNER JOIN, country = country)
    └─ Projection(country)
      └─ SeqScan(shipments)
    └─ SeqScan(orders)
  └─ SeqScan(customers)
```

`country`どうしの低選択的な結合を先に行い、`customers`との選択的な結合を最後に回すという、まさに冒頭で見積もった悪い順序です。
`cost_model::plan_cost`(第28章)でこの計画のコストを求めると`201.10`になります。
Join方式の選択はコストベースになったのに、Join順序の選択には最初から候補が1つしかありませんでした。

## Join GraphとLeft-deep木: 探索空間を絞る

`n`個のテーブルを結合する順序は、単純に数えると`n!`通りあります。
さらに、どの2つをひとまとめにしてから次のテーブルと結合するか(木の形)まで考えると、候補はもっと増えます。

この章は、探索する木の形を**Left-deep木**に絞ります。
`right`は必ず単一のテーブル(の物理計画)で、それまでに結合したテーブル群を表す`left`に1個ずつ追加していく形しか考えません。
`(A JOIN B) JOIN (C JOIN D)`のような、両側が複数テーブルの木(**Bushy木**)は候補にしません。

Left-deep木に絞る動機は、探索を減らすことだけではありません。
第22章の`HashJoin`はBuild側(`right`)を先に丸ごと読み切ってからProbe側(`left`)を1行ずつ流す実装であり、`left`を「それまでの結合結果」、`right`を「新しく加える1個のテーブル」に固定すると、Pull型Executor(第19章)の構造をそのまま使い回せます。
Bushy木は両側とも中間結果になるため、Build側の材質化タイミングや、`IndexNestedLoopJoin`(第25章)が前提とする「`right`は`SeqScan`のまま」という条件と整合させるのに、実行エンジン側の変更が要ります。
探索対象を絞るという判断が、そのままExecutorを変えずに済ませるという判断にもなっています。

もう1つの制限は、**部分集合ごとに最良の1個だけを覚える**ことです。
「テーブル`{customers, orders}`を結合する`left`側の作り方」は、内部の結合順序が何通りあっても、コスト最小の1個だけを覚えておけば十分です。
`{customers, orders, shipments}`を作るときにどの順序で`{customers, orders}`を作ったかは、それ単体のコストさえ分かっていればもう関係ありません(最適性の原理)。
これにより、探索する候補は「テーブルの部分集合」の数(`2^n`)だけに収まります。
Left-deep限定とこの最適性の原理は、どちらもSystem R(1979年の論文でSelingerらが示した、最初のコストベースオプティマイザ)がとった設計そのものです。

## Subset DP: 部分集合ごとに最良の1つだけを覚える

この章の新しいモジュール`join_order`は、この動的計画法(Dynamic Programming、以下DP)を実装します。
部分集合は`u32`のビットマスクで表します。

```rust
/// DPの1状態(部分集合)が持つ、その部分集合に対する最良の計画。
#[derive(Clone)]
struct DpEntry {
    /// この部分集合を結合した左深いPhysicalPlan。
    plan: PhysicalPlan,
    /// `plan`を組み立てるのに使ったテーブルの並び(元の`FROM`での位置、
    /// 0始まり)。`plan`の出力スキーマは、この並びの各テーブルのスキーマを
    /// 順に連結したものになる。
    order: Vec<usize>,
    cost: Cost,
}
```

`order`を持ち回っているのが、この実装のややこしいところです。
`Binder`(第17章)が`ON`条件の`BoundExpr::ColumnRef`へ割り当てる`column_index`は、`FROM`に書かれたテーブルの並びをそのまま連結した、結合後スキーマ上の添字です。
DPが`shipments`より先に`customers`と`orders`を結合すると決めた瞬間、実際に組み立てる`PhysicalPlan`の列の並びは、その`Binder`が前提にしていた並びとずれます。
`order`は、今組み立てている部分集合が実際にどの並びで列を連結したかを覚えておき、次にテーブルを1個追加するときに`ON`条件の列添字をその並びへ組み替える(`remap_condition`)ために使います。

DP本体は、部分集合を表す`mask`を`1`から`2^n - 1`まで昇順に見ていきます。

```rust
let full_mask: u32 = (1u32 << n) - 1;
for mask in 1u32..=full_mask {
    if mask.count_ones() < 2 {
        continue; // ビット1個の部分集合は初期状態としてすでに登録済み
    }
    let mut connected_best: Option<DpEntry> = None;
    let mut disconnected_best: Option<DpEntry> = None;
    for (i, right_plan) in leaf_plans.iter().enumerate() {
        let bit = 1u32 << i;
        if mask & bit == 0 {
            continue;
        }
        let prev_mask = mask & !bit;
        let Some(prev) = dp.get(&prev_mask) else { continue };
        // prev(mask からiを除いた部分集合)へiを1個追加した候補を作り、
        // costで比較する(以下、connecting_condition・join_leafに続く)
        ...
    }
    let best = connected_best.or(disconnected_best).expect("popcount>=2のmaskは、必ずどれかの葉を1個足す経路を持つ");
    dp.insert(mask, best);
}
```

`mask`を値の昇順に見ていくのがポイントです。
`mask`からどれか1ビット落とした`prev_mask`は、`mask`より必ず小さい値になります。
昇順に埋めていけば、`prev_mask`のDPはこの時点ですでに確定しており、`dp.get(&prev_mask)`が失敗することはありません。

`mask`に含まれるテーブルの数だけ、「最後に加えたのはこのテーブルだった」という場合分けを試します。
それぞれについて、`prev`(1個少ない部分集合の最良解)に`i`を1個結合した候補を組み立て、`cost_model::plan_cost`でコストを求め、最小のものを`dp[mask]`として残します。
`n`個のテーブルに対して状態数は`2^n`、各状態が高々`n`個の拡張先を試すので、計算量は`O(n × 2^n)`です。

## Cartesian Productの抑制

`connecting_condition`と`connected_best`、`disconnected_best`という2つの変数が、この章のもう1つの主題です。

```rust
let condition = connecting_condition(&edges, prev_mask, i, n, &orig_offset, &new_offset);
let is_connected = condition.is_some();
...
let slot = if is_connected { &mut connected_best } else { &mut disconnected_best };
if slot.as_ref().is_none_or(|best: &DpEntry| candidate.cost.value() < best.cost.value()) {
    *slot = Some(candidate);
}
```

`connecting_condition`は、`prev_mask`に含まれるどれかのテーブルと`i`を結ぶ`ON`条件を探します。
見つからなければ`None`を返し、それは「`i`を`prev_mask`の結果に加えるには、`ON`条件を持たないCartesian Product(直積)しか無い」ことを意味します。

`ON`条件を1つも持たない2つのテーブル同士を結合すると、結果行数は双方の行数の積になり、たいてい爆発します。
この章のDPは、連結できる拡張(`connected_best`)が1つでもあれば、それだけを候補にします。
連結できる拡張が1つも無いときに限り、Cartesian Product(`disconnected_best`)を許します。
「結合条件で繋がる拡張を優先し、繋がらない組は連結不能な場合のみ許可する」という規則は、コード上はこの2行(`connected_best.or(disconnected_best)`)に集約されています。

3テーブルのうち、2つの間に`ON`条件が無い場合で確かめます。

```console
minidb> CREATE TABLE a (id BIGINT NOT NULL);
minidb> CREATE TABLE b (id BIGINT NOT NULL);
minidb> CREATE TABLE c (id BIGINT NOT NULL);
minidb> INSERT INTO a VALUES (1), (2);
minidb> INSERT INTO b VALUES (1), (2);
minidb> INSERT INTO c VALUES (1), (2);
minidb> ANALYZE a;
minidb> ANALYZE b;
minidb> ANALYZE c;
minidb> EXPLAIN SELECT a.id FROM a JOIN b ON a.id = b.id JOIN c ON true;
QUERY PLAN
----------
Projection(a.id) rows=1 cost=3.17
  └─ Filter(true) rows=1 cost=3.16
    └─ NestedLoopJoin(INNER JOIN, true) rows=2 cost=3.14
      └─ HashJoin(INNER JOIN, id = id) rows=2 cost=2.08
        └─ SeqScan(a) rows=2 cost=1.02
        └─ SeqScan(b) rows=2 cost=1.02
      └─ SeqScan(c) rows=2 cost=1.02
(7 rows)
```

`a`と`b`はHash Joinで先に結合され、`c`だけが最後に`NestedLoopJoin(INNER JOIN, true)`としてCartesian Productに回っています。
`a`、`b`、`c`のどの2つを先に結合しても、`c`を含む結合だけは条件を持てないので、DPは`{a, b}`という連結できる部分集合を先に完成させ、`c`をやむを得ず最後に掛け合わせる、という一番マシな配置を選んでいます。

## テーブル数の上限とフォールバック

DPの状態数は`2^n`です。

```rust
pub const MAX_DP_TABLES: usize = 8;
```

`n = 8`なら256状態、各状態が高々8個の拡張先を試すため、2,000通り程度の候補を評価するだけで済み、この教材の実行時間としては問題になりません。
`n`がこれを超える`FROM`は、`optimize_join_order`がDPを打ち切り、`combine_in_syntactic_order`(「前章の限界」で見た、構文順のまま左深い木を組み立てる関数)へフォールバックします。

```rust
if n > MAX_DP_TABLES {
    let leaf_plans: Vec<PhysicalPlan> = leaves.into_iter().map(|leaf| physical_plan::optimize(leaf, storage, stats)).collect();
    return combine_in_syntactic_order(leaf_plans, conditions, storage, stats);
}
```

3〜4テーブルの結合を主な対象とするこの章の演習の規模からすれば、`8`という上限にはかなり余裕があります。
それでも`n`が大きくなるほど、DPが見つける最適な順序と、構文順で妥協するフォールバックとの差は開きうる、という制約はこの章の範囲では受け入れます(章末の演習課題)。

## 実装: 葉と`ON`条件を平らにしてから組み立て直す

DPが探索するのは、あくまで**3個以上のテーブルを結合するINNER JOINの連鎖**です。
2テーブルの結合(`JOIN`が1個)には、そもそも順序の選びようがないので、この章でも第22章、第25章、第28章の`choose_join_plan`をそのまま使います。

```rust
fn flatten_join_chain(plan: LogicalPlan, leaves: &mut Vec<LogicalPlan>, conditions: &mut Vec<BoundExpr>) {
    match plan {
        LogicalPlan::Join(join) => {
            flatten_join_chain(*join.left, leaves, conditions);
            conditions.push(join.condition);
            leaves.push(*join.right);
        }
        other => leaves.push(other),
    }
}
```

`build_from`(第22章)が組み立てる木は、常に`((t0 JOIN t1) JOIN t2) JOIN ...`という形で、`right`は必ずその段で新しく加わった1個の葉、`left`はさらに`Join`か最初の葉です。
`rules::optimize`のPredicate Pushdown(第26章)は`left`、`right`の直上に`Filter`を追加することはあっても、`Join`の構造そのものは変えません。
`flatten_join_chain`はこの木を、`n`個の葉(`Scan`または`Filter(Scan)`)と`n - 1`個の`ON`条件へ平らにします。

`physical_plan::optimize`の`LogicalPlan::Join`の分岐は、葉が3個以上のときだけこの章の`join_order::optimize_join_order`へ委ねます。

```rust
let mut leaves = Vec::new();
let mut conditions = Vec::new();
flatten_join_chain(LogicalPlan::Join(join), &mut leaves, &mut conditions);
if leaves.len() >= 3 {
    crate::join_order::optimize_join_order(leaves, conditions, storage, stats)
} else {
    // 第22・25・28章と同じ、2引数のchoose_join_plan
    ...
}
```

`ON`条件は、`FROM`に登場する全テーブルの結合後スキーマを前提にした添字を持っています。
`optimize_join_order`は、それぞれの`ON`条件をANDの連言に分解し、参照するテーブルの数で仕分けます。
ちょうど2個のテーブルを参照する項が、Join Graphの辺(`edges`)になります。
1個だけを参照する項(`WHERE`ではなく`ON`の側に紛れ込んだ単一テーブルの条件)はその葉自身の`Filter`へ折りたたみ、0個または3個以上を参照する項(`ON true`のような定数、複数テーブルにまたがる式)は、最後にまとめて結果全体への`Filter`として適用します。

DPが選んだ部分集合の並び(`order`)が、元の`FROM`の並びと違う場合は、その上に立つ`Filter`、`Projection`、`Aggregate`が引き続き元の結合後スキーマの添字を使えるよう、列を元の並びへ戻す`Projection`を1段だけ追加します(`reorder_to_original_layout`)。
この`Projection`自体もタダではなく、結果行数ぶんの`CPU_TUPLE_COST`(第28章)というコストを持ちます。
そのため`optimize_join_order`は、DPが見つけた順序(並べ替え込み)と、並べ替えが要らない構文順の計画を実際にコストで比較し、それでも安いほうだけを採用します。
探索した順序が必ずしも構文順より安いとは限らない、という事実を無視しないための、最後の安全弁です。

## Physical Properties: 出力順序としてのInteresting Order

`ORDER BY`は、これまで常に`Sort`という演算子1個で満たしてきました(第21章)。
`amount`という列にB+Tree索引があり、`WHERE amount >= 100 AND amount <= 200`のような範囲述語からIndex Range Scanが選ばれたなら、その出力はすでに`amount`の昇順で並んでいます(`BTree::range`が昇順を返す、第23章)。
`ORDER BY amount`が続くなら、その`Sort`はもう仕事をしていません。

この「演算子の出力がすでに特定の列で並んでいる」という性質を**Physical Property**と呼びます。
原案(`docs-local/chatgpt_opinion.md`)は単一ノード版のPhysical Propertyを主に「出力順序」として扱うとしており、この章もそれに倣います。

```rust
pub(crate) fn output_ordering(plan: &PhysicalPlan) -> Option<usize> {
    match plan {
        PhysicalPlan::IndexScan(scan) => match &scan.kind {
            IndexScanKind::Range { .. } => scan.schema.index_of(&scan.column_name),
            IndexScanKind::Point(_) => None,
        },
        PhysicalPlan::Filter(filter) => output_ordering(&filter.input),
        PhysicalPlan::Sort(sort) => match sort.keys.as_slice() {
            [key] if !key.desc => match &key.expr {
                BoundExpr::ColumnRef { column_index, .. } => Some(*column_index),
                _ => None,
            },
            _ => None,
        },
        PhysicalPlan::NestedLoopJoin(join) => output_ordering(&join.left),
        PhysicalPlan::HashJoin(join) => output_ordering(&join.left),
        PhysicalPlan::IndexNestedLoopJoin(join) => output_ordering(&join.left),
        PhysicalPlan::Projection(projection) => {
            let input_order = output_ordering(&projection.input)?;
            projection.projection.iter().position(|item| {
                matches!(&item.expr, BoundExpr::ColumnRef { column_index, .. } if *column_index == input_order)
            })
        }
        PhysicalPlan::SeqScan(_)
        | PhysicalPlan::Values(_)
        | PhysicalPlan::Aggregate(_)
        | PhysicalPlan::Distinct(_)
        | PhysicalPlan::Limit(_)
        | PhysicalPlan::Insert(_)
        | PhysicalPlan::Update(_)
        | PhysicalPlan::Delete(_) => None,
    }
}
```

`Filter`は行を間引くだけで列の意味も相対順序も変えないので、子の順序をそのまま引き継ぎます。
3つのJoinはどれも同じ性質を持ちます。
`NestedLoopJoinExec`、`HashJoinExec`、`IndexNestedLoopJoinExec`(第22章、第25章)は、`left`の行を1件ずつ`next()`で引いた順序をそのまま外側ループに使い、`right`側だけを`Vec`やハッシュテーブルへ先に読み切ります。
`right`の順序は失われますが、`left`側の順序は、`left`が結合後スキーマの先頭側を占めることもあって、同じ列添字のまま保たれます。
Join方式を問わず「左側の順序は生き残る」というこの性質は、実装するまで気づきにくいものの、確実に成り立っています。

`Sort`を実際に省略するのは、`optimize`の`LogicalPlan::Sort`の分岐です。

```rust
LogicalPlan::Sort(sort) => {
    let input = optimize(*sort.input, storage, stats);
    if sort_is_already_satisfied(&sort.keys, &input) {
        input
    } else {
        PhysicalPlan::Sort(SortNode { input: Box::new(input), keys: sort.keys })
    }
}
```

`sort_is_already_satisfied`は、`Sort`が要求する並び順(Required Ordering)が単一列、昇順の場合に限って、`input`の`output_ordering`と突き合わせます。
一致すれば`Sort`そのものを積まず、`input`をそのまま返します。

原案が言う「Interesting Order」は、本来はもっと広い考え方です。
DPが部分集合ごとに1個の最良解しか覚えないのに対し、教科書的なInteresting Order DPは「コスト最小の1個」に加えて「順序を持つがコストはやや高い1個」も一緒に残し、その順序が後段の`Sort`を省く形で得になるかどうかまで比較します。
たとえば、この章のDPが単独では選ばない、コストがわずかに高いIndex Nested Loop Joinの計画が、`left`の順序を保つことで後段の`ORDER BY`の`Sort`を省ければ、全体としては安くなる場合があります。
この章のDPは、そこまでは踏み込みません。
部分集合ごとに覚えるのはコスト最小の1個だけのままにし、出力順序の活用は`optimize`の`LogicalPlan::Sort`の分岐(すでにDPが決め終えた計画に対する後付けのチェック)に絞ります。
DPの状態を(コスト, 順序)の組へ増やす実装の複雑さは、この教材の規模でJoin順序を1段階変えるだけで得られるコスト差(次節の実測)に比べると、割に合わないと判断したためです。

## この章の限界: Range Index Scanはまだこの最適化の主役になれない

第28章はすでに、Range Index Scanが実務でどれだけ弱いかを明らかにしていました。
Histogramのバケツ数は固定`10`個で、境界をまたぐ範囲述語の一致行数は最良でも「バケツ1個ぶん(全体の約10%)」の粒度でしか見積もれません。
この章のコスト定数(`SEQ_PAGE_COST = 1`、`RANDOM_PAGE_COST = 4`、`CPU_TUPLE_COST = 0.01`、`DEFAULT_ROWS_PER_PAGE = 50`)のもとでは、Range Index Scanが`SeqScan`+`Filter`より安くなるには、見積もり選択率がおよそ1%を下回る必要があります。
バケツ1個ぶんという10%の下限は、その基準の10倍粗いままです。

つまり、`sort_is_already_satisfied`が実際に`Sort`を省く場面(`cheapest`がRange Index Scanを実際に選んだ場面)は、この教材のコストモデルが現状のままである限り、ほとんど訪れません。
`output_ordering`と`sort_is_already_satisfied`という仕組みそのものは、`PhysicalPlan`を直接組み立てるテスト(`physical_plan`モジュールの`output_ordering_range_index_scan_returns_the_scanned_column`ほか)で検証済みですが、`ANALYZE`済みの実データから`EXPLAIN`だけでこの効果を再現するのは、この章の時点ではまだ難しいということです。
Bitmap Index Scan(第28章の演習課題)やより細かいHistogramが加われば、Range Index Scanが選ばれる場面自体が増え、この章のSort省略もそれに応じて働き始めます。

## 測って確認する: 構文順とDPが選ぶ順序のコスト比較

冒頭の`customers`、`orders`、`shipments`のクエリへ戻ります。

```console
minidb> EXPLAIN SELECT customers.name, orders.id FROM shipments JOIN orders ON shipments.country = orders.country JOIN customers ON orders.customer_id = customers.id;
QUERY PLAN
----------
Projection(customers.name, orders.id) rows=25 cost=181.85
  └─ Projection(country, id, customer_id, country, id, name) rows=25 cost=181.60
    └─ HashJoin(INNER JOIN, country = country) rows=25 cost=181.35
      └─ HashJoin(INNER JOIN, id = customer_id) rows=25 cost=81.10
        └─ SeqScan(customers) rows=5 cost=1.05
        └─ SeqScan(orders) rows=2000 cost=60.00
      └─ Projection(country) rows=2000 cost=80.00
        └─ SeqScan(shipments) rows=2000 cost=60.00
(8 rows)
```

`FROM`には`shipments`、`orders`、`customers`の順に書きましたが、実際に選ばれた計画は`customers`と`orders`を先に(選択的な`customer_id`の一致で)結合し、`shipments`を最後に回しています。
根の直下に`Projection(country, id, customer_id, country, id, name)`という、見慣れない列の並べ替えが1段挟まっているのは、「実装」の節で見た並べ替え用`Projection`です。
DPが選んだ並びは`FROM`の記述順とは異なるので、この計画はその代償(並べ替え1回ぶんのコスト)を払ってもなお構文順より安い、という比較を経て採用されています。

その比較を、`cost_model::plan_cost`で直接確かめます。
DPが選んだ計画のコストは`181.85`、`combine_in_syntactic_order`で`FROM`の記述順のまま組み立てた計画のコストは`201.10`です。
約10%、DPが見つけた順序のほうが安くなりました。
`shipments`と`orders`は同じ2,000行なので、両者が持つ差は「`customers`という5行だけのテーブルとの選択的な結合を先に済ませるか、低カーディナリティな`country`どうしの結合を先に済ませるか」という、たった1つの順序の違いだけです。

## 到達点

`join_order`モジュールは、3個以上のテーブルを結合する`INNER JOIN`の連鎖に対して、Left-deep限定のSubset DPでJoin順序を探索します。
`ON`条件が繋がる拡張を優先し、繋がらない組はどうしても必要なときだけCartesian Productとして許します。
探索した順序が実際に構文順より安いかどうかは、並べ替えのコストまで含めて最後にもう一度比較し、負けていれば構文順へフォールバックします。
`output_ordering`と`sort_is_already_satisfied`は、Index Range Scanの出力順序が`ORDER BY`をすでに満たしている場合に`Sort`を省く、Physical Propertyの最小限の活用です。

`FROM`にテーブルが3個以上並ぶかどうかで、`optimize`は今や2通りの経路を持ちます。
2個までは第22章、第25章、第28章の`choose_join_plan`がそのまま働き、3個以上ではこの章のDPが構文順を含む複数の候補を比較してから選びます。
第26章のルールベース最適化、第27章の統計、第28章のコストモデル、そしてこの章のJoin順序探索が揃ったことで、`optimize`はSQLに書かれた形をなぞるだけの変換から、統計とコストに基づいてアクセスパス、Join方式、Join順序を選べるオプティマイザになりました。

これで第4部が完成します。
minidbは、統計情報とコストモデルに基づいて、Scan方式やJoin順序を選べるオプティマイザになりました。

## 演習問題

### 必須課題

1. 本文の`customers`、`orders`、`shipments`の例は、`country`という1列だけを低カーディナリティにして中間結果を膨らませました。`orders`と`shipments`の間にもう1つ低カーディナリティな列(たとえば`warehouse`)を加え、`ON`条件を2本(`country`と`warehouse`の両方が一致)に増やしたとき、`connecting_condition`が2本の条件をANDでまとめて評価することを`EXPLAIN`の`HashJoin`の表示(`AND`で連結された条件)から確認してください。
2. `MAX_DP_TABLES`(8)を超えるテーブル数の`FROM`で、DPが構文順へフォールバックすることを`EXPLAIN`から確認してください。そのうえで、`MAX_DP_TABLES`を一時的に4程度まで下げ、5〜7テーブルの結合で選ばれる計画がどう変わるかを比較してください。
3. `join_order::tests::dp_and_syntactic_order_return_the_same_rows`は、DPが選んだ順序と構文順とで最終的な行集合が一致することを確認しています。この教材の3テーブル以外の`FROM`(4テーブル、Cartesian Productを含む`FROM`)についても同様の等価性テストを書き足し、探索する順序を変えても結果が変わらないことを確認してください。
4. この章のDPは、Left-deep木しか探索しません。`(A JOIN B) JOIN (C JOIN D)`のようなBushy木を候補に加えると、`{A, B}`と`{C, D}`という2つの独立した部分集合を`right`側に置く必要があります。Bushy木を許した場合にDPの状態がどう変わるか(単に部分集合ごとの最良解を覚えるだけでは足りない理由)を検討し、設計だけで構わないので変更案をまとめてください。

### 発展課題

1. この章のDPは部分集合ごとにコスト最小の1個しか覚えません。「Physical Properties」の節で触れたとおり、教科書的なInteresting Order DPは(コスト, 出力順序)の組ごとにPareto最適な複数の計画を覚えます。単一列、昇順の順序に限定してよいので、DPの状態を`HashMap<(u32, Option<usize>), DpEntry>`のように順序込みに拡張し、`ORDER BY`を伴うクエリで、単独では最安でない計画が全体としては選ばれる例を作ってください。
2. `optimize_join_order`は`ON`条件のうち、ちょうど2個のテーブルを参照する項だけをJoin Graphの辺にし、3個以上のテーブルを参照する項は`residual`として最後にまとめて`Filter`で適用します。3個以上のテーブルにまたがる`ON`条件(`a.x + b.y = c.z`のような式)を、DPの拡張ステップに組み込む(その式が参照する全テーブルが揃った時点で評価する)よう設計を変更し、Join Graphを「辺」ではなく「ハイパーエッジ」として扱う実装を検討してください。
3. 「この章の限界」で見たとおり、この章のコストモデルではRange Index Scanがほとんど選ばれず、Sort省略の効果を`EXPLAIN`で自然に再現するのが難しい状態です。第28章の発展課題であるBitmap Index Scanを実装したうえで、`ORDER BY`を伴う範囲述語のクエリで実際に`Sort`が省略される場面を`EXPLAIN`で再現してください。
