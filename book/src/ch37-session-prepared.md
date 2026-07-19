# 第37章 SessionとPrepared Statement

第36章のサーバーは、`SELECT`を1本送るたびに、そのSQL文字列をまるごと2回パースしています。

`Session::execute_inner`は、届いたSQLを`BEGIN`かどうか判定するために一度パースし、`BEGIN`でなければ`sql`という文字列のまま`SharedDatabase::execute_in_tx`へ渡します。
ところが`execute_in_tx`は、内部でもう一度`crate::parser::parse_statement(sql)`を呼びます。
`SELECT id FROM users WHERE id = 1`のような1行のクエリでも、字句解析と構文解析という同じ仕事を、サーバーが受け取るたびに2回こなしていることになります。
第37章はまずこの二重パースを解消します。

もう1つ、第36章の`Session`が正直に告白していた重複があります。
`BEGIN`、`COMMIT`、`ROLLBACK`の3文を`Session`側で判定して`TxHandle`のAPIへ翻訳する処理は、`Database::execute`の先頭にある同じ3文の分岐とほぼ同じ形をしています。
接続ごとのトランザクション状態を`Database`ではなく接続の側に持たせる設計を選んだ以上、この重複は避けようがありませんでした。
「セッションの状態と責務を1箇所にまとめる」ことが、この章の名前になっている理由です。

## 文字列を継ぎ足して`WHERE`句を作るとどうなるか

`Session`を正式に導入する前に、なぜこの章が同時に「値をSQLのテキストから切り離す」仕組みを必要とするかを確認します。

クライアントから届いた検索語をそのままSQL文字列へ埋め込むコードを考えます。

```rust
let sql = format!("SELECT name FROM users WHERE name = '{user_input}'");
db.execute(&sql)
```

`user_input`が`"Alice"`であれば、これは`WHERE name = 'Alice'`という素直な条件になります。
しかし`user_input`が`"x' OR '1'='1"`だったらどうでしょうか。
文字列の中身をそのまま`'...'`の内側へ展開すると、組み立てられるSQLは次の形になります。

```sql
SELECT name FROM users WHERE name = 'x' OR '1'='1'
```

`'1'='1'`は常に真です。
つまりこの`WHERE`句は、`name`の値に関係なく全行にマッチする条件へすり替わっています。
検索語のつもりで送った文字列が、SQLの構文そのものの一部として解釈されてしまう。
これがSQLインジェクションで、`'`を含む入力を許すあらゆる文字列連結クエリに起こりえます。

原因は、SQL文字列というたった1つのチャネルに、「これから実行する構文」と「検索語という値」という性質の異なる2つの情報を混ぜて送っていることです。
`Parser`は`'...'`の中身がどこから来たかを知りません。
アプリケーションコードが書いた`WHERE name =`のリテラルなのか、ユーザーが入力した検索語なのかを、構文解析はSQLのバイト列だけから区別できないのです。

この章が導入する**Prepared Statement**は、この2つのチャネルを実際に分離します。
`$1`のような**プレースホルダ**を使って構文だけを先に確定させ、値はあとから別の経路(この教材では`Session::execute_prepared`という、SQL文字列を経由しないRust API)で渡します。
構文解析は`$1`という記号だけを見て、そこに何の値が来るかを知る必要がありません。
値が構文の一部として解釈される余地そのものが無いので、値の中に`'`が何個入っていても、その`'`は「1文字の`'`という値」のままです。

## Session:接続の状態を一元管理する

`Session`は、Embedded、REPL、Serverの3つの利用箇所すべてが使う、接続ひとつぶんの状態です。

```rust
pub struct Session {
    shared: Arc<SharedDatabase>,
    tx: Option<TxHandle>,
    prepared: HashMap<String, PreparedStatement>,
}
```

`Arc<SharedDatabase>`を1個持つだけの薄い型なので、3つの利用箇所は同じコードをそのまま使い回せます。

