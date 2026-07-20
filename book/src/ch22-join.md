# 第22章 Joinアルゴリズム

`customers`と`orders`という2つのテーブルがあり、「各顧客の注文一覧」を知りたいとします。
前章までのminidbに、これをそのまま尋ねられるでしょうか。

```console
minidb> SELECT customers.name, orders.item FROM customers, orders WHERE customers.id = orders.customer_id;
行1列45: 構文エラー: 文の終端が必要です: Comma(区切り記号)が見つかりました
```

`FROM`に書けるテーブルは1つだけです。
`customers`の情報と`orders`の情報を1つの結果に並べたければ、`SELECT * FROM customers`と`SELECT * FROM orders`をそれぞれ実行し、`id`と`customer_id`が一致する組み合わせをアプリケーション側のループで探すしかありません。
テーブルが2つあるだけで、SQLの外に出て手で付き合わせる作業が発生します。

## 前章の限界

`FROM`に複数のテーブルを書けないという制約は、構文だけの話ではありません。
[第17章](./ch17-binder.md)の`Binder`はすでに`BoundSelect::tables`を`Vec<BoundTableRef>`として設計してあり、曖昧な列参照を検出する`resolve_column`も複数テーブルを前提に書いてありました。
それでも第17章から第21章までのどの章でも、この`Vec`の要素数は0か1にしかなりません。
`Parser`が`FROM`に1つのテーブルしか許していないからです。

`SELECT`が返す1行を、1つのテーブルの1行としてしか表現できない設計は、`BoundExpr::ColumnRef`にも表れています。
列参照は`table_ordinal`(どのテーブルか)と`column_index`(そのテーブルの中の何列目か)という2つの座標を持っていましたが、`eval::eval_bound_expr`が受け取る`row`は常に1つの`Row`(1つのテーブルの1行)であり、`table_ordinal`は実行時には一度も参照されていませんでした。
複数のテーブルの行を同時に扱う演算子が、まだこのクレートに存在しなかったからです。

この章では、`FROM`に複数のテーブルを`JOIN`で連ねる構文を追加し、Binder、Logical Plan、Physical Plan、Executorのすべてにその結合を実装します。
実装するのはInner Joinに限り、結合条件は`ON`に書く形だけを受け付けます。
`LEFT OUTER JOIN`のような他の結合種別は、章末の演習課題に譲ります。

## 構文を追加する

追加する構文は次のとおりです。

```console
minidb> SELECT customers.name, orders.item FROM customers INNER JOIN orders ON customers.id = orders.customer_id;
minidb> SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id;
```

`INNER JOIN`と`JOIN`単独はどちらも同じ意味です。
標準SQLも`JOIN`だけを書いた場合は`INNER JOIN`とみなす規則を定めており、Lexerに`Inner`、`Join`、`On`という3つの予約語を追加したうえで、この2つの書き方を`Parser`の時点で1つの`JoinKind::Inner`へ統一してしまいます。
この列挙は`src/ast.rs`に追加します。

```rust
pub enum JoinKind {
    Inner,
}
```

`JoinKind`を`enum`にしたのは、選択肢が1つしか無い今の時点で意味を持つ設計ではありません。
`LEFT OUTER JOIN`を演習課題で追加する読者が、このバリアントを1つ増やすだけで済むようにするための先取りです。

`FromClause`は、最初の1テーブルに続く`JOIN`の並びを`joins`として持つように広がります。

```rust
pub struct FromClause {
    pub table: Ident,
    pub alias: Option<Ident>,
    pub joins: Vec<JoinClause>,
    pub span: Span,
}

pub struct JoinClause {
    pub kind: JoinKind,
    pub table: Ident,
    pub alias: Option<Ident>,
    pub on: Expr,
    pub span: Span,
}
```

`src/parser.rs`の`Parser::parse_select_statement`は、`FROM`の最初のテーブルを読み終えた直後、`INNER`または`JOIN`が続く限り`parse_join_clause`を呼び続けます。

```rust
fn parse_join_clause(&mut self) -> DbResult<Option<JoinClause>> {
    let start = match self.peek_kind() {
        TokenKind::Keyword(Keyword::Join) => self.peek().span.start,
        TokenKind::Keyword(Keyword::Inner) => self.peek().span.start,
        _ => return Ok(None),
    };

    if let TokenKind::Keyword(Keyword::Inner) = self.peek_kind() {
        self.advance();
        self.expect_keyword(Keyword::Join, "JOIN")?;
    } else {
        self.advance();
    }

    let table = self.expect_ident()?;
    let alias = if let TokenKind::Keyword(Keyword::As) = self.peek_kind() {
        self.advance();
        Some(self.expect_ident()?)
    } else {
        None
    };
    self.expect_keyword(Keyword::On, "ON")?;
    let on = self.parse_expr(0)?;
    let end = on.span().end;

    Ok(Some(JoinClause {
        kind: JoinKind::Inner,
        table,
        alias,
        on,
        span: Span::new(start, end),
    }))
}
```

`JOIN`に続くトークンが`INNER`でも`JOIN`でもなければ`None`を返し、呼び出し元の`while let Some(join) = self.parse_join_clause()?`ループがそこで終わります。
`FROM a JOIN b ON a.id = b.id JOIN c ON b.id = c.id`のように、`ON`の直後にまた`JOIN`が続く形は、このループがそのまま繰り返すだけで対応できます。

