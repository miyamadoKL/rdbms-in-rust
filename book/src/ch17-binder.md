# 第17章 Binderと名前解決

`SELECT age FROM users`という、`users`に存在しない`age`という列を参照するSQLを実行すると、このクレートは今どこでエラーに気付くべきでしょうか。

第16章までの答えは、「実行の途中」でした。

```console
minidb> SELECT age FROM users;
エラー: 評価エラー: 列'age'が見つかりません
```

構文としては何も壊れていません。
`SELECT`、識別子、`FROM`、識別子という並びは、第7章の`Parser`が受理する形そのものです。
`age`という名前が本当に`users`の列かどうかは、構文の知識だけでは判定できません。
第16章までのこのクレートは、その判定を実行の途中、`executor::project`が`table_schema.column(name)`を呼ぶ瞬間まで持ち越していました。
エラーメッセージに位置情報が無いのも、この持ち越しの結果です。
`executor`が受け取る`Expr`は、もう`Parser`の手を離れた後の値であり、`age`という文字列がソースコードのどこに書かれていたかを`executor`は覚えていません。

`WHERE`句の型検査(`infer_type`)にも、同じ構造が見えます。
第10章で見たとおり、`infer_type`は`WHERE 1`のような書き誤りを、行を1件も評価せずに静的に検出できる関数でした。
それでも置き場所は`executor`モジュールの中で、`filter`、`project`、`update`、`delete`のそれぞれが、行ループへ入る前に`check_predicate_type`を呼ぶという形で間借りしています。
名前が実在するかどうかを確かめる仕事(`schema.column(name)`)と、型が正しいかどうかを確かめる仕事(`infer_type`)と、実際に値を計算する仕事(`eval_expr`)が、同じモジュールの別々の関数として並んでいるだけで、互いを呼び出す順序に頼って正しさを保っています。

この歪みは、`SELECT *`の扱いにも表れています。
`*`は構文としては1個のトークンですが、`executor::resolve_items`は`project`が呼ばれるたびに、`*`を具体的な列参照へ展開し直していました。
`WHERE`句を経ずに`SELECT`が実行されることはないので実害は目立たないものの、「`*`が何を意味するか」という情報は、本来なら構文解析の直後に1回だけ確定してよいはずのものです。
実行のたびに展開し直していたのは、そのための場所がこのクレートのどこにも無かったからにすぎません。

この章では、この「名前を確かめる」という仕事を1つの層として独立させます。
`Parser`が組み立てる`Statement`(構文だけを確定させたAST)と、名前解決、型検査を終えた`BoundStatement`(Bound AST)を分離し、両者を変換する`Binder`を新設します。
`Database::execute`は、構文解析(`parser`)、名前解決(`binder`)、実行(`executor`)という3段階のパイプラインになります。
`SELECT age FROM users`は、この章からは実行に進む前、束縛の段階で位置情報付きのエラーになります。

```console
minidb> SELECT age FROM users;
エラー: 行1列8: 名前解決エラー: 列'age'が見つかりません
```

エラーメッセージ自体の文言(「列'age'が見つかりません」)は変えていません。
変わったのは、そのエラーが「どの層の責任で、いつ検出されたか」であり、それを利用者にも「行1列8」という形で伝えられるようになったことです。

## ASTとBound ASTを分離する

`Binder`が変換の入り口で最初に手にするのは、`src/ast.rs`に定義された第7章の`Expr::ColumnRef`です。

```rust
/// 列参照。`users.id`のような修飾名も、`qualifier`に`users`を持つことで
/// 表現できる(第17章で`Dot`トークンに対応した)。
ColumnRef {
    qualifier: Option<Ident>,
    name: String,
    span: Span,
},
```

`name`は文字列でしかなく、それが`users`の列を指すのか、単なる書き誤りなのかは、この型からは何も分かりません。
これに対応する`Binder`側の型が`BoundExpr::ColumnRef`です。
新規ファイル`src/binder.rs`を作り、次のように定義します。
`src/lib.rs`に`pub mod binder;`を追加します。

```rust
ColumnRef {
    table_ordinal: usize,
    column_index: usize,
    name: String,
    data_type: DataType,
    span: Span,
},
```

`table_ordinal`と`column_index`は、`FROM`に並ぶテーブルの何番目の、`Schema`の何番目の列かという、解決済みの座標です。
`data_type`は、その列がカタログに登録された型そのものです。
`Expr::ColumnRef`が「`id`という名前が書かれていた」という構文上の事実しか持たないのに対し、`BoundExpr::ColumnRef`は「`users`の0番目の列、`BIGINT`型」という意味上の事実を持ちます。
この違いが、AST(構文)とBound AST(意味)を分ける境界になります。

