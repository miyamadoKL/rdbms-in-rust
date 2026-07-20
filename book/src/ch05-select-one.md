# 第5章 `SELECT 1`を実行する

`cargo doc --open`でminidbのAPIを開いてみます。
`DbError`、`PageId`、`TableId`、`TransactionId`、そして`Column`、`DataType`、`Schema`、`Tuple`、`Value`。
前章までに揃えた型が並んでいますが、そこに`Database`という名前はどこにもありません。

試しに、`SELECT 1`という文字列を渡したら`Value::BigInt(1)`が1件返ってくるプログラムを書こうとしてみます。

```rust
let value = minidb::???; // "SELECT 1"をどう渡せばいいのか、渡す先がない
```

`Value`はすでにあります。
`Schema`も`Tuple`も、値がSchemaに適合するかどうかを検査する`validate_tuple`もあります。
それでも、SQLの文字列をこれらの型に変換する経路が1本もありません。
`src/main.rs`を見ても、挨拶を1行`println!`するだけの関数が置いてあるばかりです。

型だけがあってSQLを受け付けないこの状態は、パーツは揃っているのに、それらをつなぐ配線が1本も引かれていない基板に似ています。
この章の目標は、配線を1本引くことです。

## 最初に引く配線を1本に絞る

配線の引き方には2通りの順番が考えられます。

1つは、字句解析器(第6章)、構文解析器(第7章)、型とNULLと式評価(第8章)、カタログとDDL(第9章)、インメモリ表とDML(第10章)を、それぞれ単体で作り込んでから最後に結線する順番です。
教材の章立てとしては自然に見えますが、実装の順番としてこれを採ると、最初の`cargo test`が通るまでに5章分の作業を要します。
どこかの型設計が誤っていた場合、その誤りは最後の結線の段階になって初めて発覚します。

もう1つが、この章で採る順番です。
字句解析も構文解析もカタログもDMLも、正しいものはまだ作らずに済ませます。
その代わり、SQL文字列を受け取ってから結果の1行が返るまでの経路を、仮の実装でよいので最初から最後まで1本通します。
この「機能ごとの層を積み上げる代わりに、入力から出力までを貫く最小の経路を先に通す」やり方を、**縦切り**と呼びます。

縦切りを先に作る理由は、`Database`の公開APIの形を早い段階で確定できるからです。
`db.execute(sql)`が何を引数に取り、何を返すべきかは、字句解析器の内部実装をどれだけ精緻にしても分かりません。
分かるのは、実際に1本の経路を端から端まで動かしたときだけです。
経路の途中を仮の実装で埋めてでも、まず両端(SQL文字列と結果の1行)を決めてしまえば、第6章以降はその中身を本物に差し替えていく作業に専念できます。

そのため、この章で書く構文解析は次章以降で丸ごと置き換わる前提の仮実装です。
`SELECT`に続く整数リテラル、真偽値リテラル、整数どうしの加算しか読めず、`FROM`句もカッコも掛け算も扱いません。
本物の字句解析器は第6章、本物の構文解析器は第7章で実装します。

## 守るべき不変条件

縦切りとはいえ、この章の実装が満たすべき条件は3つあります。

1. **`execute`の呼び出しはパニックで落ちない**：対応できない入力を渡されても、プロセスを異常終了させず`DbResult`として呼び出し元に返す
2. **返る行は必ずSchemaに適合する**：`QueryResult`の各行は、その回の`Schema`が定める列数、型、nullable制約を満たす。第4章の`Tuple::new`(内部で`validate_tuple`を呼ぶ)を経由しない行は作らない
3. **対応外の構文は明示的に拒否する**：この仮実装が読めない構文を、値を捨てて黙って無視したり、誤った解釈で処理したりしない。読めない入力は`DbError::Parse`として伝える

1つめは、REPLやサーバーとして動かし続けるプログラムにとって特に重要です。
1件のクエリの構文エラーでプロセス全体が落ちるようでは、対話的に使うツールとして成立しません。

