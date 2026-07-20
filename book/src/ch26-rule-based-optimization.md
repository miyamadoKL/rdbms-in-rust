# 第26章 ルールベース最適化

```console
minidb> CREATE TABLE orders (id BIGINT NOT NULL, status BIGINT NOT NULL, amount BIGINT NOT NULL);
CREATE TABLE
minidb> EXPLAIN SELECT amount FROM orders WHERE 1 = 1 AND status = 1 AND amount > 0;
QUERY PLAN
----------
Projection(amount)
  └─ Filter(1 = 1 AND status = 1 AND amount > 0)
    └─ SeqScan(orders)
(3 rows)

minidb> EXPLAIN SELECT amount FROM orders WHERE status = 1 AND amount > 0;
QUERY PLAN
----------
Projection(amount)
  └─ Filter(status = 1 AND amount > 0)
    └─ SeqScan(orders)
(3 rows)
```

この2つの`WHERE`は同じ行を返します。
`1 = 1`は常に真なので、`status = 1 AND amount > 0`という条件を変えません。
それでも`EXPLAIN`が見せる計画は、律儀に`1 = 1`という比較を式の中に残したままです。
`orders`の行数が増えるほど、この無駄な比較を評価する回数も増えていきます。

`WHERE`が複数のテーブルにまたがるとき、無駄はもっと大きくなります。

```console
minidb> CREATE TABLE customers (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> EXPLAIN SELECT name FROM customers JOIN orders ON customers.id = orders.id \
  WHERE customers.id = 1;
QUERY PLAN
----------
Projection(name)
  └─ Filter(id = 1)
    └─ HashJoin(INNER JOIN, id = id)
      └─ SeqScan(customers)
      └─ SeqScan(orders)
(5 rows)
```

`customers.id = 1`という条件は`customers`だけを見れば決まるのに、`Filter`は`Join`の真上に居座っています。
`Join`は`customers`と`orders`をまず全件結合してから、その結果を`Filter`が絞り込みます。
`customers`の`id = 1`という行が1件しかなくても、結合そのものは`orders`の全行に対して行われます。

## 前章の限界: 「何を計算するか」は変えずに「どう並べるか」だけを変える

第25章までの`physical_plan::optimize`は、`LogicalPlan`が渡ってきた形をそのまま信じて実行アルゴリズムを選んでいました。
`WHERE`にどんな冗長な項が混じっていても、`Filter`が`Join`のどちら側に置かれていても、`optimize`はそれを疑わずに`PhysicalPlan`へ変換します。
第18章から積み上げてきたLogical Plan、Physical Plan、Volcano Executorという層は、SQLの意味を演算子の木として表現し、その木を実行アルゴリズムへ変換する土台でした。
その土台は、木の**形**そのものが冗長かどうかまでは面倒を見てくれません。

`WHERE 1 = 1 AND status = 1`と`WHERE status = 1`は、同じ`BoundStatement`にはなりません。
`Binder`(第17章)は書かれたとおりの式を型検査するだけで、「意味が同じだから書き方を揃える」という仕事は担っていないからです。
`customers.id = 1`が`customers`だけを見れば決まる条件だと気づくのも、`Binder`の役目ではありません。
`Binder`は`customers.id`という列参照がどのテーブルの何列目かを解決するところまでで、その列参照が`Filter`の中でどこに置かれるべきかは関与しません。

この章が持ち込むのは、`LogicalPlan`を`LogicalPlan`へ書き換える処理です。
`SELECT`が組み立てた木を、実行アルゴリズムを選ぶ前に、**同じ行を返す、より無駄の少ない形**へ整えます。
`1 = 1`のような冗長な項を消し、`customers.id = 1`のような片側だけの条件を該当するテーブルの直前まで運ぶのがこの章の仕事です。

## 意味を変えない書き換え、という最適化の本質

最適化と聞くと「速くする工夫」を思い浮かべますが、この章が扱うルールが満たすべき条件はもっと限定的です。
**書き換えの前後で、同じデータに対して同じ行を返すこと**。
これだけです。

この条件は当たり前に見えて、律儀に守ろうとすると簡単ではありません。
`WHERE status = 1 AND amount > 0`の`AND`の両辺を入れ替えても結果は変わりませんが、`WHERE amount > 0 OR status IS NULL`の`OR`を勝手に`AND`へ読み替えれば結果は変わってしまいます。
`customers.id = 1`を`orders`の直前まで運んでしまえば、そもそも`orders`だけを見て判定できない条件を評価することになり、結果はおろか計画自体が壊れます。
「意味を変えない」と一言で言っても、どの書き換えがその条件を満たすかは、書き換えごとに証明が要ります。

とりわけ注意が要るのが**三値論理**です。
第8章で見たとおり、SQLの`WHERE`は`TRUE`、`FALSE`、`UNKNOWN`(`NULL`)の3つの値を持つ論理で評価され、`UNKNOWN`になった行は`TRUE`になった行と同じようには残りません。
`col AND TRUE`を`col`へ簡約してよいかどうかは、`col`が`UNKNOWN`のときに両者が同じ値になるかどうかを確かめて初めて言えることです。
確かめずに「見た目が似ているから」で押し通す書き換えは、`NULL`を含む行でこっそり結果を変えてしまいます。

## `Rule`トレイトと固定点まで反復するドライバ

書き換えの種類は複数あり、この先の章でも増えていく見込みです(第27章以降、統計情報を使う書き換えが加わります)。
この章では`src/rules.rs`をモジュールとして新規作成し、個々の書き換えを`Rule`という1つのインターフェースの実装として登録できるようにします。