`Expr`自身に`table_ordinal`、`column_index`のようなフィールドを追加し、構文解析の直後は`None`、束縛が終わったら`Some`を書き込むという案も考えられます。
1つの型で済むので新しい列挙型を増やさずに済む一方、この案では「未束縛の`Expr`」と「束縛済みの`Expr`」を型で区別できません。
`table_ordinal`が`None`のままの`Expr`を誤って`executor`へ渡してしまっても、コンパイラは何も指摘してくれず、実行時に`unwrap`が失敗して初めて気付くことになります。
`Expr`と`BoundExpr`を別の型として分けておけば、束縛前の値を実行に渡すコードはそもそも型が合わずコンパイルできません。
「構文解析器は`Expr`しか返さない」「`executor`は`BoundExpr`しか受け取らない」という制約を、テストではなく型で保証できることが、この2つを分けた最大の理由です。

`BoundExpr`は`Expr`の他のバリアントとも1対1に対応しますが、`UnaryOp`、`BinaryOp`、`FunctionCall`、`Cast`にはそれぞれ`data_type: DataType`が加わります。
同じ`src/binder.rs`に、`BinaryOp`を例に次のように追記します。

```rust
BinaryOp {
    op: BinaryOperator,
    lhs: Box<BoundExpr>,
    rhs: Box<BoundExpr>,
    data_type: DataType,
    span: Span,
},
```

型は`Binder`が式を組み立てる際に一度だけ計算し、以後(`executor`での評価)は再計算しません。
`BoundExpr`には`data_type(&self) -> Option<DataType>`というメソッドがあり、`NullLiteral`とそれを素通しする`Paren`の入れ子だけが`None`(型が定まらない)を返します。
値そのものではなく型だけを問う`bind_predicate`や`executor::project`は、この関数を呼ぶだけで済み、式木をもう一度たどり直す必要がありません。

`SELECT`全体は、同じ`src/binder.rs`に定義する`BoundSelect`という型に変換します。

```rust
pub struct BoundSelect {
    pub tables: Vec<BoundTableRef>,
    pub projection: Vec<BoundSelectItem>,
    pub predicate: Option<BoundExpr>,
    pub span: Span,
}
```

`SelectStatement`が持っていた`items: Vec<SelectItem>`(`*`か式かのどちらかを表す列挙型)は、ここでは`projection: Vec<BoundSelectItem>`という、常に1つの式と1つの出力列名を持つ並びに変わります。
`*`はこの変換の途中で、`FROM`先の具体的な列参照へ展開され尽くします。
束縛を終えた`BoundSelect`を後段(`executor`)がどれだけ読んでも、`*`という記法そのものにはもう出会いません。
構文上の省略記法が、意味解決の時点で確定した情報へ置き換わっているのです。

## テーブル名とAliasを解決する

`Binder`がテーブル名を解決するには、名前からテーブル定義を引ける何かが必要です。
このクレートには、その役目を果たす型がすでに2つあります。
インメモリモードの`Catalog`(第9章)と、永続モードの`Storage`(第15章)です。
どちらも`table(&self, name: &str) -> Option<&TableInfo>`という同じ形のメソッドを持っているので、この共通部分を`src/binder.rs`にtraitとして取り出します。

```rust
pub trait CatalogLookup {
    fn table(&self, name: &str) -> Option<&TableInfo>;
}

impl CatalogLookup for Catalog {
    fn table(&self, name: &str) -> Option<&TableInfo> {
        Catalog::table(self, name)
    }
}

impl CatalogLookup for Storage {
    fn table(&self, name: &str) -> Option<&TableInfo> {
        Storage::table(self, name)
    }
}
```

`Binder`は`&dyn CatalogLookup`を受け取るだけで、今`Database`がどちらのバックエンドで動いているかを一切意識しません。
第16章の`Database::table_info`は、この分岐を`match`で書き下ろしていましたが、この章ではその分岐を型の側(trait)へ移しました。
`Database`自身が持つ`Backend`は、第16章の判断どおり引き続き`enum`のままにしてあります。
`Memory`と`Disk`のどちらで動くかは`Database::memory()`か`Database::open()`かで起動時に1回だけ決まり、実行中に入れ替わることが無いという事情は、この章になっても変わりません。
一方`Binder`にとって、`Catalog`と`Storage`は「テーブル名から`TableInfo`を引ける」という1つの操作しか要らない相手であり、`Database`のように両者の差を`match`で読み比べたい理由がありません。
同じ「2つの実装を切り替える」問題でも、呼び出し側が知りたい情報の量によって`enum`とtraitのどちらが素直かが変わる、という一例になっています。

