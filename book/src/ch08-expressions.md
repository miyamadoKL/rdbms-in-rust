# 第8章 型、NULL、式評価

前章のParserは`1 + 2 * 3`を、`Add(1, Multiply(2, 3))`という正しい木として解析できるようになりました。
それでも`Database::execute("SELECT 1 = 1;")`を実行すると、返るのはまだ`DbError::NotImplemented`です。
`=`という演算子は字句解析でも構文解析でも一度もつまずいていません。
`1 = 1`は正しいASTに組み上がったうえで、評価する段になって初めて「わかりません」と言われるのです。

もう1つ、`NULL`が絡む式には別種の落とし穴があります。
`SELECT NULL = NULL;`を実行できるようになったとして、その結果は何であるべきでしょうか。
両辺は同じ`NULL`なので、直感的には`TRUE`を返してほしくなります。
しかしSQLの答えは`TRUE`ではなく、`FALSE`でもありません。
`NULL`のままです。

この章で作る`eval`モジュールは、`Expr`を受け取って`Value`を返す評価器です。
算術演算、比較演算、論理演算、`IS NULL`、`CAST`、Scalar Function呼び出しがここに揃い、`Database::execute`はようやく`SELECT`の式を最後まで実行できるようになります。
`NULL = NULL`が`NULL`になる理由も、この章の設計がそのまま説明します。

## NULLは値ではなく「わからない」という状態を表す

`src/types.rs`で定義した`crate::types::Value`はすでに`Null`というバリアントを持っています。

```rust
pub enum Value {
    Null,
    Boolean(bool),
    BigInt(i64),
    Text(String),
}
```

`Value::Null`は「空の文字列」や「0」のような、型の中にある特別な値ではありません。
`BigInt`の`0`はれっきとした整数であり、他の`BigInt`と大小を比較できます。
`Null`はそうではなく、「その列にどんな値が入っているか、今はわからない」という状態そのものを表します。

この違いが比較演算にそのまま影響します。
`1 = 1`が`TRUE`なのは、比較する2つの値が両方とも確定しているからです。
`NULL = NULL`では、両辺とも「わからない」としか言っていません。
片方の「わからない」ともう片方の「わからない」が同じ値を指しているかどうかは、それ自体がわからないことです。
だから答えは`TRUE`ではなく、`FALSE`でもなく、「わからない」を表す`NULL`になります。

比較の結果が2値(`TRUE`/`FALSE`)ではなく3値(`TRUE`/`FALSE`/`UNKNOWN`)を取りうるという性質を、**三値論理**と呼びます[^kleene]。
SQLの`AND`、`OR`、`NOT`は、この3値を引数に取り3値を返す論理演算として定義されています。
`Value::Boolean(bool)`と`Value::Null`の組が、ちょうどこの3状態(`TRUE`、`FALSE`、`UNKNOWN`)に対応します。

[^kleene]: 三値論理そのものはS. C. Kleeneが1938年に導入した強three-valued logicに遡る。SQL標準はこのKleeneの真理値表をそのまま採用している。

## `Option<bool>`の言葉でAND/ORを書いてはいけない

`Value::Boolean(bool)`と`Value::Null`という組は、見た目には`Option<bool>`とよく似ています。
`Some(true)`が`TRUE`、`Some(false)`が`FALSE`、`None`が`UNKNOWN`に対応しそうです。
この対応を信じて、Rustの`?`演算子で`AND`を実装できるかどうか、次のコードを例に考えます。

```rust
fn and(l: Option<bool>, r: Option<bool>) -> Option<bool> {
    Some(l? && r?)
}
```

このコードは`and(Some(true), Some(false))`を`Some(false)`に、`and(None, None)`を`None`に正しく変換します。
`and(Some(false), None)`を試しても、結果は`Some(false)`になります。
Rustの`&&`は左辺が`false`のとき右辺を評価しない短絡評価を行うため、`l?`が`false`に解決した時点で`r?`には触れず、`r`が`None`であっても結果に影響しないからです。
ここまでは三値論理の直感と一致しているように見えます。

ところが、引数の順序を入れ替えて`and(None, Some(false))`を試すと、`Some(false)`を返してほしいところで`None`が返ります。
`l?`が`None`に出会った時点で、関数全体がその場で`None`を返して打ち切られてしまうからです。
`r`の値がすでに`FALSE`だと確定しているにもかかわらず、`l`が`UNKNOWN`だというだけで、関数は`r`を見ずに投げ出してしまいます。

