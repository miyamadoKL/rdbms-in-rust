# 第21章 Sort、Limit、Distinct、Aggregate

「もっとも注文件数が多い部門はどこですか」というリクエストに、前章までのminidbはどう答えるでしょうか。

```console
minidb> SELECT dept, COUNT(*) FROM orders GROUP BY dept ORDER BY dept;
行1列17: 構文エラー: 文の終端が必要です: Keyword(From)が見つかりました
```

`GROUP BY`はおろか、`COUNT`という関数名すら知りません。
アプリケーション側でできることは、`SELECT dept FROM orders`で全行を取り、部門ごとに件数を数え、多い順に並べ替えるという後処理をRust(あるいはこのクレートを呼ぶどんな言語)で書き直すことだけです。

## 前章の限界

第19章のVolcanoモデルは`SeqScan`、`Filter`、`Projection`の3つの演算子しか持ちません。
`Filter`は行を減らすだけで並べ替えず、`Projection`は列を選ぶだけで行数を変えません。
`SELECT`が返せる行の順序は、`SeqScan`が読む順序(このクレートでは常に挿入順)以外にありえず、件数を絞ることも、重複を除くことも、複数行を1つの値へ集計することもできません。

`ORDER BY`、`LIMIT`、`DISTINCT`、`GROUP BY`、集約関数はどれも、SQLが「1行を1行のまま処理する」という前章までの演算子の型を破ります。
`ORDER BY`は結果の順序を決めるために全行を見終える必要があり、集約関数は複数行を1行へ畳み込みます。
この章では、この2種類の演算子をどちらもVolcanoモデルの上に実装し、標準SQLが定める`SELECT`の評価順序をそのまま演算子の木として表現します。

## 構文を追加する

`ORDER BY`、`LIMIT`、`OFFSET`、`DISTINCT`、`GROUP BY`、`HAVING`、集約関数(`COUNT`、`SUM`、`MIN`、`MAX`)の構文を、Lexer、Parser、ASTへ追加します。

```console
minidb> SELECT dept, COUNT(*) FROM orders GROUP BY dept HAVING COUNT(*) > 1 ORDER BY dept LIMIT 5;
```

対応するキーワードを、第6章の`src/lexer.rs`へ追加します。

```rust
pub enum Keyword {
    // ...
    Order,
    By,
    Asc,
    Desc,
    Limit,
    Offset,
    Distinct,
    Group,
    Having,
}
```

`src/ast.rs`の`SelectStatement`(第7章のAST)に、これらの句を表すフィールドを追加します。

```rust
pub struct SelectStatement {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    pub span: Span,
}

pub struct OrderByItem {
    pub expr: Expr,
    pub desc: bool,
    pub span: Span,
}
```

`Parser::parse_select_statement`は、`SELECT`直後の`DISTINCT`を読んでから射影対象を読み、`WHERE`の後ろに`GROUP BY`、`HAVING`、`ORDER BY`、`LIMIT`、`OFFSET`をこの順で(すべて省略可能な形で)読み進めます。
この並び順は構文上の都合ではなく、標準SQLが句の並べ方として定めている順序そのものです。

集約関数は少し特殊な扱いを要ります。
`COUNT(*)`の`*`は、乗算演算子の`*`(`TokenKind::Star`)と同じトークンであり、`SELECT *`のワイルドカードとも同じトークンです。
第7章の`parse_select_item`がすでに`SELECT`直後の`*`を先読みして`SelectItem::Wildcard`へ振り分けていたのと同じ理由で、`COUNT(...)`の中の`*`も通常の式として`parse_expr`に渡すわけにはいきません。
`src/ast.rs`に、集約関数の種類を表す`AggregateFunc`を次のように定義します。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    Min,
    Max,
}

