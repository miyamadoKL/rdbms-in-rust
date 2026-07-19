# 第25章 Index Scanとアクセスパス

```console
minidb> CREATE TABLE orders (id BIGINT NOT NULL, amount BIGINT);
CREATE TABLE
minidb> CREATE INDEX idx_id ON orders (id);
CREATE INDEX
minidb> INSERT INTO orders VALUES (1, 100), (2, 200), (3, 300);
INSERT 3
minidb> EXPLAIN SELECT amount FROM orders WHERE id = 2;
QUERY PLAN
----------
Projection(amount)
  └─ Filter(id = 2)
    └─ SeqScan(orders)
(3 rows)
```

`idx_id`という索引はもう存在します。
`id = 2`という条件も、索引がいちばん得意とする形の検索です。
それでも`EXPLAIN`が見せる計画は、`orders`の全行を舐めてから`id = 2`で選り分ける`SeqScan`のままです。

## 前章の限界

前章で`CREATE INDEX`が作る索引は、2つの仕事にしか使われていませんでした。
`INSERT`、`UPDATE`、`DELETE`のたびに自分自身を追従させるIndex Maintenanceと、`PRIMARY KEY`と`UNIQUE`列の重複を`O(log n)`で検出する一意性検査です。
どちらも、行を書き込む側からしか索引を見ていません。

`SELECT`の実行計画を組み立てる`physical_plan::optimize`(第19章)は、`Storage`が持つ索引の一覧を一度も参照しません。
`LogicalPlan::Scan`は問答無用で`PhysicalPlan::SeqScan`に変換され、`WHERE`はその後ろに積まれた`Filter`が全行に対して評価します。
索引を持つ列であっても、`SELECT`の側からは索引が存在しないのと同じです。

この非対称は設計判断の結果ではなく、前章がまだ手を付けていなかった領域です。
前章の本文は「`SELECT`の実行計画がインデックスを使うようにはしません」と明言し、この章の宿題として残していました。
索引はあるのに使われないという状態は、教材の途中経過としては正直ですが、実際のRDBMSとしては片手落ちです。

この章では、`WHERE`や`JOIN ... ON`に現れた条件を見て、使える索引があればそちらを選ぶという判断を`optimize`に持ち込みます。
索引を検索の入口として使う経路を**アクセスパス**と呼び、`SeqScan`もPoint Index ScanもRange Index Scanも、同じ「1つのテーブルから行を取り出す方法」というアクセスパスの選択肢として並びます。

## アクセスパスの選択: WHEREの連言から索引の効く述語を抽出する

`WHERE id = 2`という条件は、`orders`の`id`列に対する**等値述語**です。
`idx_id`が`id`列を昇順に並べたB+Treeである以上(第23章)、この条件は索引の根から葉までを1回たどるだけで答えが出ます。
全行を舐める`SeqScan`と、索引の`lookup`1回で済む**Point Index Scan**とでは、狙える行数が同じでも掛かる仕事の量がまったく違います。

`WHERE amount >= 100 AND amount <= 200`のような範囲条件も同様です。
`crate::btree::BTree::range`(第24章)がそのまま使えるので、下限を含む葉を1回探し当てれば、あとは`next_leaf`のリンクをたどるだけで上限までの行を拾えます。
これを**Range Index Scan**と呼びます。

`WHERE`にはPointともRangeとも判定できない条件が混じっていることも珍しくありません。
`id = 2 AND name = 'Bob'`という条件のうち索引で引けるのは`id = 2`だけで、`name = 'Bob'`は索引に無い列への条件です。
そこでこの章の`optimize`は、`WHERE`をANDの連言に分解し、索引で引ける述語だけを取り出してIndex Scanに渡し、残りは今までどおり`Filter`に残すという役割分担を採ります。

```rust
enum AccessPath {
    /// Point・Rangeいずれかの索引述語が見つかった。`remaining`は、抽出した
    /// 述語を取り除いた残りの連言(すべて索引に吸収された場合は`None`)。
    Index { node: IndexScanNode, remaining: Option<BoundExpr> },
    /// 索引の効く述語が無かった。`predicate`は元の`predicate`をそのまま返す
    /// (`flatten_conjuncts`は参照だけを取り出すため、何も見つからなかった
    /// 場合はここで初めて所有権を返す。フラット化してから再構築すると
    /// 元と等価だが同一ではないASTになり、`EXPLAIN`の表示がわずかに変わる
    /// 余地があるため、それを避けるために所有権をそのまま持ち回す)。
    SeqScan { predicate: BoundExpr },
}
```

`choose_access_path`は、`predicate`をANDで分解した連言(conjunct)を出現順に見ていき、Point述語を最優先で探します。
見つからなければ、列ごとに下限と上限の候補を集めながらRangeを探し、それも無ければ`SeqScan`を返します。

