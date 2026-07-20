# 第6章 字句解析器

前章の`toy_sql`に、コメントを1つ足したSQLを渡してみます。

```sql
SELECT 1 + 2 -- 後で3も足す
```

`--`から行末までは、SQLでは注釈であって式の一部ではありません。
ところが`toy_sql`の`parse_expr`は、`+`で文字列を分割するだけの実装でした。

```rust
/// `+`で連結された式を解析する。項が1つなら`parse_term`にそのまま委ねる。
fn parse_expr(src: &str) -> DbResult<ToyExpr> {
    let terms: Vec<&str> = src.split('+').map(str::trim).collect();
    // ...
```

このコードに`"1 + 2 -- 後で3も足す"`を渡すと、`+`で区切られた2番目の項は`"2 -- 後で3も足す"`という1つの文字列になります。
これは整数として解析できないので、`db.execute`はエラーを返します。

```console
minidb> SELECT 1 + 2 -- 後で3も足す
エラー: 構文エラー: 加算は整数リテラルにのみ対応しています: "2 -- 後で3も足す"
```

このエラーメッセージには2つの問題があります。
1つは、コメントという概念そのものを`toy_sql`が知らないことです。
`--`をSQLの一部として読もうとした結果、本来は正しいはずの`1 + 2`が壊れた式として扱われています。
もう1つは、このメッセージがどこにも位置を示さないことです。
`"2 -- 後で3も足す"`という断片を提示されても、それが1行目のどのあたりにあるのかは、SQL文が短いこの例でこそ一目で分かりますが、数十行のCREATE TABLE文やJOINを含むSELECT文になれば、目視で探すしかなくなります。

この2つの問題は、根が同じところにあります。
`toy_sql`は文字列を`+`という1文字で機械的に分割しているだけで、「どこからどこまでが1つの意味のあるかたまりか」をまったく判断していません。
コメントも、文字列リテラルも、負符号と減算の違いも、この分割方法では区別できません。
そして分割の結果として残る断片は、元の文字列のどこにあったかという情報を持たずに次の処理へ渡ります。
位置情報が失われるのは実装の手抜きではなく、文字列を`+`で割るという設計そのものの必然です。

この章で作るのは、SQL文字列を「意味のある最小単位」の列に変換する**字句解析器**(Lexer)です。
空白とコメントを読み飛ばし、識別子、キーワード、リテラル、演算子、記号を`Token`として切り出し、各`Token`がソースのどこにあったかを覚えておきます。
第2章で経路の1段階目として名前だけ挙げていた`Lexer`を、ここで実装します。

## Tokenの粒度をどこで区切るか

字句解析器の設計で最初に決めるべきは、どこまでを1個の`Token`として切り出すかです。
`SELECT`という6文字を1個の`Token`として扱うのか、`S`、`E`、`L`……と1文字ずつの`Token`にするのかは、どちらでもプログラムとして書けます。
後者を選ぶと、後段の構文解析器は「6文字並んだら`SELECT`」という判定を毎回やり直す羽目になり、字句解析器を挟む意味がなくなります。

字句解析器の役割は、**文字面から一意に決まる情報をこの段階で確定させ、後段に持ち越さない**ことです。
`SELECT`という並びがキーワードであることは、文脈に関係なく文字面だけで決まります。
一方、`users`という並びがテーブル名なのか列名なのかは、文脈(構文上の位置とカタログの中身)を見ないと決まりません。
前者を`Token`の種類として確定させ、後者の判断は構文解析器とBinderに委ねます。

この境界を踏まえて、`Token`の種類を次のように決めます。

- **識別子**(`Ident`)：引用符で囲まれていない名前。テーブル名や列名になる
- **キーワード**(`Keyword`)：SQLの予約語。`SELECT`、`FROM`など
- **整数リテラル**(`IntLiteral`)：`1`、`42`のような数字の並び
- **文字列リテラル**(`StringLiteral`)：`'...'`で囲まれたテキスト
- **演算子**：`+ - * / = <> < <= > >=`
- **括弧、カンマ、セミコロン**：`( ) , ;`
- **番兵**(`Eof`)：入力の終端を表す、実際の文字に対応しない`Token`

コメントと空白はここに入りません。
どちらも「後段が読む必要のない部分」なので、`Token`として残さずこの章で読み飛ばします。

識別子とキーワードを分けているのは、`SELECT`という並びと`users`という並びが文字面だけ見れば同じ「英字の並び」だからです。
両者を区別する唯一の手がかりは、その並びが予約語の一覧に載っているかどうかです。
`Ident`と`Keyword`という別のバリアントにしておけば、構文解析器は`match`の時点でキーワードと識別子を取り違えられなくなります。