impl AggregateFunc {
    pub fn from_name(name: &str) -> Option<AggregateFunc> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(AggregateFunc::Count),
            "SUM" => Some(AggregateFunc::Sum),
            "MIN" => Some(AggregateFunc::Min),
            "MAX" => Some(AggregateFunc::Max),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AggregateFunc::Count => "COUNT",
            AggregateFunc::Sum => "SUM",
            AggregateFunc::Min => "MIN",
            AggregateFunc::Max => "MAX",
        }
    }
}
```

`AggregateFunc::from_name`で`COUNT`、`SUM`、`MIN`、`MAX`という名前を識別し、`parse_function_call`から専用の関数へ振り分けます。
`src/parser.rs`に次の`parse_aggregate_call`を追加します。

```rust
fn parse_aggregate_call(&mut self, func: AggregateFunc, name_span: Span) -> DbResult<Expr> {
    self.expect_punct(TokenKind::LParen, "(")?;

    let arg = if func == AggregateFunc::Count && *self.peek_kind() == TokenKind::Star {
        self.advance();
        None
    } else if *self.peek_kind() == TokenKind::Star {
        return Err(self.unexpected(&format!("{}の引数には式が必要です(*は使えません)", func.name())));
    } else {
        Some(Box::new(self.parse_expr(0)?))
    };

    let end = self.expect_punct(TokenKind::RParen, ")")?.end;
    Ok(Expr::Aggregate {
        func,
        arg,
        span: Span::new(name_span.start, end),
    })
}
```

`arg`が`None`になるのは`COUNT(*)`だけです。
`SUM(*)`や`MIN(*)`のような書き方は、この時点(構文解析)で拒否します。
`*`が意味を持つ場所は`COUNT`という1つの関数に限られており、`SUM(*)`を実行時のエラーまで持ち越す理由がありません。

集約関数を`Expr::FunctionCall`とは別の`Expr::Aggregate`ノードにしたのは、`abs`や`length`(第8章)のようなScalar Functionと集約関数が、評価のしかたそのものが違うからです。
Scalar Functionは1行の値だけから1つの値を計算しますが、集約関数は複数行にまたがる状態(件数、合計、最小値)を必要とします。
`FunctionRegistry`(第8章)にこの状態を持たせる設計も検討しましたが、`FunctionRegistry`は「1回の呼び出しで完結する関数」という前提のまま第8章から一貫して使われており、状態を持つ関数を混ぜると、Scalar Functionの呼び出し(`eval::eval_bound_expr`の中で完結する)と集約関数の呼び出し(複数行にまたがって`Executor`が管理する)という異なる性質を1つの`FunctionEntry`型に押し込むことになります。
型を分けたことで、この章の`Binder`は「この式は集約関数を含むか」という問いに、`Expr::Aggregate`という1つのバリアントを`match`するだけで答えられます。

## Binderでの検査: 集約はどこで使えるか

`SELECT COUNT(*) FROM orders WHERE COUNT(*) > 1`という文がなぜ意味をなさないか、`Binder`(第17章)の言葉で説明できるでしょうか。

`WHERE`は、集約が行われる**前**の個々の行に対して評価されます。
まだグループ化も集計もされていない1行に対して「件数」を問うことに意味がないため、標準SQLは`WHERE`の中で集約関数を使うことを禁じています。
この章の`src/binder.rs`の`Binder::bind_select`は、`WHERE`を束縛した直後にこの検査を行います。

```rust
let predicate = match &select.where_clause {
    Some(expr) => {
        let bound = self.bind_predicate(expr, &tables)?;
        if bound_contains_aggregate(&bound) {
            return Err(self.error_at(
                bound.span(),
                "集約関数はWHEREでは使えません(集約はWHEREによる絞り込みの後に計算されます)".to_string(),
            ));
        }
        Some(bound)
    }
    None => None,
};
```

`bound_contains_aggregate`は、式木のどこかに`BoundExpr::Aggregate`が現れるかどうかを再帰的に調べる関数です。
同じ関数が、`GROUP BY`の中の集約関数(`GROUP BY COUNT(*)`のような、グループ化キー自体が集約結果に依存する書き方)と、集約関数の引数に現れるさらなる集約関数(`SUM(COUNT(*))`のような入れ子)の検査にも使われます。
後者を禁じる理由は、集約が「複数行を1行へ畳み込む」演算だからです。
`COUNT(x)`の結果はすでに1個のグループにつき1つの値であり、それをさらに`SUM`で畳み込む対象(複数行)がその場には存在しません。

`SELECT`と`HAVING`に現れる集約関数は、いつ集計されるのでしょうか。
`SELECT dept, COUNT(*) FROM orders GROUP BY dept`は、`GROUP BY`が確定させたグループごとに`COUNT(*)`を1回だけ計算し、その結果を`dept`と並べて返します。
`Binder`はこの計算を1箇所(後の節で見る`Aggregate`演算子)に集約するため、`SELECT`、`HAVING`に現れる集約関数呼び出しを、最初に現れた順に重複無く1つのリストへ集める`build_aggregate`を`src/binder.rs`に用意します。

```rust
fn build_aggregate(
    &self,
    group_by: Vec<BoundExpr>,
    projection: Vec<BoundSelectItem>,
    having: Option<BoundExpr>,
) -> DbResult<(BoundAggregate, Vec<BoundSelectItem>, Option<BoundExpr>)> {
    // ...(group_byの各式とcallsを、Aggregate演算子が出力する列として並べる)
}
```

`SELECT COUNT(*), COUNT(*) FROM orders`のように同じ呼び出しが2回書かれても、出力列は1つにまとめます。
どの呼び出しが「同じ」かの判定は、式の構造を人間が読める文字列に変換する`logical_plan::fmt_bound_expr`(第18章、`EXPLAIN`の表示にも使っている関数)が返す文字列の一致で行います。
`Span`(ソース中の位置)まで比較してしまうと、同じ書き方の呼び出しでも書かれた位置が違うだけで「別の呼び出し」と判定されてしまうため、位置情報を持たないこの文字列表現のほうが、この判定には向いています。

### 射影は集約結果とGROUP BYの列だけを参照できる

`SELECT dept, amount FROM orders GROUP BY dept`という文を実行すると何が起きるべきでしょうか。

`dept`ごとにグループ化された後、各グループには複数の`amount`が含まれえます。
`amount`という列参照は、そのグループのどの行の`amount`を指しているのか定まりません。
標準SQLはこれを**関数従属性**の違反として拒否し、`SELECT`、`HAVING`に書けるのは`GROUP BY`の列(またはその式)と、集約関数の呼び出しだけに限ります。

この検査を、`Binder::rewrite_for_aggregate`が担います。
この関数は、射影、`HAVING`の式木を先頭から辿り、式全体が`GROUP BY`のいずれかの式と一致すれば、その式をまるごと`Aggregate`演算子の出力列への参照へ書き換えます。
一致しなければ子の式へ再帰し、途中で生の列参照(`BoundExpr::ColumnRef`)に出会った時点でエラーにするこの関数を、`src/binder.rs`に次のように定義します。

```rust
fn rewrite_for_aggregate(
    &self,
    expr: BoundExpr,
    group_key_slots: &HashMap<String, usize>,
    call_slots: &HashMap<String, usize>,
    group_by_len: usize,
    schema: &Schema,
) -> DbResult<BoundExpr> {
    let key = logical_plan::fmt_bound_expr(&expr);
    if let Some(&slot) = group_key_slots.get(&key) {
        return Ok(self.slot_column_ref(slot, schema, expr.span()));
    }

    match expr {
        BoundExpr::Aggregate { span, .. } => {
            let slot = group_by_len + call_slots.get(&key).copied().expect("...");
            Ok(self.slot_column_ref(slot, schema, span))
        }
        BoundExpr::ColumnRef { span, name, .. } => Err(self.error_at(
            span,
            format!("列'{name}'はGROUP BYの列か集約関数の引数としてのみ使用できます"),
        )),
        // ...(リテラルはそのまま、UnaryOp、BinaryOp等は子を再帰的に書き換える)
    }
}
```

「式全体が一致すれば、それ以上子には再帰しない」という順序が要点です。
`GROUP BY a + b`のもとで`SELECT a + b`と書いた場合、`a + b`全体が1つのグループ化キーとして一致するため、内部の`a`、`b`を個別に検査する必要がありません。
先に子から検査してしまうと、`a`、`b`という(グループ化されていない)生の列参照として誤って拒否してしまいます。

書き換えた後の式は、`table_ordinal = 0`の`ColumnRef`になります。
これは架空のテーブルではなく、`Aggregate`演算子が実際に生成する行の列を指します。
`GROUP BY`の各式(先頭の列)に、集約関数呼び出し(残りの列)を続けた`Schema`を、`Binder`が`src/binder.rs`の`BoundAggregate`としてあらかじめ組み立てておくためです。
続けて、集約関数の呼び出し1個を表す`AggregateCall`を次のように定義します。

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateCall {
    pub func: AggregateFunc,
    pub arg: Option<Box<BoundExpr>>,
}
```