```rust
pub trait Rule {
    /// `EXPLAIN`のデバッグ表示やログでルールを識別するための名前。
    fn name(&self) -> &str;
    // ...(applyのドキュメントコメントは省略)
    fn apply(&self, plan: LogicalPlan, functions: &FunctionRegistry) -> (LogicalPlan, bool);
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod rules;
```

`apply`は`plan`を受け取り、書き換え後の`plan`と「実際に何か変えたか」を表す`bool`を返します。
戻り値を新しい`plan`にせず`bool`も添えているのは、`src/rules.rs`に定義するドライバが「もうこれ以上変化が起きない」ことを判定できるようにするためです。

```rust
pub fn optimize(mut plan: LogicalPlan, functions: &FunctionRegistry) -> LogicalPlan {
    let rules = default_rules();
    for _ in 0..MAX_ITERATIONS {
        let mut changed = false;
        for rule in &rules {
            let (next, rule_changed) = rule.apply(plan, functions);
            plan = next;
            changed |= rule_changed;
        }
        if !changed {
            break;
        }
    }
    plan
}
```

登録された全ルールを順に1回ずつ適用し、どのルールも`changed`を返さなくなったところで止めます。
この**固定点**まで反復する設計が必要になる理由は、後で具体例を通して確かめます。
反復回数には`MAX_ITERATIONS`という上限を設けてあります。
この章のルールはどれも式や木を単調に小さくする性質を持ち、通常はクエリの構文要素数のオーダーで収束するため、正常な実行でこの上限に達することはありません。
上限は、将来ルールを追加した際に互いを無限に行き来させてしまうバグへの安全弁として置いてあります。

`src/database.rs`の`Database::execute_select`と`execute_explain`は、`logical_plan::build_select`が返した木を`physical_plan::optimize`へ渡す前に、この`rules::optimize`を通します。

```rust
fn execute_select(&self, plan: LogicalPlan) -> DbResult<QueryResult> {
    let plan = rules::optimize(plan, &self.functions);
    let physical = physical_plan::optimize(plan, self.index_storage());
    // ...
}
```

`EXPLAIN`が見せる計画は実際に実行される計画そのものでなければならないので、`execute_explain`も同じ`rules::optimize`を経由します。
`INSERT`、`UPDATE`、`DELETE`はこの段階を経由しません。
対象の行を減らす書き換え(この章のルールはどれも`SELECT`の木にしか意味を持たない`Filter`、`Join`、`Aggregate`、`Projection`を対象にしています)がまだ無いので、通しても何も起きないからです。

## Constant Folding: 実行前に評価できる部分式

`1 = 1`や`ABS(-1)`のように、列を1つも参照しない部分式は、行ごとに評価し直す必要がありません。
`Constant Folding`は、こうした部分式を実行前に一度だけ評価し、結果のリテラルへ置き換えます。

畳み込んでよい対象は、列参照(`ColumnRef`)も集約(`Aggregate`)も含まない式に限ります。
`src/rules.rs`に次の`is_constant`を定義します。

```rust
fn is_constant(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. } => true,
        BoundExpr::ColumnRef { .. } | BoundExpr::Aggregate { .. } => false,
        BoundExpr::UnaryOp { expr, .. } | BoundExpr::Paren { expr, .. } | BoundExpr::Cast { expr, .. } => {
            is_constant(expr)
        }
        BoundExpr::BinaryOp { lhs, rhs, .. } => is_constant(lhs) && is_constant(rhs),
        BoundExpr::IsNull { expr, .. } => is_constant(expr),
        BoundExpr::FunctionCall { args, .. } => args.iter().all(is_constant),
    }
}
```

`ABS`と`LENGTH`(第8章)はどちらも同じ入力に対して常に同じ値を返す決定的な関数です。
この前提がある限り、`ABS(-1)`のような関数呼び出しも安全に畳み込めます。
将来、時刻や乱数を返す非決定的な関数を`FunctionRegistry::register`で追加する場合、この前提が崩れることに注意が必要ですが、この章の時点ではそのような関数を持ちません。

畳み込みの本体は、`src/rules.rs`に定義する、子から先に評価する`post-order`の再帰`fold_expr`です。

```rust
fn fold_expr(expr: BoundExpr, functions: &FunctionRegistry) -> (BoundExpr, bool) {
    match expr {
        BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. }
        | BoundExpr::ColumnRef { .. } => (expr, false),
        BoundExpr::BinaryOp { op, lhs, rhs, data_type, span } => {
            let (lhs, c1) = fold_expr(*lhs, functions);
            let (rhs, c2) = fold_expr(*rhs, functions);
            try_fold(BoundExpr::BinaryOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs), data_type, span }, functions, c1 || c2)
        }
        // IsNull・FunctionCall・Paren・Castも同じ形で子を先に畳み込む
        // ...
        BoundExpr::Aggregate { .. } => (expr, false),
    }
}
```

子を畳み込んでから自分自身を、`src/rules.rs`に定義する`try_fold`に渡すのは、`(1 + 1) = 2`のように、子同士が定数になって初めて親も定数だと判明する式があるからです。

```rust
fn try_fold(expr: BoundExpr, functions: &FunctionRegistry, children_changed: bool) -> (BoundExpr, bool) {
    if matches!(
        expr,
        BoundExpr::IntLiteral { .. } | BoundExpr::StringLiteral { .. } | BoundExpr::BoolLiteral { .. } | BoundExpr::NullLiteral { .. }
    ) {
        return (expr, children_changed);
    }
    if !is_constant(&expr) {
        return (expr, children_changed);
    }
    let span = expr.span();
    match eval_bound_expr(&expr, functions, None) {
        Ok(value) if value.is_null() && expr.data_type().is_some() => (expr, children_changed),
        Ok(value) => (value_to_literal(value, span), true),
        Err(DbError::Eval(_)) => (expr, children_changed),
        Err(other) => unreachable!("eval_bound_exprはDbError::Eval以外を返さない: {other:?}"),
    }
}
```