```rust
fn choose_access_path(storage: &Storage, scan: &logical_plan::ScanNode, predicate: BoundExpr) -> AccessPath {
    let mut conjuncts: Vec<&BoundExpr> = Vec::new();
    collect_conjuncts(&predicate, &mut conjuncts);

    // Point: 出現順で最初に見つかった、索引付き列への等値比較。
    let point = conjuncts.iter().enumerate().find_map(|(i, conjunct)| {
        let (column_index, op, value) = as_column_literal_comparison(conjunct)?;
        if op != BinaryOperator::Eq || value.is_null() {
            return None;
        }
        let info = storage.index_for_column(scan.table_id, column_index)?;
        Some((i, info.name.clone(), info.column_name.clone(), value))
    });

    if let Some((i, index_name, column_name, value)) = point {
        let remaining = rebuild_conjunction(exclude(&conjuncts, &[i]));
        let node = IndexScanNode {
            table_id: scan.table_id,
            table_name: scan.table_name.clone(),
            schema: scan.schema.clone(),
            index_name,
            column_name,
            kind: IndexScanKind::Point(value),
        };
        return AccessPath::Index { node, remaining };
    }

    // Range: 列ごとに下限・上限の候補を集め、出現順で最初に列挙された
    // (かつ索引がある)列を選ぶ。同じ列に複数の下限(または上限)があっても
    // 最初の1本だけを使う(残りは`Filter`に残る残差条件として働くので、
    // 正しさには影響しない。本文の解説を参照)。
    let mut range_order: Vec<usize> = Vec::new();
    let mut ranges: HashMap<usize, RangeAccum> = HashMap::new();
    for (i, conjunct) in conjuncts.iter().enumerate() {
        let Some((column_index, op, value)) = as_column_literal_comparison(conjunct) else { continue };
        if op == BinaryOperator::Eq || value.is_null() {
            continue;
        }
        let is_lower = matches!(op, BinaryOperator::Gt | BinaryOperator::GtEq);
        let bound = match op {
            BinaryOperator::Gt => Bound::Excluded(value),
            BinaryOperator::GtEq => Bound::Included(value),
            BinaryOperator::Lt => Bound::Excluded(value),
            BinaryOperator::LtEq => Bound::Included(value),
            _ => continue,
        };
        let accum = ranges.entry(column_index).or_insert_with(|| {
            range_order.push(column_index);
            RangeAccum::default()
        });
        if is_lower {
            accum.lower.get_or_insert((i, bound));
        } else {
            accum.upper.get_or_insert((i, bound));
        }
    }

    for column_index in range_order {
        let Some(info) = storage.index_for_column(scan.table_id, column_index) else { continue };
        let accum = ranges.remove(&column_index).expect("range_orderに積んだ列は必ずrangesに存在する");
        let mut used = Vec::new();
        let lower = match accum.lower {
            Some((i, bound)) => {
                used.push(i);
                bound
            }
            None => Bound::Unbounded,
        };
        let upper = match accum.upper {
            Some((i, bound)) => {
                used.push(i);
                bound
            }
            None => Bound::Unbounded,
        };
        let remaining = rebuild_conjunction(exclude(&conjuncts, &used));
        let node = IndexScanNode {
            table_id: scan.table_id,
            table_name: scan.table_name.clone(),
            schema: scan.schema.clone(),
            index_name: info.name.clone(),
            column_name: info.column_name.clone(),
            kind: IndexScanKind::Range { lower, upper },
        };
        return AccessPath::Index { node, remaining };
    }

    AccessPath::SeqScan { predicate }
}
```

`as_column_literal_comparison`は、`column OP literal`(あるいは`literal OP column`)という形の比較を、列の添字、演算子、リテラル値の組に変換します。
定数式ではない項(`a.x + 1 = 3`のような)や、比較の両辺がどちらも列参照の項(`a.x = a.y`のような)は`None`を返し、この関数の対象から外れます。

```rust
fn as_column_literal_comparison(expr: &BoundExpr) -> Option<(usize, BinaryOperator, Value)> {
    let BoundExpr::BinaryOp { op, lhs, rhs, .. } = strip_paren(expr) else { return None };
    let op = *op;
    if !matches!(
        op,
        BinaryOperator::Eq | BinaryOperator::Lt | BinaryOperator::LtEq | BinaryOperator::Gt | BinaryOperator::GtEq
    ) {
        return None;
    }
    match (column_index_of(lhs), column_index_of(rhs)) {
        (Some(column_index), None) => literal_value(rhs).map(|value| (column_index, op, value)),
        (None, Some(column_index)) => literal_value(lhs).map(|value| (column_index, flip_comparison(op), value)),
        _ => None,
    }
}
```

