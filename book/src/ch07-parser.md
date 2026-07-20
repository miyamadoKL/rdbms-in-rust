# 第7章 構文解析器とAST

前章で`Lexer`が生まれ、`toy_sql`は文字列を直接分割する代わりにToken列を読むようになりました。
それでも`toy_sql`の中身は、前章の前から変わっていない部分が1つ残っています。
`src/toy_sql.rs`の`parse_expr`が、Token列を`Plus`というトークンの前後で分割し、分割できた個数がいくつかで場合分けするという構造です。

```rust
fn parse_expr(tokens: &[&Token]) -> DbResult<ToyExpr> {
    let terms: Vec<&[&Token]> = tokens
        .split(|t| matches!(t.kind, TokenKind::Plus))
        .collect();
    // ...(termsの個数で分岐し、2個以上なら整数リテラルの加算として畳み込む)
}
```

この関数に`SELECT 1 + 2 * 3`を渡すとどうなるか、実際に追ってみます。
`+`で分割すると、`terms`は`[1]`と`[2, *, 3]`という2つの断片になります。
1つめの断片は`IntLiteral(1)`という1トークンなのでそのまま整数として読めますが、2つめの断片は`2`、`*`、`3`という3トークンです。
`parse_expr`は「1トークンの断片」と「そうでない断片」でしか場合分けしておらず、後者はすべて`加算は整数リテラルにのみ対応しています`という同じエラーになります。

```console
minidb> SELECT 1 + 2 * 3
エラー: 構文エラー: 加算は整数リテラルにのみ対応しています
```

このエラーメッセージは誤りです。
この入力で本当に対応していないのは乗算であって、加算はどこも壊れていません。
`parse_expr`が`+`という1つの記号だけを特別扱いする構造になっているせいで、`*`という別の記号に出会うと、その正体を確かめる手段を持たないまま「加算の話」として押し込んでしまうのです。

問題は乗算という1機能が足りないことではありません。
`toy_sql`は、そもそも「どの部分がどの部分より先に計算されるべきか」という関係を表現するデータ構造を持っていません。
`Vec<&[&Token]>`という平らな配列に分割した時点で、`2 * 3`というまとまりが1個の値になるべきだという情報は失われ、後段には3個のトークンが並んでいるという事実しか残らないのです。
`Token`列を手に入れたことで文字面の曖昧さは前章で解消しましたが、その並びがどんな構造を持つべきかという問いには、前章はまだ何も答えていません。

この章で作るのは、Token列を**構文木**(AST、Abstract Syntax Tree)に変換する**構文解析器**(Parser)です。
`2 * 3`が1個のまとまりであり、それが`1 +`の右側に来るという関係を、木構造として表現します。
文の解析には`Recursive Descent`を、式の解析には`Pratt Parser`という手法を使い、この2つを使い分ける理由から見ていきます。

## ASTの設計をSQL構文の表現に限定する

構文解析器が組み立てるASTは、`Statement`(文)と`Expr`(式)という2種類のノードからなります。
この章では`src/ast.rs`を新規に作成し、AST関連の型をまとめて置きます。

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `SELECT`文。
    Select(SelectStatement),
    /// `CREATE TABLE`文。
    CreateTable(CreateTableStatement),
    /// `INSERT INTO`文。
    Insert(InsertStatement),
}