キーワードに含めるのは、今後の章で確実に使う範囲に絞ります。

```text
SELECT FROM WHERE CREATE TABLE DROP INSERT INTO VALUES
UPDATE SET DELETE TRUE FALSE NULL NOT AND OR IS AS CAST
```

`SELECT`、`FROM`、`WHERE`は第7章の構文解析、`AND`、`OR`、`NOT`、`IS`、`CAST`は第8章の式評価、`CREATE`、`TABLE`、`DROP`は第9章のDDL、`INSERT`、`INTO`、`VALUES`、`UPDATE`、`SET`、`DELETE`は第10章のDMLで使います。
使う予定のない語を先回りして予約語にすると、その語をたまたま列名に使いたくなった章で無関係な制約に突き当たります。
予約語は、使う根拠が立った時点で増やす方針にします。

## 識別子の大文字小文字をどう扱うか

`SELECT`と`select`が同じキーワードとして通ることは、すでに前章の`toy_sql`で決めています。
`strip_keyword`が`eq_ignore_ascii_case`で比較していたのがその実装で、この章でも踏襲します。

一方、識別子については新しく決めることがあります。
`users`という列名を`Users`や`USERS`と書いたとき、これを同じ列として扱うかどうかです。
主要な実装の間でもここは割れていて、PostgreSQLは引用符なしの識別子を小文字へ畳み込み、一部の処理系は大文字へ畳み込みます。

このLexerでは、識別子の大文字小文字を**畳み込まず、入力されたままのテキストを`Ident`に保持します**。
畳み込みを字句解析の段階で行うと、「`users`と`Users`が同じ名前かどうか」という比較の意味づけを、まだテーブルという概念すら存在しないこの章で決めてしまうことになります。
この比較が実際に必要になるのは、識別子をカタログへ登録する第9章のCatalogです。
Lexerは元の文字列をそのまま渡すだけにしておき、畳み込むかどうかの判断は、その判断を必要とする章に残します。

## 守るべき不変条件

実装に入る前に、この章で保つべき条件を3つ決めます。

1. **トークン化はパニックで落ちない**：字句解析器が読めない文字列は、必ず`DbResult`のエラーとして呼び出し元に返す
2. **すべてのTokenがSpanを持つ**：ソース中のどのバイト範囲から作られたTokenかを、後段が常にたどれる
3. **失敗した場合、行番号と列番号を含むエラーになる**：「行N列M」という形式で、利用者がソースの該当箇所を特定できる

1つめは、第5章で決めた「`execute`はパニックで落ちない」という不変条件を、字句解析の層でも同じように保つものです。
不正な文字列を渡されても、`Lexer`はプロセスを異常終了させてはいけません。

## 最小実装

### SpanとToken

`Token`は種類(`TokenKind`)と、ソース中のバイト範囲(`Span`)の組です。
この章では`src/lexer.rs`を新規に作成し、`Lexer`とその周辺の型をすべてここへ置きます。
`src/lib.rs`には`pub mod lexer;`を追加します。

```rust
/// ソースコード中のバイト範囲。`start`を含み`end`を含まない半開区間。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }
}

/// 1個のトークンと、それがソース中で占めるバイト範囲。
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}
```

`Span`が持つのは開始と終了のバイトオフセットだけで、行番号や列番号は持ちません。
理由は2つあります。

1つは、`Token`を1個作るたびに行と列を計算するコストを避けたいからです。
バイトオフセットは、走査中に現在位置を1つ加算するだけで求まりますが、「その位置が何行目の何列目か」は、直前の改行の数を数える必要があり、余分な計算です。
すべてのSQL文でエラーが起きるわけではないので、大多数のトークン化が成功する場合にまでこの計算をつきあわせる理由がありません。

もう1つは、バイトオフセットさえあれば`source[start..end]`でトークンの元の文字列を直接切り出せるからです。
これは`toy_sql`が`expr_text`(結果の列名に使う、対象式の元テキスト)を組み立てるのに使っています。
行番号や列番号の情報だけでは、この切り出しはできません。

行番号と列番号への変換は、それが実際に必要になったとき(エラーを利用者に表示するとき)にだけ、次の関数で行います。

```rust
/// バイトオフセットから、ソース中の1始まりの行番号・列番号を求める。
///
/// `Token`のSpanはバイトオフセットしか持たないため、利用者向けにエラー位置を
/// 表示する側(構文解析器など)がこの関数で行・列へ変換する。字句解析器自身が
/// 検出するエラー(`DbError::Lex`)は、走査中にすでに行・列を追っているため
/// この関数を経由しない。
pub fn line_col(source: &str, byte_offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for (offset, ch) in source.char_indices() {
        if offset >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}
```