`1 / 0`のようにゼロ除算になる式、`i64`の範囲を超える式は、`eval_bound_expr`がエラーを返します。
このとき`try_fold`は畳み込みを諦め、元の式をそのまま残します。
実行時に評価されれば同じ理由でエラーになる式なので、畳み込みを諦めても最終的な振る舞いは変わりません。

もう1つ、`Ok(value) if value.is_null() && expr.data_type().is_some()`という分岐は、単体テストで実際につまずいて追加したガードです。
`ABS(id)`のように`id`列(`BIGINT`)が`NULL`だった場合、この式は`Value::Null`に評価されます。
これをそのまま`NullLiteral`へ畳み込むと問題が起きます。
`BoundExpr::NullLiteral::data_type()`は常に`None`を返す(裸の`NULL`は型を持たないリテラルだからです)一方、`ABS(id)`という式自身は`Binder`が`data_type: BigInt`を確定させています。
`logical_plan::projection_schema`(第18章)は、射影する式の型を`item.expr.data_type()`から決め、`None`のときは`TEXT`で代用します。
`ABS(id)`を`NullLiteral`へ畳み込んでしまうと、`SELECT ABS(id) FROM t`の出力列の型が`BIGINT`から`TEXT`へ後退してしまうのです。
`Value`は`Value::Null`という型を持たない値しか運べず、`expr`が持っていた静的な型情報を運ぶ手段がありません。
そこでこのケースだけは畳み込みを諦め、実行時の評価に委ねることで、出力スキーマを変えずに済ませています。

## Boolean Simplification: 三値論理のもとでの簡約

`TRUE AND status = 1`のような式は、`AND`の片方が定数の`TRUE`か`FALSE`だと分かった時点で簡約できます。
`Boolean Simplification`が行うのは次の8通りの書き換えで、`src/rules.rs`の`simplify_expr`に実装します。

```rust
fn simplify_expr(expr: BoundExpr) -> (BoundExpr, bool) {
    match expr {
        BoundExpr::BinaryOp { op: BinaryOperator::And, lhs, rhs, data_type, span } => {
            let (lhs, c1) = simplify_expr(*lhs);
            let (rhs, c2) = simplify_expr(*rhs);
            match (bool_literal(&lhs), bool_literal(&rhs)) {
                (Some(true), _) => (rhs, true),
                (_, Some(true)) => (lhs, true),
                (Some(false), _) => (lhs, true),
                (_, Some(false)) => (rhs, true),
                _ => (BoundExpr::BinaryOp { op: BinaryOperator::And, lhs: Box::new(lhs), rhs: Box::new(rhs), data_type, span }, c1 || c2),
            }
        }
        // Orも同じ形(TRUE OR x → TRUE、x OR TRUE → TRUE、
        // FALSE OR x → x、x OR FALSE → x)
        BoundExpr::UnaryOp { op: UnaryOperator::Not, expr, data_type, span } => {
            let (expr, changed) = simplify_expr(*expr);
            if let Some(value) = bool_literal(&expr) {
                return (BoundExpr::BoolLiteral { value: !value, span }, true);
            }
            if let BoundExpr::UnaryOp { op: UnaryOperator::Not, expr: inner, .. } = expr {
                return (*inner, true);
            }
            (BoundExpr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(expr), data_type, span }, changed)
        }
        // ...
    }
}
```

見た目はブール代数の教科書どおりですが、この式が扱う`AND`と`OR`は二値論理ではなく、`UNKNOWN`(`NULL`)を含む三値論理(第8章)です。
`FALSE AND x`を`FALSE`へ、`TRUE AND x`を`x`へ書き換えてよいのは、`x`が`TRUE`、`FALSE`、`UNKNOWN`のどの値であっても、書き換え前後で同じ値になると確かめられるからです。

`src/eval.rs`の`tri_and`はこう定義されています。

```rust
fn tri_and(l: Tri, r: Tri) -> Tri {
    match (l, r) {
        (Tri::False, _) | (_, Tri::False) => Tri::False,
        (Tri::True, Tri::True) => Tri::True,
        _ => Tri::Unknown,
    }
}
```

`tri_and(Tri::False, r)`は`r`が`Tri::Unknown`であっても`(Tri::False, _) => Tri::False`という最初の腕にそのまま一致し、`Tri::False`になります。
これが`FALSE AND x → FALSE`という書き換えの正しさそのものです。
`x`が`NULL`を含む行から来た値であっても、結果は変わりません。
`tri_and(Tri::True, r)`は`r`が`Tri::False`なら`Tri::False`(2番目の腕`(_, Tri::False)`)、`Tri::Unknown`なら最後の腕で`Tri::Unknown`、`Tri::True`なら`Tri::True`になり、いずれも`r`自身の値と一致します。
これが`TRUE AND x → x`の正しさです。
`OR`の4パターンも`tri_or`の定義から同じように確かめられ、`NOT NOT x → x`は`Tri`の`Not`実装が`Unknown`を`Unknown`のまま据え置くことから従います。

正しさの証明は式の値についてのものであり、式の**評価そのもの**についてではないことに注意が必要です。
`src/eval.rs`の`eval_binary_bound`の`AND`と`OR`は、短絡評価をせず両辺を必ず評価してから`tri_and`と`tri_or`を適用します。

```rust
BinaryOperator::And => {
    let l = value_to_tri(&eval_bound_expr(lhs, functions, row)?)?;
    let r = value_to_tri(&eval_bound_expr(rhs, functions, row)?)?;
    Ok(tri_to_value(tri_and(l, r)))
}
```