```rust
pub struct BoundAggregate {
    pub group_by: Vec<BoundExpr>,
    pub calls: Vec<AggregateCall>,
    pub schema: Schema,
}
```

この書き換えの結果、`FilterExec`(`HAVING`用)や`ProjectionExec`(第19章)は、集約が絡んでいることを一切意識せずに済みます。
どちらも「`Schema`と`BoundExpr`を受け取り、`Row`から値を計算する」という第19章から変わらないインターフェースのままで、`Aggregate`演算子が生成した行を、`Scan`が生成した行と同じように扱えます。

### `ORDER BY`はどの範囲を束縛するか

`SELECT name FROM users ORDER BY id`は、`SELECT`が返す`name`という1列とは別に、並べ替えの基準として`id`という列を必要とします。
`id`は射影の対象式には無い列であり、`ORDER BY`を射影の出力列だけに限ってしまうと、この最頻出のパターンすら書けません。
この章の`Binder`は、`ORDER BY`の式を次の優先順位で解決します。

1. **`SELECT`の対象式の中に、構造が完全に一致する式が無いか探す**。あれば、その列をそのまま並べ替えのキーに使う。
2. 見つからなければ、`FROM`(集約が絡む`SELECT`では`GROUP BY`の列と集約関数)のスコープで新しく束縛し、その結果を射影の末尾に**隠し列**として追加する。

「構造が完全に一致するか」の判定には、集約関数の呼び出しの重複排除(前節)と同じ`logical_plan::fmt_bound_expr`を使います。
`SELECT dept ... ORDER BY dept`のように、`ORDER BY`が`SELECT`の対象式と同じ列を指す場合、`src/binder.rs`の`resolve_order_by_in_plain_scope`は新しい列を増やさずに済ませます(「射影に同名の出力列があればそれが優先される」という、標準SQLの名前解決の優先順位そのものです)。

```rust
fn resolve_order_by_in_plain_scope(
    &self,
    expr: &Expr,
    tables: &[BoundTableRef],
    projection: &mut Vec<BoundSelectItem>,
) -> DbResult<usize> {
    let bound = self.bind_expr(expr, tables)?;
    let key = logical_plan::fmt_bound_expr(&bound);
    if let Some(index) = projection.iter().position(|item| logical_plan::fmt_bound_expr(&item.expr) == key) {
        return Ok(index);
    }
    let output_name = self.sql[expr.span().start..expr.span().end].to_string();
    projection.push(BoundSelectItem { expr: bound, output_name });
    Ok(projection.len() - 1)
}
```

集約が絡む`SELECT`も、`src/binder.rs`の`resolve_order_by_in_aggregate_scope`が同じ考え方で解決しますが、`tables`ではなく`aggregate`(`HAVING`と同じスコープ、集約関数呼び出しを含んでよい)を経由し、前節の`rewrite_for_aggregate`にそのまま通します。

```rust
fn resolve_order_by_in_aggregate_scope(
    &self,
    expr: &Expr,
    tables: &[BoundTableRef],
    aggregate: &mut BoundAggregate,
    projection: &mut Vec<BoundSelectItem>,
) -> DbResult<usize> {
    let bound = self.bind_expr(expr, tables)?;
    let rewritten = self.rewrite_for_aggregate(bound, aggregate)?;
    let key = logical_plan::fmt_bound_expr(&rewritten);
    if let Some(index) = projection.iter().position(|item| logical_plan::fmt_bound_expr(&item.expr) == key) {
        return Ok(index);
    }
    let output_name = self.sql[expr.span().start..expr.span().end].to_string();
    projection.push(BoundSelectItem { expr: rewritten, output_name });
    Ok(projection.len() - 1)
}
```

`rewrite_for_aggregate`(前節)は、`aggregate.calls`にまだ無い呼び出しを新しい列として追記できる関数でした。
`SELECT dept FROM orders GROUP BY dept ORDER BY COUNT(*) DESC`のように、`SELECT`には現れない集約関数を`ORDER BY`だけに書いた場合も、この追記が働きます。

```console
minidb> SELECT dept FROM orders GROUP BY dept ORDER BY COUNT(*) DESC;
dept
----
eng
sales
hr
(3 rows)
```

`COUNT(*)`は`aggregate.schema`の新しい列になり、その列への参照が`projection`の末尾に隠し列として加わります。
`dept`という1列だけを`SELECT`したはずなのに、内部的には`dept`と`COUNT(*)`の2列を持つ行が`Sort`まで流れているわけです。

### 隠し列を`Sort`の後で取り除く

隠し列は`SELECT`が宣言した出力の一部ではないため、利用者に返す結果には含めるわけにいきません。
`BoundSelect`は、`projection`の末尾何列が隠し列かを`hidden_column_count`というフィールドで持ち、`src/logical_plan.rs`の`LogicalPlan::build_select`はこれが`0`でなければ、`Limit`(またはそれより手前の最後の演算子)の上にもう1段`Projection`を積んで、先頭の可視列だけを残します。