`line_col`はソースの先頭から`byte_offset`まで文字を数え直す関数なので、呼ぶたびにソースの長さに比例した時間がかかります。
エラー表示という頻度の低い場面でしか呼ばれないので、この単純な実装で十分です。
第7章の構文解析器がエラー位置を表示する際も、この関数をそのまま再利用します。

### TokenKindとKeyword

```rust
/// トークンの種類。
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// 識別子。引用符では囲まれていない、大文字小文字を保持したままのテキスト。
    Ident(String),
    /// 予約語。
    Keyword(Keyword),
    /// 整数リテラル。符号を持たない絶対値のまま保持する。
    ///
    /// `i64::MIN`(`-9223372036854775808`)の絶対値`9223372036854775808`は
    /// `i64`の範囲を超える(`i64::MAX`は`9223372036854775807`)ため、`i64`では
    /// なく`u64`で持つ。符号を`i64`へ適用して範囲検査する仕事は、単項`-`と
    /// このTokenを組み合わせる`Parser`(第7章)に委ねる。
    IntLiteral(u64),
    /// 文字列リテラル。`''`によるエスケープは解決済みの値を持つ。
    StringLiteral(String),
    /// `+`
    Plus,
    // ...(`-` `*` `/` `=` `<>` `<` `<=` `>` `>=` `(` `)` `,` `;` も1バリアント1トークンで続く)
    /// 入力の終端を表す番兵トークン。
    Eof,
}
```

整数リテラルが`i64`ではなく`u64`を持つのは、`i64::MIN`を表現するためです。
`-9223372036854775808`という値そのものは`i64`の範囲に収まりますが、その絶対値`9223372036854775808`は`i64::MAX`(`9223372036854775807`)を1だけ超えます。
字句解析の段階では符号がまだ`Minus`という別のTokenとして分かれているため、もし`IntLiteral`が`i64`を持つ設計にすると、`9223372036854775808`という絶対値そのものをこの型に収められず、`i64::MIN`だけがどうしても書けないリテラルになってしまいます。
そこで`IntLiteral`は符号なしの絶対値を`u64`として保持するにとどめ、符号の適用と最終的な範囲検査は第7章の構文解析器に委ねます。

`Keyword`は先ほど決めた21個の予約語を持つ列挙型です。
文字列から`Keyword`への変換は、大文字小文字を無視して比較する`from_word`にまとめます。

```rust
impl Keyword {
    /// 大文字小文字を無視して`word`を予約語に変換する。予約語でなければ`None`。
    fn from_word(word: &str) -> Option<Keyword> {
        let keyword = match word.to_ascii_uppercase().as_str() {
            "SELECT" => Keyword::Select,
            "FROM" => Keyword::From,
            "WHERE" => Keyword::Where,
            // ...(残り18キーワードも同じ形で、大文字化した文字列を1対1で対応付ける)
            "CAST" => Keyword::Cast,
            _ => return None,
        };
        Some(keyword)
    }
}
```

識別子とキーワードは、走査そのものは同じ処理で行い、切り出した文字列を`Keyword::from_word`に渡した結果で分岐します。

```rust
    /// 識別子として読める間読み進め、予約語なら`Keyword`、そうでなければ`Ident`にする。
    fn lex_ident_or_keyword(&mut self) -> TokenKind {
        let start = self
            .peek_offset()
            .expect("呼び出し元が識別子開始文字の存在を確認済み");

        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                self.bump();
            } else {
                break;
            }
        }

        let end = self.peek_offset().unwrap_or(self.source.len());
        let text = &self.source[start..end];
        match Keyword::from_word(text) {
            Some(keyword) => TokenKind::Keyword(keyword),
            None => TokenKind::Ident(text.to_string()),
        }
    }
```

`is_ident_continue`は英数字とアンダースコアを許し、`is_ident_start`は先頭に数字を許しません。
先頭の1文字が数字か英字かによって、`tokenize`のループが数値リテラルの読み取り(`lex_number`)と識別子の読み取り(`lex_ident_or_keyword`)のどちらに進むかが決まります。
`1abc`のように数字の直後へ区切りなく識別子文字が続く入力をどう扱うかは、この振り分けとは別の話で、`lex_number`側の仕事です(後述)。

### 走査の骨格

`Lexer`は、ソース文字列と現在位置(バイトオフセット、行番号、列番号)を持つ構造体です。