## 最小実装

### 仮の構文解析器

`SELECT`に続く式だけを解析する仮実装を、新規作成する`src/toy_sql.rs`に置きます。
まず、式を表す`ToyExpr`を定義します。

```rust
/// この仮実装が扱える式。整数リテラル・真偽値リテラル・整数の加算のみを持つ。
#[derive(Debug, Clone, PartialEq)]
pub enum ToyExpr {
    /// 整数リテラル。
    IntLiteral(i64),
    /// 真偽値リテラル。
    BoolLiteral(bool),
    /// 整数どうしの加算。両辺は整数を返す式であることを構文解析側が保証する。
    Add(Box<ToyExpr>, Box<ToyExpr>),
}
```

`ToyExpr`という名前にしたのは、これが本物のASTではないことを型名からも分かるようにするためです。
第7章で本物のASTを設計するとき、この型は残さず削除します。

あわせて`src/lib.rs`に次の行を加え、このモジュールをクレート内だけで使う非公開モジュールとして登録します。
公開APIとして外部に見せる必要がないため、`pub`は付けません。

```rust
mod toy_sql;
```

評価は、同じ`src/toy_sql.rs`に定義する`eval`が受け持ちます。

```rust
impl ToyExpr {
    /// 式を評価して`Value`を返す。
    ///
    /// 整数どうしの加算がオーバーフローする場合は`DbError::Eval`を返す。
    pub fn eval(&self) -> DbResult<Value> {
        match self {
            ToyExpr::IntLiteral(v) => Ok(Value::BigInt(*v)),
            ToyExpr::BoolLiteral(v) => Ok(Value::Boolean(*v)),
            ToyExpr::Add(lhs, rhs) => match (lhs.eval()?, rhs.eval()?) {
                (Value::BigInt(l), Value::BigInt(r)) => l
                    .checked_add(r)
                    .map(Value::BigInt)
                    .ok_or_else(|| DbError::Eval(format!("整数オーバーフロー: {l} + {r}"))),
                _ => unreachable!(
                    "ToyExpr::Addの両辺はparse_selectが整数リテラルにしか構築しない"
                ),
            },
        }
    }
}
```

`Add`の評価は`(Value::BigInt(l), Value::BigInt(r))`だけを扱い、それ以外の組み合わせは`unreachable!`にしています。
真偽値どうしの加算のような組み合わせは、この関数の中で弾いているわけではありません。
弾いているのは構文解析側です。
`parse_select`が`ToyExpr::Add`を組み立てられるのは、両辺がともに整数リテラルであると確認できたときだけに限定するので、`eval`側では「両辺が整数である」という前提を安全に置けます。

一方で、両辺が整数であることは、その加算結果が`i64`の範囲に収まることまでは保証しません。
素の`+`演算子は、`debug`ビルドでは範囲を超えた瞬間にパニックします。
`SELECT`文1本の入力がプロセス全体を落とすのは、この章の不変条件1への違反です。
そのため`Add`の評価には`checked_add`を使い、範囲を超えたら`None`を`DbError::Eval`に変換して呼び出し元へ返すようにしています。
オーバーフローの扱いをどう設計するかは第8章で改めて詰めますが、この仮実装の段階でも「パニックさせない」という条件だけは満たしておく必要があります。

`src/toy_sql.rs`に置く構文解析の入り口が`parse_select`です。