テーブル名の解決自体は、`src/binder.rs`の`resolve_table`という1つの関数に集まります。

```rust
fn resolve_table(&self, table: &Ident, alias: Option<&Ident>) -> DbResult<BoundTableRef> {
    let info = self
        .catalog
        .table(&table.name)
        .ok_or_else(|| self.error_at(table.span, format!("テーブルが見つかりません: {}", table.name)))?;
    Ok(BoundTableRef {
        table_id: info.id,
        table_name: info.name.clone(),
        alias: alias.map(|a| a.name.clone()),
        schema: info.schema.clone(),
    })
}
```

見つからなければ`self.error_at`が位置情報付きの`DbError::Bind`を作ります。
`error_at`は第7章の`Parser::error_at`と同じ形で、`Span`のバイトオフセットを`lexer::line_col`で行、列に変換するだけの関数です。

```console
minidb> SELECT id FROM does_not_exist;
エラー: 行1列16: 名前解決エラー: テーブルが見つかりません: does_not_exist
```

`Alias`(`FROM users AS u`)に対応するには、まずParserにその構文を追加する必要があります。
`SelectStatement`の`from`フィールドは、これまで`Option<Ident>`(テーブル名だけ)でしたが、この章から`Option<FromClause>`に変えます。
`src/ast.rs`に次の`FromClause`を追加します。

```rust
pub struct FromClause {
    pub table: Ident,
    pub alias: Option<Ident>,
    pub span: Span,
}
```

`AS`というキーワードは第8章の`CAST(expr AS type)`ですでにLexerが認識しているので、`FROM`の直後に`AS`が続けば`Alias`を読む、という分岐を1つ足すだけで済みます。
`u.id`のような修飾列参照には、もう1つ構文上の穴がありました。
第7章の`Lexer`は`.`をどのTokenにも対応させておらず、`ast.rs`のコメントにも「`users.id`のような修飾名は、Lexerが`.`を扱わないため対象外」と明記されていました。
この章で`TokenKind::Dot`を追加し、識別子の直後に`.`が続けば、もう1つ識別子を読んで`Expr::ColumnRef`の`qualifier`に詰めます。
この分岐は`src/parser.rs`に追加します。

```rust
if *self.peek_kind() == TokenKind::Dot {
    self.advance();
    let column = self.expect_ident()?;
    let span = Span::new(start_span.start, column.span.end);
    Ok(Expr::ColumnRef {
        qualifier: Some(Ident { name, span: start_span }),
        name: column.name,
        span,
    })
}
```

`resolve_table`が返す`BoundTableRef`は、`qualifier()`という補助メソッドを持ちます。
`src/binder.rs`の`BoundTableRef`に、次のメソッドを定義します。

```rust
pub fn qualifier(&self) -> &str {
    self.alias.as_deref().unwrap_or(&self.table_name)
}
```

`Alias`があればそれを、無ければテーブル名そのものを返します。
`FROM users AS u`のもとでは`users.id`という修飾は使えなくなり、`u.id`だけが通ります。
これは標準SQLが定める規則(`AS`は元のテーブル名を覆い隠す)であり、`qualifier()`が`Alias`を無条件に優先することでそのまま実現されています。

`BoundSelect::tables`は`Vec<BoundTableRef>`という、要素数を`1`に固定しない型にしました。
この章の`Parser`は`FROM`に1つのテーブルしか書けないので、`tables`は常に0個か1個のどちらかにしかなりません。
それでも`Option<BoundTableRef>`ではなく`Vec`を選んだのは、第22章で`JOIN`が入ったときに、この型をそのまま複数テーブルへ拡張できるようにするためです。
テーブルが増えても、`table_ordinal`という座標軸はすでに用意されています。

`Alias`と修飾列参照を組み合わせると、次のように書けます。

```console
minidb> SELECT u.id FROM users AS u WHERE u.name = 'Alice';
u.id
----
1
(1 row)
```