これは`Option`の`?`という道具そのものの性質です。
`?`は「最初に出会った`None`で処理を打ち切る」という短絡評価のために設計されており、Rustのエラー伝播にはうってつけの挙動です。
しかし三値論理の`AND`が要求する規則は、これとは違います。
`FALSE AND UNKNOWN`と`UNKNOWN AND FALSE`は、どちらも`FALSE`でなければならず、2つの引数のどちらを先に書くかによって結果が変わってはいけません。
`?`による`None`の伝播は、書かれた順に左から評価を打ち切るだけの規則なので、SQLでは同じ答えになるべき`and(Some(false), None)`と`and(None, Some(false))`という2つの呼び出しに、異なる結果を返してしまいます。

`Option<bool>`という型そのものが間違っているわけではありません。
問題は、`?`や、素朴に連ねた`and_then`のように「最初に出会った`None`で打ち切る」という評価順序に結果を依存させるコンビネータでは、引数を書いた順序がそのまま答えに漏れ出してしまう、という一点です。
三値論理の`AND`は、`FALSE`がどちらの引数の位置にあっても`UNKNOWN`より強い値として振る舞う、引数の順序に依存しない規則を必要とします。
この規則を表現するには、評価順序に頼る短絡評価をやめ、2引数の総当たりで真理値表そのものを書き下すしかありません。

## 真理値表をコードに落とす

この章の`eval`モジュールでは、`Value`とは別に`Tri`という3値の列挙型を評価の内部でだけ使います。
この章では`src/eval.rs`を新規に作成し、評価器の実装をまとめて置きます。

```rust
enum Tri {
    True,
    False,
    Unknown,
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod eval;
```

`Value`に3つめのバリアントを増やすのではなく、論理演算の間だけ使う専用の型を用意しているのは、`Value::Boolean`/`Value::Null`という対外的な表現と、論理演算の内部計算を分離するためです。
`src/eval.rs`に定義する`value_to_tri`と`tri_to_value`が、この2つの表現を橋渡しします。

```rust
fn value_to_tri(value: &Value) -> DbResult<Tri> {
    match value {
        Value::Null => Ok(Tri::Unknown),
        Value::Boolean(true) => Ok(Tri::True),
        Value::Boolean(false) => Ok(Tri::False),
        other => {
            // `Value::Null`と`Value::Boolean`は直前の分岐で処理済みなので、
            // `data_type()`は必ず`Some`を返す。
            let data_type = other
                .data_type()
                .expect("NullとBooleanは既に処理済みなのでdata_type()は必ずSomeを返す");
            Err(DbError::Eval(format!(
                "論理演算はBOOLEANまたはNULLに対してのみ使えます: {data_type}が渡されました"
            )))
        }
    }
}
```

`BOOLEAN`でも`NULL`でもない値(`BIGINT`や`TEXT`)を論理演算に渡すとエラーになります。
エラーメッセージには`Option<DataType>`のRust内部表現(`Some(BigInt)`)ではなく、`DataType`の`Display`実装によるSQLの型名(`BIGINT`)だけを表示します。
`1 AND true`のような式を暗黙に`Boolean`へ変換して通す設計も選べますが、この章では暗黙変換を採らない方針(次節で改めて述べます)を通しています。

続けて、次の`tri_to_value`を定義します。

```rust
fn tri_to_value(tri: Tri) -> Value {
    match tri {
        Tri::True => Value::Boolean(true),
        Tri::False => Value::Boolean(false),
        Tri::Unknown => Value::Null,
    }
}
```

`AND`と`OR`は、前節の`?`で崩れた規則を、9通りの組み合わせをすべて列挙するmatch式で書き直します。

```rust
fn tri_and(l: Tri, r: Tri) -> Tri {
    match (l, r) {
        (Tri::False, _) | (_, Tri::False) => Tri::False,
        (Tri::True, Tri::True) => Tri::True,
        _ => Tri::Unknown,
    }
}

fn tri_or(l: Tri, r: Tri) -> Tri {
    match (l, r) {
        (Tri::True, _) | (_, Tri::True) => Tri::True,
        (Tri::False, Tri::False) => Tri::False,
        _ => Tri::Unknown,
    }
}
```

`tri_and`の最初の腕`(Tri::False, _) | (_, Tri::False) => Tri::False`が、まさに前節で`?`が実現できなかった規則です。
左右どちらの位置に`False`が来ても、もう一方が`Unknown`であっても、結果は`False`に固定されます。
`tri_or`はこの勝ち負けが反転するだけで、`True`がどちらの位置にあっても`Unknown`に優先します。