つまり`FALSE AND (1 / 0 = 1)`という式は、書き換え前ならゼロ除算のエラーで実行が止まります。
`Boolean Simplification`がこれを`FALSE`へ書き換えたあとは、右辺を評価すること自体がなくなり、エラーにならず0行を返します。
どちらの場合も「この式が`WHERE`に現れた行が結果に残ることはない」という点では変わらないので、返す行の集合という意味では書き換えは正しいと言えますが、エラーになるか成功するかという**振る舞い**までは保存していません。
実際のRDBMSの多くも、こうしたデッドブランチを畳み込んで消すのが一般的な挙動です。
この章もその慣行に合わせ、`src/rules.rs`の`mod tests`でこの振る舞いの変化そのものを確認します。

```rust
for predicate in [
    "FALSE AND col = 1",
    "col = 1 AND FALSE",
    "TRUE OR col = 1",
    "col = 1 OR TRUE",
    // ...
] {
    let mut db = Database::memory();
    db.execute("CREATE TABLE t (id BIGINT NOT NULL, col BIGINT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 1), (2, NULL), (3, 2)").unwrap();
    // ... colがNULLの行を含む3行に対して、簡約前後で一致結果が変わらないことを確認する
}
```

`col`が`NULL`の行を含むテーブルに対してこの8つの述語すべてを実行し、期待どおりの行だけが返ることを確認しています。

## Filter Merge: 連続するFilterを1つにまとめる

`Filter(Filter(x, p1), p2)`という連続する2つの`Filter`は、`Filter(x, p1 AND p2)`という1つの`Filter`にまとめられます。
`p1`と`p2`をそれぞれ独立に評価しても、`p1 AND p2`を1回評価しても、三値論理のもとで結果は変わりません。
この事実を使う`merge_filters`を、`src/rules.rs`に定義します。

```rust
fn merge_filters(plan: LogicalPlan) -> (LogicalPlan, bool) {
    if !matches!(plan, LogicalPlan::Filter(_)) {
        return map_plan_children(plan, merge_filters);
    }

    let mut predicates = Vec::new();
    let mut current = plan;
    while let LogicalPlan::Filter(filter) = current {
        predicates.push(filter.predicate);
        current = *filter.input;
    }
    let chain_changed = predicates.len() > 1;
    let (input, inner_changed) = merge_filters(current);

    predicates.reverse(); // 元々いちばん内側(Scanに近い)にあった述語を先頭にする
    let predicate = rebuild_conjunction(predicates).expect("Filterノードは必ず1個以上の述語を持つ");
    (LogicalPlan::Filter(FilterNode { input: Box::new(input), predicate }), chain_changed || inner_changed)
}
```

`rebuild_conjunction`は第25章の`choose_access_path`がすでに使っていた関数です(この章で`physical_plan`から`pub(crate)`に開放し、再利用しています)。
このルール単体では、`SELECT`が1つの`WHERE`から組み立てる木に連続する`Filter`は現れません。
`Filter Merge`が働くのは、次の`Predicate Pushdown`が同じテーブルへ複数の条件を別々に押し下げた直後のような、他のルールが連続する`Filter`を作り出した場面です。

## Predicate Pushdown: JOINをまたぐWHEREを片側へ運ぶ

章の冒頭で見た`customers.id = 1`が`Join`の真上に居座る例に戻ります。
`WHERE`の中には、`customers`だけを参照する項、`orders`だけを参照する項、両方を参照する項が混在しえます。
`Predicate Pushdown`は、`Filter`の述語をANDの連言に分解し、片側だけを参照する項をそのテーブルの`Scan`の直前まで運ぶ、次の`pushdown_plan`を`src/rules.rs`に実装します。

```rust
fn pushdown_plan(plan: LogicalPlan) -> (LogicalPlan, bool) {
    match plan {
        LogicalPlan::Filter(filter) => match *filter.input {
            LogicalPlan::Join(join) => {
                let left_len = join.left.output_schema().len();
                let mut conjuncts: Vec<&BoundExpr> = Vec::new();
                collect_conjuncts(&filter.predicate, &mut conjuncts);

                let mut left_preds = Vec::new();
                let mut right_preds = Vec::new();
                let mut residual = Vec::new();
                for conjunct in conjuncts {
                    match columns_side(conjunct, left_len) {
                        Some(Side::Left) => left_preds.push(conjunct.clone()),
                        Some(Side::Right) => right_preds.push(shift_column_index(conjunct, left_len)),
                        None => residual.push(conjunct.clone()),
                    }
                }
                // left_predsをleftの直上に、right_predsをrightの直上に、
                // それぞれ新しいFilterとして積む。residualはJoinの上に残す。
                // ...
            }
            other => {
                let (input, changed) = pushdown_plan(other);
                (LogicalPlan::Filter(FilterNode { input: Box::new(input), predicate: filter.predicate }), changed)
            }
        },
        other => map_plan_children(other, pushdown_plan),
    }
}
```

`collect_conjuncts`、`columns_side`、`shift_column_index`は、第25章の`choose_access_path`と`index_scan_target`がすでに実装していた関数です。
`columns_side`は式が参照する列がすべて左側(`Join`の`left`)か、すべて右側(`right`)かを判定します。
`shift_column_index`は、右側だけを参照する項の列添字を、結合後スキーマ上のフラットな添字から`right`単体のスキーマ上のローカルな添字へシフトします(第22章の`HashJoin`のBuild段階が同じ変換を必要としていたのと同じ理由です)。
この章では、この2つの判定と変換のロジックを`physical_plan`から`pub(crate)`として開放し、Logical Planのルールと物理計画の両方から同じ関数を呼ぶようにしました。
片側だけを参照するかどうかの判定基準が2箇所に分かれてズレる事態を避けるためです。