```rust
/// `SELECT <式> [;]`を解析する。
///
/// 対応する構文は次のみ。
/// - 整数リテラル: `1`、`42`
/// - 真偽値リテラル: `true`、`false`
/// - 整数どうしの加算: `1 + 2`
///
/// これ以外の入力(`SELECT`で始まらない、対象式が空、加算の片方が整数でない等)は
/// すべて`DbError::Parse`を返す。
pub fn parse_select(sql: &str) -> DbResult<ToySelect> {
    let sql = sql.trim();
    let rest = strip_keyword(sql, "SELECT")
        .ok_or_else(|| DbError::Parse(format!("SELECT文ではありません: {sql:?}")))?;
    let rest = rest.trim();
    let rest = rest.strip_suffix(';').unwrap_or(rest).trim();
    if rest.is_empty() {
        return Err(DbError::Parse(
            "SELECTの対象式がありません".to_string(),
        ));
    }

    let expr = parse_expr(rest)?;
    Ok(ToySelect {
        expr_text: rest.to_string(),
        expr,
    })
}
```

`ToySelect`は`expr_text`(`SELECT`と`;`を取り除いた対象式の元のテキスト)と、解析済みの`expr`を持つ構造体です。
`expr_text`をあとで捨てずに残しているのは、結果の列名として使うためです。
`SELECT 1 + 2;`を実行したとき、返ってくる列の名前を`"1 + 2"`にしたいので、パースの過程で得られる元のテキストをそのまま持ち運びます。
`SELECT`キーワードの判定は、`strip_keyword`という小さな関数で大文字小文字を無視して行っています。

同じ`src/toy_sql.rs`に加える式の解析は、`+`で文字列を分割するだけの単純な作りです。

```rust
/// `+`で連結された式を解析する。項が1つなら`parse_term`にそのまま委ねる。
fn parse_expr(src: &str) -> DbResult<ToyExpr> {
    let terms: Vec<&str> = src.split('+').map(str::trim).collect();
    if terms.iter().any(|t| t.is_empty()) {
        return Err(DbError::Parse(format!("式を解析できません: {src:?}")));
    }

    if let [only] = terms.as_slice() {
        return parse_term(only);
    }

    // 2項以上の加算は、このミニ実装では整数リテラルどうしにしか対応しない。
    // 真偽値の加算(`SELECT true + 1;`)のような入力は、ここで弾いて
    // `ToyExpr::Add`の評価側に不正な形が渡らないようにする。
    let mut sum: Option<ToyExpr> = None;
    for term in &terms {
        let n = term
            .parse::<i64>()
            .map_err(|_| DbError::Parse(format!("加算は整数リテラルにのみ対応しています: {term:?}")))?;
        let next = ToyExpr::IntLiteral(n);
        sum = Some(match sum {
            None => next,
            Some(acc) => ToyExpr::Add(Box::new(acc), Box::new(next)),
        });
    }
    Ok(sum.expect("2項以上のtermsを回るループなのでNoneのままにはならない"))
}
```

項が1つしかない場合(`"1"`や`"true"`)は`parse_term`に委ね、整数と真偽値のどちらも受け付けます。
項が2つ以上ある場合(`"1 + 2"`)は、各項を`i64`として解析できることを要求し、失敗したら`DbError::Parse`を返します。
この分岐が、`eval`側で`unreachable!`が実際に起こらないことを保証している境目です。

`+`という1文字で文字列を分割する実装は、カッコも掛け算も演算子の優先順位も表現できません。
`(1 + 2) * 3`のような式や、文字列の中に`+`を含むリテラルは、この章の範囲では最初から扱う気がありません。
そうした構文を正しく扱うには、文字列を1文字ずつ意味のある単位(トークン)に区切る字句解析器と、トークン列から演算子の優先順位を踏まえて木を組み立てる構文解析器が要ります。
それが第6章と第7章の仕事です。

### `Database`と`QueryResult`

`Database`は、SQL文字列を受け取って結果を返す入り口です。
新規作成する`src/database.rs`に、次の`Database`を定義します。