`NOT`は引数を1つしか取らないので、`src/eval.rs`で`Tri`に`std::ops::Not`を実装するだけで済みます。

```rust
impl std::ops::Not for Tri {
    type Output = Tri;

    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }
}
```

`NOT UNKNOWN`が`Unknown`のままなのは自然です。
「わからない」ものを打ち消しても、「わかった」ことにはならないからです。

この3つの関数がSQLの三値論理の全体です。
`AND`の真理値表は次のとおりで、`tri_and`の実装がそのままこの表に対応します。

| AND     | TRUE    | FALSE | UNKNOWN |
|---------|---------|-------|---------|
| TRUE    | TRUE    | FALSE | UNKNOWN |
| FALSE   | FALSE   | FALSE | FALSE   |
| UNKNOWN | UNKNOWN | FALSE | UNKNOWN |

`OR`の真理値表は`AND`の`TRUE`と`FALSE`を入れ替えた形になります。

| OR      | TRUE | FALSE   | UNKNOWN |
|---------|------|---------|---------|
| TRUE    | TRUE | TRUE    | TRUE    |
| FALSE   | TRUE | FALSE   | UNKNOWN |
| UNKNOWN | TRUE | UNKNOWN | UNKNOWN |

`NOT`は1引数なので3行だけです。

| NOT     |         |
|---------|---------|
| TRUE    | FALSE   |
| FALSE   | TRUE    |
| UNKNOWN | UNKNOWN |

## 算術演算：NULLの伝播、ゼロ除算、オーバーフロー

論理演算の次は算術演算です。
対応するのは`+ - * /`の4つで、いずれも`BIGINT`同士にのみ、`src/eval.rs`の`eval_arith`として定義します。

```rust
fn eval_arith(op: BinaryOperator, l: Value, r: Value) -> DbResult<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    match (l, r) {
        (Value::BigInt(l), Value::BigInt(r)) => {
            let result = match op {
                BinaryOperator::Add => l.checked_add(r),
                BinaryOperator::Subtract => l.checked_sub(r),
                BinaryOperator::Multiply => l.checked_mul(r),
                BinaryOperator::Divide => {
                    if r == 0 {
                        return Err(DbError::Eval("ゼロ除算です".to_string()));
                    }
                    l.checked_div(r)
                }
                _ => unreachable!("eval_arithはAdd/Subtract/Multiply/Divideのみを受け取る"),
            };
            result
                .map(Value::BigInt)
                .ok_or_else(|| DbError::Eval(format!("整数オーバーフロー: {l} {op:?} {r}")))
        }
        (l, r) => {
            // `Value::Null`は直前の分岐で処理済みなので、両辺とも`data_type()`は
            // 必ず`Some`を返す。
            let l_type = l
                .data_type()
                .expect("Nullは既に処理済みなのでdata_type()は必ずSomeを返す");
            let r_type = r
                .data_type()
                .expect("Nullは既に処理済みなのでdata_type()は必ずSomeを返す");
            Err(DbError::Eval(format!(
                "算術演算はBIGINT同士にのみ使えます: {l_type}と{r_type}"
            )))
        }
    }
}
```

最初の分岐が三値論理と同じ形の規則を持っています。
比較演算のところで詳しく述べますが、算術演算もNULLを「片方でもあれば結果全体がNULL」という形で伝播させます。
`1 + NULL`が`NULL`になるのはこの規則によるもので、`AND`/`OR`のようにどちらかの値が結果を決定づけるということはありません。
足し算にとって「わからない数」を足した結果は、常に「わからない数」だからです。

オーバーフローの扱いは、この章で決めておくべき設計判断です。
Rustの`i64`は、`release`ビルドでは既定でオーバーフロー時に静かに折り返し(wrapping)ますが、`debug_assert`が有効な`debug`ビルドではパニックします。
どちらの挙動も、SQLを実行するデータベースとしては受け入れられません。
折り返しは`i64::MAX + 1`が`i64::MIN`という筋の通らない値をなにごともなく返してしまいますし、パニックはSQL文1本の失敗でプロセス全体を落としてしまいます。
`checked_add`、`checked_sub`、`checked_mul`、`checked_div`という`Option`を返すメソッド群を使い、オーバーフローを`DbError::Eval`という通常のエラー値として利用者に返す設計にしました。
`checked_div`は`i64::MIN / -1`という、商が`i64::MAX`を1つ超えてしまう唯一のケースでもオーバーフローを検出してくれるので、ゼロ除算の判定と合わせて2種類の失敗をどちらも取りこぼしません。