`u`という短い名前は、`users`というテーブル名の代わりに列参照を書くためだけの一時的な名前であり、テーブル自体の名前を変えるわけではありません。
`Binder`にとっては、`resolve_table`が返す`BoundTableRef::alias`にこの`u`が入っているかどうかだけの違いであり、以後の列参照の解決(`resolve_column`)は`table_name`と`alias`のどちらであっても`qualifier()`という同じ入り口から見ます。

## 列名を解決し、曖昧な参照を検出する

列参照の解決は`resolve_column`が担います。
修飾子(`qualifier`)の有無で経路が分かれます。

修飾子が無い場合、`src/binder.rs`の`resolve_column`は`tables`を先頭から順に見て、その列名を持つテーブルを探します。

```rust
let matches: Vec<(usize, usize, DataType)> = tables
    .iter()
    .enumerate()
    .filter_map(|(table_ordinal, table)| {
        table
            .schema
            .index_of(name)
            .map(|column_index| (table_ordinal, column_index, table.schema.columns()[column_index].data_type))
    })
    .collect();

match matches.as_slice() {
    [] => Err(self.error_at(span, format!("列'{name}'が見つかりません"))),
    [(table_ordinal, column_index, data_type)] => Ok(BoundExpr::ColumnRef { .. }),
    _ => {
        // 複数テーブルに同名列がある場合はここに来る。
        Err(self.error_at(span, format!("列'{name}'は複数のテーブルに存在するため曖昧です: {}", owners.join(", "))))
    }
}
```

この`matches`は、最初に一致したテーブルで探索を打ち切らず、`tables`全体を最後まで見てから集めます。
最初の1件で打ち切ってしまうと、2番目以降のテーブルに同名列がもう1つあっても気付けず、曖昧なはずの参照を誤って解決済みとして扱ってしまいます。
「1件見つかったら即座に返す」のではなく「全件集めてから件数で分岐する」という、`resolve_column`が愚直に見える形を選んでいるのはこのためです。

一致が0件なら未知の列、1件なら解決成功、2件以上なら曖昧という3分岐になります。
この3分岐目、複数テーブルにまたがる曖昧な列参照の検出は、`JOIN`が無ければ意味を持たない機能に見えるかもしれません。
実際、この章の`Parser`は`FROM`に1テーブルしか書けないため、`tables`の要素数は常に1以下であり、`SELECT`文からこの分岐へ到達することはありません。
それでもこの分岐を今のうちに書いておくのは、`resolve_column`という関数自体を「`tables`の要素数を1個と決め打たない」形で設計しておけば、第22章で`JOIN`が`tables`を複数要素にしたときに、この関数を書き直す必要が無いからです。
テストでは、`resolve_column`を`Binder`の外から直接呼び、2つのテーブルが同じ列名`id`を持つ状況を人工的に作って、この分岐が実際に機能することを確認しています。

修飾子がある場合(`u.id`)は、`src/binder.rs`の同じ`resolve_column`が、まず`tables`の中から`qualifier() == "u"`のテーブルを探し、見つかったテーブルの中だけで列名を探します。

```rust
let (table_ordinal, table) = tables
    .iter()
    .enumerate()
    .find(|(_, table)| table.qualifier() == qualifier.name)
    .ok_or_else(|| self.error_at(qualifier.span, format!("テーブルまたはAlias'{}'が見つかりません", qualifier.name)))?;
let column_index = table
    .schema
    .index_of(name)
    .ok_or_else(|| self.error_at(span, format!("列'{name}'は'{}'に存在しません", qualifier.name)))?;
```

修飾子付きの参照は、そもそも探索範囲を1つのテーブルへ絞り込む行為なので、この経路には曖昧さの判定が要りません。

```console
minidb> SELECT nickname FROM users;
エラー: 行1列8: 名前解決エラー: 列'nickname'が見つかりません
minidb> SELECT id FROM users AS u WHERE users.id = 1;
エラー: 行1列33: 名前解決エラー: テーブルまたはAlias'users'が見つかりません
```

## `*`の展開と式の型検査をBinderへ統合する

`SELECT *`の展開は、`src/binder.rs`の`bind_select`が射影対象リストを組み立てる中で行います。

```rust
SelectItem::Wildcard { span } => {
    if tables.is_empty() {
        return Err(self.error_at(span, "*はFROMを伴うSELECTでのみ使えます"));
    }
    for (table_ordinal, table) in tables.iter().enumerate() {
        for (column_index, column) in table.schema.columns().iter().enumerate() {
            projection.push(BoundSelectItem {
                expr: BoundExpr::ColumnRef { table_ordinal, column_index, .. },
                output_name: column.name.clone(),
            });
        }
    }
}
```