```rust
if select.hidden_column_count == 0 {
    limited
} else {
    let extended_schema = limited.output_schema();
    let trim_projection = (0..visible_len)
        .map(|index| {
            let column = &extended_schema.columns()[index];
            BoundSelectItem {
                expr: BoundExpr::ColumnRef { table_ordinal: 0, column_index: index, name: column.name.clone(), data_type: column.data_type, span: select.span },
                output_name: column.name.clone(),
            }
        })
        .collect();
    LogicalPlan::Projection(ProjectionNode { input: Box::new(limited), projection: trim_projection })
}
```

新しい演算子を作らず、既存の`Projection`をもう1段重ねているだけです。
`SELECT name FROM users ORDER BY id`の計画は、次のようになります。

```text
Projection(name)
  └─ Sort(id ASC)
    └─ Projection(name, id)
      └─ SeqScan(users)
```

内側の`Projection(name, id)`が、`SELECT`が宣言した`name`に`id`を隠し列として加えた**拡張された射影**です。
`Sort`はこの2列の行を並べ替え、外側の`Projection(name)`が`id`を落として、利用者が実際に受け取る形へ戻します。
`EXPLAIN`にこの2段の`Projection`がそのまま現れるのは、意図を隠していないという意味で正直な表示です(実運用のRDBMSも、`EXPLAIN`に似た「表示専用の一時列」が現れることがあります)。

この隠し列の仕組みには1つだけ制約があります。
`DISTINCT`を伴う`SELECT`では、隠し列を使う`ORDER BY`を許しません。

```console
minidb> SELECT DISTINCT name FROM users ORDER BY id;
行1列39: 名前解決エラー: DISTINCTを伴うSELECTでは、ORDER BYはSELECTの対象式だけを参照できます
```

`DISTINCT`は、拡張された射影(`name`、`id`)の段階では別々だった複数の行を、`name`が同じというだけで1行にまとめてしまいます。
まとめられた後の1行が、まとめられる前のどの`id`を代表するかは定まりません。
`id`が`1`の行と`id`が`2`の行がどちらも`name = 'Alice'`だったとき、`DISTINCT`後の`Alice`という1行にどちらの`id`を残せばよいのか、決める根拠がないのです。
この制約はminidb独自の妥協ではなく、PostgreSQLが同じ状況に対して課している制約と同じです。
`DISTINCT`を伴わない`SELECT`(`Distinct`演算子を経由しない計画)には、この制約はかかりません。

### `LIMIT`、`OFFSET`は束縛の時点で評価する

`LIMIT`、`OFFSET`は、特定の行に依存しない定数式です。
標準SQLもこれを実行のたびに変わる値として扱いません。
`src/binder.rs`の`Binder::eval_row_count_expr`は、この式を空の(列を持たない)スコープで束縛し、その場で評価してしまいます。

```rust
fn eval_row_count_expr(&self, expr: &Expr, clause: &str) -> DbResult<usize> {
    let bound = self.bind_expr(expr, &[])?;
    // ...(BIGINTであることの検査)
    let value = eval_bound_expr(&bound, self.functions, None)?;
    match value {
        Value::BigInt(n) if n >= 0 => Ok(n as usize),
        Value::BigInt(n) => Err(/* 負の値は指定できない */),
        Value::Null => Err(/* NULLは指定できない */),
        _ => unreachable!(),
    }
}
```

`tables`を空にして束縛しているため、`LIMIT id`のような列参照は、構文としては書けても「列'id'が見つかりません」という束縛エラーになります。
評価をここで済ませておくことで、`LogicalPlan`、`PhysicalPlan`は`Option<usize>`という評価済みの値だけを持ち回ればよく、`Executor`が実行のたびに同じ定数式を再評価する理由がありません。

## LogicalPlanとPhysicalPlan: 評価順序をそのまま木にする

標準SQLが定める`SELECT`の論理的な評価順序は、次のとおりです。

```text
FROM → WHERE → GROUP BY → HAVING → SELECT(射影) → DISTINCT → ORDER BY → LIMIT/OFFSET
```

`LogicalPlan::build_select`は、この順序をそのまま演算子の木の深さとして組み立てます。

```text
Limit
  └─ Sort
    └─ Distinct
      └─ Projection
        └─ Filter(HAVING)
          └─ Aggregate
            └─ Filter(WHERE)
              └─ Scan
```

`HAVING`を独立した演算子にせず、`WHERE`と同じ`Filter`ノードを再利用しているのは、この2つが「行を絞り込む」という点で全く同じ演算だからです。
違うのは述語が評価する行の由来(`WHERE`は`Scan`が返す生の行、`HAVING`は`Aggregate`が返すグループごとの集約結果)だけであり、これは`FilterNode::input`が指す子が変わることで表現できます。

`ORDER BY`が`SELECT`の対象式に無い式を参照した場合(前節の隠し列)は、この木の根にもう1段`Projection`が積まれます。
`Projection`が2回現れる木そのものは新しい演算子の追加ではなく、「隠し列を含めて計算する射影」と「隠し列を落とす射影」という、同じ演算子の異なる用途での2回の適用にすぎません。

`GROUP BY`も集約関数も無い`SELECT`では`Aggregate`と`HAVING`のFilterを、`DISTINCT`、`ORDER BY`、`LIMIT`/`OFFSET`を伴わない`SELECT`では対応するノードを、そもそも積みません。
第18章までの実装が「`Filter`の後に必ず`Projection`が続く」という固定の2段構成だったのに対し、この章では`select`が持つ情報の有無に応じて木の深さそのものが変わります。