```rust
struct Lexer<'a> {
    source: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    line: usize,
    column: usize,
}
```

1文字先読みできる`Peekable`にしているのは、`<`の次が`=`かどうかで`Lt`と`LtEq`を区別するなど、多くの分岐が「次の1文字を覗いてから消費するかどうかを決める」形になるからです。
文字を実際に消費するのは`bump`という小さなメソッドで、`self.chars.next()`を呼びつつ、読んだ文字が改行なら`line`を1増やして`column`を1に戻し、それ以外なら`column`を1増やします。
以降に出てくる`self.bump()`という呼び出しは、すべてこのメソッドを指します。

トークン化全体のループは、空白とコメントを読み飛ばしたあと、残った最初の1文字の種類で分岐します。

```rust
    fn tokenize(mut self) -> DbResult<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments()?;

            let Some(start) = self.peek_offset() else {
                let end = self.source.len();
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    span: Span::new(end, end),
                });
                break;
            };

            let ch = self.peek_char().expect("直前にNoneでないことを確認済み");
            let kind = if ch.is_ascii_digit() {
                self.lex_number()?
            } else if ch == '\'' {
                self.lex_string()?
            } else if is_ident_start(ch) {
                self.lex_ident_or_keyword()
            } else {
                self.lex_operator_or_punct()?
            };
            let end = self.peek_offset().unwrap_or(self.source.len());
            tokens.push(Token {
                kind,
                span: Span::new(start, end),
            });
        }
        Ok(tokens)
    }
```

数字なら整数リテラル、`'`なら文字列リテラル、識別子の開始文字なら識別子かキーワード、それ以外は演算子か記号として扱います。
入力が尽きたら`Eof`を1個積んでループを終えます。
`Eof`を明示的な`Token`として残すのは、第7章の構文解析器が「次のトークンを覗く」という操作を、入力の終端でも同じコードで書けるようにするためです。
`Option<&Token>`で終端を表す設計も可能ですが、そうすると構文解析器は「次のトークンがあるかどうか」と「次のトークンの種類が何か」という2つの分岐を毎回両方書く必要があります。
`Eof`という値を持つトークンにしておけば、後者の分岐だけで済みます。

### コメントと空白

行コメント(`--`から行末まで)とブロックコメント(`/*`から`*/`まで)は、`skip_whitespace_and_comments`でまとめて読み飛ばします。

```rust
    /// 空白、行コメント(`--`)、ブロックコメント(`/* */`)を読み飛ばす。
    ///
    /// 3種類が交互に現れる入力(`  -- 注釈\n  /* 注釈 */  SELECT`)にも対応するため、
    /// どれにも該当しなくなるまでループする。
    fn skip_whitespace_and_comments(&mut self) -> DbResult<()> {
        loop {
            if let Some(c) = self.peek_char()
                && c.is_whitespace()
            {
                self.bump();
                continue;
            }

            if self.remaining().starts_with("--") {
                self.bump();
                self.bump();
                while let Some(c) = self.peek_char() {
                    if c == '\n' {
                        break;
                    }
                    self.bump();
                }
                continue;
            }

            // ブロックコメント("/*"から"*/"まで)の読み飛ばしも同じ形で続く。
            // 閉じずに入力が尽きたら、開始位置の行・列でDbError::Lexを返す。

            break;
        }
        Ok(())
    }
```

空白、行コメント、ブロックコメントのどれにも該当しなくなるまでループしているのは、`--注釈` `/* 注釈 */` `SELECT`のように3種類が交互に現れる入力があるためです。
1回のパスで空白だけ、次のパスでコメントだけ、という処理にすると、この交互パターンを取りこぼします。

ブロックコメントは、開始位置の行番号と列番号を`start_line`と`start_column`に保存しておき、閉じずに入力が尽きた場合はそこをエラー位置として報告します。
利用者が実際に直したいのは`/*`を書いた場所であって、コメントが尽きて力尽きた入力の末尾ではないからです。

冒頭で見た`-- 後で3も足す`は、この`skip_whitespace_and_comments`によって読み飛ばされ、後段には渡りません。
`SELECT 1 + 2 -- 後で3も足す`をトークン化すると、`Select`、`IntLiteral(1)`、`Plus`、`IntLiteral(2)`、`Eof`という5個の`Token`が残り、コメントの内容はどこにも現れなくなります。

### 文字列リテラル

`'...'`で囲まれた部分を文字列リテラルとして読みます。
SQLでは、文字列リテラルの中に`'`自体を含めたい場合、`''`と2つ並べて書く約束になっています。