`100 <= amount`のように定数を左に置く書き方も、`flip_comparison`が向きを`amount >= 100`へ裏返すので同じ経路で処理できます。
下限(`>`または`>=`)と上限(`<`または`<=`)の候補は列ごとに集め、索引がある列のうち出現順で最初のものを選びます。
同じ列に`amount >= 100`と`amount >= 50`のような下限が2つあっても、使うのは出現順で最初の1本だけです。
2本目を索引の境界として使わなくても、正しさは崩れません。
使わなかった`amount >= 50`は`remaining`(残差条件)としてそのまま`Filter`に残り、`amount >= 100`をすでに満たす行に対しては常に`TRUE`になるので、結果には影響しないからです。
索引の境界としてどちらがより厳しい絞り込みになるかを比べて選ぶ最適化は、この章では行いません。

`col = NULL`という述語は、Point述語の候補から意図的に外してあります。
`value.is_null()`という1行がその番人です。
`crate::btree::BTree`は`NULL`をキーとして受け付けず(第23章)、`NULL`を持つ行はそもそも索引に登録されません(第24章のIndex BuildとIndex Maintenance)。
`col = NULL`を無理にPoint Index Scanへ回すと、存在しないキーへの`lookup`を試みることになるか、あるいは「索引にNULLが無いから0行」という誤った理由で0行を返すことになります。
どちらであっても、この条件が0行になるべき本当の理由(SQLの三値論理で`NULL = NULL`はUNKNOWNであり、`col = NULL`は`col`の値によらず常にUNKNOWN、第8章)とは無関係です。
`col = NULL`をFilterに残せば、`eval_bound_expr`と`predicate_matches`が三値論理どおりに0行だけを返し、索引はそもそも引かれません。

最後に`optimize`本体です。
`Filter`の直下が`Scan`で、かつ`storage`が索引を持てるディスクバックエンドであるときに限り、`choose_access_path`を試します。

```rust
        LogicalPlan::Filter(filter) => match (storage, *filter.input) {
            (Some(storage), LogicalPlan::Scan(scan)) => match choose_access_path(storage, &scan, filter.predicate) {
                AccessPath::Index { node, remaining: Some(predicate) } => {
                    PhysicalPlan::Filter(FilterNode { input: Box::new(PhysicalPlan::IndexScan(node)), predicate })
                }
                AccessPath::Index { node, remaining: None } => PhysicalPlan::IndexScan(node),
                AccessPath::SeqScan { predicate } => PhysicalPlan::Filter(FilterNode {
                    input: Box::new(PhysicalPlan::SeqScan(SeqScanNode {
                        table_id: scan.table_id,
                        table_name: scan.table_name,
                        schema: scan.schema,
                    })),
                    predicate,
                }),
            },
            (_, input) => {
                PhysicalPlan::Filter(FilterNode { input: Box::new(optimize(input, storage)), predicate: filter.predicate })
            }
        },
```

`storage`は`Database::execute_select`と`execute_explain`が`Backend::Disk`のときだけ`Some`を渡します。
`Database::memory`は`CREATE INDEX`自体を拒否しているので(第24章)、`Backend::Memory`では索引という選択肢が最初から存在しません。
`optimize`はメモリバックエンドに対しては`storage`を`None`のまま呼ばれ、`Scan`は常に`SeqScan`になります。
ディスクバックエンドと同じSQLを流しても実行アルゴリズムだけが違うという状態は、これまでの章でも繰り返し確認してきた設計です。

`Filter`の直下が`Scan`である場合だけをこの章の対象にしているのは、`WHERE`が`JOIN`をまたいで個々のテーブルへ**押し下げ**られる保証がまだ無いからです。
`FROM a JOIN b ON ...`の`WHERE`は、`Join`ノードの上に1つだけ乗っており、`a`や`b`単体の`Scan`の真上には来ません。
`WHERE`の中から「`a`だけに関係する部分」を見つけ出して`a`の`Scan`の直前まで運ぶ変換は**Predicate Pushdown**と呼ばれ、この章ではまだ実装しません。
第26章のルールベース最適化がこの変換を担うまで、`JOIN`を伴うクエリの`WHERE`は`SeqScan`と同じ全件走査から絞り込むしかありません。

## Executor: IndexScanExec

`choose_access_path`が選んだアクセスパスは、`IndexScanNode`という値として`PhysicalPlan`の木に積まれます。
これを実際に実行するのが`IndexScanExec`で、`source`フィールドがPointかRangeかを`IndexScanSource`という2択の`enum`で持ちます。

```rust
enum IndexScanSource<'a> {
    Point(std::vec::IntoIter<RecordId>),
    Range(RangeScan<'a>),
}
```

Pointは`BTree::lookup`が返す`RecordId`の一覧を、Rangeは`BTree::range`が返す`RangeScan`(第24章、Leaf間リンクをたどるイテレータ)をそのまま持ち回すだけです。
索引はディスクバックエンドにしか存在しない(第24章)ため、`MemSeqScanExec`に対応する索引版はこのクレートにはなく、`IndexScanExec`は常に`Storage`への参照を持ちます。