`PhysicalPlan::optimize`(第19章)は、`LogicalPlan`の`Aggregate`、`Distinct`、`Sort`、`Limit`をほぼ同じ形のまま`PhysicalPlan`へ変換します。
`Scan`が`SeqScan`という具体的な走査アルゴリズムへ変換されたのとは違い、この4つの演算子はこの章の時点で選択の余地がある実行アルゴリズムを持ちません(`Sort`のIn-memory Sortと外部ソートの選択、`Aggregate`のHash AggregateとSort-based Aggregateの選択は、どちらも第4部のコストベース最適化まで持ち越します)。
それでも`LogicalPlan`と型を分けたのは、第19章から一貫している「何を計算するかを決める層と、どう計算するかを決める層を分ける」という設計方針を崩さないためです。

## Executor: blockingとstreamingの対比

Volcanoモデルの`next()`は、呼ばれるたびにちょうど1行を返すという契約を持っていました(第19章)。
`FilterExec`、`ProjectionExec`はこの契約を、子から1行受け取ってはすぐ加工して返すことで実現していました。
この章の4つの演算子は、この契約こそ守りますが、契約を守るまでに子をどれだけ読むかという点で2つの型に分かれます。

**blocking演算子**は、子を`None`が返るまで読み切ってからでなければ、1行目すら返せません。
`ORDER BY`は最後の1行を読むまでどの行が先頭に来るか決まらず、集約は最後の1行を読むまでどのグループの`COUNT`がいくつになるか確定しません。
`SortExec`と`HashAggregateExec`がこれに当たります。

**streaming演算子**は、子から1行受け取るたびに、それを加工してすぐ返せます。
`LimitExec`は要求された件数を返し終えた時点で、子への問い合わせそのものを止めます。

### `HashAggregateExec`: グループごとの状態を`AggState`に持つ

`src/physical_plan.rs`の`HashAggregateExec`は、`GROUP BY`が計算するグループ化キー(`Vec<Value>`)をハッシュテーブルの鍵にして、行を1件読むたびに該当するグループの状態を更新します。
`HashAggregateExec`と、グループ1個ぶんの状態を持つ`AggState`を次のように定義します。

```rust
pub struct HashAggregateExec {
    schema: Schema,
    rows: std::vec::IntoIter<Tuple>,
}

struct AggState {
    count: i64,
    sum: Option<i64>,
    extreme: Option<Value>,
}

impl AggState {
    fn new() -> Self {
        AggState { count: 0, sum: None, extreme: None }
    }
}
```

```rust
pub fn new(
    mut input: Box<dyn Executor + '_>,
    group_by: &[BoundExpr],
    calls: &[AggregateCall],
    schema: Schema,
    functions: &FunctionRegistry,
) -> DbResult<Self> {
    let mut order: HashMap<Vec<Value>, usize> = HashMap::new();
    let mut groups: Vec<(Vec<Value>, Vec<AggState>)> = Vec::new();

    while let Some(tuple) = input.next()? {
        let row = Row::new(&input_schema, &tuple);
        let key: Vec<Value> = group_by.iter().map(|expr| eval_bound_expr(expr, functions, Some(&row))).collect::<DbResult<_>>()?;

        let index = *order.entry(key.clone()).or_insert_with(|| {
            groups.push((key, calls.iter().map(|_| AggState::new()).collect()));
            groups.len() - 1
        });

        for (call, state) in calls.iter().zip(groups[index].1.iter_mut()) {
            let value = match &call.arg {
                Some(arg) => Some(eval_bound_expr(arg, functions, Some(&row))?),
                None => None,
            };
            state.update(call.func, value.as_ref())?;
        }
    }
    // ...
}
```

`order`(鍵→`groups`の添字)と`groups`(状態そのもの)を分けて持っているのは、出力の順序を`HashMap`の走査順という非決定的なものに委ねないためです。
グループの並びは、そのグループの鍵が最初に現れた行の順序になります。
`ORDER BY`が無い集約結果の行の順序はSQLの意味論上未規定ですが、未規定だからといって実行のたびに変わってよい理由にはなりません。
`golden`テストや`differential`テストの出力を安定させるには、この順序をどこかで固定する必要があり、挿入順以外に恣意的でない基準がここには無いため、最初に現れた順を採用しています。