`FROM a, b`というカンマ区切りの複数テーブル(カンマ結合)は、この章では構文として受理しません。
カンマ結合は結合条件を持たない直積(Cross Join)であり、`WHERE`に書かれた等価条件を後から見つけ出して結合条件へ昇格させる変換(Cross Join + FilterからEqui-Joinへの変換)が必要になります。
この変換はルールベースの書き換えであり、第26章以降のOptimizerが持つべき仕事です。
この章はまだOptimizerを持たないため、結合条件を`ON`に明示させる`JOIN`構文だけをサポートし、カンマ結合への対応は章末の演習課題に回します。

## Binder: 複数テーブルスコープでの列解決

### `tables`が複数要素になる

`src/binder.rs`の`Binder::bind_from`は、`FROM`の最初のテーブルを`resolve_table`で解決したあと、`joins`を先頭から順に処理します。

```rust
fn bind_from(&self, from: Option<&FromClause>) -> DbResult<(Vec<BoundTableRef>, Vec<BoundJoinStep>)> {
    let Some(from) = from else {
        return Ok((Vec::new(), Vec::new()));
    };

    let mut tables = vec![self.resolve_table(&from.table, from.alias.as_ref())?];
    let mut joins = Vec::with_capacity(from.joins.len());
    for join in &from.joins {
        let right = self.resolve_table(&join.table, join.alias.as_ref())?;
        tables.push(right);
        let condition = self.bind_predicate(&join.on, &tables, "ON")?;
        if bound_contains_aggregate(&condition) {
            return Err(self.error_at(
                condition.span(),
                "集約関数はON句では使えません(集約はJOINの後に計算されます)".to_string(),
            ));
        }
        joins.push(BoundJoinStep { kind: join.kind, condition });
    }
    Ok((tables, joins))
}
```

要点は、`ON`条件を束縛する`self.bind_predicate(&join.on, &tables, "ON")`の`tables`が、その時点までに`push`済みの全テーブルだということです。
`tables`に新しいテーブルを`push`してから`ON`を束縛しているため、`FROM a JOIN b ON a.id = b.id JOIN c ON a.id = c.id`のように、3番目の`JOIN`の条件が直前の`b`を飛び越えて最初の`a`を参照する書き方も、`tables`のスコープにすでに`a`、`b`、`c`が並んでいることでそのまま解決できます。

[第17章](./ch17-binder.md)で`BoundSelect::tables`を`Vec<BoundTableRef>`にしておいたのは、この章で要素数が2つ以上になる日のためでした。
`resolve_column`の曖昧列検出(2件以上マッチしたらエラーにする分岐)も、[第17章](./ch17-binder.md)の時点ですでに書いてありながら、`Parser`が複数テーブルの`FROM`を受け付けなかったせいで一度も実行されたことがありませんでした。
この章から、`FROM customers JOIN orders ON customers.id = orders.customer_id`に対して`SELECT id`と書けば、この分岐が実際に働きます。

```console
minidb> SELECT id FROM customers JOIN orders ON customers.id = orders.customer_id;
エラー: 行1列8: 名前解決エラー: 列'id'は複数のテーブルに存在するため曖昧です: customers, orders
```

### 結合後スキーマとフラットな`column_index`

`BoundExpr::ColumnRef`は`table_ordinal`と`column_index`という2つの座標を持っていましたが、複数テーブルの行を1個の`Tuple`として評価する`eval_bound_expr`(後述)は、`Row`を1個しか受け取りません。
`table_ordinal`でテーブルを選んでから`column_index`でそのテーブル内の位置を引く、という2段階の解決を実行時に行う代わりに、この章では`column_index`そのものを**結合後スキーマ**(`tables`を左から右へ連結した列の並び)上のフラットな添字にします。

```console
customers: id, name        (2列、offset 0)
orders:    customer_id, item  (2列、offset 2)
```

`orders.item`の`column_index`は、`orders`の中ではローカルに1番目の列ですが、結合後スキーマでは`2 + 1 = 3`番目の列になります。
この計算を担うのが`table_offset`です。

```rust
fn table_offset(tables: &[BoundTableRef], table_ordinal: usize) -> usize {
    tables[..table_ordinal].iter().map(|table| table.schema.len()).sum()
}
```

`resolve_column`は、修飾子の有無どちらのどちらの経路でも、見つけた列のローカルな添字にこのオフセットを足してから`BoundExpr::ColumnRef`を組み立てます。

```rust
let local_index = table
    .schema
    .index_of(name)
    .ok_or_else(|| self.error_at(span, format!("列'{name}'は'{}'に存在しません", qualifier.name)))?;
let data_type = table.schema.columns()[local_index].data_type;
let column_index = table_offset(tables, table_ordinal) + local_index;
```

`table_ordinal`が`0`の唯一のテーブルしか無い(`JOIN`を持たない)`SELECT`では、`table_offset`は常に`0`を返すため、`column_index`はこれまでどおりテーブル内のローカルな添字と一致します。
この章の変更は、単一テーブルの`SELECT`が経由するコード経路を1つも書き換えていません。
`SELECT *`の展開(`bind_select`のWildcard分岐)も同じ理由でオフセットを足すように直しましたが、結果として複数テーブルの`*`は「テーブルの登場順、各テーブル内は列の宣言順」に展開されます。