```rust
    /// 開きの`'`から閉じの`'`までを読む。`''`は1個の`'`として値に含める。
    fn lex_string(&mut self) -> DbResult<TokenKind> {
        let start_line = self.line;
        let start_column = self.column;
        self.bump(); // 開きの'

        let mut value = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(DbError::Lex {
                        message: "閉じない文字列リテラルです".to_string(),
                        line: start_line,
                        column: start_column,
                    });
                }
                Some((_, '\'')) => {
                    if self.peek_char() == Some('\'') {
                        self.bump();
                        value.push('\'');
                    } else {
                        break;
                    }
                }
                Some((_, c)) => value.push(c),
            }
        }
        Ok(TokenKind::StringLiteral(value))
    }
```

`'`を1個読んだ直後にもう1個`'`が続いていれば、それはエスケープされた`'`なので`value`に`'`を1文字追加して読み続けます。
続いていなければ、その`'`が文字列を閉じる引用符です。
`'it''s'`という入力は`it's`という5文字の値に、`'unterminated`のように閉じの`'`が現れないまま入力が尽きた場合は`DbError::Lex`になります。
このエラーの行と列も、ブロックコメントと同じ理由で開きの`'`の位置を指します。

### 整数リテラルと演算子

整数リテラルは数字が続く間読み進め、`u64`として解析します。
数字の直後に区切りなく識別子文字(英字、数字、`_`)が続く場合(`1abc`、`1_2`)は、`1`と`abc`のように黙って2個のTokenへ分けず、その場で字句エラーにします。
区切りの無い数字と識別子の並びを許すと、`1abc`が`1 abc`と書いたのと同じ意味に誤読されかねないためです。

```rust
    /// 数字が連続する間読み進め、`u64`として解析する(符号は付けない。
    /// `TokenKind::IntLiteral`のドキュメント参照)。
    ///
    /// 数字の直後に識別子文字(英字・数字・`_`)が続く場合(`1abc`、`1_2`)は、
    /// `1`と`abc`の2個のTokenへ黙って分割せず、字句エラーにする。区切りの
    /// 無い数字と識別子の並びを許すと、`1abc`が`1 abc`と書いたのと同じ意味に
    /// 誤読されかねないため。
    fn lex_number(&mut self) -> DbResult<TokenKind> {
        let start_line = self.line;
        let start_column = self.column;
        let start = self
            .peek_offset()
            .expect("呼び出し元が数字の存在を確認済み");

        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }

        let end = self.peek_offset().unwrap_or(self.source.len());
        let text = &self.source[start..end];

        if let Some(c) = self.peek_char()
            && is_ident_continue(c)
        {
            // メッセージに残りの識別子文字も含めるため、字句解析の位置は
            // 動かさずに`remaining()`を覗き見して`is_ident_continue`が
            // 続く分だけを切り出す。
            let tail_len = self
                .remaining()
                .char_indices()
                .find(|(_, c)| !is_ident_continue(*c))
                .map(|(offset, _)| offset)
                .unwrap_or_else(|| self.remaining().len());
            let tail = &self.remaining()[..tail_len];
            return Err(DbError::Lex {
                message: format!("数値リテラルの直後に識別子文字が続いています: {text}{tail}"),
                line: start_line,
                column: start_column,
            });
        }

        text.parse::<u64>()
            .map(TokenKind::IntLiteral)
            .map_err(|_| DbError::Lex {
                message: format!("整数リテラルの範囲を超えています: {text}"),
                line: start_line,
                column: start_column,
            })
    }
```

`text.parse::<u64>()`が失敗するのは、桁数が多すぎて`u64`の範囲(`0`から`18446744073709551615`)を超えた場合だけです。
数字の並びである時点で構文としては正しいので、この失敗は構文エラーではなく、値が扱える範囲を超えたという字句レベルのエラーとして報告します。

符号は数値リテラルの一部にしません。
`-1`は、`Minus`という演算子トークンと`IntLiteral(1)`という2個のトークンに分かれます。
`1 - 2`(減算)と`1 + -2`(負の数の加算)のどちらも、`-`という1個の演算子トークンから組み立てられるようにしておくことで、単項の`-`と2項の`-`をどう構文木に組み立てるかという判断を、第7章の構文解析器にまとめて委ねられます。
`IntLiteral`が符号を持たない`u64`である以上、この委譲にはもう1つ役目が加わります。
`Minus`の直後に`IntLiteral`が来た場合に限り、両者を合わせて符号付きの`i64`へ変換し、その変換が`i64`の範囲を超えていないかを検査するのも第7章の構文解析器の仕事です(`i64::MIN`はこの経路でしか書けません)。