```rust
pub struct IndexScanExec<'a> {
    storage: &'a Storage,
    table_id: TableId,
    schema: &'a Schema,
    source: IndexScanSource<'a>,
}

impl<'a> IndexScanExec<'a> {
    pub fn new(storage: &'a Storage, table_id: TableId, schema: &'a Schema, index_name: &str, kind: &IndexScanKind) -> DbResult<Self> {
        let btree = storage
            .index_btree(index_name)
            .unwrap_or_else(|| unreachable!("optimizeが選んだ索引'{index_name}'はStorageに必ず存在する"));
        let source = match kind {
            IndexScanKind::Point(value) => IndexScanSource::Point(btree.lookup(value)?.into_iter()),
            IndexScanKind::Range { lower, upper } => IndexScanSource::Range(btree.range(lower.as_ref(), upper.as_ref())?),
        };
        Ok(IndexScanExec { storage, table_id, schema, source })
    }
}
```

どちらも索引が返すのは`RecordId`であって行の中身ではないため、`next()`は`RecordId`を1件受け取るたびに`Storage::get`でHeapページから実データを`fetch`し、`decode_tuple`で`Tuple`へ復元します。

```rust
    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            let rid = match &mut self.source {
                IndexScanSource::Point(iter) => match iter.next() {
                    Some(rid) => rid,
                    None => return Ok(None),
                },
                IndexScanSource::Range(iter) => match iter.next() {
                    Some(Ok((_, rid))) => rid,
                    Some(Err(err)) => return Err(err),
                    None => return Ok(None),
                },
            };
            if let Some(bytes) = self.storage.get(self.table_id, rid)? {
                return decode_tuple(self.schema, &bytes).map(Some);
            }
            // Lazy Delete済み(索引にエントリが残っているのにHeapから
            // すでに消えている)場合はここに来る。モジュールドキュメントの
            // とおり読み飛ばして次の`RecordId`へ進む。
        }
    }
```

`Storage::get`が`None`(該当スロットが空)を返す枝が気になった読者もいるはずです。
`DELETE`は`Storage::index_delete_row`(第24章)で索引側のエントリも同時に取り除くため、正しく運用している限り、索引が指す`RecordId`のHeap行が消えているという状況は起こりません。
これは`BTree::delete`(第24章)が、削除対象の`(key, rid)`が同じキーを持つ複数のLeaf Pageのどこにあっても正しく見つけて取り除くことに支えられています。
`find_leaf`(一致の最後の葉に着地する探索、第23章)から探索を始める実装だった間は、対象の`RecordId`がそれより左の葉にある場合に取りこぼしがあり、`DELETE`が成功したように見えても索引側にエントリが残るという、この段落の前提そのものが崩れる状況がありました(詳しくは第24章の`delete`を参照)。
それでも`IndexScanExec`はこの枝を`panic`にはせず、黙って次の`RecordId`へ進みます。
索引とHeapの整合性を保つのは`Storage`の責務であり、`IndexScanExec`はその結果を信じたうえで、万一の食い違いを取りこぼしではなく無害な読み飛ばしとして吸収する側に倒しています。
前章の`BTree::delete`が占有率の低下を受け入れてでもLazy Deleteを選んだのと同じ姿勢が、ここにも表れています。

`EXPLAIN`は、選ばれたアクセスパスと、索引に吸収された述語、`Filter`に残った述語の両方をそのまま見せます。

```console
minidb> EXPLAIN SELECT id, amount, name FROM orders WHERE id = 1;
QUERY PLAN
----------
Projection(id, amount, name)
  └─ IndexScan(idx_id, id = 1)
(2 rows)

minidb> EXPLAIN SELECT id, amount, name FROM orders WHERE id = 1 AND name = 'Alice';
QUERY PLAN
----------
Projection(id, amount, name)
  └─ Filter(name = 'Alice')
    └─ IndexScan(idx_id, id = 1)
(3 rows)

minidb> EXPLAIN SELECT id, amount, name FROM orders WHERE amount >= 100 AND amount <= 200;
QUERY PLAN
----------
Projection(id, amount, name)
  └─ IndexScan(idx_amount, amount >= 100 AND amount <= 200)
(2 rows)
```

`id = 1 AND name = 'Alice'`では`id = 1`だけが`IndexScan`のラベルに現れ、`name = 'Alice'`は`Filter`として1段上に残っています。
索引がどの述語を引き受け、何が生き残ったのかが、計画を読むだけで分かります。

## Index Nested Loop Join

索引が効くのは`WHERE`だけではありません。
`customers JOIN orders ON customers.id = orders.customer_id`のような等値結合でも、`orders.customer_id`に索引があれば、`orders`の全行を読まずに済みます。
`customers`の行1件ごとに`orders.customer_id`の索引を`lookup`して一致する行だけを取り出す結合方式を**Index Nested Loop Join**と呼びます。