`left`と`right`はどちらも`Scan`単体か、それ自体が`Join`(左深い木の途中)でありえます。
押し下げた先が`Join`であれば、そのまま`pushdown_plan`を再帰させます。
これによって、3個以上の`JOIN`を持つ`FROM`でも、1回の`apply`呼び出しの中で必要な段数だけ潜っていけます。

### なぜ常に安全なのか: `INNER JOIN`しか無いという前提

この押し下げが常に安全なのは、このクレートの`FROM`が対応する結合が`INNER JOIN`(第22章の`JoinKind::Inner`)だけだからです。
`LEFT JOIN`や`RIGHT JOIN`のような外部結合があると、事情は変わります。
外部結合では、結合条件に一致しない側の行が`NULL`で埋められて生き残ります。
`WHERE`がその`NULL`で埋められる側の列を参照する条件を持っていた場合、この条件を結合より先(結合前の片側の`Filter`)へ動かしてしまうと、本来`NULL`埋めされて生き残るはずだった行が結合前に消えてしまい、結果が変わります。
`WHERE`は本来「結合が終わった後の行」に対して評価されるべき条件であり、結合前の`Filter`へ押し下げてよいのは、その条件を満たさない行が`NULL`埋めされても最終的に一致しないと分かる場合、つまり内部結合の場合だけです。
このクレートの構文には外部結合が無いため、この問題はそもそも起こりません。

## Projection Pruning: 使われない列をどこまで削れるか

`GROUP BY`と`SELECT`の対象式が実際に参照する列だけが、`Scan`から先の演算子を流れるようにするのがこの章最後のルールです。
狙いそのものは単純です。
`customers(id, name, address, phone)`のうち`SELECT customers.name`しか使わないなら、`address`、`phone`を運ぶ意味はありません。

素朴に実装しようとすると、すぐに壁にぶつかります。
`Scan`が読み出すバイト列は、テーブルの全列を前提にエンコードされています(第12章の`tuple_codec`)。
`SeqScanNode`の`schema`を勝手に縮めて`decode_tuple`に渡せば、列の並びと実際のバイト列がズレて壊れます。
つまり、この章のProjection Pruningは`Scan`が実際に読み込むバイト数を減らすわけではありません。
Heap Fileは行志向のストレージで、部分列だけを読み出す手段を持たないからです(列志向ストレージでの部分列読み出しは発展編Dの範囲です)。
それでも、`Scan`のすぐ上に**剪定用の`Projection`**を挟むことで、それより上の演算子(`Filter`、`Join`、`Aggregate`)が受け渡す`Tuple`の列数を減らせます。
`HashJoin`のBuild段階(第22章)は内側テーブルの全行を`Vec<Tuple>`として1つのハッシュテーブルへ積み込みます。
このとき積み込む`Tuple`が使われない列を持ったまま複製されれば、その分だけ余計なメモリ確保と複製コストがかかります。

### 適用範囲を絞る: 索引アクセスパスとの衝突を避ける

このルールが列を削るのは、`Aggregate`または`Projection`の直下が`Filter`や`Join`を経由して`Scan`へ至る場合に限られ、この判定を`src/rules.rs`の`is_from_where_subtree`が担います。

```rust
fn is_from_where_subtree(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Scan(_) | LogicalPlan::Values(_) => true,
        LogicalPlan::Filter(filter) => is_from_where_subtree(&filter.input),
        LogicalPlan::Join(join) => is_from_where_subtree(&join.left) && is_from_where_subtree(&join.right),
        _ => false,
    }
}
```

`Aggregate`や`Projection`が直接`Scan`の親であるとき(`Filter`も`Join`も経由しないとき)は、そもそも列を運ぶ演算子が1段しか無く、剪定用の`Projection`を挟んでも運ぶ手間が1段から2段に増えるだけで得るものがありません。
第18章から第21章で見てきた大半の単一テーブル`SELECT`はこの形であり、このルールはほとんど何もしません。
これは手落ちではなく、単一テーブルの計画では`Scan`が返す行を読むのは`Filter`か`Projection`のうち高々1つずつであり、削って得をする「使われない列を持ったまま何段も運ばれる行」がそもそも存在しないからです。

もっと大きな理由が、`Filter`直下の`Scan`と`Join`の`right`直下の`Scan`をそれぞれ剪定の対象から外していることで、`src/rules.rs`の`prune_scope`は次のように書きます。

```rust
LogicalPlan::Filter(filter) => {
    let mut child_required = required.clone();
    collect_columns(&filter.predicate, &mut child_required);
    if matches!(*filter.input, LogicalPlan::Scan(_)) {
        // 第25章のchoose_access_pathがFilter直下のScanに依存する
        // ため、この位置のScanは剪定しない(モジュールドキュメント参照)。
        let LogicalPlan::Scan(scan) = *filter.input else { unreachable!() };
        let map = identity_map(scan.schema.len());
        let plan = LogicalPlan::Filter(FilterNode { input: Box::new(LogicalPlan::Scan(scan)), predicate: filter.predicate });
        (plan, map, false)
    } else {
        // ...
    }
}
```

第25章の`choose_access_path`は、`Filter`の直下が`Scan`そのものであることを前提に索引アクセスパスを選びます。
この位置に剪定用の`Projection`を挟むと、`Filter`の直下は`Scan`ではなく`Projection`になり、`choose_access_path`のパターンマッチが素通りされ、索引が二度と選ばれなくなってしまいます。
`Join`の`right`(内側テーブル)が剥き出しの`Scan`である場合も同じ理由で対象から外し、`src/rules.rs`の`prune_scope`では次のように扱います。