- **Embedded**: `Database`を`SharedDatabase::new`で包み、`Arc`に入れて`Session::new`へ渡します。
- **REPL**(`src/main.rs`): プロセス全体で`Session`を1個だけ作り、標準入力の行ごとに`Session::execute`を呼びます。
- **Server**(`src/server.rs`): 接続を受け付けるたびに、サーバーが持つ`Arc<SharedDatabase>`を`Arc::clone`して`Session::new`へ渡します。

`SharedDatabase`はもともと複数スレッドから安全に共有するための型(第35章)でした。
単一スレッドのEmbedded、REPLで使っても`Mutex`の獲得は競合しないので、複数スレッドの場合と地続きの型のまま使えます。

`Session::execute`は、届いたSQLを一度だけパースし、以後は`Statement`という構造化された値だけを使います。

```rust
pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
    let statement = crate::parser::parse_statement(sql)?;
    match statement {
        Statement::Begin(begin) => {
            if self.tx.is_some() {
                return Err(DbError::TransactionAlreadyActive);
            }
            let level = begin.isolation_level.unwrap_or(crate::ast::IsolationLevel::RepeatableRead);
            self.tx = Some(self.shared.begin_tx_with_isolation(level));
            Ok(QueryResult::command("BEGIN"))
        }
        Statement::Commit(_) => {
            let handle = self.tx.take().ok_or(DbError::NoActiveTransaction)?;
            self.shared.commit_tx(handle)?;
            Ok(QueryResult::command("COMMIT"))
        }
        Statement::Rollback(_) => {
            let handle = self.tx.take().ok_or(DbError::NoActiveTransaction)?;
            self.shared.rollback_tx(handle)?;
            Ok(QueryResult::command("ROLLBACK"))
        }
        Statement::Checkpoint(_) => {
            Err(DbError::NotImplemented("CHECKPOINTはSession経由では未対応です".to_string()))
        }
        Statement::Prepare(prepare) => self.execute_prepare(prepare, sql),
        Statement::Execute(execute) => self.execute_execute(&execute),
        Statement::Deallocate(deallocate) => self.execute_deallocate(&deallocate),
        other => {
            let bound = self.shared.bind_statement(other, sql)?;
            self.run_bound(bound)
        }
    }
}
```

`BEGIN`、`COMMIT`、`ROLLBACK`、`PREPARE`、`EXECUTE`、`DEALLOCATE`のどれでもない文(`other`の分岐)は、`SharedDatabase::bind_statement`でその場で1回だけ束縛し、得られた`BoundStatement`を`run_bound`へ渡します。
`bind_statement`は、`Database`が第9章以来ずっと使ってきた`bind`という内部関数を、この章で公開しただけの薄いラッパーです。

```rust
pub fn bind_statement(&self, statement: Statement, sql: &str) -> DbResult<BoundStatement> {
    self.bind(statement, sql)
}
```

問題は、`SharedDatabase::execute_in_tx`が受け取るのが束縛済みの`BoundStatement`ではなく、生のSQL文字列だったことです。
そのままでは、`Session`が一度パースして束縛した結果を渡す先がありません。
そこでこの章は、`Database::execute_in_tx`が内部で共有していた「文を実行する本体」を切り出し、束縛済みの文をそのまま受け取る経路を新設しました。

```rust
fn execute_bound_statement(&mut self, statement: Statement, sql: &str) -> DbResult<QueryResult> {
    let bound = self.bind(statement, sql)?;
    self.run_bound_statement(bound)
}

pub fn execute_bound_statement_prebound(&mut self, bound: BoundStatement) -> DbResult<QueryResult> {
    self.run_bound_statement(bound)
}

fn run_bound_statement(&mut self, bound: BoundStatement) -> DbResult<QueryResult> {
    let owner = self.lock_owner();
    let result = match bound {
        // ...
    };
    if self.tx.is_none() {
        self.lock_manager.release_all(owner);
    }
    result
}
```

`execute_in_tx`自身も、同じ形に切り出した`run_in_tx`を共有するように書き直しました。