前章までの`optimize`は、等値結合を見つけると常に`HashJoin`を選んでいました(第22章)。
`HashJoin`は`right`(内側テーブル)の全行を読み切ってハッシュテーブルへ積む**Build**を必ず1回行います。
`right`に使える索引があるなら、この全件読み込みを丸ごと避け、`left`(外側テーブル)の行数ぶんだけ索引を`lookup`する方が少ない仕事で済むはずです。
この章では、等値結合の鍵がちょうど1本で、かつ内側テーブルの結合列に索引があるときに限り、`HashJoin`より`IndexNestedLoopJoin`を優先します。

```rust
fn index_scan_target(
    storage: Option<&Storage>,
    right: &PhysicalPlan,
    keys: &[(BoundExpr, BoundExpr)],
) -> Option<IndexJoinTarget> {
    let storage = storage?;
    let [(left_key, right_key)] = keys else { return None };
    let right_column_index = column_index_of(right_key)?;
    let PhysicalPlan::SeqScan(scan) = right else { return None };
    let info = storage.index_for_column(scan.table_id, right_column_index)?;
    Some(IndexJoinTarget {
        outer_key: left_key.clone(),
        table_id: scan.table_id,
        table_name: scan.table_name.clone(),
        schema: scan.schema.clone(),
        index_name: info.name.clone(),
        column_name: info.column_name.clone(),
    })
}
```

`keys.len() == 1`という条件を課しているのは、`crate::btree::BTree`の索引キーが単一列に限られているからです(第24章)。
`a.x = b.x AND a.y = b.y`のように鍵が2本以上ある結合を、1本のB+Tree索引だけで引く手段はこの章にはありません。
`right`が`PhysicalPlan::SeqScan`のままであることも条件にしています。
`JOIN`の右辺には`WHERE`が押し下げられない(前節と同じ理由)ため、この章では`right`が`Filter`を伴うことはなく、この条件は常に満たされます。

`optimize`の`Join`アームは、等値結合の鍵を取り出せた場合、まず`index_scan_target`を試し、それが失敗したときだけ`HashJoin`を組み立てます。

```rust
                    match index_scan_target(storage, &right, &keys) {
                        Some(target) => PhysicalPlan::IndexNestedLoopJoin(IndexNestedLoopJoinNode {
                            left: Box::new(left),
                            kind: join.kind,
                            condition: join.condition,
                            outer_key: target.outer_key,
                            table_id: target.table_id,
                            table_name: target.table_name,
                            schema: target.schema,
                            index_name: target.index_name,
                            column_name: target.column_name,
                        }),
                        None => PhysicalPlan::HashJoin(HashJoinNode {
                            left: Box::new(left),
                            right: Box::new(right),
                            kind: join.kind,
                            keys,
                            condition: join.condition,
                        }),
                    }
```

`IndexNestedLoopJoinNode`が`right`を独立した`PhysicalPlan`として持たない点が、`HashJoinNode`と`NestedLoopJoinNode`との違いです。
内側テーブルの行は、外側の1行が来るたびに`outer_key`を評価し、その値で索引を`lookup`して初めて決まります。
`HashJoinExec`のBuildのように内側の全行を先読みして`Vec`やハッシュテーブルへ積む段階そのものがありません。

```rust
    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            if let Some(rid) = self.matches.next() {
                let left_tuple = self.current_left.as_ref().expect("直前にSomeを設定済み");
                if let Some(bytes) = self.storage.get(self.table_id, rid)? {
                    let right_tuple = decode_tuple(&self.right_schema, &bytes)?;
                    return concat_tuple(&self.schema, left_tuple, &right_tuple).map(Some);
                }
                // Lazy Delete済み(IndexScanExecのドキュメントを参照)。
                // この`rid`は読み飛ばし、`matches`の続きへ進む。
                continue;
            }

            let Some(tuple) = self.left.next()? else {
                return Ok(None);
            };
            let row = Row::new(&self.left_schema, &tuple);
            let key = eval_bound_expr(self.outer_key, self.functions, Some(&row))?;
            self.current_left = Some(tuple);
            self.matches = if key.is_null() {
                Vec::new().into_iter() // NULLキーは結合しない
            } else {
                let btree = self
                    .storage
                    .index_btree(self.index_name)
                    .unwrap_or_else(|| unreachable!("optimizeが選んだ索引'{}'はStorageに必ず存在する", self.index_name));
                btree.lookup(&key)?.into_iter()
            };
        }
    }
}
```

`key.is_null()`の分岐は`HashJoinExec`のNULLキー除外(第22章)と同じ理由です。
SQLの等価比較は`NULL = NULL`をUNKNOWNとみなすため、外側の行の結合キーが`NULL`なら、索引を引くまでもなく一致は0件になります。

`EXPLAIN`では、`IndexNestedLoopJoin`は`left`だけを木の子として持ちますが、内側テーブルへの索引アクセスも1行として書き加えます。