展開の順序は「テーブルの登場順、各テーブル内は列の宣言順」と決めてあります。
この章では`tables`が高々1個なので実質的には「列の宣言順」と同じ結果にしかなりませんが、この順序規則自体は複数テーブルを前提にして書いてあるので、第22章で`JOIN`が入っても書き直しは要りません。

式の型検査は、`bind_expr`という1つの再帰関数に集約しました。
規則そのものは第10章の`executor::infer_type`と同一で、算術演算は両辺が`BIGINT`か型未定の`NULL`であること、比較演算は両辺が同じ型であること、論理演算は両辺が`BOOLEAN`か`NULL`であること、関数呼び出しは`FunctionRegistry`に登録された引数の型と一致することを、式木全体にわたって再帰的に検査します。
`WHERE`句には、この検査に加えて「最終的な型が`BOOLEAN`または型未定の`NULL`であること」をもう1段検査する、`src/binder.rs`の`bind_predicate`を通します。

```rust
fn bind_predicate(&self, expr: &Expr, tables: &[BoundTableRef]) -> DbResult<BoundExpr> {
    let bound = self.bind_expr(expr, tables)?;
    match bound.data_type() {
        Some(DataType::Boolean) | None => Ok(bound),
        Some(other) => Err(self.error_at(
            bound.span(),
            format!("WHERE句はBOOLEANを返す式である必要があります: 式の型は{other}です"),
        )),
    }
}
```

第10章がこの検査を`executor`に置いた理由は、当時は「実行の前段」という層がまだ存在せず、`Database::execute`が`executor`の演算子を直接呼ぶだけの構造だったからです。
しかし`infer_type`が実際にしていたことは、式の評価(`Value`を計算すること)ではなく、式の意味(列参照が指す列、演算子が要求する型)を決めることであり、これは名前解決と同じ層の仕事です。
`executor`に残したままだと、列参照の解決(`schema.column(name)`)と型検査(`infer_type`)が、`Binder`が新設する列インデックス、型付きの`BoundExpr`と二重に、しかも別々の場所で行われることになります。
この章で`infer_type`と`check_predicate_type`を`Binder`へ統合し、`executor`からは削除しました。
`executor`の`filter`、`project`は、束縛済みで型検査済みの`BoundExpr`だけを受け取るようになり、`rows`が空でも`WHERE 1`のような書き誤りを見逃さないための事前検査を、行ループのたびに呼び直す必要も無くなりました。

```console
minidb> SELECT id FROM users WHERE 1;
エラー: 行1列28: 名前解決エラー: WHERE句はBOOLEANを返す式である必要があります: 式の型はBIGINTです
```

このエラーは、`users`が空でも、行を何件持っていても同じ文言、同じ位置で返ります。
第10章の`check_predicate_type`が持っていた「行の有無に関わらず同じ検査結果になる」という不変条件は、検査の場所を`Binder`に変えても、束縛が実行より必ず先に走るという順序によってそのまま保たれています。

`executor::predicate_matches`(`WHERE`の評価結果を`bool`へ変換する関数)だけは、`BOOLEAN`でも`NULL`でもない値に出会った場合の分岐を消さずに残しました。
`Binder`を経由しない呼び出し経路を想定した保険ではありません。
`Database::execute`は必ず`Binder`を経由するため、そのような経路はこの章にはありません。
残しているのは、「`BoundExpr::data_type()`が`Some(Boolean)`または`None`である」という事実を、Rustの型システムがコンパイル時に保証してはくれないからです。
仮に`Binder`側にバグがあって型検査をすり抜けたとしても、`executor`が`BOOLEAN`でない値を暗黙に「マッチしない」側へ丸めてしまう(誤りを隠してしまう)ことだけは避けたい、という最終防衛線としてこの分岐を残しました。

`Aggregate`(`COUNT`、`SUM`等)の使用位置の検査は、この章では行いません。
`SELECT`の対象式にだけ許し、`GROUP BY`の無い列との共存を禁じるといった規則は、`Aggregate`という式の種類自体が第21章まで実装されないため、検査する対象がまだ存在しません。

## Database::executeをparse→bind→executeへ再編する

`src/database.rs`の`Database::execute`は、構文解析の直後に束縛を挟む1行が増えました。