```console
minidb> SELECT * FROM customers JOIN orders ON customers.id = orders.customer_id;
id | name | customer_id | item
-----------------------------
...
```

同名の列(この例では両テーブルとも`id`のような名前を持つ場合)が結合後スキーマに並んでも、`Binder`はそれ自体をエラーにしません。
結合後スキーマの列名が重複していても、後段(`Filter`、`Projection`)は名前ではなく`column_index`という数値でしか列を参照しないため、実害がないからです。
利用者が最終的に受け取る列名は、常に`SELECT`の対象式(`projection`)の`output_name`から決まり、結合後スキーマの列名を直接見ることはありません。
重複した列名が問題になるのは、`SELECT id FROM customers JOIN orders ON ...`のように、利用者自身が修飾子を付けずに曖昧な参照を書いた場合だけです。

`src/eval.rs`の`eval::eval_bound_expr`は、この章から`table_ordinal`を実行時には一切見ません。

```rust
BoundExpr::ColumnRef { column_index, name, .. } => {
    match row {
        Some(row) => row
            .get_index(*column_index)
            .cloned()
            .ok_or_else(|| DbError::Eval(format!("列'{name}'が見つかりません"))),
        None => Err(DbError::Eval(format!("列参照'{name}'は行を伴わない文脈では使えません"))),
    }
}
```

`row`は、後述する`Join`演算子が左右のタプルを連結して作った1個の`Tuple`を指しています。
複数テーブルの行を同時に扱うといっても、評価器の側から見れば「連結済みの1個の行を、単一テーブルのときと同じ`get_index`で読む」というだけの変更で済んでいます。
`table_ordinal`は、エラーメッセージや`EXPLAIN`表示のための付随情報として型には残してありますが、値そのものの計算にはもう関与しません。

## Logical Plan: Joinノードと左深い木

`LogicalPlan`に`Join`という新しい演算子を追加します。
`src/logical_plan.rs`に次の`JoinNode`を定義します。

```rust
pub struct JoinNode {
    pub left: Box<LogicalPlan>,
    pub right: Box<LogicalPlan>,
    pub kind: JoinKind,
    pub condition: BoundExpr,
}
```

`n`個の`JOIN`を持つ`FROM`は、`n`個の`Join`ノードが縦に連なる**左深い木**(left-deep tree)になります。

```rust
fn build_from(tables: Vec<crate::binder::BoundTableRef>, joins: Vec<crate::binder::BoundJoinStep>) -> LogicalPlan {
    let mut tables = tables.into_iter();
    let Some(first) = tables.next() else {
        return LogicalPlan::Values(ValuesNode { schema: Schema::new(Vec::new()), rows: vec![Vec::new()] });
    };

    let mut plan = LogicalPlan::Scan(ScanNode { table_id: first.table_id, table_name: first.table_name, schema: first.schema });
    for (table, join) in tables.zip(joins) {
        let right = LogicalPlan::Scan(ScanNode { table_id: table.table_id, table_name: table.table_name, schema: table.schema });
        plan = LogicalPlan::Join(JoinNode {
            left: Box::new(plan),
            right: Box::new(right),
            kind: join.kind,
            condition: join.condition,
        });
    }
    plan
}
```

`FROM a JOIN b ON ... JOIN c ON ...`は、`((a JOIN b) JOIN c)`という木になります。
`a`と`b`をまず結合し、その結果を新しい「左」として`c`をさらに結合するという形です。
`a`、`b`、`c`をすべて同時に結合する演算子(3項以上のJoin)を作らなかったのは、2つの入力を結合するという演算子の形をどれだけ`JOIN`が連なっても変えずに済むからです。
Joinの結合順序(`a`と`b`を先に結合するか、`b`と`c`を先に結合するか)を選ぶ余地は、この章にはまだありません。
第29章のJoin Orderが、この左深い木の組み方そのものを最適化の対象にします。

左深い木のもとで、`table_offset`が計算した結合後スキーマ上のフラットな`column_index`がそのまま正しく機能する理由を確認しておきます。
`(a JOIN b)`という部分木の出力スキーマは`a`の列に`b`の列を連結したものであり、これは`Binder`が計算したグローバルなオフセット(`a`の列がoffset 0から、`b`の列がoffset `a.len()`から)と一致します。
続けて`c`を結合するとき、この部分木の出力列数はちょうど`a.len() + b.len()`であり、これは`Binder`が`c`の列に割り当てたオフセットと一致します。
木の深さに関係なく、左部分木の出力列数が常に「次に結合するテーブルのグローバルなオフセット」と一致し続けるのは、テーブルが左から右へ登場した順序のまま木を組み立てているからです。

`Join`が生成する行の`Schema`は、左右の`Schema`を単純に連結するだけの関数`join_schema`で決めます。

```rust
pub fn join_schema(left: &Schema, right: &Schema) -> Schema {
    let mut columns = Vec::with_capacity(left.len() + right.len());
    columns.extend(left.columns().iter().cloned());
    columns.extend(right.columns().iter().cloned());
    Schema::new(columns)
}
```