```rust
/// minidbのデータベース1つを表す。
///
/// 現時点ではインメモリの状態しか持たない。ディスクへの永続化は第2部で
/// `Database::open`のような別のコンストラクタとして追加する。
pub struct Database;

impl Database {
    /// インメモリのDatabaseを作る。
    pub fn memory() -> Self {
        Database
    }

    /// SQL文字列を1本実行し、結果を返す。
    ///
    /// 現時点で受理できる構文は`SELECT <式>;`のみ。構文の解析は`toy_sql`の
    /// 仮実装に委ねている。
    pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
        let select = toy_sql::parse_select(sql)?;
        let value = select.expr.eval()?;
        let data_type = value
            .data_type()
            .expect("toy_sqlが生成する式はリテラルの評価結果しか返さず、NULLにはならない");

        let schema = Schema::new(vec![Column::new(select.expr_text, data_type, false)]);
        let tuple = Tuple::new(&schema, vec![value])?;

        Ok(QueryResult {
            schema,
            rows: vec![tuple],
        })
    }
}
```

`execute`の中身は、この章の主張を素直にコードへ落としただけです。
`parse_select`が返した`expr`を`eval`で`Value`に変換し、その`Value`から1列だけの`Schema`を組み立て、`Tuple::new`に通します。
`Tuple::new`は内部で`validate_tuple`を呼ぶので、ここで作られる`Tuple`は必ず不変条件2を満たします。
`Database`自身はまだフィールドを1つも持ちません。
テーブルを持たないデータベースなので、それで正しい状態です。
テーブルを持つカタログは第9章で`Database`に追加します。

あわせて`src/lib.rs`に次の行を加え、このモジュールを公開します。

```rust
pub mod database;
```

同じ`src/database.rs`に定義する`QueryResult`は、`Schema`と行の並びを持つだけの型です。

```rust
/// `Database::execute`の結果。列構成(`Schema`)と、それに従う行の並びを持つ。
pub struct QueryResult {
    schema: Schema,
    rows: Vec<Tuple>,
}

impl QueryResult {
    /// 結果の列構成を返す。
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// 結果の行を返す。
    pub fn rows(&self) -> &[Tuple] {
        &self.rows
    }
}
```

REPLでの表示のために、`src/database.rs`に`QueryResult`の`Display`実装を追加します。

```rust
impl std::fmt::Display for QueryResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header = self
            .schema
            .columns()
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        writeln!(f, "{header}")?;
        writeln!(f, "{}", "-".repeat(header.chars().count().max(1)))?;

        for tuple in &self.rows {
            let row = tuple
                .values()
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(" | ");
            writeln!(f, "{row}")?;
        }

        let row_word = if self.rows.len() == 1 { "row" } else { "rows" };
        write!(f, "({} {row_word})", self.rows.len())
    }
}
```

列名の行、区切り線、値の行、件数の footer という並びは、`psql`のような既存のSQLクライアントの表示に寄せた形です。
値そのものの文字列化は、同じ`src/database.rs`に置く`format_value`という小さな関数に切り出しました。

```rust
/// `Value`をユーザー向けの表示形式に変換する。
fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Text(s) => s.clone(),
    }
}
```

`Value`に`Display`を直接実装せず`database`モジュール側に関数を置いたのは、`Value`のユーザー向け表示形式を将来変える余地(たとえば`NULL`を`(null)`と書きたくなる、`Text`をクォートで囲みたくなる、など)を、`types`モジュールの外に持たせておくためです。
`DbError`には、構文解析の失敗を表す`Parse(String)`バリアントを1つ追加しています。

### REPL

`src/main.rs`を、標準入力から1行ずつSQLを読んで実行するループに書き換えます。

```rust
fn main() {
    let mut db = Database::memory();
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    prompt(&mut stdout);
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let input = line.trim();

        if input.is_empty() {
            prompt(&mut stdout);
            continue;
        }
        if input == "\\q" {
            break;
        }

        match db.execute(input) {
            Ok(result) => println!("{result}"),
            Err(e) => println!("エラー: {e}"),
        }
        prompt(&mut stdout);
    }
}
```