```rust
let (new_right, map_right, c2) = if matches!(join.right.as_ref(), LogicalPlan::Scan(_)) {
    // 第25章のindex_scan_targetがJoinのright直下のSeqScanに
    // 依存するため、この位置のScanは剪定しない(モジュール
    // ドキュメント参照)。
    let LogicalPlan::Scan(scan) = *join.right else { unreachable!() };
    let map = identity_map(scan.schema.len());
    (LogicalPlan::Scan(scan), map, false)
} else {
    prune_scope(*join.right, &required_right)
};
```

第25章の`index_scan_target`は、`right`が`PhysicalPlan::SeqScan`のままであることを前提にIndex Nested Loop Joinを選びます。
ここに`Projection`を挟むと、この判定が素通りされ、内側テーブルに索引があってもIndex Nested Loop Joinが選ばれなくなります。
1つ前の章が積み上げた最適化(索引アクセスパスの選択)を、この章の最適化(列の剪定)が黙って壊してしまう、という組み合わせです。
静的な解析だけでは検出できない食い違いなので、この章では2つの位置を無条件に保護することで衝突そのものを避けています。

この2つの除外によって、実際に列を削れるのは「`WHERE`の効かない列を持つテーブルが`JOIN`の左側(`left`)に来る場合」にほぼ限られます。
`n`個の`JOIN`を持つ左深い木では、`right`は常に単一のテーブル(`Scan`かその直上の`Filter`)であり、上で見た2つの保護のどちらかに必ず該当します。
剪定の余地が残るのは、木の左端、つまり`FROM`に最初に書かれたテーブルが自分自身の`WHERE`条件を持たない場合だけです。

### 剪定の本体: 必要な列を上から下へ運ぶ

`Aggregate`か`Projection`が対象になったとき、まずその式が参照する列を集める処理を、`src/rules.rs`の`prune_scope`に次のように書きます。

```rust
LogicalPlan::Aggregate(mut aggregate) => {
    if is_from_where_subtree(&aggregate.input) && !is_bare_leaf(&aggregate.input) {
        let mut required = BTreeSet::new();
        for expr in &aggregate.group_by {
            collect_columns(expr, &mut required);
        }
        for call in &aggregate.calls {
            if let Some(arg) = &call.arg {
                collect_columns(arg, &mut required);
            }
        }
        let (input, map, changed) = prune_scope(*aggregate.input, &required);
        // group_by・callsの列添字をmapで書き換え、inputを差し替える
        // ...
    }
    // ...
}
```

`prune_scope`は、`required`(この部分木の出力のうち上位で実際に必要とされる列の添字の集合)を受け取り、`Scan`、`Filter`、`Join`だけからなる部分木を剪定します。
`Join`のケースでは、`required`を`left_len`を境に左側と右側へ振り分け、`Join`の結合条件自身が参照する列も同じように振り分けたうえで、両側を再帰的に剪定する処理を、`src/rules.rs`の`prune_scope`に次のように書きます。

```rust
LogicalPlan::Join(join) => {
    let left_len = join.left.output_schema().len();
    let mut required_left = BTreeSet::new();
    let mut required_right = BTreeSet::new();
    for &global in required {
        split_by_side(global, left_len, &mut required_left, &mut required_right);
    }
    let mut condition_columns = BTreeSet::new();
    collect_columns(&join.condition, &mut condition_columns);
    for global in condition_columns {
        split_by_side(global, left_len, &mut required_left, &mut required_right);
    }

    let (new_left, map_left, c1) = prune_scope(*join.left, &required_left);
    // rightは剥き出しのScanなら保護、そうでなければ再帰
    // ...

    // conditionの列添字を、左右それぞれの新しい添字へ組み替える
    // ...
}
```

`Join`条件そのものが参照する列を`required`に加えているのが要点です。
`customers JOIN orders ON customers.id = orders.customer_id`で`SELECT orders.amount`しか使わなくても、`customers.id`は結合を成立させるために必要であり、勝手に落とすと結合そのものができなくなります。

葉である`Scan`では、`required`に含まれる列だけを残した`Projection`を新しく挟む処理を、`src/rules.rs`の`prune_scan`に実装します。

```rust
fn prune_scan(scan: ScanNode, required: &BTreeSet<usize>) -> (LogicalPlan, BTreeMap<usize, usize>, bool) {
    let width = scan.schema.len();
    if required.len() >= width {
        return (LogicalPlan::Scan(scan), identity_map(width), false);
    }

    let kept: Vec<usize> = required.iter().copied().collect(); // BTreeSetなので昇順
    let map: BTreeMap<usize, usize> = kept.iter().enumerate().map(|(new_index, &old_index)| (old_index, new_index)).collect();
    let projection: Vec<BoundSelectItem> = kept
        .iter()
        .map(|&old_index| {
            let column = &scan.schema.columns()[old_index];
            BoundSelectItem {
                expr: BoundExpr::ColumnRef {
                    table_ordinal: 0,
                    column_index: old_index,
                    name: column.name.clone(),
                    data_type: column.data_type,
                    span: Span::new(0, 0),
                },
                output_name: column.name.clone(),
            }
        })
        .collect();
    // Projection(input: Scan(scan), projection)を返す
    // ...
}
```

`required`が全列を覆っている(削れる列が無い)場合は、`Scan`をそのまま返して`Projection`を挟みません。
剪定した場合に返す`BTreeMap`は、剪定前の添字から剪定後の添字への対応です。
呼び出し側はこの対応表を使い、`Aggregate`、`Join`、`Filter`が持つ式の中の列添字を、剪定後のスキーマに合わせて書き換えます。

## `EXPLAIN`でルール適用の結果を確認する

`EXPLAIN`は、この章の書き換えが終わったあとの`LogicalPlan`を`physical_plan::optimize`へ渡した結果を表示します。
`rules::optimize`が組み立てるのは新しい構文でも別コマンドでもなく、既存の`LogicalPlan`のままです。
そのため、章の冒頭で見た`WHERE 1 = 1 AND ...`と`customers.id = 1`の例は、この章のあとでは次のように変わります。