`EXPLAIN`で`LogicalPlan`を表示すると、`Join`ノードは2つの子を持つ形で現れます。

```text
Projection(customers.name, orders.item)
  └─ Join(INNER JOIN, id = customer_id)
    └─ Scan(customers)
    └─ Scan(orders)
```

## Physical Plan: 等値ならHash Join、そうでなければNested Loop Join

`Join`をどう実行するかには、大きく2つのアルゴリズムがあります。

**Nested Loop Join**は、左の行1件ごとに右の全行を突き合わせ、`ON`条件に一致した組み合わせだけを結果として返します。
`ON`が`customers.id <> orders.customer_id`のような任意の条件でも動く、この章の基準実装です。
左の行数を`n`、右の行数を`m`とすると、比較の回数は`n × m`に比例します。

**Hash Join**は、右の行をあらかじめ結合キーでハッシュテーブルに積んでおき(Build)、左の行を1件ずつ引いては同じキーの行をハッシュテーブルから探します(Probe)。
`ON`が等値条件でなければ、そもそも「同じキー」という概念が無いためこの手法は使えませんが、使える場面ではBuildに`m`、Probeに`n`というほぼ線形の手間で済み、比較の回数は`n + m`に比例します。

この章の物理選択は、この1点だけを見る単純なルールです。
`src/physical_plan.rs`の`optimize`に、次の分岐を追加します。

```rust
LogicalPlan::Join(join) => {
    let left = optimize(*join.left);
    let right = optimize(*join.right);
    let left_len = left.output_schema().len();
    match split_equi_join_keys(&join.condition, left_len) {
        Some(keys) => {
            let keys = keys
                .into_iter()
                .map(|(left_key, right_key)| (left_key, shift_column_index(&right_key, left_len)))
                .collect();
            PhysicalPlan::HashJoin(HashJoinNode {
                left: Box::new(left),
                right: Box::new(right),
                kind: join.kind,
                keys,
                condition: join.condition,
            })
        }
        None => PhysicalPlan::NestedLoopJoin(NestedLoopJoinNode {
            left: Box::new(left),
            right: Box::new(right),
            kind: join.kind,
            condition: join.condition,
        }),
    }
}
```

`split_equi_join_keys`が鍵の対を取り出せれば`HashJoin`、取り出せなければ`NestedLoopJoin`という、選択肢が2つしか無い分岐です。
どちらが速いかを実測やコスト推定と比較して選ぶわけではありません。
統計情報(第27章)とコストモデル(第28章)が揃うまでは、「等値条件かどうか」という構文的な性質だけが、このクレートが持つ唯一の判断材料だからです。

### 等値条件をハッシュキーへ分解する

`split_equi_join_keys`は、`ON`条件を`AND`で分解し、それぞれの項が「左側だけを参照する式 = 右側だけを参照する式」という形になっているかを調べます。

```rust
fn split_equi_join_keys(condition: &BoundExpr, left_len: usize) -> Option<Vec<(BoundExpr, BoundExpr)>> {
    let mut conjuncts = Vec::new();
    collect_conjuncts(condition, &mut conjuncts);

    let mut keys = Vec::with_capacity(conjuncts.len());
    for conjunct in conjuncts {
        let BoundExpr::BinaryOp { op: BinaryOperator::Eq, lhs, rhs, .. } = strip_paren(conjunct) else {
            return None;
        };
        let (left_key, right_key) = match (columns_side(lhs, left_len), columns_side(rhs, left_len)) {
            (Some(Side::Left), Some(Side::Right)) => (lhs.as_ref().clone(), rhs.as_ref().clone()),
            (Some(Side::Right), Some(Side::Left)) => (rhs.as_ref().clone(), lhs.as_ref().clone()),
            _ => return None,
        };
        keys.push((left_key, right_key));
    }
    if keys.is_empty() { None } else { Some(keys) }
}
```

`a.x = b.x AND a.y = b.y`のように複数の等値条件が`AND`で連なっていれば、`keys`は2組の鍵を持つ`Vec`になります。
Hash Joinの鍵が`Vec<Value>`という複数列の組み合わせに対応できるのは、この分解が1個の式ではなく`Vec`を返す設計になっているためです。

`columns_side`は、式の中の列参照がすべて左側(`column_index < left_len`)か、すべて右側(`column_index >= left_len`)かを判定します。
`OR`で結ばれた条件、`=`以外の比較演算子、左右の列を1つの式の中で混ぜた項(`a.x + b.x = 3`のような)は、この分解の対象外として`None`を返し、`Join`全体がNested Loop Joinへ倒れます。
定数だけの項(`a.x = 5`)も、列参照を1つも持たないため`columns_side`は`None`を返します。
このような項は本来`ON`ではなく`WHERE`(あるいはFilterへ押し下げる最適化)に書くべき条件であり、Hash Joinの鍵と残差条件を組み合わせる最適化は、この章では行いません(章末の演習課題)。