```rust
pub fn execute_in_tx(&mut self, handle: &TxHandle, sql: &str) -> DbResult<QueryResult> {
    self.run_in_tx(handle, |db| {
        let statement = crate::parser::parse_statement(sql)?;
        db.execute_bound_statement(statement, sql)
    })
}

pub fn execute_in_tx_bound(&mut self, handle: &TxHandle, bound: BoundStatement) -> DbResult<QueryResult> {
    self.run_in_tx(handle, |db| db.run_bound_statement(bound))
}
```

`execute_in_tx`は「パースしてから実行する」クロージャを`run_in_tx`へ渡し、`execute_in_tx_bound`は「(パースも束縛もせず)そのまま実行する」クロージャを渡すだけの違いです。
`run_in_tx`自身は、`harness_contexts`(第30章)との出し入れ、`Aborted`状態の検査、`finish`によるAbort遷移という、トランザクション境界の規律をどちらの経路でも同じように適用します。
`Session::run_bound`は、この`execute_in_tx_bound`(明示的な`BEGIN`の中)と、1文だけのAutocommit(`begin_tx`→`execute_in_tx_bound`→`commit_tx`/`rollback_tx`)を、`self.tx`の有無で振り分けます。

```rust
fn run_bound(&mut self, bound: BoundStatement) -> DbResult<QueryResult> {
    match &self.tx {
        Some(handle) => self.shared.execute_in_tx_bound(handle, &bound),
        None => self.run_bound_autocommit(bound),
    }
}
```

これで、`SELECT`のようなふつうの文は「パース1回、束縛1回」で実行されます。
`BEGIN`かどうかを判定するためのパースと、実行するためのパースと束縛が同じ1回で済むようになったのが、この節の変更です。

`Database::execute`(第9章以来の低レベルAPI)自体は、この章でも変更していません。
構文解析から実行までを1回の呼び出しで済ませる単純さは、これまでの章のテストのほとんどが`db.execute("...")`という形で直接使ってきたもので、テストのたびに`Session`を組み立てさせる理由がありません。
`Session`は`Database::execute`を置き換えるのではなく、その上に接続の寿命(トランザクション状態、Prepared Statementの名前空間)を積む層として追加しました。
`Database::execute`に直接`PREPARE`、`EXECUTE`、`DEALLOCATE`を渡した場合は、`Binder::bind`が位置情報つきの`DbError::Bind`で「Sessionを経由してください」と案内し、`Database`自身がこの3文の意味を知る必要が無いようにしてあります。

## Prepared Statement:構文と値を分ける

`PREPARE`、`EXECUTE`、`DEALLOCATE`は、PostgreSQLに倣った構文で受理します。

```sql
PREPARE find_by_name AS SELECT name FROM users WHERE name = $1
EXECUTE find_by_name('Alice')
DEALLOCATE find_by_name
```

`$1`という記号(プレースホルダ)は、字句解析器(`crate::lexer`)に新しいトークンとして追加しました。

```rust
/// `$`に続く数字列を読み、`TokenKind::Param`にする(第37章、`PREPARE`が
/// 受け付けるPlaceholder)。`$`の直後に数字が1個も無ければ字句エラーにする。
fn lex_param(&mut self) -> DbResult<TokenKind> {
    let start_line = self.line;
    let start_column = self.column;
    self.bump(); // '$'

    let Some(digits_start) = self.peek_offset() else {
        return Err(DbError::Lex {
            message: "'$'の直後に番号がありません".to_string(),
            line: start_line,
            column: start_column,
        });
    };
    // ...
    text.parse::<u32>().map(TokenKind::Param).map_err(|_| DbError::Lex {
        message: format!("Parameterの番号の範囲を超えています: ${text}"),
        line: start_line,
        column: start_column,
    })
}
```

AST(`crate::ast`)には、`Expr`の新しいバリアントとして`Param`を追加します。

```rust
/// `$1`のようなParameter Binding用のプレースホルダ(第37章)。`index`は
/// `1`始まりの番号(`$1`なら`1`)。`PREPARE`本体の式木にだけ現れ、
/// `EXECUTE`が渡す値へ束縛される。
Param {
    index: u32,
    span: Span,
},
```