```console
minidb> EXPLAIN SELECT amount FROM orders WHERE 1 = 1 AND status = 1 AND amount > 0;
QUERY PLAN
----------
Projection(amount)
  └─ Filter(status = 1 AND amount > 0)
    └─ SeqScan(orders)
(3 rows)

minidb> EXPLAIN SELECT name FROM customers JOIN orders ON customers.id = orders.id \
  WHERE customers.id = 1;
QUERY PLAN
----------
Projection(name)
  └─ HashJoin(INNER JOIN, id = id)
    └─ Filter(id = 1)
      └─ SeqScan(customers)
    └─ SeqScan(orders)
(5 rows)
```

`1 = 1`は消え、`customers.id = 1`は`customers`の`SeqScan`の直上まで運ばれています。
`customers`に`id`列への索引があれば、この`Filter`は第25章の`choose_access_path`によって`IndexScan`にも変わります。
`Predicate Pushdown`が`Filter`を`Scan`の直上まで運んでおいたおかげで、第25章が「`JOIN`を伴うクエリの`WHERE`はどのテーブルの`Scan`にも索引として届かない」と書き残していた限界(第25章の演習課題3)が、この章でようやく解消されます。

## 適用順序と固定点

`optimize`が全ルールを1度ずつ順に適用するだけでなく、変化が無くなるまで繰り返す理由を、具体的な式で確かめます。

```console
minidb> EXPLAIN SELECT id FROM t WHERE (1 + 1 = 2) AND id = 5;
QUERY PLAN
----------
Projection(id)
  └─ Filter(id = 5)
    └─ SeqScan(t)
(3 rows)
```

`(1 + 1 = 2) AND id = 5`が`id = 5`だけに簡約されるまでには、2つのルールがこの順番で働く必要があります。

1. `Constant Folding`が`1 + 1 = 2`を`TRUE`というリテラルへ畳み込む。
2. `Boolean Simplification`が`TRUE AND id = 5`を`id = 5`へ簡約する。

`Boolean Simplification`の`bool_literal`は、`BoundExpr::BoolLiteral`という**構文上のリテラル**を探すのであって、「評価すれば`TRUE`になる式」を意味的に探すわけではありません。
`Constant Folding`が畳み込みを終える前に`Boolean Simplification`だけを1回動かしても、`1 + 1 = 2`はまだ`BinaryOp`のままで、`bool_literal`はこれを`None`と判定し、簡約は起こりません。

`default_rules`はこの2つを実行順で並べているので、1回のパスの中でも`Constant Folding`が先に`1 + 1 = 2`を畳み込み、直後の`Boolean Simplification`がそれを見て簡約できます。
ここで気にしておきたいのは、ルールの並び順を工夫すれば1回のパスで足りる場合があっても、それに頼った設計は別のルールの組み合わせが増えたときに崩れるという点です。
`Predicate Pushdown`が新しく作った`Filter`同士を`Filter Merge`がまとめる、`Filter Merge`がまとめた結果を`Constant Folding`がさらに畳み込む、といった連鎖は、ルールの数が増えるほど「これで全部畳み込みきった」と1回のパスの並び順だけで保証するのが難しくなります。
`optimize`がどのルールも変化を起こさなくなるまでループするのは、この順序依存を個々のルールの並べ方ではなく「収束するまで回す」という一律の手段で消すためです。

固定点まで回した結果が、それ以上ルールを適用しても変わらないことは、`optimize`を2回続けて呼んでも同じ木になるという形で、`src/rules.rs`の`mod tests`でテストしています。

```rust
let once = optimize(logical, &functions);
let twice = optimize(once.clone(), &functions);
assert_eq!(once, twice);
```

## 測って確認する

`Predicate Pushdown`が`HashJoin`のBuild段階を変える様子を実測します。
`customers`(50行)と`orders`(`m`行)を結合し、`orders`の`status`列への条件を、押し下げ**できる**形(`orders.status = 0`)と押し下げ**できない**形(`orders.status = customers.token`、両側を参照するので`Join`の上に残る)の2通りで書いて比べます。
どちらも一致する行の割合は揃えてあり、違いは述語が`orders`の`Scan`直上まで運ばれているかどうかだけです。

```console
$ cargo test --release --lib -- --ignored --nocapture predicate_pushdown_shrinks_hash_join_build_side
m=  2000  pushable= 166.815µs  not_pushable= 510.687µs
m=  8000  pushable= 566.919µs  not_pushable=1.971673ms
m= 32000  pushable= 2.15792ms  not_pushable=7.689918ms
```

`m`をどの大きさにしても、押し下げられた場合はおよそ3倍速く終わっています。
`not_pushable`側の`HashJoin`は、`Filter`が`Join`の上に残っているぶん、`orders`の`m`行すべてをBuild段階でハッシュテーブルへ積み込みます。
`pushable`側は`orders`の`Scan`直上の`Filter`が先に効き、`status = 0`に一致する行(全体のおよそ1%)だけがBuild段階に渡ります。
`m`が増えるほど両者の差も開いていくのは、`not_pushable`側のBuildコストが`m`にほぼ比例して伸び続けるのに対し、`pushable`側は絞り込まれた後の行数に比例するだけだからです。

## 見送った変換

原案(`chatgpt_opinion.md`)は、この章のルールとして不要な`Sort`の除去と、`CROSS JOIN + Filter`から`Equi-Join`への変換も挙げていました。
どちらも、このクレートの現在の文法では発生しない書き換えなので、この章では実装しません。