`Vec<Value>`をハッシュテーブルの鍵にするには、`Value`が`Hash`を実装している必要があります。
`src/types.rs`の`Value`は浮動小数点数のような、反射的でない等価性の問題を持つ型を1つも含まないため、`PartialEq`をそのまま`Eq`、`Hash`へ強めても安全です。

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    Null,
    Boolean(bool),
    BigInt(i64),
    Text(String),
}
```

`GROUP BY dept`のもとで`dept`が`NULL`の行が複数あれば、それらは同じグループとしてまとめられます。
`NULL`同士が等しいとみなされるのは、`Vec<Value>`という鍵の比較が`Value`の`derive(PartialEq)`(構造的な等価性)を使うためであり、SQLの三値論理(`NULL = NULL`は`UNKNOWN`)とは別の規則です。
`GROUP BY`が`NULL`同士を同じグループとして扱うのは標準SQLの規則であり、第20章の`UNIQUE`制約が`NULL`同士を別の値として扱った(重複とみなさなかった)のとは対照的です。
`UNIQUE`が守るのは「値が分かっていて、かつ重なっている」ことの禁止であるのに対し、`GROUP BY`はそもそも値が分かっているかどうかを問わず、値の見た目が同じ行を1つにまとめる演算だからです。

### 集約関数のNULL規則

`COUNT`、`SUM`、`MIN`、`MAX`が`NULL`をどう扱うかは、`src/physical_plan.rs`の`AggState`という1個のグループの状態を更新する関数に集約されています。

```rust
fn update(&mut self, func: AggregateFunc, value: Option<&Value>) -> DbResult<()> {
    match func {
        AggregateFunc::Count => {
            let counts = value.is_none_or(|v| !v.is_null());
            if counts {
                self.count += 1;
            }
        }
        AggregateFunc::Sum => {
            let Some(value) = value else { unreachable!(/* ... */) };
            if !value.is_null() {
                let Value::BigInt(n) = value else { unreachable!(/* ... */) };
                let base = self.sum.unwrap_or(0);
                self.sum = Some(base.checked_add(*n).ok_or_else(|| /* ... */)?);
            }
        }
        AggregateFunc::Min | AggregateFunc::Max => {
            // ...(NULLでなければcompare_valuesで現在の最小/最大と比較して更新)
        }
    }
    Ok(())
}
```

`value`が`None`なのは`COUNT(*)`のときだけです。
`COUNT(*)`は行の中身を一切見ないため、行が存在する限り必ず数えます。
`COUNT(expr)`は`expr`を評価した`value`が`NULL`でない行だけを数えます。
1つの`match`アームに2つの規則(`COUNT(*)`は無条件、`COUNT(expr)`は非`NULL`のみ)が同居しているように見えますが、これは`value.is_none_or(|v| !v.is_null())`という1つの式が両方を表しています。
`None`(`COUNT(*)`)なら`is_none_or`は無条件で`true`を返し、`Some(v)`(`COUNT(expr)`)なら`v`が`NULL`でないときだけ`true`を返します。

`SUM`、`MIN`、`MAX`は、`NULL`の行を単純に読み飛ばします。
`self.sum`、`self.extreme`が`None`のままなら、そのグループには非`NULL`の値が1件も無かったということであり、`src/physical_plan.rs`の`finish`はその最終結果を`NULL`として返します。

```rust
fn finish(&self, func: AggregateFunc) -> Value {
    match func {
        AggregateFunc::Count => Value::BigInt(self.count),
        AggregateFunc::Sum => self.sum.map(Value::BigInt).unwrap_or(Value::Null),
        AggregateFunc::Min | AggregateFunc::Max => self.extreme.clone().unwrap_or(Value::Null),
    }
}
```

まとめると、この章が実装するNULL規則は次のとおりです。

- **`COUNT(*)`**: `NULL`を含むすべての行を数える。空グループは`0`。
- **`COUNT(expr)`**: `expr`が`NULL`でない行だけを数える。空グループは`0`。
- **`SUM`、`MIN`、`MAX`**: `NULL`の行を無視する。対象行が1件も無い(全行が`NULL`だった、またはグループが空の)場合は`NULL`。

`COUNT`だけが「空でも`0`」で、他の3つが「空なら`NULL`」になる非対称性は、恣意的な選択ではありません。
`0`は「何も無かった」という事実をそのまま表せる値ですが、`SUM`、`MIN`、`MAX`には「対象が無いときの値」を表せる数値が存在しません(合計の無い集合の合計を`0`とみなすのは、加算という演算にとってはたまたま都合がよいだけの選択で、最小値の無い集合の最小値には対応する値がありません)。
標準SQLはこの非対称性をそのまま採用しており、この章もそれに従います。

`GROUP BY`が無い集約(`SELECT COUNT(*) FROM orders`)は、対象行が0件でもちょうど1行を返します。
`groups`が空で、かつ`group_by`も空だった場合に、`src/physical_plan.rs`が初期状態のままの`AggState`を持つグループを1つだけ次のように合成しているのはこのためです。

```rust
if groups.is_empty() && group_by.is_empty() {
    groups.push((Vec::new(), calls.iter().map(|_| AggState::new()).collect()));
}
```

`GROUP BY dept`のように`group_by`が空でない場合はこの合成を行いません。
対象行が0件なら、そもそもグループ化する対象そのものが存在しないため、0行を返します。
「集約結果がちょうど1行になるかどうか」は、`GROUP BY`の有無だけで決まり、集約関数の種類には関係しません。

### `SortExec`: In-memory SortとNULLの順序

`SortExec`も`HashAggregateExec`と同じ理由でblocking演算子です。
`src/physical_plan.rs`の`SortExec`は、子を`None`まで読み切って`Vec<Tuple>`へ溜め、ソートキーを1回ずつ評価してから、`Vec::sort_by`で次のように並べ替えます。

```rust
let mut keyed: Vec<(Vec<Value>, Tuple)> = Vec::new();
while let Some(tuple) = input.next()? {
    let row = Row::new(&schema, &tuple);
    let key: Vec<Value> = keys.iter().map(|k| eval_bound_expr(&k.expr, functions, Some(&row))).collect::<DbResult<_>>()?;
    keyed.push((key, tuple));
}