数字の直後に識別子文字が続く場合の字句エラーも、この章で実際に確かめておきます。

```console
minidb> SELECT 1abc;
エラー: 行1列8: 字句エラー: 数値リテラルの直後に識別子文字が続いています: 1abc
```

`1`と`abc`という2個のTokenへ黙って分割してしまうと、`1abc`と`1 abc`(空白で区切った場合)が同じToken列になり、書き手が区切りを忘れただけなのか、本当に2つの別々の値のつもりだったのかを後段が区別できません。
`lex_number`がこの場で字句エラーにすることで、区切りの欠落をこの章のうちに検出します。

比較演算子と残りの記号は`lex_operator_or_punct`が読みます。
1文字読んでから次の1文字を覗き、2文字の演算子かどうかを判定する分岐が`<`と`>`にあります。

```rust
    /// 演算子・括弧・カンマ・セミコロンを1個読む。
    fn lex_operator_or_punct(&mut self) -> DbResult<TokenKind> {
        let start_line = self.line;
        let start_column = self.column;
        let (_, ch) = self.bump().expect("呼び出し元が文字の存在を確認済み");

        let kind = match ch {
            '+' => TokenKind::Plus,
            // ...(`-` `*` `/` `=` も同様に1文字1トークンで対応付ける)
            '<' => {
                if self.peek_char() == Some('=') {
                    self.bump();
                    TokenKind::LtEq
                } else if self.peek_char() == Some('>') {
                    self.bump();
                    TokenKind::NotEq
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                if self.peek_char() == Some('=') {
                    self.bump();
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
            }
            // ...(`(` `)` `,` `;` の1文字1トークンがここに続く)
            other => {
                return Err(DbError::Lex {
                    message: format!("不明な文字です: {other:?}"),
                    line: start_line,
                    column: start_column,
                });
            }
        };
        Ok(kind)
    }
```

`<>`を`NotEq`という1個のトークンにするか、`Lt`と`Gt`の2個のトークンにするかも設計の選択肢ですが、後者では構文解析器が「`<`の直後に`>`が来たら不等号」という文法規則を持たなければならず、字句解析器が確定できるはずの情報をわざわざ後段に持ち越すことになります。
この章で立てた「文字面から一意に決まる情報はこの段階で確定させる」という方針どおり、`<>`は1個のトークンにします。

### DbError::Lex

字句解析のエラーは、`DbError`に新しく追加した`Lex`バリアントで表します。
`src/error.rs`の`DbError`に、次のバリアントを追加します。

```rust
    /// SQL文字列をToken列へ変換できなかったエラー。発生位置の行・列を持つ。
    #[error("行{line}列{column}: 字句エラー: {message}")]
    Lex {
        /// エラーの内容。
        message: String,
        /// 発生位置の行番号(1始まり)。
        line: usize,
        /// 発生位置の列番号(1始まり)。
        column: usize,
    },
```

第5章で追加した`DbError::Parse`を拡張するのではなく、新しいバリアントとして追加しました。
`Parse`は`toy_sql`の構文レベルのエラーにすでに使われていて、そちらは(第7章で本物の構文解析器に置き換わるまでは)位置情報を持ちません。
`Parse`の意味を「位置情報を持つこともあれば持たないこともあるエラー」に変えるより、「字句解析の失敗は必ず行と列を持つ」という約束を型で表せる別バリアントを立てるほうが、呼び出し側が`match`で迷わずに済みます。

### `toy_sql`をLexer経由に置き換える

`toy_sql`の構文自体(`SELECT <式>`しか読めない、という制限)はこの章では変えません。
変えるのは、その構文を文字列から直接読み取っていた部分を、`Lexer`が返す`Token`列を読む形に置き換えることです。
`src/toy_sql.rs`の`parse_select`を、次のように書き換えます。

```rust
/// `SELECT <式> [;]`を解析する。
///
/// 対応する構文は次のみ。
/// - 整数リテラル: `1`、`42`
/// - 真偽値リテラル: `true`、`false`
/// - 整数どうしの加算: `1 + 2`
///
/// 字句解析(`lexer::tokenize`)が失敗した場合は`DbError::Lex`を、トークン列は
/// 得られたがこの仮実装が読める構文でない場合(`SELECT`で始まらない、対象式が
/// 空、加算の片方が整数でない等)は`DbError::Parse`を返す。
pub fn parse_select(sql: &str) -> DbResult<ToySelect> {
    let sql = sql.trim();
    let tokens = lexer::tokenize(sql)?;
    let mut iter = tokens.iter();

    match iter.next().map(|t| &t.kind) {
        Some(TokenKind::Keyword(Keyword::Select)) => {}
        _ => return Err(DbError::Parse(format!("SELECT文ではありません: {sql:?}"))),
    }

    let rest: Vec<&Token> = iter
        .take_while(|t| !matches!(t.kind, TokenKind::Semicolon | TokenKind::Eof))
        .collect();

    if rest.is_empty() {
        return Err(DbError::Parse("SELECTの対象式がありません".to_string()));
    }

    let expr = parse_expr(&rest)?;
    Ok(ToySelect {
        expr_text: expr_text(sql, &rest),
        expr,
    })
}
```