Hash Joinの`right_key`には、もう1つ変換が必要です。
`condition`の`column_index`は結合後スキーマ上のフラットな添字ですが、Build段階は`right`単体の`Executor`が返す行(`left`をまだ連結していない、`right`自身のスキーマを持つ行)に対して鍵を評価しなければなりません。
`shift_column_index`が、この添字を`left_len`だけ引き戻します。

```rust
fn shift_column_index(expr: &BoundExpr, delta: usize) -> BoundExpr {
    match expr {
        BoundExpr::ColumnRef { table_ordinal, column_index, name, data_type, span } => BoundExpr::ColumnRef {
            table_ordinal: *table_ordinal,
            column_index: column_index - delta,
            name: name.clone(),
            data_type: *data_type,
            span: *span,
        },
        // ...(他のバリアントは子を再帰的に変換する)
    }
}
```

`left_key`のほうは変換が要りません。
左深い木の性質(前節)により、`left`部分木の出力はすでに結合後スキーマの左半分そのものであり、`condition`の`column_index`がそのまま`left`の`Executor`が返す行に対して正しく機能するからです。

## Executor: NestedLoopJoinExec

`Executor` trait(第19章)の`next()`は、子の`Executor`を「1回だけ、前から順に」読み進めるという契約を持っていました。
教科書的なNested Loop Joinは、左の行1件ごとに右の子計画を最初から実行し直します(rescan)が、`Box<dyn Executor>`というTrait Objectには、この「巻き戻し」を行う手段がありません。

```rust
pub struct NestedLoopJoinExec<'a> {
    left: Box<dyn Executor + 'a>,
    right_rows: Vec<Tuple>,
    condition: &'a BoundExpr,
    functions: &'a FunctionRegistry,
    schema: Schema,
    current_left: Option<Tuple>,
    right_index: usize,
}

impl<'a> NestedLoopJoinExec<'a> {
    pub fn new(
        left: Box<dyn Executor + 'a>,
        mut right: Box<dyn Executor + 'a>,
        condition: &'a BoundExpr,
        functions: &'a FunctionRegistry,
    ) -> DbResult<Self> {
        let left_schema = left.output_schema().clone();
        let right_schema = right.output_schema().clone();
        let mut right_rows = Vec::new();
        while let Some(tuple) = right.next()? {
            right_rows.push(tuple);
        }
        let schema = logical_plan::join_schema(&left_schema, &right_schema);
        Ok(NestedLoopJoinExec { left, right_rows, condition, functions, schema, current_left: None, right_index: 0 })
    }
}
```

この実装は、`right`をコンストラクタで一度だけ`Vec<Tuple>`へ読み切ります。
以後は`left`から1行受け取るたびに、この`Vec`を先頭から順に見比べるだけで、`right`を再実行する必要がありません。
比較の回数は、`left`の行数×`right`の行数のまま変わらないため、計算量(O(n×m))は教科書的な実装と同じです。

```rust
impl<'a> Executor for NestedLoopJoinExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            if self.current_left.is_none() {
                let Some(tuple) = self.left.next()? else {
                    return Ok(None);
                };
                self.current_left = Some(tuple);
                self.right_index = 0;
            }
            let left_tuple = self.current_left.as_ref().expect("直前にSomeを設定済み");

            while self.right_index < self.right_rows.len() {
                let right_tuple = &self.right_rows[self.right_index];
                self.right_index += 1;
                let combined = concat_tuple(&self.schema, left_tuple, right_tuple)?;
                let row = Row::new(&self.schema, &combined);
                let value = eval_bound_expr(self.condition, self.functions, Some(&row))?;
                if predicate_matches(value)? {
                    return Ok(Some(combined));
                }
            }
            // `right_rows`を使い切った。この`left`の行についてはこれ以上
            // 一致しないので、次の`left`の行へ進む。
            self.current_left = None;
        }
    }
}
```

`current_left`と`right_index`という2つのフィールドが、外側のループ(`left`のどの行を見ているか)と内側のループ(`right`のどこまで見たか)の状態を、`next()`が呼ばれるたびに1歩ずつ進める形で持ち回っています。
`right_rows`を最後まで見終えたら`current_left`を`None`に戻し、次の`next()`呼び出しで`left`の次の行を引きます。
`ON`条件の評価自体は、`WHERE`の評価(`FilterExec`、第19章)と全く同じ`eval_bound_expr`、`predicate_matches`を経由します。
JOIN専用のNULL処理をこの演算子が持たなくても、`customers.id = orders.customer_id`の`customer_id`が`NULL`である行は、三値論理(第8章)によって`predicate_matches`が自然に`false`を返し、結合結果から除外されます。

## Executor: HashJoinExec

Hash Joinは、Build(右側を全件読んでハッシュテーブルを作る)とProbe(左側を1件ずつ読んで引く)という、性質の異なる2つの段階を持ちます。