keyed.sort_by(|(a, _), (b, _)| {
    for (i, key) in keys.iter().enumerate() {
        let ordering = compare_values(&a[i], &b[i]);
        let ordering = if key.desc { ordering.reverse() } else { ordering };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
});
```

ソートキーを行ごとに1回だけ評価してから比較しているのは、`sort_by`の比較関数が`O(n log n)`回呼ばれる間、同じ式を毎回`eval_bound_expr`で評価し直すコストを避けるためです。

`NULL`をどう並べるかは、比較演算(`=`、`<`等)の三値論理とは別に決める必要があります。
`WHERE`の三値論理は、`NULL`が絡む比較を常に`UNKNOWN`(比較不能)にしますが、`ORDER BY`は`NULL`を含む列に対しても行の並び順を一意に決めなければなりません。
この章は`compare_values`という、`NULL`をどの値よりも小さいとみなす全順序を`src/types.rs`に新しく定義し、`SortExec`と`HashAggregateExec`の`MIN`/`MAX`の両方がこれを使います。

```rust
pub fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Boolean(x), Value::Boolean(y)) => x.cmp(y),
        _ => unreachable!(/* Binderが式の型を静的に確定させている */),
    }
}
```

`ASC`はこの順序をそのまま、`DESC`は反転させて使います。
結果として、`NULL`は`ASC`では先頭に、`DESC`では末尾に来ます。
この規則はminidbが独自に定めたものではなく、SQLiteの既定の並び順と一致させてあります。
`differential`テスト(次節)で、`ORDER BY`を伴う`SELECT`をSQLiteと突き合わせられるのはこの一致のおかげです。

安定性(同じキーを持つ行同士の相対順序)も、この章が明示的に選んだ性質です。
`Vec::sort_by`は安定ソートであり、`ORDER BY dept`だけを指定して`amount`について何も指定しなかった場合、同じ`dept`を持つ行同士は、`Sort`に入力される前の順序(この章の実装では`Scan`が返す挿入順)のまま保たれます。

外部ソート(メモリに載り切らないほど大きい入力を、一時ファイルへのスピルを挟んで並べ替える手法)は、この章では実装しません。
教材が扱う規模のテストデータは、まずメモリに収まるという前提を崩さない限り、In-memory Sortで十分だからです。
`ORDER BY`に`LIMIT`が続く場合、理論的には全件を並べ替えずに`BinaryHeap`で上位`N`件だけを追跡する最適化(Top-N)が可能ですが、この章の`SortExec`はその最適化を行わず、常に全件を並べ替えてから`LimitExec`に渡します。
`Sort`と`Limit`を独立した演算子のまま保ち、両者をまたぐ最適化を`PhysicalPlan::optimize`に持ち込まなかったのは、この章の時点ではまだルールベースの最適化(第26章)もコストベースの最適化(第28章)も無く、「特定の2演算子の組み合わせを検出して書き換える」という最適化を置く場所が定まっていないためです。
この最適化自体は章末の演習課題で実装します。

### `DistinctExec`: streamingとblockingの中間

`src/physical_plan.rs`の`DistinctExec`は、`HashAggregateExec`、`SortExec`のようにすべての行を読み切ってから1行目を返すわけではありません。

```rust
fn next(&mut self) -> DbResult<Option<Tuple>> {
    loop {
        let Some(tuple) = self.input.next()? else {
            return Ok(None);
        };
        if self.seen.insert(tuple.values().to_vec()) {
            return Ok(Some(tuple));
        }
    }
}
```

子から引いた行が、すでに返した行の集合(`seen`)に無ければ、その場でそのまま返します。
`FilterExec`(第19章)と同じ「一致するまで子を引く」という形の`next()`であり、1行も返さないうちに全件読み切る必要はありません。

それでも`DistinctExec`は`FilterExec`ほど単純ではありません。
`FilterExec`は一致しなかった行を評価し終えた瞬間に忘れますが、`DistinctExec`の`seen`は、これまでに返したすべての行の値を保持し続けます。
`next()`を呼ぶたびに、その時点までに返した行数に比例するメモリを使い続けるという点で、`DistinctExec`のメモリ使用量は`Filter`のようには有界になりません。
「最初の行を返すまでに全件読み切るかどうか」というblocking/streamingの分類軸だけでは、`DistinctExec`の性質を言い切れません。

`DISTINCT`も`NULL`同士を同じ値とみなします。
`seen`が`Vec<Value>`(1行分の値の並び)の`HashSet`であり、`Value`の構造的な等価性で重複を判定するためです。
`GROUP BY`のときと同じ理由(`Vec<Value>`の比較は三値論理を経由しない)で、`SELECT DISTINCT amount FROM orders`に`amount`が`NULL`の行が複数あっても、結果には`NULL`が1行だけ残ります。

### `LimitExec`: 子を打ち切るstreaming演算子

`src/physical_plan.rs`の`LimitExec`(この章で唯一の純粋なstreaming演算子)の`next()`は、次のようになります。

```rust
fn next(&mut self) -> DbResult<Option<Tuple>> {
    if self.remaining_limit == Some(0) {
        return Ok(None);
    }
    while self.remaining_offset > 0 {
        self.remaining_offset -= 1;
        if self.input.next()?.is_none() {
            return Ok(None);
        }
    }
    let Some(tuple) = self.input.next()? else {
        return Ok(None);
    };
    if let Some(remaining) = &mut self.remaining_limit {
        *remaining -= 1;
    }
    Ok(Some(tuple))
}
```

冒頭の`if self.remaining_limit == Some(0)`が、この演算子の要点です。
要求された件数を返し終えた時点で`remaining_limit`が`0`になり、以後の`next()`呼び出しは子の`next()`を1回も呼ばずに`None`を返します。
`SELECT * FROM orders LIMIT 1`のように`Sort`を伴わない`SELECT`では、`SeqScan`が2行目以降を1行も読まずに済みます(章末の演習課題で、この違いを実際に確認します)。
`ORDER BY`を伴う場合は、`Sort`が全件を読み切ってから並べ替える必要があるため、`Limit`が子を早期に打ち切れる相手は「並べ替え済みの結果」であり、`SELECT`全体としてはやはり全件を読むblockingな計画のままです。

## テストで確認する

各演算子の単体テストは`src/physical_plan.rs`の`#[cfg(test)]`モジュールに置き、`orders`(`dept TEXT`、`amount BIGINT`、どちらも`NULL`を許す)という専用のテーブル定義を使っています。
第19章までの`users`テーブルは`id`が`NOT NULL`のため、`SUM`、`MIN`、`MAX`の`NULL`規則を確かめるテストが書けず、次のテストを追加しています。

```rust
#[test]
fn sum_of_empty_group_is_null_but_count_is_zero() {
    let select = bind_select_orders("SELECT COUNT(*), SUM(amount), MIN(amount), MAX(amount) FROM orders");
    let aggregate = select.aggregate.unwrap();
    let mut exec = HashAggregateExec::new(exec_over_rows(Vec::new()), &aggregate.group_by, &aggregate.calls, aggregate.schema.clone(), &functions).unwrap();
    let result = collect_all(&mut exec);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].values(), &[Value::BigInt(0), Value::Null, Value::Null, Value::Null]);
}
```