ここで述べた「NULLの伝播」は、`eval_arith`という関数単体を見たときの規則であることに注意してください。
`if l.is_null() || r.is_null()`という早期リターンは、もう片方の値の型を見るより前に評価されるため、`eval_arith`だけを取り出せば、`(Value::Null, Value::Text("x".to_string()))`のような型として誤った組み合わせを渡しても、型の検査に一度も届かないまま`NULL`を返してしまいます。
実際に`SELECT NULL + 'x'`のようなSQLを実行したときにこの誤りが素通りしないのは、`eval_arith`が呼ばれるより前に、`SELECT`文全体を静的に検査する仕組み(第10章で導入します)が両辺の型を確かめ、型として誤った式をそこで拒否するからです。
この章で述べる「NULLの伝播」は、あくまで型として正しい式に対する実行時の規則であり、型そのものが誤っている式まで通す規則ではありません。

## 比較演算：同じ型どうしにしか意味がない

比較演算(`= <> < <= > >=`)は、`BIGINT`同士、`TEXT`同士、`BOOLEAN`同士のときにだけ、`src/eval.rs`の`eval_compare`として定義します。

```rust
fn eval_compare(op: BinaryOperator, l: Value, r: Value) -> DbResult<Value> {
    use std::cmp::Ordering;

    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let ordering = match (&l, &r) {
        (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
        _ => {
            // `Value::Null`は直前の分岐で処理済みなので、両辺とも`data_type()`は
            // 必ず`Some`を返す。
            let l_type = l
                .data_type()
                .expect("Nullは既に処理済みなのでdata_type()は必ずSomeを返す");
            let r_type = r
                .data_type()
                .expect("Nullは既に処理済みなのでdata_type()は必ずSomeを返す");
            return Err(DbError::Eval(format!(
                "比較演算は同じ型同士にのみ使えます: {l_type}と{r_type}"
            )));
        }
    };
    let result = match op {
        BinaryOperator::Eq => ordering == Ordering::Equal,
        BinaryOperator::NotEq => ordering != Ordering::Equal,
        BinaryOperator::Lt => ordering == Ordering::Less,
        BinaryOperator::LtEq => ordering != Ordering::Greater,
        BinaryOperator::Gt => ordering == Ordering::Greater,
        BinaryOperator::GtEq => ordering != Ordering::Less,
        _ => unreachable!("eval_compareは比較演算子のみを受け取る"),
    };
    Ok(Value::Boolean(result))
}
```

`1 = 'a'`のように型が食い違う比較は、`BigInt`を`Text`へ、あるいはその逆へ暗黙に変換してから比較する道もありえます。
この章ではその道を採らず、`match (&l, &r)`の3パターン以外はすべてエラーにしています。
`BOOLEAN`どうしの大小比較(`false < true`)まで許しているのは、PostgreSQLをはじめとする実装が採用している素朴な規約(`false`が`true`より小さい)にそのまま従ったためで、実用上`ORDER BY`で真偽値を並べ替えたいときに使います。

冒頭で予告した`NULL = NULL`の答えは、この関数の最初の分岐がそのまま担っています。
`l.is_null() || r.is_null()`が`true`になった時点で、型の一致検査にすら進まず`Value::Null`を返すので、`1 = 'a'`のような型不一致とは違うエラーにはなりません。
`NULL`との比較は、比較する相手の型が何であっても常に`UNKNOWN`(`Value::Null`)になります。

## `IS [NOT] NULL`は三値論理から独立している

`IS NULL`だけは、ここまでの三値論理の外側にあり、`src/eval.rs`の`eval_expr`には次の分岐を追加します。

```rust
Expr::IsNull { expr, negated, .. } => {
    let is_null = eval_expr(expr, functions)?.is_null();
    Ok(Value::Boolean(if *negated { !is_null } else { is_null }))
}
```

`1 = NULL`が`UNKNOWN`になるのとは対照的に、`1 IS NULL`は必ず`TRUE`か`FALSE`のどちらかになり、`UNKNOWN`(`Value::Null`)を返すことはありません。
`IS NULL`は「値がわからないかどうか」という、値そのものの中身ではなくメタな性質を尋ねる演算だからです。
「わからない」かどうかという問いに対する答えは、それ自体が「わからない」ことにはなりようがありません。
`WHERE`句で`NULL`を含む行を確実に拾いたいとき、`= NULL`ではなく`IS NULL`を使う理由はここにあります。