**不要な`Sort`の除去**は、同じ列への`ORDER BY`が2回続く、あるいは`Sort`の直後に別の`Sort`が来るような計画を対象にした変換です。
`Parser`(第7章)が受け付ける`ORDER BY`は`SELECT`につき最上位の1個だけで、`logical_plan::build_select`(第18章、第21章)が積む`Sort`ノードもクエリ全体で高々1個です。
`Sort`が2つ連続する`LogicalPlan`は、このクレートの文法からはそもそも組み立てられません。

**`CROSS JOIN + Filter`から`Equi-Join`への変換**は、`FROM a, b WHERE a.id = b.id`のような、カンマ区切りの直積とその直後の等値条件を`a JOIN b ON a.id = b.id`という真の結合へ読み替える変換です。
`Parser`(第22章)は`FROM`のカンマ区切りをそもそも受け付けず、複数テーブルを書くには`JOIN ... ON`が必須です。
この文法である以上、直積とその後の絞り込みという形自体が生じません。

どちらも「無理に作らない」という判断です。
存在しない入力のための変換を書けば、テストで確かめる手立てもなく、コードだけが死んだまま残ります。

## 到達点

`EXPLAIN`が見せる計画は、この章から`Constant Folding`、`Boolean Simplification`、`Filter Merge`、`Predicate Pushdown`、`Projection Pruning`という5つのルールを、変化が無くなるまで通した後の形になりました。
同じ意味のSQLを違う書き方で書いても、`WHERE`の冗長な項は畳み込まれ、`JOIN`をまたぐ条件は該当するテーブルまで運ばれます。
これは、第25章までの「構文的な性質だけを見る」アクセスパス選択に、書き換えという新しい層を足したことを意味します。
`Rule`トレイトによる登録の仕組みは、次章以降で増えていく統計情報ベースの書き換えも同じ形で迎え入れられるように作ってあります。

第27章では、この章がまだ手を付けていない領域に進みます。
行数、`NULL`数、distinct値数といった統計情報を集め、`WHERE`が実際にどれだけの行を絞り込むかを推定します。
この章の書き換えが「構文的に無駄かどうか」だけを見ていたのに対し、統計情報は「実際のデータでどれだけ効くか」を数値で答えます。
第25章の実測が示した、Index Nested Loop Joinが密な結合でHash Joinより15倍以上遅くなるという逆転現象に、第28章のコストベース最適化がようやく答えを出します。

## 演習問題

### 必須課題

1. `Boolean Simplification`は`NOT NOT x → x`を扱いますが、`NOT (x AND y)`を`NOT x OR NOT y`へ変換するド・モルガンの法則は実装していません。三値論理のもとでこの変換が常に成り立つかどうかを`tri_and`、`tri_or`、`Tri`の`Not`実装から確かめ、成り立つなら`simplify_expr`に追加してください。成り立たないなら、その反例をテストとして書いてください。
2. `Predicate Pushdown`は、`Join`の`right`だけを参照する項を`shift_column_index`でシフトしていますが、`left`を参照する項はシフトしません。この非対称性がなぜ正しいのか(`Join::output_schema`が`left`と`right`をどう連結しているか、第18章の`join_schema`を参照)を説明したうえで、`left`が単なる`Scan`ではなく別の`Join`(左深い木の途中)である場合に、押し下げた`Filter`がさらに1段深く運ばれることをテストで確認してください。
3. `Projection Pruning`は、`Filter`直下の`Scan`と`Join`の`right`直下の`Scan`を無条件に保護対象としています。この保護によって、`SELECT amount FROM users JOIN orders ON users.id = orders.user_id WHERE orders.amount > 100`のような、`orders`に`amount`以外の未使用列(`note`など)がある場合でも`orders`側は剪定されません。`physical_plan::optimize`が実際に`IndexScan`かIndex Nested Loop Joinを選んだ場合に限って保護を外す(それ以外はこれまでどおり保護する)設計を検討し、実装の難しさ(`rules::optimize`はLogical Planの段階で動き、その時点ではまだ索引が実際に選ばれるかどうか分からないこと)を含めて考察してください。
4. `Constant Folding`は`Value::Null`を返す評価結果を、`expr.data_type()`が`Some`の場合は畳み込まずに残します。この防御が無かった場合に実際に何が壊れるかを、`SELECT ABS(id) FROM t`のようなクエリの`output_schema()`を使って再現するテストを書いてください(`try_fold`の`Ok(value) if value.is_null() && expr.data_type().is_some()`という分岐を一時的にコメントアウトして確認し、元に戻してください)。

### 発展課題

1. この章の`optimize`は`default_rules`が返す5つのルールを固定の順序で並べていますが、本文で確認したとおり、順序に強く依存しない設計は固定点までの反復に支えられています。`default_rules`の並び順を(例えば`ProjectionPruning`を先頭に)入れ替えても、`optimize`の最終結果(`LogicalPlan`の構造)が変わらないことをproperty的なテストで確認してください。反復回数(`optimize`が固定点に達するまでのループ回数)は順序によって変わりうることも合わせて観察してください。
2. `Predicate Pushdown`は、`Join`の`ON`条件そのもの(結合条件)には手を付けません。`a JOIN b ON a.x = b.x AND a.y = 1`のように、`ON`の中に片側だけを参照する項(`a.y = 1`)が混ざっている場合、この項を`WHERE`と同じように該当するテーブルの`Scan`直上まで運んでも結果は変わりません(`INNER JOIN`である限り、`ON`の中の片側条件も`WHERE`の片側条件と同じ理由で押し下げが安全です)。この変換を新しいルールとして設計し、実装してください。
3. `Projection Pruning`は`Values`を剪定の対象にしていません(`prune_scope`の`Values`アームは常に`identity_map`を返します)。`INSERT INTO t SELECT a, b, c FROM ...`のような構文がこのクレートに追加された場合(現在は無い)、`Values`の剪定にどんな意味が出てくるかを考察してください。