`PREPARE`、`EXECUTE`、`DEALLOCATE`自身も、`Statement`の新しいバリアントです。
`EXECUTE`の引数は式ではなくリテラル(整数、文字列、真偽値、`NULL`)に限定しています。

```rust
/// `EXECUTE name [(値, ...)]`文(第37章)。
///
/// `args`に置けるのはリテラルだけである(`Literal`)。`EXECUTE`の引数は
/// `PREPARE`済みの文へ渡す**値**であり、列参照や式評価を必要としない。
pub struct ExecuteStatement {
    pub name: Ident,
    pub args: Vec<Literal>,
    pub span: Span,
}
```

`Parser`は`EXECUTE`の引数を、式の完全な文法(Pratt Parser)ではなく専用の`parse_literal`で読みます。

```rust
fn parse_literal(&mut self) -> DbResult<Literal> {
    match self.peek_kind().clone() {
        TokenKind::IntLiteral(magnitude) => {
            let token = self.advance();
            let value = i64::try_from(magnitude).map_err(|_| {
                self.error_at(token.span, format!("整数リテラルの範囲を超えています: {magnitude}"))
            })?;
            Ok(Literal::Int { value, span: token.span })
        }
        // ...(Minus、StringLiteral、True、False、Nullも同様)
        _ => Err(self.unexpected("リテラル(整数、文字列、真偽値、NULL)")),
    }
}
```

構文としての`EXECUTE`引数を式ではなくリテラルに絞ったのは、単純化のためだけではありません。
`EXECUTE find_by_name('Alice')`のようにSQL文字列として`EXECUTE`を書く経路は、結局のところ値をSQLのテキストへ埋め込んでいます。
`'Alice'`という文字列の中に`'`自身が含まれていれば、この経路もまた「値がテキストとして構文の一部に混ざる」という、冒頭で見たインジェクションと同じ危険を抱えます。
`Session`は、SQL文字列を経由せず値を直接渡す`execute_prepared`というRust APIも公開しており、これが実際に注入を防ぐ経路です(後述)。
SQL文字列の`EXECUTE`は、あくまで人間が対話的に`psql`相当のクライアントから打つときの利便性のための構文だと割り切っています。

## プレースホルダの型をいつ決めるか

`PREPARE`本体を束縛するとき、`$1`にどんな型がついているかを`Binder`はどこまで知っているべきでしょうか。

このSQLサブセットは静的型付けで、`Binder`(第17章)が式1つ1つの型を確定させてから実行に渡します。
`col = $1`という比較式は、`col`の型が決まれば`$1`もその型でなければ意味を持ちません。
`$1 + 1`という算術式なら、演算子自体が`BIGINT`同士しか受け付けないので、`$1`は`BIGINT`のはずです。
これらは、`$1`という記号だけを見ても分からず、`$1`を包んでいる式(比較演算子、算術演算子、`CAST`、関数呼び出しの引数)を見て初めて分かる情報です。

そこでこの章は、**プレースホルダの型を`PREPARE`実行時、周囲の文脈から推論する**という設計を選びました。
`BoundExpr`に`Param`という新しいバリアントを追加し、`data_type`を`Option<DataType>`で持たせます。

```rust
/// `$1`のようなParameter Binding用のプレースホルダ(第37章)。`index`は
/// `1`始まりの番号。`data_type`は、囲む式(比較の相手、算術演算子、
/// `CAST`、関数の仮引数など)から`bind_expr`が推論できた場合だけ`Some`に
/// なる。推論できなかった場合(`$1`同士の比較など)は`None`のままにし、
/// `EXECUTE`の値がどんな型であっても受理する。
Param {
    index: u32,
    data_type: Option<DataType>,
    span: Span,
},
```

推論そのものは、`coerce_param_type`という小さな関数が担います。

```rust
/// `expr`が型未確定の`BoundExpr::Param`であれば、その`data_type`を`hint`で
/// 埋める。`expr`がParamでない場合、または`hint`が`None`の場合は変更しない。
fn coerce_param_type(expr: BoundExpr, hint: Option<DataType>) -> BoundExpr {
    match (expr, hint) {
        (BoundExpr::Param { index, data_type: None, span }, Some(hint)) => {
            BoundExpr::Param { index, data_type: Some(hint), span }
        }
        (expr, _) => expr,
    }
}
```