## CAST：明示的な型変換だけを許す

`CAST(expr AS type)`は、この章で新しくASTに追加する構文です。
第7章時点の`Expr`にはこの構文が無かったので、`src/ast.rs`の`Expr`に`Cast`バリアントを足します。

```rust
Cast {
    expr: Box<Expr>,
    type_name: Ident,
    span: Span,
},
```

`type_name`を`CreateTableStatement`の`ColumnDef`と同じ`Ident`のまま持たせているのは、Parserが型名の一覧を知らなくてよいという第7章の設計をそのまま踏襲しているためです。
`Parser`はこの構文を`CAST` `(` 式 `AS` 型名 `)`という並びとして受理するだけで、`type_name`が本当に妥当な型かどうかは見ません。
その判定は評価器の`resolve_data_type`に任せます。
`src/eval.rs`に戻り、次の関数を定義します。

```rust
fn resolve_data_type(type_name: &str) -> DbResult<DataType> {
    match type_name.to_ascii_uppercase().as_str() {
        "BIGINT" => Ok(DataType::BigInt),
        "TEXT" => Ok(DataType::Text),
        "BOOLEAN" => Ok(DataType::Boolean),
        other => Err(DbError::Eval(format!("未知の型名です: {other}"))),
    }
}
```

対応する変換の一覧は、次の表に決めました。

| from      | to        | 変換内容 |
|-----------|-----------|----------|
| `NULL`    | 任意      | `NULL`のまま(型を持たないため常に成功) |
| `T`       | `T`       | 恒等変換(常に成功) |
| `BIGINT`  | `TEXT`    | 10進数の文字列表現 |
| `TEXT`    | `BIGINT`  | `i64`として構文解析。失敗時はエラー |
| `BOOLEAN` | `TEXT`    | `"true"` / `"false"` |
| `TEXT`    | `BOOLEAN` | 大小文字を無視して`"true"`/`"false"`を解釈。他はエラー |
| `BIGINT`  | `BOOLEAN` | 非対応(エラー) |
| `BOOLEAN` | `BIGINT`  | 非対応(エラー) |

`BIGINT`と`BOOLEAN`を相互変換しない決定は、C言語の「0以外はすべて真」のような規約をこのSQLサブセットが持たないためです。
`CAST(1 AS BOOLEAN)`のような式は、実行時エラーとしてはっきり拒否します。
この一覧に無いすべての組み合わせも同様にエラーにする実装を、`src/eval.rs`に`eval_cast`として次のとおり加えます。

```rust
fn eval_cast(value: Value, target: DataType) -> DbResult<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match (value, target) {
        (Value::BigInt(n), DataType::BigInt) => Ok(Value::BigInt(n)),
        (Value::Text(s), DataType::Text) => Ok(Value::Text(s)),
        (Value::Boolean(b), DataType::Boolean) => Ok(Value::Boolean(b)),
        (Value::BigInt(n), DataType::Text) => Ok(Value::Text(n.to_string())),
        (Value::Boolean(b), DataType::Text) => {
            Ok(Value::Text(if b { "true" } else { "false" }.to_string()))
        }
        (Value::Text(s), DataType::BigInt) => s
            .parse::<i64>()
            .map(Value::BigInt)
            .map_err(|_| DbError::Eval(format!("TEXTからBIGINTへのCASTに失敗しました: {s:?}"))),
        (Value::Text(s), DataType::Boolean) => match s.to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Boolean(true)),
            "false" => Ok(Value::Boolean(false)),
            _ => Err(DbError::Eval(format!(
                "TEXTからBOOLEANへのCASTに失敗しました: {s:?}"
            ))),
        },
        (value, target) => {
            // `Value::Null`は直前の分岐で処理済みなので、`data_type()`は必ず
            // `Some`を返す。`target`も含め、Rustの`Debug`表現(`BigInt`)ではなく
            // `Display`によるSQLの型名(`BIGINT`)で表示する。
            let data_type = value
                .data_type()
                .expect("Nullは既に処理済みなのでdata_type()は必ずSomeを返す");
            Err(DbError::Eval(format!(
                "{data_type}から{target}へのCASTは対応していません"
            )))
        }
    }
}
```