```console
minidb> EXPLAIN SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id;
QUERY PLAN
----------
Projection(customers.name, orders.item)
  └─ IndexNestedLoopJoin(INNER JOIN, id = customer_id)
    └─ SeqScan(customers)
    └─ IndexScan(idx_customer_id, customer_id = id)
(4 rows)
```

`SeqScan(customers)`と`IndexScan(idx_customer_id, customer_id = id)`は、どちらも`IndexNestedLoopJoin`の直下に並んで表示されますが、実行時の立場は対称ではありません。
`SeqScan(customers)`は`Executor`の木を根から葉までたどれば実際にそこにある子ですが、`IndexScan(idx_customer_id, ...)`は`IndexNestedLoopJoinExec::next()`の中で`customers`の1行ごとに動的に起こる索引アクセスを、計画を読む側にも見えるようにするための表示専用の1行です。

## 正しさの検証

アクセスパスが変わっても、`SELECT`が返す行は変わってはいけません。
これを確かめる一番直接的な方法は、同じデータと同じクエリを索引あり(Index Scan)と索引無し(SeqScan)の両方の`Storage`に対して実行し、行集合を突き合わせることです。

```rust
        for query in [
            "SELECT id, amount, name FROM orders WHERE id = 42",
            "SELECT id, amount, name FROM orders WHERE amount >= 100 AND amount <= 200",
            "SELECT id, amount, name FROM orders WHERE amount > 590",
            "SELECT id, amount, name FROM orders WHERE id = 999", // 一致なし
        ] {
            let with_index_plan = with_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string();
            assert!(with_index_plan.contains("IndexScan"), "索引ありDBはIndexScanを選ぶはず: {with_index_plan}");
            let without_index_plan = without_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string();
            assert!(without_index_plan.contains("SeqScan"), "索引無しDBはSeqScanのまま: {without_index_plan}");

            let mut with_index_rows: Vec<Vec<Value>> =
                with_index.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
            let mut without_index_rows: Vec<Vec<Value>> =
                without_index.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
            with_index_rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            without_index_rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            assert_eq!(with_index_rows, without_index_rows, "クエリ`{query}`の結果が索引の有無で食い違った");
        }
```

一致なし(`id = 999`)を含めているのは、境界値としての意味があります。
索引が空を返す0行という結果は、`RangeScan`が1件も返さない場合と`Vec::new()`のまま`Point`を終える場合の両方で正しく`Ok(None)`へたどり着く必要があり、どちらの経路が壊れても検出できるからです。

NULLの扱いは、Point述語の抽出そのものを避けるという設計であることをすでに見ました。
テストでは、`id = NULL`が`SeqScan`のまま(`IndexScan`を1度も選ばない)ことと、結果が0行になることの両方を確認します。

```rust
        let plan = db.execute("EXPLAIN SELECT id FROM orders WHERE id = NULL").unwrap().to_string();
        assert!(!plan.contains("IndexScan"), "col = NULLは索引を引いてはならない: {plan}");
        assert!(plan.contains("SeqScan"));

        let result = db.execute("SELECT id FROM orders WHERE id = NULL").unwrap();
        assert!(result.rows().is_empty());
```

削除済みエントリの除外は、`DELETE`の直後にそのキーへ`IndexScan`を実行して確かめます。

```rust
        assert_eq!(db.execute("DELETE FROM orders WHERE id = 2").unwrap().to_string(), "DELETE 1");

        // 削除された`id = 2`はIndexScanでも0行(索引エントリ自体が
        // Index Maintenanceで取り除かれている、第24章)。
        let plan = db.execute("EXPLAIN SELECT id FROM orders WHERE id = 2").unwrap().to_string();
        assert!(plan.contains("IndexScan(idx_id, id = 2)"));
        assert!(db.execute("SELECT id FROM orders WHERE id = 2").unwrap().rows().is_empty());
```

`DELETE`は`Storage::index_delete_row`(第24章)で索引エントリ自体を取り除くため、`id = 2`はもう索引の中に存在しません。
`IndexScanExec`が読み飛ばすのは、モジュールドキュメントで説明したとおりHeap側にだけ食い違いが生じた万一の場合であり、この削除のような通常の運用では`BTree::lookup`の時点ですでに0件です。
`id`列は`PRIMARY KEY`なので今回のキーは1つの葉に収まっていますが、`UNIQUE`ではない列を索引化した場合、同じキーを持つエントリがLeaf Splitで複数の葉にまたがっていても、`BTree::delete`が一致の最初の葉から`next_leaf`をたどって対象の`RecordId`を探し当てるため(第24章)、この「削除したキーはもう索引に無い」という前提は崩れません。

`UPDATE`が索引キーを書き換えた場合は、更新前の値では見つからず、更新後の値では見つかるという2つの向きを両方確かめます。