`bind_expr`は、二項演算子、単項演算子、`CAST`、関数呼び出しという4箇所で、この`hint`を組み立てて`coerce_param_type`に渡します。
二項演算子の場合を見ます。

```rust
Expr::BinaryOp { op, lhs, rhs, span } => {
    let bound_lhs = self.bind_expr(lhs, tables)?;
    let bound_rhs = self.bind_expr(rhs, tables)?;
    let hint = match op {
        BinaryOperator::Add | BinaryOperator::Subtract | BinaryOperator::Multiply | BinaryOperator::Divide => {
            Some(DataType::BigInt)
        }
        BinaryOperator::And | BinaryOperator::Or => Some(DataType::Boolean),
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq => bound_lhs.data_type().or_else(|| bound_rhs.data_type()),
    };
    let bound_lhs = coerce_param_type(bound_lhs, hint);
    let bound_rhs = coerce_param_type(bound_rhs, hint);
    let data_type = self.check_binary_type(*op, &bound_lhs, &bound_rhs, *span)?;
    // ...
}
```

算術演算子、論理演算子は、演算子自身が要求する型が一意に決まるので、`hint`は演算子から直接決まります。
比較演算子は「両辺が同じ型でありさえすればよい」という規則なので、もう一方の辺の型を`hint`にします。
`col = $1`なら`col`の型、`$1 = col`でも同じ理由で`col`の型が`hint`になります。

`$1 = $2`のように両辺とも`$n`だと、この式1つだけでは型を決める材料がありません。
`bound_lhs.data_type()`も`bound_rhs.data_type()`もまだ`None`なので、`hint`も`None`のままです。
この場合、型は未確定(`None`)のまま`PreparedStatement`に残ります。
`EXECUTE`はこの`$n`に対して、渡された値がどんな型であっても受理します(型検査を省略するのであって、値を拒否するわけではありません)。
値が式の要求する型と食い違っていれば(たとえば`$1 = $2`に整数と文字列を渡す)、`Binder`ではなく`crate::eval::eval_bound_expr`が実行時の`DbError::Eval`として検出します。
つまりこの片務的な設計は、「決まる場合には`PREPARE`の時点で決める」だけであり、決まらない場合の実行時エラーへ倒れる余地を最初から許容しています。

もう1つの実装場所は`CAST`です。

```rust
Expr::Cast { expr, type_name, span } => {
    let data_type = DataType::from_sql_name(&type_name.name).ok_or_else(|| {
        self.error_at(type_name.span, format!("未知の型名です: {}", type_name.name))
    })?;
    let bound = coerce_param_type(self.bind_expr(expr, tables)?, Some(data_type));
    Ok(BoundExpr::Cast { expr: Box::new(bound), data_type, span: *span })
}
```

`CAST($1 AS TEXT)`と書けば、`$1`同士の比較のように文脈から決まらない場合でも、明示的に型を宣言できます。
PostgreSQLの`$1::int`という省略記法に相当する書き方を、`CAST`という既存の構文の中で実現した形です。

`INSERT INTO ... VALUES`の`$n`は少し事情が違います。
`BoundInsert`の`rows`は`Binder`を経由しない生の`Expr`のまま保持されています(`VALUES`は既存の列を参照しないため、名前解決の必要が無いという第10章以来の設計です)。
そのため`VALUES`に現れる`$n`の型は、`Binder`ではなく`Session`側の`collect_insert`が、対応する列の`Schema`から直接引きます。

```rust
fn collect_insert(insert: &BoundInsert, types: &mut Vec<Option<DataType>>) -> DbResult<()> {
    for row in &insert.rows {
        for (position, value_expr) in row.iter().enumerate() {
            let column_index = match &insert.columns {
                Some(columns) => columns.get(position).copied(),
                None => Some(position),
            };
            let target_type = column_index.and_then(|index| insert.schema.columns().get(index)).map(|c| c.data_type);
            collect_ast_expr(value_expr, target_type, types)?;
        }
    }
    Ok(())
}
```