`Sort`の安定性、`NULL`の順序、`Distinct`が子を最小限しか引かないこと、`Limit`が要求件数を返した後は子を1回も引かないことは、それぞれ`CountingExecutor`(第19章で導入した、`next()`の呼び出し回数を数えるテスト専用の葉演算子)を使って確認しています。

`src/binder.rs`の`#[cfg(test)]`モジュールに置く`Binder`のテストは、`WHERE`、`GROUP BY`の中の集約関数、入れ子の集約関数、`GROUP BY`に無い列の射影、`DISTINCT`と隠し列の組み合わせといった、この章で新しく導入した検査それぞれについて、期待どおり`DbError::Bind`になることを確認します。
`ORDER BY`が隠し列を追加する側のテストは、逆にエラーにならないことと、`hidden_column_count`が期待どおりの値になることを、次のように確認します。

```rust
#[test]
fn order_by_referencing_a_column_outside_the_select_list_adds_a_hidden_column() {
    let select = bind_select_orders("SELECT dept FROM orders ORDER BY amount");
    assert_eq!(select.hidden_column_count, 1);
    assert_eq!(select.projection.len(), 2);
    assert_eq!(select.projection[1].output_name, "amount");
}
```

`tests/differential.rs`の`differential`テストは、`ORDER BY`の追加にあわせて比較のしかたを見直します。
第20章まで、`SELECT`の行の順序はSQLの意味論上未規定であるという理由から、`differential`テストは常に両エンジンの結果をソートしてから比較していました。
`ORDER BY`が構文解析器を通るようになった今、`ORDER BY`を伴う`SELECT`については、並べ替え自体が検証したい意味論の一部です。
`assert_same_result_ordered`という、ソートせずに順序ごと突き合わせる比較を新設し、`ORDER BY`を持たないテストは引き続き`assert_same_result`(ソートしてから比較)を使う形で使い分け、`tests/differential.rs`に次のようなテストを追加しています。

```rust
#[test]
fn order_by_descending_matches_sqlites_null_last_order() {
    assert_same_result_ordered(
        &["CREATE TABLE t (x BIGINT)", "INSERT INTO t VALUES (3), (1), (NULL), (2)"],
        "SELECT x FROM t ORDER BY x DESC",
    );
}
```

このテストが緑になることは、`compare_values`が定めた`NULL`の順序がSQLiteの既定の並び順と実際に一致していることの、実装をまたいだ裏付けになっています。

```console
$ cargo test
test result: ok. 439 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.56s
...
test result: ok. 28 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

## 演習問題

### 必須課題

1. `SELECT * FROM orders LIMIT 1`(`ORDER BY`を伴わない)を`CountingExecutor`を使って実行し、`SeqScan`が実際に1行しか読まないことを確認するテストを書いてください。`ORDER BY`を追加した`SELECT * FROM orders ORDER BY amount LIMIT 1`と読み込み行数を比較し、`Sort`の有無がこの件数をどう変えるか説明してください。
2. `HAVING`が無く`GROUP BY`だけを持つ`SELECT`(`SELECT dept FROM orders GROUP BY dept`)で、`dept`が`NULL`の行が複数あるとき、結果に`NULL`が何行現れるか予想してから、実際に`db.execute`で確認してください。予想と一致しなければ、`HashAggregateExec`のどの部分がその挙動を決めているかを`group_by`というフィールドの型(`Vec<Value>`)に触れながら説明してください。
3. `AggregateCall`に`AVG`(平均)を追加してください。`SUM`と`COUNT`の両方を1グループにつき保持する必要があるか、`AggState`に既存の`sum`、`count`フィールドを再利用できるかを検討し、空グループの`AVG`が`NULL`になることを確認するテストを書いてください。
4. `SELECT id, name FROM users ORDER BY id + 1`のように、`ORDER BY`の式が`SELECT`のどの対象式とも構造的に一致しない場合を`EXPLAIN`で確認してください。`Projection`が何列を持つ木になるか予想してから実行し、`id + 1`という式が`fmt_bound_expr`でどんな文字列になるかを`SELECT id + 1 FROM users`という別の文と比べ、両者が一致しない(=別々の隠し列になりうる)理由を説明してください。

### 発展課題

1. `ORDER BY`に`LIMIT`が続く場合の、`BinaryHeap`を使ったTop-N最適化を実装してください。`SortExec`とは別の`TopNExec`という演算子を新設し、`PhysicalPlan::optimize`に「`Sort`の親が`Limit`なら`TopNExec`へ変換する」というルールを追加します。全件を並べ替える`SortExec`と比べて、`LIMIT`が小さく入力行数が大きいときにどれだけ速くなるか、`std::time::Instant`で測定してください。
2. `SELECT <expr> AS <alias>`という列の別名付けを実装してください。`ORDER BY COUNT(*) DESC`のように式をもう一度書かなくても、`ORDER BY cnt DESC`のように短い別名で同じ列を参照できるようにするのが目的です。`SelectItem`に別名のフィールドを追加し、`Binder::bind_select`の`output_name`の決め方(現在は式の原文をそのまま使っている)をどう変えるべきか設計したうえで、`SELECT dept, COUNT(*) AS cnt FROM orders GROUP BY dept ORDER BY cnt DESC`が、新しい隠し列を1つも作らずに(`cnt`が`SELECT`の対象式そのものと`fmt_bound_expr`上一致するように)動くところまで実装してください。
3. `HashAggregateExec`は集約結果をすべてメモリ上の`Vec`に保持します。グループ数が非常に多い(たとえば数百万件の一意な`dept`がある)場合、この`Vec`はテーブル本体と同程度のメモリを消費します。この章の`Storage`(第15章)を使って、集約結果を一時的なヒープテーブルへspillする案を検討し、少なくとも設計(どの時点でメモリからディスクへ切り替えるか、`AggState`をどうシリアライズするか)をドキュメントとして書き下ろしてください。