`\q`を打つと終了します。
それ以外の入力は`db.execute`にそのまま渡し、成功なら`QueryResult`の`Display`実装を、失敗なら`エラー:`に続けてエラーメッセージを表示します。
結果もエラーメッセージも、利用者に向けた出力なので標準出力(`println!`)に出します。
第3章で決めた「利用者向けの出力とログの出し先を分ける」方針を、ここでも引き継いでいます。

実際に動かすと、次のようになります。

```console
$ cargo run
minidb> SELECT 1;
1
-
1
(1 row)
minidb> SELECT 1 + 2;
1 + 2
-----
3
(1 row)
minidb> SELECT true;
true
----
true
(1 row)
minidb> SELECT 9223372036854775807 + 1;
エラー: 評価エラー: 整数オーバーフロー: 9223372036854775807 + 1
minidb> bogus
エラー: 構文エラー: SELECT文ではありません: "bogus"
minidb> \q
```

`i64`の最大値`9223372036854775807`に`1`を足す入力も、プロセスを落とすことなく`エラー:`の1行として返ってきます。
`checked_add`が`None`を返し、`eval`がそれを`DbError::Eval`に変換した結果です。

エラーを起こしても、その1行だけがエラーとして表示され、プロンプトは次の入力を受け付け続けます。
不変条件1が守られていることが、この動作で確認できます。

## テストで確認する

### 一時データベース用のテストヘルパー

これから先の章でも、インメモリの`Database`を作ってSQLを実行するテストを繰り返し書くことになります。
その都度`Database::memory()`を呼ぶ代わりに、`tests/common/mod.rs`にヘルパーをまとめます。

```rust
/// インメモリのDatabaseを1つ作る。各テストはこの関数から独立したDBを得る。
#[allow(dead_code)]
pub fn temp_db() -> Database {
    Database::memory()
}

/// インメモリDBを1つ作り、SQLを1文実行した結果を返す。
///
/// 1本のSQLを実行して結果だけを確認したいテストのための近道。
#[allow(dead_code)]
pub fn execute_sql(sql: &str) -> DbResult<QueryResult> {
    temp_db().execute(sql)
}
```

`tests/`配下の各ファイルは、それぞれ独立したクレートとしてコンパイルされます。
`tests/common/mod.rs`という名前にして`tests/golden.rs`側から`mod common;`で読み込むことで、このファイル自体は単独のテストバイナリにならず、共有コードの置き場所として扱われます。

### 単体テスト

`src/toy_sql.rs`の`#[cfg(test)] mod tests`には、構文解析が受理すべき入力と拒否すべき入力を確認するテストを追加しました。
加算の右辺に整数以外を置いた式が`DbError::Parse`として拒否されることを、次のように確認します。

```rust
    #[test]
    fn rejects_addition_with_non_integer_operand() {
        let result = parse_select("SELECT true + 1;");
        assert!(matches!(result, Err(DbError::Parse(_))));
    }
```

`src/database.rs`の`#[cfg(test)] mod tests`には、`Database::execute`が返す`Tuple`の値と列名を確認するテストを追加しました。
`SELECT 1;`の結果が`Value::BigInt(1)`を1件返し、列名が`1`になることを、次のように確認します。

```rust
    #[test]
    fn executes_integer_literal() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1;").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(1)]);
        assert_eq!(result.schema().columns()[0].name, "1");
    }
```

### SQL Golden Test

第3章で用意した`tests/golden.rs`の`run_sql`は、まだクエリエンジンが無いためSQLをそのままエコーするだけの仮実装でした。
これを、同じ`tests/golden.rs`の中で`Database::execute`の呼び出しに差し替えます。

```rust
/// SQLを1本実行し、Golden Testと突き合わせるための文字列表現を返す。
fn run_sql(sql: &str) -> String {
    match execute_sql(sql) {
        Ok(result) => result.to_string(),
        Err(e) => format!("ERROR: {e}"),
    }
}
```