```rust
pub struct HashJoinExec<'a> {
    left: Box<dyn Executor + 'a>,
    left_schema: Schema,
    build: HashMap<Vec<Value>, Vec<Tuple>>,
    keys: &'a [(BoundExpr, BoundExpr)],
    functions: &'a FunctionRegistry,
    schema: Schema,
    current_left: Option<Tuple>,
    current_key: Option<Vec<Value>>,
    match_index: usize,
}

impl<'a> HashJoinExec<'a> {
    pub fn new(
        left: Box<dyn Executor + 'a>,
        mut right: Box<dyn Executor + 'a>,
        keys: &'a [(BoundExpr, BoundExpr)],
        functions: &'a FunctionRegistry,
    ) -> DbResult<Self> {
        let left_schema = left.output_schema().clone();
        let right_schema = right.output_schema().clone();
        let schema = logical_plan::join_schema(&left_schema, &right_schema);

        let mut build: HashMap<Vec<Value>, Vec<Tuple>> = HashMap::new();
        while let Some(tuple) = right.next()? {
            let row = Row::new(&right_schema, &tuple);
            let key: Vec<Value> =
                keys.iter().map(|(_, right_key)| eval_bound_expr(right_key, functions, Some(&row))).collect::<DbResult<_>>()?;
            if key.iter().any(Value::is_null) {
                continue; // NULLキーは結合しない(モジュールのドキュメント参照)
            }
            build.entry(key).or_default().push(tuple);
        }

        Ok(HashJoinExec { left, left_schema, build, keys, functions, schema, current_left: None, current_key: None, match_index: 0 })
    }
}
```

`build`は`HashMap<Vec<Value>, Vec<Tuple>>`という、鍵1つにつき複数の行を持てる多重写像です。
`HashMap<Vec<Value>, Tuple>`のように1鍵1行にしてしまうと、`customers`が同じ`id`を2行持つような重複キーのケースで、後から挿入した行が先の行を上書きして消えてしまいます。
`orders`側に同じ`customer_id`の注文が複数あるとき、その全件が結果に現れなければならないのはINNER JOINの意味論そのものであり、[第21章](./ch21-sort-aggregate.md)の`GROUP BY`が同じ鍵の行を1つのグループへ畳み込んでいたのとは、鍵の使い方が違います。

Build段階の`while let`ループの中に、この章のもう1つの要点があります。

```rust
if key.iter().any(Value::is_null) {
    continue;
}
```

SQLの等価比較は`NULL = NULL`をUNKNOWN(一致とはみなさない)として扱います(第8章の三値論理)。
鍵にNULLを含む行をハッシュテーブルへそもそも挿入しなければ、その行はどのProbe行の鍵とも一致しようがなく、結合結果から自然に除外されます。
Probe側も同じ理由で、鍵にNULLを含む行はハッシュテーブルを引かずに次の行へ進みます。

```rust
impl<'a> Executor for HashJoinExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            if self.current_left.is_none() {
                let Some(tuple) = self.left.next()? else {
                    return Ok(None);
                };
                let row = Row::new(&self.left_schema, &tuple);
                let key: Vec<Value> = self
                    .keys
                    .iter()
                    .map(|(left_key, _)| eval_bound_expr(left_key, self.functions, Some(&row)))
                    .collect::<DbResult<_>>()?;
                self.current_left = Some(tuple);
                if key.iter().any(Value::is_null) {
                    self.current_key = None;
                } else {
                    self.current_key = Some(key);
                }
                self.match_index = 0;
            }

            let Some(key) = &self.current_key else {
                self.current_left = None;
                continue;
            };
            let matches = self.build.get(key);
            let found = matches.and_then(|rows| rows.get(self.match_index));
            match found {
                Some(right_tuple) => {
                    self.match_index += 1;
                    let left_tuple = self.current_left.as_ref().expect("直前にSomeを設定済み");
                    return concat_tuple(&self.schema, left_tuple, right_tuple).map(Some);
                }
                None => {
                    self.current_left = None;
                }
            }
        }
    }
}
```

`Vec<Value>`をそのままハッシュテーブルの鍵にするのは[第21章](./ch21-sort-aggregate.md)の`GROUP BY`と同じですが、あの章の`Vec<Value>`の等価性はRustの`derive(PartialEq)`が定める構造的な等価性であり、`NULL`同士も等しいとみなしていました(`GROUP BY`が`NULL`を1つのグループへまとめる規則の根拠でした)。
この章のJOINでその判定に頼らなかったのは、`Vec<Value>`の構造的な等価性とSQLの等価比較(NULLはどれとも一致しない)が異なる規則だからです。
鍵を計算した時点で明示的にNULLを検査することで、ハッシュテーブル自体は`GROUP BY`と同じ道具を使いながら、意味論だけをJOINのものに変えています。

Buildは`right`を`None`が返るまで読み切る**blocking**な段階です。
Probeは`FilterExec`と同じ「一致するまで子を引く」形の**streaming**であり、`current_left`、`current_key`、`match_index`という3つのフィールドが、「今どの左の行を見ているか」「その鍵の一致がどこまで進んだか」を`next()`をまたいで覚えています。
1つの左の行に対して複数の右の行が一致する場合(重複キー)は、`match_index`を1つずつ進めながら同じ左の行を使い回し、`build`から取り出せる行が尽きた時点で次の左の行へ進みます。

## Volcanoの中でJoinがどう`next()`を回すか

第19章のVolcanoモデルは、「呼び出しは根から葉へ下り、行は葉から根へ返る」という向きを持っていました。
`Join`はこの向きに、もう1つの非対称性を持ち込みます。

```text
Projection(customers.name, orders.item)
  └─ HashJoin(INNER JOIN, id = customer_id)
    └─ SeqScan(customers)
    └─ SeqScan(orders)
```