`lexer::tokenize(sql)?`が失敗すれば、`?`によって`DbError::Lex`がそのまま`parse_select`の呼び出し元まで伝わります。
`SELECT`キーワードだったかどうかの判定は、`strip_keyword`という文字列比較の関数から、`Keyword::Select`という`Token`の種類の比較に変わりました。
対象式のトークン列を`Semicolon`か`Eof`が出るまで集める`take_while`が、前章の`strip_suffix(';')`に対応します。

`expr_text`(結果の列名に使う元テキスト)は、対象式の最初と最後のトークンが持つ`Span`から直接切り出します。

```rust
/// 対象式のトークン列が、元のSQL文字列中で占める範囲をそのまま切り出す。
///
/// トークンの`Span`を経由することで、`SELECT 1 + 2;`から`"1 + 2"`のように、
/// 内部の空白は保ちつつ前後の`SELECT`と`;`だけを取り除いたテキストが得られる。
fn expr_text(sql: &str, tokens: &[&Token]) -> String {
    let start = tokens
        .first()
        .expect("空でないことを呼び出し元(parse_select)が保証する")
        .span
        .start;
    let end = tokens
        .last()
        .expect("空でないことを呼び出し元(parse_select)が保証する")
        .span
        .end;
    sql[start..end].to_string()
}
```

`SELECT 1 + 2;`であれば、対象式の最初のトークン`IntLiteral(1)`の`span.start`と、最後のトークン`IntLiteral(2)`の`span.end`の間を切り出すことで、`"1 + 2"`が前章までと同じ形で得られます。
文字列を毎回スライスし直すのではなく`Span`を経由するのは、この`expr_text`が字句解析の結果をそのまま再利用できることを示す小さな例です。

式の解析も、`+`という文字での文字列分割から、`Plus`という`Token`での配列分割に置き換わります。

```rust
/// `+`で連結された式を解析する。項が1つなら`parse_term`にそのまま委ねる。
fn parse_expr(tokens: &[&Token]) -> DbResult<ToyExpr> {
    let terms: Vec<&[&Token]> = tokens
        .split(|t| matches!(t.kind, TokenKind::Plus))
        .collect();
    if terms.iter().any(|t| t.is_empty()) {
        return Err(DbError::Parse("式を解析できません".to_string()));
    }

    if let [only] = terms.as_slice() {
        return parse_term(only);
    }

    // 2項以上の加算は、このミニ実装では整数リテラルどうしにしか対応しない。
    // ...(前章と同じ制限が続く)
}
```

`str::split`が`slice::split`に変わっただけで、項を集める考え方そのものは前章から変わっていません。
違うのは、分割の基準が「`+`という文字」から「`Plus`という`Token`の種類」になったことです。
これにより、文字列リテラルの中に`+`が含まれていても(この章で文字列リテラルを読めるようになったので、これは実在する入力です)、字句解析の段階ですでに1個の`StringLiteral`トークンにまとまっているため、誤って分割されることがなくなります。

## テストで確認する

`Lexer`と`toy_sql`のそれぞれに単体テストを追加しました。
キーワード、識別子、リテラル、演算子、コメント、そして位置情報付きのエラーを確認します。
`src/lexer.rs`の末尾の`mod tests`に、次のテストを追加します。

```rust
    #[test]
    fn tokenizes_keywords_case_insensitively() {
        assert_eq!(
            kinds("select FROM Where"),
            vec![
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Keyword(Keyword::From),
                TokenKind::Keyword(Keyword::Where),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn tokenizes_string_literal_with_escaped_quote() {
        assert_eq!(
            kinds("'it''s'"),
            vec![TokenKind::StringLiteral("it's".to_string()), TokenKind::Eof]
        );
    }

    #[test]
    fn skips_block_comment_across_lines() {
        assert_eq!(
            kinds("1 /* comment\nspans lines */ 2"),
            vec![
                TokenKind::IntLiteral(1),
                TokenKind::IntLiteral(2),
                TokenKind::Eof,
            ]
        );
    }
```