```rust
pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
    let statement = crate::parser::parse_statement(sql)?;
    let bound = self.bind(statement, sql)?;
    match bound {
        BoundStatement::Select(select) => self.execute_select(&select),
        BoundStatement::CreateTable(create) => self.execute_create_table(&create),
        BoundStatement::DropTable(drop) => self.execute_drop_table(&drop),
        BoundStatement::Insert(insert) => self.execute_insert(insert),
        BoundStatement::Update(update) => self.execute_update(update),
        BoundStatement::Delete(delete) => self.execute_delete(delete),
    }
}
```

`bind`自身は、`src/database.rs`の中で`Backend`の分岐を1箇所に閉じ込めるだけの薄い関数です。

```rust
fn bind(&self, statement: Statement, sql: &str) -> DbResult<BoundStatement> {
    match &self.backend {
        Backend::Memory { catalog, .. } => Binder::new(catalog, &self.functions, sql).bind(statement),
        Backend::Disk { storage } => Binder::new(storage, &self.functions, sql).bind(statement),
    }
}
```

`catalog`も`storage`も`CatalogLookup`を実装しているので、`Binder::new`はどちらを渡されても同じように動きます。

`CREATE TABLE`だけは`Binder`を素通りします。
`BoundStatement::CreateTable`はASTの`CreateTableStatement`をそのまま持ち回るバリアントで、`Binder`が行う名前解決とは性質が違う仕事をする文だからです。
`Binder`が解決するのは、すでにカタログにある名前を指す参照(`SELECT`の列、`INSERT`の行き先、`WHERE`の述語)ですが、`CREATE TABLE`が持つ名前(テーブル名、列名)はこれから新しく作る名前であり、突き合わせるべき既存のエントリがありません。
列の型名(`BIGINT`等)をテキストから`DataType`へ解決する処理も、`Database::execute_create_table`にそのまま残しました。
これは既存の列への参照ではなく、新しい`Schema`を組み立てる作業の一部であり、`Binder`の名前解決とは扱う対象が異なります。
`DROP TABLE`は、テーブルが存在することだけを`Binder`(`bind_drop_table`)が事前に検査し、位置情報付きの`DbError::Bind`にします。
実行(`Catalog::drop_table`、`Storage::drop_table`)は引き続き名前で削除するので、`BoundStatement::DropTable`もASTのバリアントをそのまま返します。

`INSERT`、`UPDATE`、`DELETE`は、それぞれ専用の`Bound`型を持ちます。
`src/binder.rs`に、次の`BoundInsert`を定義します。

```rust
pub struct BoundInsert {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub columns: Option<Vec<usize>>,
    pub rows: Vec<Vec<Expr>>,
    pub span: Span,
}
```

`columns`は、`INSERT INTO users (name, id) VALUES (...)`のような明示的な列リストを、`Schema`上の列インデックスへ解決した並びです。
未知の列名や、同じ列を2回指定する重複は、この束縛の時点で位置情報付きの`DbError::Bind`になります。
第10章の`expand_to_schema`(`executor`モジュール)は、この検査を実行のたびに行っていましたが、この章からは検査済みの`Vec<usize>`を受け取るだけの、単純な並べ替えに専念できるようになりました。

```console
minidb> INSERT INTO users (id, id) VALUES (1, 2);
エラー: 行1列24: 名前解決エラー: 列'id'がINSERTの列リストに重複しています
```

`VALUES`の各行(`rows`)だけは、`Expr`のまま`BoundInsert`に残ります。
`VALUES`は既存の行を参照する構文を持たないので、列参照が現れようが無く、`Binder`が解決すべき名前もそこにはありません。

`UPDATE`、`DELETE`は、テーブル名の解決に加えて、`SET`の対象列と`WHERE`の述語を束縛します。
`SET`の対象列を束縛する`bind_assignment`は、`src/binder.rs`に次のように定義します。

```rust
fn bind_assignment(&self, assignment: &Assignment, tables: &[BoundTableRef]) -> DbResult<BoundAssignment> {
    let column_index = tables[0].schema.index_of(&assignment.column.name).ok_or_else(|| {
        self.error_at(assignment.column.span, format!("列'{}'が見つかりません", assignment.column.name))
    })?;
    let value = self.bind_expr(&assignment.value, tables)?;
    Ok(BoundAssignment { column_index, value })
}
```

`SET`の右辺(`value`)は、対象テーブルの列を参照できる式として束縛します。
`UPDATE users SET age = age + 1`の右辺`age + 1`は、対象テーブル自身の列`age`を参照する、ごく普通の式として扱われます。