`HashJoinExec::new`は、構築された瞬間に`right`(`orders`側の`SeqScan`)へ`next()`を呼び続け、`None`が返るまで読み切ります。
まだ`Projection`が一度も`next()`を呼んでいないうちに、`orders`テーブルの全件走査がすでに終わっているということです。
`left`(`customers`側の`SeqScan`)は対照的に、`HashJoinExec::next()`が呼ばれるたびに1行ずつしか引かれません。
Build側は構築時にblockingに読み切り、Probe側は`next()`のたびにstreamingに読み進める、という非対称性が、`Join`という1つの演算子の内側に同居しています。

`NestedLoopJoinExec`も同じ非対称性を持ちます。
`right`はコンストラクタで全件読み切り(Build相当)、`left`は`next()`のたびに1行ずつ引く(Probe相当)という構造は、Hash Joinと形の上では同じです。
違うのは、右側から読み切った結果を鍵で仕分けるか(Hash Join)、単に`Vec`として並べておくか(Nested Loop Join)という、Build段階でのデータの持ち方だけです。

`EXPLAIN`は、この2つのアルゴリズムのどちらが選ばれたかをそのまま見せます。

```console
minidb> EXPLAIN SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id;
QUERY PLAN
----------
Projection(customers.name, orders.item)
  └─ HashJoin(INNER JOIN, id = customer_id)
    └─ SeqScan(customers)
    └─ SeqScan(orders)
(4 rows)

minidb> EXPLAIN SELECT customers.name FROM customers JOIN orders ON customers.id <> orders.customer_id;
QUERY PLAN
----------
Projection(customers.name)
  └─ NestedLoopJoin(INNER JOIN, id <> customer_id)
    └─ SeqScan(customers)
    └─ SeqScan(orders)
(4 rows)
```

## テストで確認する

`NestedLoopJoinExec`と`HashJoinExec`は、アルゴリズムが違うだけで、同じ入力、同じ等値条件に対しては同じ行集合を返さなければなりません。
`physical_plan`モジュールのテストは、同じデータを両方の演算子に流し込んで結果を突き合わせることでこれを確認します。

```rust
#[test]
fn nested_loop_and_hash_join_produce_identical_results_for_an_equi_join() {
    let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id = b.id");
    let condition = select.joins[0].condition.clone();
    let keys = equi_keys(&condition).expect("等値条件のはず");

    let a_rows = vec![a_row(1, "a1"), a_row(2, "a2"), a_row(2, "a2b"), a_row(3, "a3")];
    let b_rows = vec![b_row(Some(2), "b2"), b_row(None, "bnull"), b_row(Some(2), "b2b"), b_row(Some(9), "bnomatch")];

    let functions = FunctionRegistry::with_builtins();
    let mut nlj =
        NestedLoopJoinExec::new(exec_over_a_rows(a_rows.clone()), exec_over_b_rows(b_rows.clone()), &condition, &functions)
            .unwrap();
    let mut hash = HashJoinExec::new(exec_over_a_rows(a_rows), exec_over_b_rows(b_rows), &keys, &functions).unwrap();

    let nlj_rows = collect_all(&mut nlj);
    let hash_rows = collect_all(&mut hash);
    assert_eq!(nlj_rows.len(), 4);
    assert_eq!(nlj_rows, hash_rows);
}
```

`a.id = 2`の行が2件、`b.id = 2`の行が2件あるため、この鍵だけで2×2=4件のマッチが生まれます(重複キーの多重集合としての結合)。
この4件が、Nested Loop JoinとHash Joinのどちらから見ても行の値、順序ともに一致することを確認しています。
`database`モジュールにも、`a.id = b.id`(Hash Join)と、これと同値な`a.id >= b.id AND a.id <= b.id`(等値条件として認識されないためNested Loop Joinが選ばれる)を実際のSQL文として実行し、同じ結果になることを確認する統合テストを追加しました。

NULLキーの除外は、`HashJoinExec`、`NestedLoopJoinExec`それぞれについて、鍵にNULLを含む行が結果に現れないことを単体テストで確認しています。
`differential`テストにも、`customer_id`が`NULL`の注文が結合結果に現れないことをSQLiteと突き合わせて確認するケースを追加しました。
空テーブル(左が空、右が空それぞれ)、3テーブルの連鎖`JOIN`、`WHERE`、`GROUP BY`、`ORDER BY`との組み合わせも、`database`モジュールと`differential`テストの両方でカバーしています。

計算量の違いは、実際に実行時間を測って確認しました。
同じデータ(`a`、`b`ともに`n`行、`a.id`と`b.id`がすべて一致する)に対し、`a.id = b.id`(Hash Join)と`a.id >= b.id AND a.id <= b.id`(意味的には同値だが等値条件として認識されないため、必ずNested Loop Joinが選ばれる)をそれぞれ実行して比較します。

```console
$ cargo test --release --lib nested_loop_join_is_quadratic -- --ignored --nocapture
n=  500  HashJoin= 297.591µs  NestedLoopJoin=25.630728ms
n= 1000  HashJoin= 596.051µs  NestedLoopJoin=102.181694ms
n= 2000  HashJoin=1.188872ms  NestedLoopJoin=413.940547ms
```