`NULL`は、どの`target`に対しても常に`NULL`のまま通します。
`Value::Null`はどの`DataType`にも属さない値なので、型を変換するという操作自体が意味を持たず、失敗のしようがないというのが理由です。

比較演算のところで「暗黙変換を採らない」と述べましたが、`CAST`はこの方針の裏返しでもあります。
`1 + '2'`のような式で`'2'`を自動的に`BigInt`へ変換してしまうと、変換に失敗したときのエラーが算術演算のエラーなのか型変換のエラーなのか、利用者から見て区別がつかなくなります。
このSQLサブセットでは、型を変えたいなら`CAST`をSQL文の中に明示的に書く、という一貫した規約にしています。

## Scalar Functionのレジストリ

`abs(-5)`や`length('hello')`のような関数呼び出しは、名前から実装への対応表を引く形で評価するため、`src/eval.rs`に次の型と構造体を定義します。

```rust
type ScalarFn = Box<dyn Fn(&[Value]) -> DbResult<Value> + Send + Sync>;

pub struct FunctionRegistry {
    functions: HashMap<String, ScalarFn>,
}

impl FunctionRegistry {
    pub fn register(
        &mut self,
        name: &str,
        f: impl Fn(&[Value]) -> DbResult<Value> + Send + Sync + 'static,
    ) {
        self.functions.insert(name.to_ascii_lowercase(), Box::new(f));
    }

    pub fn call(&self, name: &str, args: &[Value]) -> DbResult<Value> {
        match self.functions.get(&name.to_ascii_lowercase()) {
            Some(f) => f(args),
            None => Err(DbError::Eval(format!("未知の関数です: {name}"))),
        }
    }
}
```

`register`が受け取る`f`の型は、`&[Value]`を受け取り`DbResult<Value>`を返すクロージャなら何でも構いません。
これにより`FunctionRegistry`自体は、`abs`や`length`という具体的な関数名を一切知らずに済みます。
組み込み関数は、`src/eval.rs`のこの空のレジストリに対する`register`呼び出しとして定義されます。

```rust
pub fn with_builtins() -> Self {
    let mut registry = Self::new();
    registry.register("abs", builtin_abs);
    registry.register("length", builtin_length);
    registry
}
```

`src/eval.rs`に定義する`builtin_abs`は引数を1個だけ受け取り、`BIGINT`に対してのみ`checked_abs`で絶対値を計算します。

```rust
fn builtin_abs(args: &[Value]) -> DbResult<Value> {
    match expect_one_arg("abs", args)? {
        Value::Null => Ok(Value::Null),
        Value::BigInt(n) => n
            .checked_abs()
            .map(Value::BigInt)
            .ok_or_else(|| DbError::Eval(format!("整数オーバーフロー: abs({n})"))),
        other => {
            // `Value::Null`は直前の分岐で処理済みなので、`data_type()`は必ず
            // `Some`を返す。
            let data_type = other
                .data_type()
                .expect("Nullは既に処理済みなのでdata_type()は必ずSomeを返す");
            Err(DbError::Eval(format!(
                "absはBIGINTを引数に取ります: {data_type}が渡されました"
            )))
        }
    }
}
```

`checked_abs`を使っているのは、算術演算と同じ理由です。
`i64::MIN.abs()`は表現できる範囲を超えるため、ここでもオーバーフローをエラーとして検出します。
`builtin_length`は`TEXT`の文字数(`chars().count()`)を`BIGINT`として返します。
バイト数ではなく文字数を数えているのは、`TEXT`が可変長の文字列を表す型である以上、UTF-8のバイト表現という実装の都合を利用者に漏らすべきではないと判断したためです。
ただし`chars()`が数えるのはRustの`char`、つまりUnicodeのスカラー値の個数であり、画面上で1文字に見える単位(書記素クラスタ)とは一致しません。
`é`という同じ見た目の文字も、単一のコードポイントとして書かれていれば`chars().count()`は1を返しますが、`e`と結合用アクセント記号の2つに分解して書かれていれば2を返します。
この章の`length`は、この差を吸収する書記素クラスタ単位の実装までは持たない、という割り切りのもとにあります。

`FunctionRegistry`を利用者が直接使う場面もこの章の設計に含めています。
`register`が公開メソッドなので、`abs`や`length`以外の関数を後から追加登録できます。
第9〜10章でカタログや`CREATE FUNCTION`のような構文を扱うことはこのサブセットの範囲外ですが、Rustのコードとして関数を足す拡張点は、この`FunctionRegistry`にすでに用意されています。