```rust
        assert_eq!(db.execute("UPDATE orders SET id = 42 WHERE id = 1").unwrap().to_string(), "UPDATE 1");

        // 更新前の値はもう見つからず、更新後の値でIndexScanが引ける
        // (Index Maintenanceが古いエントリを削除し、新しいエントリを
        // 挿入している、第24章)。
        assert!(db.execute("SELECT name FROM orders WHERE id = 1").unwrap().rows().is_empty());
        let updated = db.execute("SELECT name FROM orders WHERE id = 42").unwrap();
        assert_eq!(updated.rows().len(), 1);
        assert_eq!(updated.rows()[0].values(), &[Value::Text("Alice".to_string())]);
```

`storage_update`(第24章)が「削除してから挿入し直す」という手順を常に踏むおかげで、索引側のエントリは`RecordId`が変わっていてもずれません。
この章のIndex Scanは、その保証の上に成り立っています。

Index Nested Loop Joinの正しさは、同じ結合を索引あり(Index Nested Loop Join)と索引無し(Hash Join)の両方で実行して突き合わせます。

```rust
        let with_index_rows: Vec<Vec<Value>> =
            with_index.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
        let without_index_rows: Vec<Vec<Value>> =
            without_index.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
        assert_eq!(with_index_rows, without_index_rows);
```

`customer_id`が`NULL`の注文行が結果に現れないことも、第22章の`HashJoinExec`と同じ形で確認しています。

## 測って確認する

第24章は、索引経由の一意性検査が`INSERT`をO(n)からO(log n)へ変えたことを実測しました。
この章では、`SELECT`の側で同じ変化が起きていることを測ります。

```console
$ cargo test --release --lib -- --ignored --nocapture point_index_scan_grows
n=  1000  IndexScan=   55.31µs  SeqScan+Filter= 961.181µs
n=  2000  IndexScan=  102.36µs  SeqScan+Filter=2.625122ms
n=  4000  IndexScan=   57.06µs  SeqScan+Filter=4.121232ms
n=  8000  IndexScan=   62.52µs  SeqScan+Filter=10.323445ms
n= 16000  IndexScan=  182.95µs  SeqScan+Filter=20.55074ms
```

`n`を16倍にしても`IndexScan`は数十〜数百マイクロ秒の範囲にとどまり、`SeqScan+Filter`はほぼ`n`に比例して伸びています。
`amount`列(索引が無い)への同じ形の点検索と、`id`列(索引がある)への点検索を同じテーブル、同じ行数で比べているので、差の理由はアクセスパスの選択だけです。

Index Nested Loop Joinの実測は、単純な話では終わりません。
`customers`(50行)と`orders`(`m`行)を等値結合し、`orders.customer_id`に索引がある場合(Index Nested Loop Join)と無い場合(Hash Join)を比べます。
`customer_id`の分布を変えて2つの場面を作りました。
一致する行がごく一部にとどまる**選択的な結合**と、`orders`のほぼ全行がどれかの`customers`と一致する**密な結合**です。

```console
$ cargo test --release --lib -- --ignored --nocapture index_nested_loop_join_is_not_always_faster
selective m=  2000  matches=     2  IndexNestedLoopJoin=1.484602ms  HashJoin=2.422873ms
selective m=  8000  matches=     9  IndexNestedLoopJoin=3.152134ms  HashJoin=13.743028ms
selective m= 32000  matches=    33  IndexNestedLoopJoin=5.116327ms  HashJoin=55.265092ms
dense     m=  2000  matches=  2000  IndexNestedLoopJoin=37.489079ms  HashJoin=2.607703ms
dense     m=  8000  matches=  8000  IndexNestedLoopJoin=217.385484ms  HashJoin=12.958357ms
dense     m= 32000  matches= 32000  IndexNestedLoopJoin=786.240948ms  HashJoin=48.669114ms
```

選択的な結合では、Index Nested Loop Joinが`m`の増加にほとんど影響を受けずに済んでいます。
`orders`の`customer_id`がどれだけ`customers`の範囲から外れて散らばっていても、`customers`側の50行ぶんの`lookup`しか行わないからです。
Hash Joinは一致するかどうかによらず`orders`の全`m`行をBuildで読むため、`m`が増えるほど遅くなります。

密な結合では、この関係が逆転します。
Index Nested Loop Joinは一致した行1件ごとに`Storage::get`でHeapページを個別に読みに行きますが、`orders`のほぼ全行が一致するこの場面では、その個別アクセスが`m`回近く積み重なります。
`crate::buffer_pool::BufferPool`の容量は固定(第14章)なので、ランダムな順序で大量のページを読みに行くと、キャッシュに載り切らずに置き換えが増えます。
Hash JoinのBuildは`Storage::scan`で`orders`を先頭から順に1回読むだけなので、この置き換えの増加を受けません。
`m = 32000`では、密な結合のIndex Nested Loop JoinはHash Joinの15倍以上遅くなっています。