`BoundInsert`、`BoundUpdate`、`BoundDelete`は、いずれも`schema: Schema`を値として(カタログからの借用ではなく複製として)持ちます。
第16章までの`execute_insert`は、`table_info`という`&TableInfo`を`self.table_info(...)`から借りたあと、`&mut self.backend`を借用する直前に`.clone()`していました。
`self`を不変借用したまま`&mut self.backend`を取ることはできないため、複製してから借用を手放す、という手順を`execute_insert`、`execute_update`、`execute_delete`のそれぞれが個別に書く必要があったのです。
この章では、その複製を`Binder::resolve_table`の内部(`schema: info.schema.clone()`)へ1箇所にまとめました。
`Database::execute_insert`が受け取る`BoundInsert`はすでに独立した値なので、`&mut self.backend`をいつ借りても構いません。
借用の都合に合わせて複製のタイミングを呼び出し側ごとに調整する、という同じ形のコードが3箇所に散らばっていた状態が、この章で1箇所に集まりました。

`src/executor.rs`側の関数は、生の`Expr`ではなく`BoundExpr`、`BoundSelectItem`、`BoundAssignment`を受け取るようになりました。

```rust
pub fn filter(
    schema: &Schema,
    functions: &FunctionRegistry,
    rows: Vec<Tuple>,
    predicate: &BoundExpr,
) -> DbResult<Vec<Tuple>> {
    let mut kept = Vec::with_capacity(rows.len());
    for tuple in rows {
        let row = Row::new(schema, &tuple);
        let value = eval_bound_expr(predicate, functions, Some(&row))?;
        if predicate_matches(value)? {
            kept.push(tuple);
        }
    }
    Ok(kept)
}
```

`BoundExpr`を評価する`eval::eval_bound_expr`は`src/eval.rs`に定義されており、列参照を名前ではなく`column_index`で引きます。

```rust
BoundExpr::ColumnRef { table_ordinal, column_index, name, .. } => {
    debug_assert_eq!(*table_ordinal, 0, "...");
    match row {
        Some(row) => row
            .get_index(*column_index)
            .cloned()
            .ok_or_else(|| DbError::Eval(format!("列'{name}'が見つかりません"))),
        None => Err(DbError::Eval(format!("列参照'{name}'は行を伴わない文脈では使えません"))),
    }
}
```

第10章の`eval_expr`は`Row::get(name)`という、列名をもう一度`Schema`と突き合わせる経路を使っていました。
`Binder`が解決した`column_index`を使う`get_index`は、この突き合わせをせず、`Tuple`の該当する位置を直接読むだけです。
名前の文字列比較は、束縛の時点で1回だけ行えばよく、行ごとに繰り返す理由がありません。
`table_ordinal`は今のところ常に`0`ですが(`FROM`は1テーブルしか持てないため)、この値は捨てずに`BoundExpr`へ残してあります。
第22章で`JOIN`が複数テーブルの行を同時に扱うようになれば、`row`は1個の`Row`ではなく、`table_ordinal`で選ぶ複数の`Row`の並びに置き換わります。

## テストで確認する

`src/binder.rs`のテストモジュールには、この章が扱う名前解決、型検査のそれぞれについて単体テストを追加しました。

```rust
#[test]
fn unknown_column_is_rejected_with_position() {
    let catalog = users_catalog();
    let (line, column) = bind_err_position("SELECT nickname FROM users", &catalog);
    assert_eq!((line, column), (1, 8));
}
```

未知のテーブル、未知の列、`WHERE`句の型不一致は、いずれも発生位置の行、列を固定するテストにしてあります。
曖昧な列参照は、`src/binder.rs`のテストモジュールで`resolve_column`を`Binder`の外から直接呼び、2つのテーブルが同じ列名を持つ状況を人工的に作って検証しました(この章の`Parser`では`FROM`に複数テーブルを書けないため、SQL文からこの分岐を踏むことはまだできません)。

```rust
#[test]
fn ambiguous_column_is_rejected_across_multiple_tables() {
    let tables = vec![
        BoundTableRef { table_id: TableId(0), table_name: "a".to_string(), alias: None, schema: schema.clone() },
        BoundTableRef { table_id: TableId(1), table_name: "b".to_string(), alias: None, schema },
    ];
    let result = binder.resolve_column(None, "id", Span::new(0, 2), &tables);
    assert!(matches!(result, Err(DbError::Bind { .. })));
}
```