## 列参照はまだ評価できない

`Expr::ColumnRef`だけは、この章でも評価できないため、`src/eval.rs`の`eval_expr`には次の分岐だけを置きます。

```rust
Expr::ColumnRef { name, .. } => Err(DbError::NotImplemented(format!(
    "列参照'{name}'の評価(行の値を名前から引く環境)は第10章で対応します"
))),
```

`id`という列参照を評価するには、「今処理している行で`id`列に入っている値は何か」を答えられる環境が要ります。
そのような環境は、テーブルの中身を持つインメモリ表が揃って初めて意味を持ちます。
この章の`eval_expr`は`Expr`と`FunctionRegistry`だけを受け取る関数として設計しており、行の環境を表す引数はまだありません。
第10章でインメモリ表とDMLを実装するときに、この関数のシグネチャへ環境を渡す変更が必要になります。

## `Database::execute`を差し替える

`eval::eval_expr`が揃ったので、`src/database.rs`にあった第7章までの`eval_expr`(リテラルと整数の加算しか対応していなかったもの)は丸ごと削除し、`execute_select`から`eval`モジュールを呼ぶように変えます。

```rust
for item in &select.items {
    let value = eval::eval_expr(&item.expr, &self.functions)?;
    let data_type = value.data_type().unwrap_or(DataType::Text);
    let nullable = value.is_null();
    let name = sql[item.span.start..item.span.end].to_string();
    columns.push(Column::new(name, data_type, nullable));
    values.push(value);
}
```

`self.functions`は`Database`が持つ`FunctionRegistry`で、`src/database.rs`の`Database::memory()`が組み込み関数を登録済みの状態で作ります。

```rust
pub fn memory() -> Self {
    Database {
        functions: FunctionRegistry::with_builtins(),
    }
}
```

`data_type`の行に、前章までは無かった`unwrap_or(DataType::Text)`が増えています。
前章までの`eval_expr`はリテラルと加算しか評価しなかったため、返る`Value`が`Null`になることはありませんでした。
この章では`SELECT NULL;`のように、結果の値そのものが`Null`になる式を実行できます。
`Value::Null`はどの`DataType`にも属さないので、結果列の表示用の型を決めようがありません。
PostgreSQLはこの状況に`unknown`という専用の型を割り当てますが、このSQLサブセットではそこまでの型を新設せず、`TEXT`をプレースホルダーとして選ぶという簡便な決定にしました。
値そのものは`Value::Null`のままなので、この選択が表示結果や以降の計算に影響することはありません。

## テストで確認する

`src/eval.rs`の`mod tests`には、三値論理の真理値表を網羅するテスト、ゼロ除算とオーバーフローのテスト、`CAST`の対応表を1行ずつ確認するテスト、Scalar Functionの呼び出しと引数検査のテストを追加します。
真理値表のテストは、9通りの組み合わせをすべて1つの関数にまとめて書いています。

```rust
#[test]
fn and_truth_table() {
    assert_eq!(eval_sql("true AND true").unwrap(), Value::Boolean(true));
    assert_eq!(eval_sql("true AND false").unwrap(), Value::Boolean(false));
    assert_eq!(eval_sql("true AND NULL").unwrap(), Value::Null);
    assert_eq!(eval_sql("false AND true").unwrap(), Value::Boolean(false));
    assert_eq!(eval_sql("false AND false").unwrap(), Value::Boolean(false));
    assert_eq!(eval_sql("false AND NULL").unwrap(), Value::Boolean(false));
    assert_eq!(eval_sql("NULL AND true").unwrap(), Value::Null);
    assert_eq!(eval_sql("NULL AND false").unwrap(), Value::Boolean(false));
    assert_eq!(eval_sql("NULL AND NULL").unwrap(), Value::Null);
}
```

`false AND NULL`と`NULL AND false`の両方を書いているのは、`FALSE`がどちらの位置にあっても結果を決定づけるという規則が、実装の対称性だけでなくテストの対称性としても保たれているかを確かめるためです。
`database`側には、`SELECT 1 = 1;`、`SELECT NULL AND FALSE;`、`SELECT CAST(42 AS TEXT);`のように、`Database::execute`が最後まで実行できることを確認するテストを、`src/database.rs`の`mod tests`に加えています。