`tests/golden/001_echo.sql`という名前も、もう実態に合いません。
`001_select_int.sql`に改名し、`002_select_add.sql`(加算)と`003_select_bool.sql`(真偽値)を追加しました。
`001_select_int.expected`の中身は、`SELECT 1;`を実行した`QueryResult`の`Display`出力そのものです。

```text
1
-
1
(1 row)
```

`cargo test`を実行すると、`toy_sql`と`database`のテストを含めて24件の単体テストと、3件のSQL Golden Testを含む`golden_tests_pass`が通ります。

```console
$ cargo test
running 24 tests
test database::tests::executes_addition ... ok
test database::tests::executes_boolean_literal ... ok
test database::tests::executes_integer_literal ... ok
test database::tests::propagates_parse_error ... ok
test toy_sql::tests::parses_addition_of_integers ... ok
test toy_sql::tests::rejects_addition_with_non_integer_operand ... ok
...
test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/golden.rs
running 1 test
test golden_tests_pass ... ok
```

## 壊して確認する

`parse_expr`の、2項以上の加算で整数リテラルを要求している部分を崩してみます。

```rust
    let mut sum: Option<ToyExpr> = None;
    for term in &terms {
        // 整数であることの検査を外し、parse_termにそのまま委ねてしまったとする
        let next = parse_term(term)?;
        sum = Some(match sum {
            None => next,
            Some(acc) => ToyExpr::Add(Box::new(acc), Box::new(next)),
        });
    }
```

この変更をすると、`rejects_addition_with_non_integer_operand`が赤くなります。
`parse_term`は`true`も`false`もリテラルとして受理するため、`"SELECT true + 1;"`が`parse_select`の時点でエラーにならず、`ToyExpr::Add(BoolLiteral(true), IntLiteral(1))`という式が組み上がってしまうためです。

このテストが赤くなるだけならまだ実害はありません。
問題は、この`ToySelect`に対して実際に`eval`を呼んだときです。
`eval`の`Add`アームは`(Value::BigInt(l), Value::BigInt(r))`以外の組み合わせを`unreachable!`としています。
検査を外した状態で`db.execute("SELECT true + 1;")`を呼ぶと、`eval`まで到達してこの`unreachable!`に行き着き、プロセスがパニックで落ちます。
REPLの1行の入力ミスでプロセス全体が終了するのは、不変条件1への明確な違反です。

`eval`側の`unreachable!`は、それ単体では成立しません。
「`Add`の両辺は整数リテラルである」という前提を、構文解析の側で保証していて初めて安全になります。
どちらか一方だけを見ていては、この前提が壊れていることに気づけません。

## 演習問題

### 必須課題

1. `Database`に、セミコロン区切りの複数の`SELECT`文をまとめて実行し、`Vec<QueryResult>`を返す`execute_all(&mut self, sql: &str) -> DbResult<Vec<QueryResult>>`を追加してください。1文でも構文エラーがあれば、どの文で失敗したかが分かるようにエラーメッセージを工夫してください。
2. REPLに、直前に実行したSQLをもう一度表示するメタコマンド`\p`を追加してください。`Database`側にも、最後に実行したSQL文字列を保持する仕組みが必要になります。

### 発展課題

1. `ToyExpr`に減算(`-`)を追加してください。`parse_expr`は現在`+`だけで文字列を分割していますが、`1 - 2 + 3`のように`+`と`-`が混在する式をどう扱うか、`split`だけで対応できるか、それとも文字を1つずつ読む実装が要るかを検討してから実装してください。
2. この章の`parse_expr`は`1 + 2 + 3`のような3項以上の加算を、左から順に`Add(Add(1, 2), 3)`という木に組み立てます。これは演算子の**左結合**という性質を反映した組み立て方です。仮に`Add(1, Add(2, 3))`という右結合の木を作っていたとしても、加算だけを扱う今回の実装では実行結果は変わりません。減算のように結合の向きで結果が変わる演算子を導入したとき、`parse_expr`のどこを直す必要があるか、コードを示して説明してください。