## EXECUTE:値を差し込んで実行する

`PREPARE`が確定させるのは、`$n`をまだプレースホルダのまま持つ`BoundStatement`です。
`EXECUTE`は、この`BoundStatement`を複製し、すべての`$n`を渡された値のリテラルへ置き換えてから実行します。
束縛(名前解決と型検査)をやり直さない、というのがこの章の`EXECUTE`の要点です。

```rust
fn substitute_bound_expr(expr: BoundExpr, values: &[Value]) -> BoundExpr {
    match expr {
        BoundExpr::Param { index, span, .. } => value_to_bound_literal(values[index as usize - 1].clone(), span),
        BoundExpr::UnaryOp { op, expr, data_type, span } => {
            BoundExpr::UnaryOp { op, expr: Box::new(substitute_bound_expr(*expr, values)), data_type, span }
        }
        // ...(BinaryOp、IsNull、FunctionCall、Aggregate、Paren、Castも再帰)
        literal @ (BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. }
        | BoundExpr::ColumnRef { .. }) => literal,
    }
}
```

木を再帰的に辿り、`Param`ノードだけをリテラルへ置き換え、それ以外のノード(演算子、型、`Span`)はそのまま複製します。
`INSERT`の`VALUES`(生の`Expr`)にも、同じ形の`substitute_ast_expr`を用意してあります。

個数と型の検査を済ませてから置き換えるのが`Session::execute_prepared`です。

```rust
pub fn execute_prepared(&mut self, name: &str, args: &[Value]) -> DbResult<QueryResult> {
    let prepared =
        self.prepared.get(name).ok_or_else(|| DbError::PreparedStatementNotFound(name.to_string()))?;

    if args.len() != prepared.param_types.len() {
        return Err(DbError::ParamCountMismatch { expected: prepared.param_types.len(), actual: args.len() });
    }
    for (position, value) in args.iter().enumerate() {
        if let Some(expected) = prepared.param_types[position]
            && let Some(actual) = value.data_type()
            && actual != expected
        {
            return Err(DbError::ParamTypeMismatch { index: (position + 1) as u32, expected, actual });
        }
    }

    let bound = substitute_bound_statement(prepared.bound.clone(), args);
    self.run_bound(bound)
}
```

`NULL`はどんな型のプレースホルダにも許します(`value.data_type()`が`None`を返すので、型検査そのものをすり抜けます)。
通常の列がNULL制約の無い限りどんな型の列にもNULLを書き込めるのと同じ扱いです。

SQL文字列の`EXECUTE find_by_name('Alice')`は、この`execute_prepared`の薄いラッパーです。

```rust
fn execute_execute(&mut self, execute: &ExecuteStatement) -> DbResult<QueryResult> {
    let values: Vec<Value> = execute.args.iter().map(literal_to_value).collect();
    self.execute_prepared(&execute.name.name, &values)
}
```

冒頭で見たインジェクションが実際に防がれるのは、`execute_prepared`が値を一度も**SQLのテキスト表現**へ変換しないからです。
`args: &[Value]`という引数は、`'`を含む文字列であっても`Value::Text(String)`という1個の値のままです。
`substitute_bound_expr`は、この`Value`を直接`BoundExpr::StringLiteral`という構文木のノードへ変換します。
変換の途中に「文字列を組み立てて、それを字句解析器にもう一度読ませる」という工程が無いので、値の中身がどんな文字を含んでいても、構文の一部として解釈される機会そのものがありません。

対して、SQL文字列としての`EXECUTE`(`format!("EXECUTE p('{user_input}')")`のように呼び出し側がテキストを組み立てる場合)は、値を一度SQLのテキスト表現へ変換してから`Session::execute`に渡すことになります。
このテキスト化を呼び出し側が手作業でやってしまうと、`'`のエスケープを呼び出し側が正しく行わない限り、冒頭で見た文字列連結の危険がそのまま残ります。
Prepared Statementが安全になるのは、「構文を先に確定させて値をあとから渡す」という設計そのものによってであり、「`PREPARE`と`EXECUTE`という2つの文に分けて書く」という構文の見た目だけでは、実は安全性は保証されません。
`Session::execute_prepared`のように、値をSQLのテキストへ一度も経由させないAPIを呼んで初めて、この章が示したい防御が効きます。