「索引があるかどうか」という構文的な性質だけでは、この2つの場面を区別できません。
どちらが実際に速いかはデータの分布(どれだけの行が一致するか)に左右されており、それを知るには行数や値の分布についての統計情報と、それを使ってコストを見積もる仕組みが要ります。
第28章のコストベース最適化が、その仕組みを導入します。

## 第3部の到達点

`Filter`が索引を素通りしていたこの章の冒頭から、`WHERE`と`JOIN ... ON`はどちらも使える索引があればそれを選ぶところまで進みました。
`EXPLAIN`は、Point Index ScanとRange Index Scanがどの述語を吸収し、何が`Filter`に残ったかをそのまま見せ、Index Nested Loop Joinは内側テーブルへの索引アクセスを合わせて表示します。
`optimize`が選ぶルールはまだ構文的な性質だけを見る単純なものですが、前節の実測が示すとおり、その単純さには実際の代償があります。

複数テーブルのJoin、集約、インデックス検索が、SQLから実行できるようになりました。
第17章のBinderから積み上げてきたLogical Plan、Physical Plan、Volcano Executorという層は、この章でSeq/Index Scan、Nested Loop/Hash/Index Nested Loop Joinという複数の実行アルゴリズムを持つ土台になっています。

第4部では、この`optimize`が構文だけを見て選んでいた判断を、統計情報とコストモデルに置き換えていきます。
Constant FoldingやPredicate Pushdownといった書き換え(第26章)、行数やNULL数などの統計情報とCardinality Estimation(第27章)を経て、第28章でこの章が実測で示した限界そのものに答えを出します。

## 演習問題

### 必須課題

1. `choose_access_path`のRange探索は、同じ列に複数の下限(または上限)があっても出現順で最初の1本しか索引の境界に使いません。`WHERE amount >= 50 AND amount >= 100`のように、後から出てくる方が実は厳しい境界であるケースをテストで再現し、正しさ(結果が変わらないこと)を確認したうえで、より厳しい境界を選ぶように`choose_access_path`を改良してください。`Value`同士の大小比較には`crate::types::compare_values`が使えます。
2. この章のIndex Nested Loop Joinは、等値結合の鍵がちょうど1本のときだけ選ばれます。`a.x = b.x AND a.y = b.y`のように鍵が2本ある結合で、`b.x`にだけ索引がある場合を考えてください。`b.x`の索引で候補行を絞り込み、`b.y = a.y`は`IndexNestedLoopJoinExec::next()`の中で追加のFilterとして評価する設計を検討し、実装してください。`IndexNestedLoopJoinNode`にどんなフィールドを追加する必要があるかも設計に含めてください。
3. `choose_access_path`は`Filter`の直下が`Scan`である場合だけを対象にしており、`JOIN`を伴うクエリの`WHERE`はどのテーブルの`Scan`にも索引として届きません。`FROM a JOIN b ON a.id = b.id WHERE a.status = 'active'`のようなクエリで、`EXPLAIN`が`IndexScan`を選ばないことを確認したうえで、`a.status = 'active'`のような「`a`だけを参照する項」を`WHERE`から見つけ出し、`a`の`Scan`の直前まで運ぶ最小限のPredicate Pushdownを実装してください(第26章の先取りです)。
4. `IndexScanExec`と`IndexNestedLoopJoinExec`は、`Storage::get`が`None`を返した`RecordId`を無条件に読み飛ばします。この章の運用ではLazy Deleteが直接の原因にはなり得ないことを本文で確認しましたが、`BufferPool::stats()`(第14章)を使って、削除と再挿入を繰り返したテーブルに対する`IndexScan`のヒット率が、削除を挟まない場合と比べてどう変わるかを実測してください。

### 発展課題

1. Index Nested Loop Joinの実測(本文)は、密な結合でHash Joinより15倍以上遅くなるという結果でした。この差の主な原因は`Storage::get`のランダムアクセスが`BufferPool`の置き換えを増やすことだと本文は推測しています。`BufferPool::stats()`(第14章)でヒット率を実際に取得し、密な結合と選択的な結合とでヒット率がどれだけ違うかを比較して、この推測を検証してください。
2. `IndexNestedLoopJoinExec`は、外側の行1件ごとに索引への`lookup`を1回行います。外側の行が同じ結合キーを複数持つ場合(`a.id`に重複がある場合)、同じキーへの`lookup`が繰り返されます。外側をあらかじめ結合キーでソートし、直前と同じキーなら前回の`lookup`結果を使い回す最適化を設計し、どのような入力分布でこの最適化が効くかを考察してください。
3. この章の`optimize`は、`WHERE`と`ON`のどちらにも使える索引が複数ある場合、`Storage::index_for_column`が索引名の辞書順で最小のものを選びます。この規則を、索引ごとの推定選択性(条件に一致する行の割合)にもとづいて選ぶ規則に変更するとしたら、どんな統計情報が必要になるか設計してください(統計情報の実装自体は第27章の範囲です)。