// ...(SelectStatement、CreateTableStatement、InsertStatementはそれぞれの節で見る)

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    IntLiteral {
        value: i64,
        span: Span,
    },
    StringLiteral {
        value: String,
        span: Span,
    },
    BoolLiteral {
        value: bool,
        span: Span,
    },
    NullLiteral {
        span: Span,
    },
    /// 列参照。`users.id`のような修飾名は、Lexerが`.`を扱わないため対象外。
    ColumnRef {
        name: String,
        span: Span,
    },
    UnaryOp {
        op: UnaryOperator,
        expr: Box<Expr>,
        span: Span,
    },
    BinaryOp {
        op: BinaryOperator,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    /// `<expr> IS [NOT] NULL`。
    IsNull {
        expr: Box<Expr>,
        negated: bool,
        span: Span,
    },
    /// `name(arg, arg, ...)`。引数0個の`name()`も許す。
    FunctionCall {
        name: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// `(expr)`。優先順位を明示するための括弧そのものをASTに残す。
    Paren {
        expr: Box<Expr>,
        span: Span,
    },
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod ast;
```

`Expr::ColumnRef`は`id`のような識別子が式として出てきたことだけを表し、名前を文字列として持つだけです。
この`id`が本当にどれかのテーブルの列なのか、それとも存在しない名前なのかは、ASTの時点では判定しません。
`CreateTableStatement`の列定義も同様で、`BIGINT`という型名を`Ident`として、つまりただの文字列として保持する`ColumnDef`を、同じ`src/ast.rs`に次のように定義します。

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: Ident,
    pub type_name: Ident,
    /// `NOT NULL`が指定されていたかどうか。
    pub not_null: bool,
    pub span: Span,
}
```

`type_name`を`crate::types::DataType`にその場で変換してしまう設計も、選択肢としてはありえます。
ですがその変換は「`BIGINT`という文字列が、この処理系が知っている型の名前と一致するか」という検査であり、Parserがその一覧を知る義務はありません。
テーブルや列が実在するかどうかを確かめる主体は、この構文解析器ではありません。
テーブルや列の定義を保持するカタログは第9章で作りますが、`SELECT`や`INSERT`が参照する名前をそのカタログと本格的に突き合わせる`Binder`というモジュールは、第17章で導入します。
第10章のDML実行では、それより手前で挿入先の列数といった最小限の照合を行いますが、それは独立したBinderの仕事ではなく、実行の途中で行う簡易な検査に留まります。
Parserは「構文として正しい形をしているかどうか」だけを見て、名前が実在するかどうかの検証はこれらの後続の章に委ねます。
この境界を崩すと、型名の一覧を増やすたびにParserと後続の章の両方を書き換える羽目になり、どちらが真実の情報源かも曖昧になります。

もう1つ、すべてのノードに共通する設計判断が`Span`です。
`Expr`の各バリアントは末尾に`span: Span`を持ち、そのノードがソース中のどのバイト範囲に対応するかを覚えています。
これは前章の`Token`が持っていた`Span`と同じ仕組みで、構文エラーの位置を報告するためだけでなく、第17章の`Binder`が「この列参照はどこに書かれていたか」を利用者に示すためにも使います。
`BinaryOp`の`span`は左辺の開始位置から右辺の終了位置までを覆うので、`1 + 2 * 3`全体のASTを見れば、そのノードがソースのどの範囲に対応する式なのかをたどれます。

## 文はRecursive Descent、式はPratt Parserで読む

対応する構文は、`SELECT`、`CREATE TABLE`、`INSERT INTO`の3つの文と、それぞれの中に現れる式です。

`SELECT <式> [, <式> ...] [FROM <table>] [WHERE <式>]`
`CREATE TABLE <table> (<col> <type> [NOT NULL], ...)`
`INSERT INTO <table> VALUES (<式>, ...)`

この3つの文法は、どれも「キーワードを読む」「識別子を読む」「区切り文字が続く限り繰り返す」という定型的な手順の組み合わせです。
`CREATE TABLE`なら、`CREATE`を読み、`TABLE`を読み、テーブル名を読み、`(`を読み、列定義をカンマ区切りで読み、`)`を読みます。
文法の各要素が、その要素を読む1つの関数にそのまま対応するので、この構造は関数呼び出しの再帰としてそのまま書けます。
これが**Recursive Descent**(再帰下降構文解析)で、`parse_create_table_statement`が`parse_column_def`を呼び、`parse_column_def`が`expect_ident`を呼ぶというように、文法の入れ子がRustの呼び出し関係の入れ子に対応します。

式は事情が異なります。
`1 + 2 * 3`を`Recursive Descent`だけで正しく読もうとすると、加算や乗算のような優先順位のレベルごとに専用の関数を用意し、レベルの低い関数がレベルの高い関数を呼ぶという形にする必要があります(`parse_or`が`parse_and`を呼び、`parse_and`が比較を呼び、というように)。
この処理系が扱う演算子は`OR`、`AND`、`NOT`、6種類の比較演算子、`+`、`-`、`*`、`/`、単項`-`と数が多く、レベルごとに関数を作ると、優先順位を1段追加したり変更したりするたびに新しい関数を書き足すことになります。
優先順位の変更が、コードの構造そのものの変更を要求してしまうのです。

**Pratt Parser**は、優先順位を「関数の数」ではなく「数値」として扱うことで、この問題を避けます[^pratt]。
それぞれの演算子に**binding power**(結合力)という数値を割り当て、1個のループ関数がその数値を比較するだけで、どの演算子を先に読み込むべきかを決めます。
優先順位の変更は、対応表の数値を書き換えるだけで済み、ループの構造自体は変わりません。

[^pratt]: Vaughan R. Pratt, "Top Down Operator Precedence", ACM Symposium on Principles of Programming Languages, 1973.

この章のParserは、文をRecursive Descentで、式をPratt Parserで読みます。
文の構造は数個の固定パターンしかなく、優先順位という概念自体が存在しないので、Recursive Descentの素直さがそのまま利点になります。
式は逆に、演算子の数だけ優先順位のレベルがあり、そのレベルが今後の章で増減する見込みもあるので、対応表の書き換えだけで済むPratt Parserの柔軟さが効いてきます。
1つの構文解析器の中で手法を使い分けるのは妥協ではなく、それぞれの下位問題の形に合わせた選択です。

## Recursive Descentで文を読む

`Parser`は、ソース文字列とToken列、現在の読み取り位置を持つ構造体です。
この章では`src/parser.rs`を新規に作成し、構文解析器の実装をまとめて置きます。

```rust
struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod parser;
```

現在のトークンを覗く`peek`、1個読み進める`advance`、期待するキーワードや記号でなければエラーを返す`expect_keyword`、`expect_punct`、`expect_ident`という基本操作を用意し、これらを組み合わせて先頭のキーワードで分岐する`parse_statement`を、同じ`src/parser.rs`に次のように定義します。

```rust
fn parse_statement(&mut self) -> DbResult<Statement> {
    match self.peek_kind() {
        TokenKind::Keyword(Keyword::Select) => {
            self.parse_select_statement().map(Statement::Select)
        }
        TokenKind::Keyword(Keyword::Create) => self
            .parse_create_table_statement()
            .map(Statement::CreateTable),
        TokenKind::Keyword(Keyword::Insert) => {
            self.parse_insert_statement().map(Statement::Insert)
        }
        _ => Err(self.unexpected("SELECT・CREATE TABLE・INSERT INTOのいずれか")),
    }
}
```

`SELECT`の解析は、式リストをカンマ区切りで読んだあと、`FROM`と`WHERE`をそれぞれ省略可能な節として読む`parse_select_statement`で行い、同じ`src/parser.rs`に次のように実装します。

```rust
    fn parse_select_statement(&mut self) -> DbResult<SelectStatement> {
        let start = self.expect_keyword(Keyword::Select, "SELECT")?.start;

        let mut items = vec![self.parse_select_item()?];
        while *self.peek_kind() == TokenKind::Comma {
            self.advance();
            items.push(self.parse_select_item()?);
        }
        let mut end = items.last().expect("直前にpushしたばかり").span.end;

        let from = if let TokenKind::Keyword(Keyword::From) = self.peek_kind() {
            self.advance();
            let table = self.expect_ident()?;
            end = table.span.end;
            Some(table)
        } else {
            None
        };

        // WHEREの読み取りもFROMと同じ形(あれば読み、endを更新する)で続く。
        // ...

        Ok(SelectStatement {
            items,
            from,
            where_clause,
            span: Span::new(start, end),
        })
    }
```

`FROM`と`WHERE`をこの章で構文として受理しておく理由は、実行できる時期とは別にあります。
テーブルの中身を読むカタログとインメモリ表は第9章と第10章で用意するので、`FROM users`を実行することはまだできません。
それでも構文として受理しておけば、この章のASTとParserは第9〜10章でそのまま使い回せます。
受理はするが実行はしない、という境界線をどこに引いたかは、後で`Database::execute`のところで扱います。

`CREATE TABLE`と`INSERT INTO`も同じ形の手続きで、同じ`src/parser.rs`に次のように実装します。

```rust
fn parse_create_table_statement(&mut self) -> DbResult<CreateTableStatement> {
    let start = self.expect_keyword(Keyword::Create, "CREATE")?.start;
    self.expect_keyword(Keyword::Table, "TABLE")?;
    let table = self.expect_ident()?;
    self.expect_punct(TokenKind::LParen, "(")?;

    let mut columns = vec![self.parse_column_def()?];
    while *self.peek_kind() == TokenKind::Comma {
        self.advance();
        columns.push(self.parse_column_def()?);
    }

    let end = self.expect_punct(TokenKind::RParen, ")")?.end;

    Ok(CreateTableStatement {
        table,
        columns,
        span: Span::new(start, end),
    })
}
```

列定義1個(`parse_column_def`)は、列名、型名の順に識別子を2つ読み、続けて`NOT`が来ていれば`NULL`まで読んで`not_null`を`true`にします。
`INSERT INTO`は、テーブル名の後に`VALUES`、`(`、カンマ区切りの式リスト、`)`と続くだけで、`CREATE TABLE`の列リストと構造上の違いはほとんどありません。
この章で対応する`INSERT`は1行分の`VALUES (...)`だけに絞り、複数行の`VALUES (...), (...)`や列名を指定する`INSERT INTO name (col, ...)`は対象外にしています。
これらは第10章のDMLでテーブルへの書き込みを本格的に作るときに、必要性を確かめてから足す拡張です。

## Pratt Parserで式を読む

式の解析は、`parse_expr(min_bp)`という1個の関数が中心になります。
`min_bp`は「この呼び出しが消費してよい演算子の下限」を表す数値で、最初の呼び出しは`0`から始まり、同じ`src/parser.rs`に次の`parse_expr`を定義します。

```rust
fn parse_expr(&mut self, min_bp: u8) -> DbResult<Expr> {
    let mut lhs = self.parse_prefix()?;

    loop {
        // IS [NOT] NULLの処理はここに入るが、先に二項演算子の分岐を見る。

        let Some((op, lbp, rbp)) = infix_binding_power(self.peek_kind()) else {
            break;
        };
        if lbp < min_bp {
            break;
        }
        self.advance();
        let rhs = self.parse_expr(rbp)?;
        let span = Span::new(lhs.span().start, rhs.span().end);
        lhs = Expr::BinaryOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            span,
        };
    }

    Ok(lhs)
}
```

まず`parse_prefix`で左辺(`lhs`)を1個確定させ、その後はループで「次のトークンが2項演算子で、その`lbp`(左結合力)が`min_bp`以上なら読み進める」を繰り返します。
`lbp`が`min_bp`を下回ったら、その演算子は今の呼び出しの管轄外なので、消費せずにループを抜けて呼び出し元に返します。
2項演算子を実際に読み込むとき、右辺の解析には`rhs`(右結合力)を新しい`min_bp`として渡します。

演算子ごとの`(lbp, rbp)`は、同じ`src/parser.rs`に置く次の対応表から機械的に求めます。

```rust
fn infix_binding_power(kind: &TokenKind) -> Option<(BinaryOperator, u8, u8)> {
    let (op, lbp) = match kind {
        TokenKind::Keyword(Keyword::Or) => (BinaryOperator::Or, 1),
        TokenKind::Keyword(Keyword::And) => (BinaryOperator::And, 3),
        TokenKind::Eq => (BinaryOperator::Eq, 6),
        // ...(NotEq, Lt, LtEq, Gt, GtEqも比較演算子として同じ6)
        TokenKind::Plus => (BinaryOperator::Add, 8),
        TokenKind::Minus => (BinaryOperator::Subtract, 8),
        TokenKind::Star => (BinaryOperator::Multiply, 10),
        TokenKind::Slash => (BinaryOperator::Divide, 10),
        _ => return None,
    };
    Some((op, lbp, lbp + 1))
}
```

数値が大きい演算子ほど強く結びつき、木の深い場所(先に計算される側)に置かれます。
`OR`が`1`、`AND`が`3`、比較演算子が`6`、`+`と`-`が`8`、`*`と`/`が`10`という並びが、そのまま`OR < AND < 比較 < 加減 < 乗除`という優先順位の低い順を表しています。
`rbp`を`lbp + 1`にしているのは、左結合(同じ優先順位なら左から先にまとまる)を実現するためです。
`1 - 2 - 3`を例にすると、最初の`-`を読んだ後の右辺解析は`min_bp = 9`(`lbp=8`の`+1`)で始まるため、続く`-`(`lbp=8`)は`9`未満で消費されず、右辺は`2`だけで確定します。
結果として`(1 - 2) - 3`という左結合の木になります。
もし`rbp`を`lbp`と同じ値にしていたら、右辺解析中の`min_bp`も`8`のままになり、続く`-`まで同じ呼び出しが飲み込んでしまい、`1 - (2 - 3)`という右結合の木になってしまいます。

冒頭の`1 + 2 * 3`をこの表で追うと、`+`の`lbp`は`8`、`*`の`lbp`は`10`です。
`1`を読んだ後、`+`(`lbp=8 >= min_bp=0`)を消費し、右辺を`min_bp=9`で解析します。
この`min_bp=9`の呼び出しは`2`を読んだ後、`*`(`lbp=10 >= 9`)も消費するので、右辺は`2 * 3`というまとまりとして先に確定し、最後に`1 + (2 * 3)`が組み上がります。
`*`の`lbp`が`+`の`rbp`より大きいという、この表の数値の並びだけから、`2 * 3`が先にまとまるという構造が導かれています。

前置演算子(単項`-`と`NOT`)は、同じ`src/parser.rs`に置く`parse_prefix`が扱います。

```rust
fn parse_prefix(&mut self) -> DbResult<Expr> {
    const NOT_RBP: u8 = 5;
    const NEGATE_RBP: u8 = 12;

    match self.peek_kind() {
        TokenKind::Keyword(Keyword::Not) => {
            let start = self.advance().span.start;
            let operand = self.parse_expr(NOT_RBP)?;
            let span = Span::new(start, operand.span().end);
            Ok(Expr::UnaryOp {
                op: UnaryOperator::Not,
                expr: Box::new(operand),
                span,
            })
        }
        TokenKind::Minus => {
            let start = self.advance().span.start;
            let operand = self.parse_expr(NEGATE_RBP)?;
            let span = Span::new(start, operand.span().end);
            Ok(Expr::UnaryOp {
                op: UnaryOperator::Negate,
                expr: Box::new(operand),
                span,
            })
        }
        _ => self.parse_primary(),
    }
}
```

単項`-`の`NEGATE_RBP`は`12`で、比較(`6`)よりも乗除(`10`)よりも大きい値にしています。
`-1 + 2`であれば、`-`を読んだ直後の右辺解析は`min_bp=12`で始まるため、続く`+`(`lbp=8`)はここでは消費されず、単項`-`は`1`だけを取り込んで`Negate(1)`を作ります。
残った`+ 2`は、`parse_prefix`から戻った外側の`parse_expr`のループがあらためて処理するので、最終的な木は`Add(Negate(1), 2)`になります。

`NOT`の`NOT_RBP`は`5`で、比較の`lbp`(`6`)より小さい値です。
これにより`NOT 1 = 2`を解析すると、`NOT`の右辺解析(`min_bp=5`)は`1`を読んだ後、`=`(`lbp=6 >= 5`)も消費してしまうため、右辺は`1 = 2`というまとまりごと`NOT`に取り込まれます。
結果は`NOT (1 = 2)`であり、`(NOT 1) = 2`にはなりません。
仮に`NOT_RBP`を比較の`lbp`より大きい値にしていたら、`NOT`は`1`だけを取り込んで`(NOT 1) = 2`になり、`NOT AND比較`の間にある優先順位の関係が逆転してしまいます。
`OR < AND < NOT < 比較`という並びを保つ`NOT_RBP=5`は、`AND`の`lbp`(`3`)より大きく比較の`lbp`(`6`)より小さい、その間の値です。

`IS [NOT] NULL`は左に式を1個取るだけで右辺を持たないので、2項演算子のための`infix_binding_power`とは別に、同じ`src/parser.rs`の`parse_expr`のループの中で直接処理します。

```rust
if let TokenKind::Keyword(Keyword::Is) = self.peek_kind() {
    const IS_NULL_BP: u8 = 6;
    if IS_NULL_BP < min_bp {
        break;
    }
    self.advance();
    let negated = if let TokenKind::Keyword(Keyword::Not) = self.peek_kind() {
        self.advance();
        true
    } else {
        false
    };
    let end = self.expect_keyword(Keyword::Null, "NULL")?.end;
    let span = Span::new(lhs.span().start, end);
    lhs = Expr::IsNull {
        expr: Box::new(lhs),
        negated,
        span,
    };
    continue;
}
```

`IS_NULL_BP`を比較演算子と同じ`6`にしているので、`1 + 2 IS NULL`は`(1 + 2) IS NULL`と解釈されます(加減の`lbp=8`が先に消費され、`+`の右辺が確定してから`IS`に出会うため)。

残る`parse_primary`は、整数や文字列、真偽値、`NULL`のリテラル、識別子(列参照か関数呼び出し)、`(`から始まる括弧の3系統を読みます。
識別子を読んだ直後に`(`が続いていれば関数呼び出しとして引数リストを読み、続いていなければ列参照として扱う処理を、同じ`src/parser.rs`の`parse_primary`に次のように実装します。

```rust
TokenKind::Ident(name) => {
    let start_span = self.advance().span;
    if *self.peek_kind() == TokenKind::LParen {
        self.parse_function_call(name, start_span)
    } else {
        Ok(Expr::ColumnRef {
            name,
            span: start_span,
        })
    }
}
```

括弧はどんな式でも`min_bp = 0`から読み直せるので、`(1 + 2) * 3`のように、外側の優先順位に関係なく中の式を独立に解析できます。
読み終えた括弧は`Expr::Paren`として木に残し、優先順位を明示するために利用者が書いた`(`と`)`という情報そのものを捨てません。

## 位置情報付き構文エラー

構文解析が失敗したときのエラーは、前章の`DbError::Lex`と表示形式を揃えます。
これまでの`DbError::Parse`はメッセージだけを持つ`String`1個のバリアントでしたが、この章で`Lex`と同じ形の構造体バリアントに変えます。
`src/error.rs`の`DbError::Parse`を、次の形に書き換えます。

```rust
#[error("行{line}列{column}: 構文エラー: {message}")]
Parse {
    /// エラーの内容。
    message: String,
    /// 発生位置の行番号(1始まり)。
    line: usize,
    /// 発生位置の列番号(1始まり)。
    column: usize,
},
```

Parserがエラーを作る場所は`error_at`の1箇所に集約し、`Span`から行番号や列番号への変換は前章で作った`lexer::line_col`をそのまま再利用します。
`src/parser.rs`に戻り、次の`error_at`を定義します。

```rust
fn error_at(&self, span: Span, message: impl Into<String>) -> DbError {
    let (line, column) = lexer::line_col(self.source, span.start);
    DbError::Parse {
        message: message.into(),
        line,
        column,
    }
}
```

期待していたトークンと違うものに出会ったときのエラーは、同じ`src/parser.rs`に置く`unexpected`が組み立てます。

```rust
fn unexpected(&self, expected: &str) -> DbError {
    let token = self.peek();
    self.error_at(
        token.span,
        format!("{expected}が必要です: {:?}が見つかりました", token.kind),
    )
}
```

`expect_keyword`や`expect_punct`、`expect_ident`は、期待どおりのトークンでなければすべてこの`unexpected`を返すので、「`SELECT`が必要です」「識別子が必要です」のように、どの文脈でどんなトークンを期待していたかがメッセージにそのまま残ります。
`SELECT 1 +`のように式の途中で入力が尽きた場合は、`Eof`トークンの`Span`(入力の末尾)がそのままエラーの位置になります。

```console
minidb> SELECT 1 +
エラー: 行1列11: 構文エラー: 式が必要です: Eofが見つかりました
```

## Database::executeをParserへ置き換える

`Database::execute`は、これまで`toy_sql::parse_select`を呼んでいた箇所を`parser::parse_statement`に置き換え、`toy_sql`モジュール自体を削除します。
戻り値が`Statement`という3種類のバリアントを持つ列挙型になったので、`execute`はまず文の種類で分岐します。
`src/database.rs`の`execute`を、次のように書き換えます。

```rust
pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
    let statement = crate::parser::parse_statement(sql)?;
    match statement {
        Statement::Select(select) => self.execute_select(sql, &select),
        Statement::CreateTable(_) => Err(DbError::NotImplemented(
            "CREATE TABLEの実行(カタログへの登録)は第9章で対応します".to_string(),
        )),
        Statement::Insert(_) => Err(DbError::NotImplemented(
            "INSERTの実行(表への追加)は第10章で対応します".to_string(),
        )),
    }
}
```

`CREATE TABLE`と`INSERT`は、構文解析までは完全に成功します。
テーブル名や列定義、挿入する値はASTとして正しく組み上がりますが、そのASTを受け取って実際にカタログへ登録したり表へ書き込んだりする実行部分は、まだこのクレートのどこにも存在しません。
存在しない機能を黙って何もしないまま`Ok`を返すのではなく、`DbError::NotImplemented`という、それとわかる形で呼び出し元に伝えます。

`SELECT`にも同じ考え方が及びます。
`FROM`や`WHERE`を構文として受理できるようにしたのは前述のとおりですが、それらを実行するにはテーブルの中身を読む手段が要るため、同じ`src/database.rs`に置く`execute_select`は次のように両者を区別します。

```rust
fn execute_select(&self, sql: &str, select: &SelectStatement) -> DbResult<QueryResult> {
    if select.from.is_some() || select.where_clause.is_some() {
        return Err(DbError::NotImplemented(
            "FROM・WHEREを伴うSELECTの実行(カタログと表の参照)は第9〜10章で対応します"
                .to_string(),
        ));
    }
    // ...(FROM・WHEREが無いSELECTだけ、この後の式評価に進む)
}
```

`FROM`が無い`SELECT`だけが、この章で最後まで実行できる構文です。
各対象式を評価して`Value`にし、そのソーステキストを列名として`Schema`と`Tuple`を組み立てる処理は、前章までの`toy_sql`が担っていた役割をそのまま引き継いでいます。

式の評価(`eval_expr`)も、この章で対応するのはリテラルと整数の加算だけに絞り、同じ`src/database.rs`に次のように実装します。

```rust
fn eval_expr(expr: &Expr) -> DbResult<Value> {
    match expr {
        Expr::IntLiteral { value, .. } => Ok(Value::BigInt(*value)),
        Expr::StringLiteral { value, .. } => Ok(Value::Text(value.clone())),
        Expr::BoolLiteral { value, .. } => Ok(Value::Boolean(*value)),
        Expr::NullLiteral { .. } => Ok(Value::Null),
        Expr::Paren { expr, .. } => eval_expr(expr),
        Expr::BinaryOp {
            op: BinaryOperator::Add,
            lhs,
            rhs,
            ..
        } => match (eval_expr(lhs)?, eval_expr(rhs)?) {
            (Value::BigInt(l), Value::BigInt(r)) => Ok(Value::BigInt(l + r)),
            _ => Err(DbError::NotImplemented(
                "整数以外の加算・型変換は第8章の式評価で対応します".to_string(),
            )),
        },
        _ => Err(DbError::NotImplemented(
            "この式の評価は第8章の式評価で対応します".to_string(),
        )),
    }
}
```

冒頭で見た`1 + 2 * 3`は、この章のParserなら`Add(1, Multiply(2, 3))`という正しい木として解析できます。
ですが`eval_expr`の対応は加算までなので、`Database::execute("SELECT 1 + 2 * 3")`を呼ぶと、`Multiply`の評価に到達した時点で`NotImplemented`が返ります。
これは`Parser`の限界ではなく、意図した線引きです。
減算、乗算、除算、比較、論理演算、`IS NULL`、列参照、関数呼び出しの**評価**(値をどう計算するか、`NULL`をどう伝播させるか、型が合わないときどうするか)は、三値論理と型変換をまとめて設計する第8章の役目であり、この章は「構文として正しく組み立てられるかどうか」だけに責任を持ちます。
構文解析と意味づけを1つの章に詰め込むと、優先順位のテストを書いているのか型変換のテストを書いているのか区別しにくくなるので、この章では前者だけに集中します。

## テストで確認する

`parser`モジュールには、優先順位、結合性、括弧、`NOT`と`IS NULL`、各文、構文エラーの位置を確認する単体テストを追加します。
優先順位は、`Span`を比較対象から外した`Expr`どうしの構造比較で検証します。
`src/parser.rs`の`mod tests`に追加します。

```rust
#[test]
fn multiplication_binds_tighter_than_addition() {
    // `1 + 2 * 3`は`1 + (2 * 3)`であって`(1 + 2) * 3`ではない。
    assert_expr_eq(
        "1 + 2 * 3",
        Expr::BinaryOp {
            op: BinaryOperator::Add,
            lhs: int(1),
            rhs: Box::new(Expr::BinaryOp {
                op: BinaryOperator::Multiply,
                lhs: int(2),
                rhs: int(3),
                span: Span::new(0, 0),
            }),
            span: Span::new(0, 0),
        },
    );
}
```

構文エラーの位置も、`SELECT 1 +`のように途中で入力が尽きる例で、同じ`src/parser.rs`の`mod tests`で確認しています。

```rust
#[test]
fn syntax_error_reports_position_of_bad_token() {
    let err = parse_statement("SELECT 1 +").unwrap_err();
    match err {
        DbError::Parse { line, column, .. } => assert_eq!((line, column), (1, 11)),
        other => panic!("DbError::Parseを期待したが{other:?}が返った"),
    }
}
```

`database`側には、構文解析は通るが評価できない式(`SELECT 1 + 2 * 3`)や、実行できない文(`CREATE TABLE`、`INSERT`、`FROM`付き`SELECT`)が、狙いどおり`NotImplemented`になることを確認するテストを加えています。
`cargo test`を実行すると、`parser`モジュールの単体テストを含めて59件の単体テストと、`golden_tests_pass`が通ります。

```console
$ cargo test
running 59 tests
test parser::tests::multiplication_binds_tighter_than_addition ... ok
test parser::tests::not_binds_looser_than_comparison ... ok
test parser::tests::syntax_error_reports_position_of_bad_token ... ok
test database::tests::multiplication_parses_but_is_not_evaluated_yet ... ok
...
test result: ok. 59 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

## 壊して確認する

`NOT_RBP`の値を、`5`から比較演算子の`lbp`より大きい`7`に変えてみます。

```rust
const NOT_RBP: u8 = 7; // 本来は5
```

この変更をすると、`not_binds_looser_than_comparison`が赤くなります。
`NOT 1 = 2`を解析する際、`NOT`の右辺解析が`min_bp=7`になるため、`=`(`lbp=6`)は`7`未満で消費されなくなり、`NOT`は`1`だけを取り込んで`Negate`ならぬ`Not(1)`を作ってしまいます。
残った`= 2`は外側のループが処理するので、木全体は`(NOT 1) = 2`になり、`NOT (1 = 2)`という正しい構造から外れます。
このテストが無ければ、`NOT`と比較演算子が両方登場する式でだけ症状が出るバグとして、実際にそのような式を書くまで気づけません。

`infix_binding_power`の`rbp`を`lbp + 1`から`lbp`に変えても、同様に壊せます。

```rust
Some((op, lbp, lbp)) // 本来は lbp + 1
```

この変更をすると`addition_is_left_associative`が赤くなり、`1 + 2 + 3`が`(1 + 2) + 3`ではなく`1 + (2 + 3)`という右結合の木になります。
加算だけを見ている限り計算結果は同じなので、この崩れ方は`Value`を比較するテストでは検出できません。
`Expr`の木構造そのものを比較するテストを用意しておいた理由は、ここにあります。

## 演習問題

### 必須課題

1. `BETWEEN <式> AND <式>`構文を追加してください。`1 BETWEEN 0 AND 10`は`0 <= 1 AND 1 <= 10`と同じ意味を持ちます。`AND`という単語がすでに別の演算子として`Keyword::And`にマップされている点に注意し、`parse_expr`のどこに手を入れるべきか、新しい`Expr`バリアントが要るかを考えてから実装してください。
2. `CREATE TABLE`の列定義に、複数列をまとめて一意にする`PRIMARY KEY (col1, col2)`という行(列定義とは別の1行)を追加できるようにしてください。列定義のリストと同じ`(`カンマ区切り`)`の形をどう再利用するかを考えてください。

### 発展課題

1. この章の`infix_binding_power`は、`=`や`<`のような比較演算子どうしを連続して書くと(`1 = 2 = 3`)、`(1 = 2) = 3`という左結合の木として解析してしまいます。多くのSQL実装では比較演算子の連鎖を構文エラーとして拒否します。`parse_expr`のループに、比較演算子を読んだ直後は別の比較演算子を許さないという制約をどう追加できるか、binding powerの仕組みを崩さずに実現する方法を考えてください。
2. `Parser`は構文エラーに出会うと、その時点で解析全体を打ち切ります。第6章の`Lexer`の発展課題と同じ発想で、`CREATE TABLE`の列定義のように区切り文字(`,`)がはっきりしている構文では、1つの列定義が壊れていても区切り文字までスキップして次の列定義から解析を再開できれば、1回の`parse_statement`呼び出しで複数の構文エラーをまとめて報告できます。この「パニックモード」と呼ばれる回復戦略を、`parse_create_table_statement`のどこに組み込めるか設計してください。