## Wire Protocolを拡張しない

`PREPARE`、`EXECUTE`、`DEALLOCATE`はどれも、`crate::protocol`が定義するフレーム形式(第36章)の上で、通常の`SELECT`等と同じ「SQL文字列を1本送る」というメッセージとして表現できます。
専用のメッセージ種別を追加する必要はこの章にはありません。

PostgreSQLのExtended Query Protocolは、`Parse`、`Bind`、`Describe`、`Execute`、`Sync`という複数のメッセージに分かれています。
メッセージを分けている理由は、`Bind`で値を送る段階と`Parse`で構文を送る段階をバイト列レベルで分離し、同じ`Parse`のまま値だけ変えて`Bind`、`Execute`を繰り返す通信を、SQL文字列の再構築、再送信無しで実現するためです。
この章の`Session`は、`PREPARE`、`EXECUTE`のどちらもSQL文字列として受け取り(`EXECUTE`のRust API版である`execute_prepared`だけが例外です)、サーバー側の`HashMap`で名前を引く形に留めています。
`Session::execute_prepared`をWire Protocol越しに直接呼べる専用メッセージを追加すれば、通信の往復でSQLテキストを再構築する必要が無くなりますが、この差を埋める設計は発展編Eに譲ります。

## テスト

`src/session.rs`の単体テストは、`Session`単体の振る舞いを次の観点で確認します。

- `PREPARE`、`EXECUTE`、`DEALLOCATE`の一連が動き、`DEALLOCATE`後の`EXECUTE`は`PreparedStatementNotFound`になる
- `EXECUTE`の引数の個数が合わないと`ParamCountMismatch`になる
- `EXECUTE`の引数の型が文脈から推論した型と食い違うと`ParamTypeMismatch`になる
- `NULL`はどの型のプレースホルダに対しても受理される
- 同じ名前を2回`PREPARE`すると`PreparedStatementAlreadyExists`になる
- `SELECT`、`INSERT`、`UPDATE`、`DELETE`以外を`PREPARE`しようとすると`CannotPrepareStatement`になる
- 別の`Session`で`PREPARE`した名前は見えない(Session単位の名前空間)
- `Session`がdropされると、そのSessionの`PREPARE`済み文はすべて消える
- `BEGIN`で開いたトランザクションの中でも`EXECUTE`が動き、`ROLLBACK`すれば変更は残らない
- 文字列連結によるインジェクションが実際に成立する例と、`execute_prepared`がその同じ入力を安全に扱える例を並べて確認する

`tests/wire_protocol.rs`には、同じ`PREPARE`、`EXECUTE`、`DEALLOCATE`の一連をTCP接続越しに動かすテストと、Prepared Statementが接続(TCP接続 = 1個の`Session`)をまたいで見えないことを確認するテストを追加しました。
REPL(`src/main.rs`)は`Session::execute`をそのまま呼ぶだけの薄いループなので、REPL固有のテストは追加していません。
`src/database.rs`側は、`Database::execute`の既存の回帰テストをすべてそのまま維持しています。
`bind`から`bind_statement`という薄いラッパーを増やし、`execute_bound_statement`の中身を`run_bound_statement`へ切り出しただけなので、`Database::execute`が返す結果は1つも変わっていません。

## この章の限界

`PREPARE`は本体を1回だけ束縛し、以後の`EXECUTE`はその`BoundStatement`をそのまま(パラメータだけ差し替えて)使い回します。
`PREPARE`した後に`ANALYZE`で統計情報が更新されても、すでに`PREPARE`済みの文の実行計画は作り直されません。
正確に言うと、`physical_plan::optimize`自体は`EXECUTE`のたびに(`Database::execute_select`が呼ぶたびに)最新の統計を使って計画を組み直します。
古いままなのは、あくまで`rules::optimize`が畳み込む定数式や、束縛時点で確定した型、列インデックスといった`BoundStatement`の構造であり、PostgreSQLが"generic plan"と"custom plan"を使い分けて対処する種類の問題(統計に応じて計画の形そのものを変える)を、この章では扱いません。