`n`を500から2000へ4倍にすると、Hash Joinの実行時間はおよそ4倍(298µs→1.19ms)にとどまりますが、Nested Loop Joinの実行時間はおよそ16倍(25.6ms→414ms)に増えています。
O(n)とO(n×m)(この測定では`n = m`なのでO(n²))という理論上の計算量の違いが、そのまま実測の伸び方の違いとして現れています。
この測定は実行時間そのものを検証する回帰テストにはしていません(実行環境によって数値が変わりうるテストは不安定になりやすいため)が、`#[ignore]`を付けたテストとして残してあり、`cargo test -- --ignored`で読者自身の環境でも再現できます。

```console
$ cargo test
test result: ok. 471 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.58s
...
test result: ok. 34 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
...
test golden_tests_pass ... ok
```

## 演習問題

### 必須課題

1. `FROM a, b`というカンマ結合を、`ON`条件を持たない`JOIN`(実質的な直積、Cross Join)として`Parser`に受理させてください。`ast::FromClause`にカンマ区切りの追加テーブルを持たせる案と、`JoinClause::on`を`Option<Expr>`にして`None`のときは常に`TRUE`とみなす案を比較し、どちらが`Binder`、`LogicalPlan`への影響が小さいかを検討したうえで実装してください。`ON`が無い`Join`は`split_equi_join_keys`が鍵を1つも取り出せないため、常にNested Loop Joinが選ばれることを`EXPLAIN`で確認してください。
2. `LEFT OUTER JOIN`を実装してください。`JoinKind`に`LeftOuter`を追加し、`NestedLoopJoinExec`(または新設する`LeftOuterNestedLoopJoinExec`)が「一致する`right`の行が1件も無かった左の行について、`right`側をすべて`NULL`で埋めた1行を返す」という規則をどこに実装するか設計してください。`LEFT OUTER JOIN`では結合後スキーマの右半分がすべて`nullable`になる必要があることに注意し、`join_schema`をどう変更すべきか検討してください。
3. `split_equi_join_keys`は、等値条件と定数条件(`a.z = 5`のような)が混在する`ON`句(`a.x = b.x AND a.z = 5`)に出会うと、`Vec`全体を諦めてNested Loop Joinへ倒します。等値条件だけをHash Joinの鍵として取り出し、残りの条件(定数条件、非等値条件)を`HashJoinExec`の中で追加のFilterとして評価する設計に変更してください。`HashJoinNode`にどんなフィールドを追加する必要があるか、`optimize`の`Join`アームをどう書き換える必要があるかを設計したうえで実装し、`SELECT ... FROM a JOIN b ON a.x = b.x AND a.z = 5`が`EXPLAIN`で`HashJoin`を選ぶようになることを確認してください。
4. `FROM a AS x JOIN a AS y ON x.id = y.parent_id`のような自己結合(self-join、同じテーブルを2つのAliasで参照する)を実際に実行するテストを書いてください。`Binder::resolve_table`が`table_id`を`Catalog`から毎回引き直す実装になっているため、自己結合が特別な分岐を必要とせずに動くはずです。動くことを確認したうえで、`FROM a JOIN a ON ...`のようにAliasを付けずに自己結合を書いた場合に何が起きるか(`qualifier()`が両方とも同じ文字列を返すことに注目して)予想し、実際に実行して確認してください。

### 発展課題

1. `NestedLoopJoinExec`は`right`を`Vec<Tuple>`としてメモリに丸ごと保持します。`right`がメモリに載らないほど大きいテーブルの場合、この`Vec`はテーブル本体と同程度のメモリを消費します。この章の`Storage`(第15章)を使って、`right`をメモリではなく一時的なヒープテーブルへ書き出し、`next()`のたびにディスクから読み直す設計を検討し、少なくとも設計(どの時点でメモリからディスクへ切り替えるか、`RecordId`をどう管理するか)をドキュメントとして書き下ろしてください。
2. `HashJoinExec`の`build`は`right`の全行をメモリ上の`HashMap`に保持します。`right`がメモリに収まらないほど大きい場合、Grace Hash Joinという手法が使われます。`left`、`right`の両方を結合キーのハッシュ値で複数のパーティションへ分割し、対応するパーティション同士だけを(メモリに載る大きさまで削減してから)結合するという設計です。この手法を調べ、この章の`HashJoinExec`とどこが変わるか(特に、パーティションをまたいで一致することがないという性質がなぜ成り立つか)を説明する文章を書いてください。
3. 現在の物理選択は「等値ならHash Join、そうでなければNested Loop Join」という構文的なルールですが、両方の入力が数行しか無い小さいテーブル同士のJoinでは、Hash Joinのハッシュテーブル構築コストがNested Loop Joinの総当たりコストを上回ることがあります。`Database`に「入力の行数がある閾値未満ならNested Loop Joinを常に選ぶ」というルールを追加し、`std::time::Instant`で小さいテーブル同士のJoinの実行時間を比較して、この閾値がどのあたりにあるか実測してください。これは統計情報を使わない簡易な物理選択の改善であり、コストベースの選択そのもの(第28章)ではないことに注意してください。