`SELECT *`の展開、`Alias`、修飾列参照(`u.id`)は、実際に`Binder::bind`を通した`BoundSelect`の中身を検査する形でテストしています。
`AS`で与えた`Alias`がテーブル名そのものによる修飾を覆い隠すこと(`alias_hides_the_original_table_name`)も、標準SQLの規則として個別に確認しました。

`executor`モジュールのテストは、`filter`、`project`、`update`に渡す`BoundExpr`、`BoundSelectItem`、`BoundAssignment`を、実際に`Binder`を通して作るように書き換えました。
第10章までのテストは`expr("id = 1")`のようにASTを直接組み立てていましたが、この章からは`Binder`を経由してしか`BoundExpr`を作れないため、演算子のテストも「束縛してから実行する」という順序をそのまま踏みます。

`database`モジュールのテストのうち、型検査、未知の名前に関するものは、返るエラーの種類を`DbError::Eval`から`DbError::Bind`へ更新しました。
たとえば`SELECT id FROM users WHERE 1`は、以前は行ループの手前で`check_predicate_type`(`executor`)が`DbError::Eval`を返していましたが、この章からは束縛の時点で`Binder`が`DbError::Bind`を返します。
一方、ゼロ除算のように値そのもの(行のデータ)に依存する失敗は、静的には判定できないため`DbError::Eval`のまま実行時に検出されます。

`cargo test`を実行すると、既存の第1部、第2部由来のテストを含め、`cargo test --lib`で337件の単体テスト、`differential`、`golden`、`persistence`の統合テストがすべて緑になります。
`golden`テストのうち、テーブル未検出を確認する`011_drop_table_missing`は、この章のエラー文言(位置情報付き)に合わせて期待値を更新しました。

```console
$ cargo test
test result: ok. 337 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.55s
...
test golden_tests_pass ... ok
```

## 演習問題

### 必須課題

1. `resolve_column`の曖昧列検出(2件以上マッチする分岐)は、現在のテストでは`Binder`の外から`resolve_column`を直接呼ぶことでしか到達できません。第7章の`Parser`を拡張し、`FROM users, orders`のようにカンマ区切りで複数のテーブルを書けるようにしたうえで(`JOIN`条件自体は付けなくてかまいません)、実際にSQL文から曖昧な列参照エラーを発生させるテストを書いてください。`BoundSelect::tables`と`resolve_column`のどちらを変更する必要があるか、変更が要らないとすればなぜかを確認しながら進めてください。
2. `bind_insert`は、明示された列名を`Vec<usize>`へ解決するだけで、`VALUES`の各行の個数がその列数と一致するかどうかまでは検査しません(この検査は`executor::expand_to_schema`が実行時に行っています)。この検査を`Binder`へ前倒しした場合、`INSERT INTO t (a, b) VALUES (1), (2, 3)`のように行ごとに個数が異なる`VALUES`をどう扱うべきか考え、実装してください。
3. `CAST(expr AS type)`の`Binder`による型検査は、`type_name`が既知の型かどうかしか見ておらず、`expr`の型と`type`の組み合わせが妥当か(`eval_cast`の対応表に載っているか)までは検査しません。この組み合わせの妥当性は値に依存しないため、原理的には`Binder`で静的に検査できます。`bind_expr`の`Cast`分岐にこの検査を追加し、`CAST(1 AS BOOLEAN)`のような対応外の組み合わせが、位置情報付きの`DbError::Bind`として実行前に拒否されるようにしてください。

### 発展課題

1. `BoundTableRef::qualifier()`は、`Alias`があれば無条件にそれを優先します。標準SQLの一部の実装は、`Alias`が既存の別のテーブル名やAliasと衝突する場合にエラーにします(`FROM users AS orders, orders`のような場合)。この検査を`bind_from`(または、複数テーブルに対応させた版)に追加してください。
2. この章の`Binder`は、`Database`が持つ`Catalog`または`Storage`への参照を`&dyn CatalogLookup`として借用します。`Binder::bind`の戻り値である`BoundStatement`は、この借用の生存期間に縛られない(`schema: Schema`を複製として持つ)ことを、実際にコンパイラのエラーメッセージを確認しながら検証してください。もし`BoundTableRef`が`schema: &'a Schema`のような借用を持っていたら、`Database::execute`の`bind`から`execute_insert`、`execute_update`、`execute_delete`(`&mut self.backend`を要求する)へ`BoundStatement`を渡す箇所で何が起きるか、実際に型を変更して確かめてください。