```rust
#[test]
fn executes_three_valued_logic() {
    let mut db = Database::memory();
    let result = db.execute("SELECT NULL AND FALSE;").unwrap();
    assert_eq!(result.rows()[0].values(), &[Value::Boolean(false)]);
}
```

Golden Testにも、比較、三値論理、`CAST`、関数呼び出し、ゼロ除算エラーの例を追加しています。
`cargo test`を実行すると、単体テストとGolden Testを合わせてすべて緑になります。

```console
$ cargo test
running 103 tests
test eval::tests::and_truth_table ... ok
test eval::tests::or_truth_table ... ok
test eval::tests::not_truth_table ... ok
test eval::tests::null_equals_null_is_null_not_true ... ok
test eval::tests::division_by_zero_is_an_error ... ok
test eval::tests::arithmetic_overflow_is_an_error ... ok
test database::tests::executes_three_valued_logic ... ok
test database::tests::executes_cast ... ok
...
test result: ok. 103 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 1 test
test golden_tests_pass ... ok
```

## 壊して確認する

`eval_arith`のオーバーフロー検出を外し、`checked_add`の代わりに素の`+`を使うとどうなるか、試しに崩してみます。

```rust
BinaryOperator::Add => Some(l + r), // 本来はl.checked_add(r)
```

`debug`ビルドでは、この変更はコンパイルこそ通りますが、`arithmetic_overflow_is_an_error`を実行するとテストがパニックで異常終了します。
`i64::MAX + 1`が`debug_assert`の対象になり、`DbError::Eval`という通常のエラー値を返す代わりにプロセスごと落ちてしまうためです。
`release`ビルド(`cargo test --release`)ではさらに悪いことが起こります。
パニックの代わりに`i64::MIN`という値が静かに返り、テストの`assert!(matches!(..., Err(DbError::Eval(_))))`はパニックではなく「オーバーフローしたのに正常な整数が返ってきた」という形で失敗します。
`checked_add`を使う設計は、この2つの挙動(デバッグ時のパニック、リリース時の静かな折り返し)のどちらにも依存しない、ビルド設定から独立したエラー処理を選んだ結果です。

`tri_and`の最初の腕を消して、`_ => Tri::Unknown`だけの実装に変えても崩せます。

```rust
fn tri_and(l: Tri, r: Tri) -> Tri {
    match (l, r) {
        (Tri::True, Tri::True) => Tri::True,
        _ => Tri::Unknown, // FALSEの優先を消した
    }
}
```

この変更をすると`and_truth_table`が赤くなり、`false AND NULL`が`Value::Boolean(false)`ではなく`Value::Null`を返すようになります。
`FALSE`が`UNKNOWN`より優先されるという非対称な規則は、コードのどこにも「当然そうなるはず」という保証がありません。
9通りの組み合わせをすべて書き下したテストが無ければ、`false AND NULL`という具体的な式を書くまでこの崩れには気づけません。

## 演習問題

### 必須課題

1. 剰余演算子`%`を追加してください。`eval_arith`と同じ設計(`NULL`の伝播、ゼロ除算のエラー、`checked_rem`によるオーバーフロー検出)に従い、`Lexer`、`Parser`、`eval`のどこに手を入れる必要があるかを確認してから実装してください。
2. `FunctionRegistry`に`upper`(`TEXT`を大文字にする)を追加してください。`builtin_abs`や`builtin_length`と同じ形で、引数の個数と型の検査を含めたテストも書いてください。

### 発展課題

1. この章の`eval_binary`は、`AND`/`OR`の両辺を必ず両方評価してから`tri_and`/`tri_or`に渡しています。多くのSQL実装は`FALSE AND <式>`や`TRUE OR <式>`のように左辺だけで結果が決まる場合、右辺の評価を省略する短絡評価を行います。この章の`eval_expr`が副作用を持たない以上、短絡評価をしてもしなくても結果は変わりませんが、右辺の評価がエラーを返す式(`FALSE AND (1 / 0)`など)ではどうなるかを確認したうえで、`eval_binary`をどう変更すれば短絡評価にできるか考えてください。
2. この章では`1 + '2'`のような型不一致をすべてエラーにし、暗黙の型変換を行わない設計を選びました。仮にTEXTから数値への暗黙変換を許す設計に変えるとしたら、`eval_arith`のどこにその変換を挿し込むべきか、変換に失敗したときのエラーメッセージを`CAST`失敗のエラーとどう区別するかを含めて設計してください。