位置情報については、開きの`'`の行と列がエラーに正しく現れることを確認します。

```rust
    #[test]
    fn unterminated_string_literal_reports_line_of_the_opening_quote() {
        let err = tokenize("SELECT 1;\n'abc").unwrap_err();
        match err {
            DbError::Lex { line, column, .. } => assert_eq!((line, column), (2, 1)),
            other => panic!("DbError::Lexを期待したが{other:?}が返った"),
        }
    }
```

`database`側にも、`Database::execute`まで通した結合的なテストを1件追加しています。
`src/database.rs`の`mod tests`に追加します。

```rust
    #[test]
    fn propagates_lex_error_with_position() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 'abc");
        match result {
            Err(DbError::Lex { line, column, .. }) => assert_eq!((line, column), (1, 8)),
            Err(e) => panic!("DbError::Lexを期待したがDbError::Parse等が返った: {e}"),
            Ok(_) => panic!("DbError::Lexを期待したがOkが返った"),
        }
    }
```

`cargo test`を実行すると、`lexer`モジュールの16件を含む42件の単体テストと、3件のSQL Golden Testを含む`golden_tests_pass`が通ります。

```console
$ cargo test
running 42 tests
test lexer::tests::tokenizes_keywords_case_insensitively ... ok
test lexer::tests::unterminated_string_literal_is_a_lex_error_at_open_quote ... ok
test database::tests::propagates_lex_error_with_position ... ok
...
test result: ok. 42 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

REPLで実際に確かめると、冒頭で見た位置の分からないエラーが、行と列を示すメッセージに変わっています。

```console
$ cargo run
minidb> SELECT 1;
1
-
1
(1 row)
minidb> SELECT 'abc
エラー: 行1列8: 字句エラー: 閉じない文字列リテラルです
minidb> \q
```

`'`を書いた列(8列目)がそのままエラーに現れているので、利用者はソースを目視で探す必要がありません。

## 壊して確認する

`lex_string`の、`''`をエスケープとして扱う分岐を崩してみます。

```rust
Some((_, '\'')) => {
    // '' によるエスケープの判定を外し、常に閉じ引用符として扱ったとする
    break;
}
```

この変更をすると、`tokenizes_string_literal_with_escaped_quote`が赤くなります。
`'it''s'`という入力は、2文字目の`'`が来た時点で文字列が閉じたことになり、`StringLiteral("it")`という短いトークンと、続く`s'`が識別子`s`とunclosedな`'`として誤って解釈されるためです。
このテストが無ければ、`''`によるエスケープが効かなくなったことに、文字列リテラルを含むSQLを実際に書くまで気づけません。

同様に、`skip_whitespace_and_comments`の`*/`を探す内側の`loop`を外し、1文字読んだだけでブロックコメントの終わりと決めつけるように崩すと、`skips_block_comment_across_lines`が失敗します。
`/* comment\nspans lines */`のような複数行にまたがるコメントは、`*/`が現れるまで何文字でも読み飛ばす必要があるためです。
1文字だけ読んで抜けてしまうと、コメントの残りが`Token`として漏れ出し、後段の構文解析にとって意味不明な入力になります。

## 演習問題

### 必須課題

1. `TokenKind`に、10進小数点を含む数値リテラル(`3.14`)を`FloatLiteral(f64)`として追加してください。`lex_number`のどこを直せば、整数リテラルと小数リテラルの両方を読めるようになるか考えてから実装してください。
2. 二重引用符(`"..."`)で囲んだ識別子を読めるようにしてください。多くのSQL実装では、二重引用符で囲んだ識別子は大文字小文字を保持したまま扱われ、囲んでいない識別子とは区別されます。この違いを`TokenKind::Ident`だけで表せるか、それとも新しいバリアントが要るかを検討してください。

### 発展課題

1. `line_col`は呼ばれるたびにソースの先頭から数え直すため、同じソースに対して何度も呼ぶと非効率です。改行が現れるバイトオフセットの一覧を事前に1回だけ作っておき、2回目以降の呼び出しを二分探索で高速化する設計を考えてください。ソースが変わるたびにこの一覧を作り直す必要がある点にも触れてください。
2. この章の`Lexer`は、無効な文字に遭遇した時点で走査全体を打ち切り、それ以降のトークンを1個も返しません。IDEの構文ハイライトのような用途では、1箇所のエラーで残り全体の情報を失うのは避けたいことがあります。エラーを記録しつつ走査を続け、最後に`Vec<Token>`と`Vec<DbError>`の両方を返すような`Lexer`の設計を考え、今回の設計との得失を比較してください。