`EXECUTE`の引数として書けるのはリテラルだけで、式は書けません。
`EXECUTE p(1 + 1)`のような式は構文エラーになります。
プレースホルダに渡す値は、呼び出し側があらかじめ計算しておく必要があります。

`INSERT`の`VALUES`に現れる`$n`は、`col = $1 + 1`のような入れ子の式まで型推論を追いません(`collect_ast_expr`が二項演算子、単項演算子の内側では`hint`を`None`に落とします)。
`WHERE`、`SET`ほど文脈を深く追わないのは、`VALUES`が生の`Expr`のまま保持されるという第10章以来の設計をこの章では変えなかったためです。

`CHECKPOINT`は、第36章から引き続き`Session`経由では未対応です。
`SharedDatabase`が`TxHandle`API越しに`Database::execute_checkpoint`を公開していないためで、この制約はこの章でも解消していません。

## 演習問題

### 必須課題

1. `collect_bound_expr`は`BoundExpr::Aggregate`の`arg`だけを辿り、`BoundAggregate::calls`の中の`AggregateCall`も同じ木の一部です。`PREPARE p AS SELECT SUM($1) FROM t`のような文を実際に試し、`$1`の型がどのように推論される(またはされない)かを確認してください。`check_aggregate_arg_type`のコードを読み、`SUM`が要求する型を`hint`として使うように`bind_aggregate`を変更するとしたら、どこに1行足せばよいか考えてください。
2. `execute_prepared`は`args.len()`と`prepared.param_types.len()`が一致することだけを検査し、`$1`が実際に使われているかどうかは見ていません。`PREPARE p AS SELECT 1`(プレースホルダを1つも使わない文)を`PREPARE`したうえで、`EXECUTE p()`と`EXECUTE p(1)`をそれぞれ試し、後者がエラーになる理由を`prepared.param_types.len()`の値から説明してください。
3. `Session::run_bound_autocommit`は、`execute_in_tx_bound`が成功したあと`commit_tx`を呼び、その`commit_tx`自体が失敗したらそのエラーをそのまま返します。第36章の演習問題1と同じ観点で、このとき`begin_tx`で確保した`TxHandle`が獲得していたロックがどうなるかを、`SharedDatabase::commit_tx`のコードを読んで説明してください。

### 発展課題

1. この章の`EXECUTE`は、SQL文字列として書く場合は`literal_to_value`でしか値を作れません。整数の四則演算(`EXECUTE p(1 + 1)`)を受理できるように、`ExecuteStatement::args`の型を`Vec<Expr>`へ広げ、`Session`が列参照、集約関数を含まないことを検査したうえで定数式として評価する経路を実装してください。式が列参照を含んでいた場合にどんなエラーを返すべきかも設計してください。
2. `PreparedStatement`は`Session`が`HashMap<String, PreparedStatement>`として保持しており、`Session`がdropされれば自動的に消えます。この設計を、`SharedDatabase`側に`SessionId`ごとの`PreparedStatement`集合を持たせる設計に変更したとして、どのような利点(たとえば、接続が短命に切れて再接続するたびに同じ`PREPARE`をやり直すコストを避けられる)と、どのような危険(名前空間の分離が壊れる条件)があるか考察してください。
3. `substitute_bound_expr`、`substitute_ast_expr`は、木全体を毎回複製してから値を差し込みます。`EXECUTE`が同じ`PREPARE`済み文に対して大量に呼ばれる状況で、この複製がボトルネックになるとしたら、どこがコピーのコストを支配しているか(`String`を持つ`ColumnRef`、`StringLiteral`、`Vec`を持つ`FunctionCall`の引数列など)をプロファイルして特定し、複製を避ける設計(`Rc`による共有、あるいは`Param`ノードの位置だけを`PREPARE`時に記録しておき、複製せずその場所だけを書き換える)を検討してください。
