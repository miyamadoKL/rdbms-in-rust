# 第4章 関係モデルとSQLサブセット

前章までで、`DbError`とNewtypeによる識別子、そしてGolden Testの仕組みが揃いました。
`cargo test`は通ります。
それでも、この`minidb`はまだ1件のデータも保持できません。

試しに、ユーザーのレコードを1件だけ表すコードを書こうとしてみます。
名前は`String`、年齢は`i64`、これくらいなら`Vec`とタプルで足りそうです。

```rust
let row: Vec<(String, i64)> = vec![("Alice".to_string(), 30)];
```

このコードはコンパイルが通ります。
ですが、テーブルに`nickname`のようなNULLを許す列を1つ増やそうとした瞬間に破綻します。
`(String, i64)`というタプルの型に3つめの要素を足すと、テーブルのすべての行の型が変わり、呼び出し側もすべて書き換えになります。
テーブルごとに列数や型の組み合わせが異なる以上、行の型をコンパイル時のタプルとして固定するわけにはいきません。

もう一つの選択肢は、型を諦めて`Vec<Vec<String>>`のように何もかも文字列にしてしまうことです。
これなら列数がいくつでも同じ型で表せますが、代わりに`"30"`が数値なのか文字列なのかを、値を見るたびに呼び出し側が判断しなければなりません。
かといって`Vec<Vec<i64>>`にすれば、今度は名前を数値の列に入れられなくなります。

つまり、行を表す型は「列数と型はテーブルごとに違う」という事実と、「列の型は一度決めたら守られてほしい」という要求を、両方満たす必要があります。
この2つの要求に答えるのが、関係モデルという理論です。

## 関係モデルの用語をRustの型に対応させる

関係モデルは、テーブルを構成する要素に名前を与えます。
E.F. Coddが1970年に提案したモデルで、SQLはこのモデルを実装の対象にした問い合わせ言語です[^codd]。

- **Relation**：テーブルそのもの。列の集合と行の集合からなる
- **Attribute**：列。名前と型を持つ
- **Tuple**：1件の行。Relationが持つAttributeの並びに従って値が並んだもの
- **Schema**：Relationが持つAttributeの並び。列名、型、NULL許可の定義そのもの
- **Key**：Tupleを一意に特定できるAttributeの集合

[^codd]: E. F. Codd, "A Relational Model of Data for Large Shared Data Banks", Communications of the ACM, 1970.

この対応表を眺めるだけでは、まだ手が動きません。
先ほどのタプル型が壊れた原因に戻ると、対応の要所が見えてきます。
壊れたのは、列の並びという情報を、値の型そのものに埋め込んでいたからです。
関係モデルの流儀は逆で、列の並び(Schema)を値の並び(Tuple)から切り離し、別の型として持たせます。
Tupleは値の配列のまま汎用の型にしておき、その配列を「どう読むか」をSchemaが管理します。

RustでいえばSchemaは`Vec<Column>`、Tupleは`Vec<Value>`という形になります。
列数が違うテーブルも、この2つの型でどちらも表現できます。
`Value`は列の型を問わず格納できる列挙型にし、実際の型検査はSchemaとの突き合わせで行います。

## 守るべき不変条件

実装に入る前に、この章で保つべき条件を3つ決めます。

1. **Tupleの列数はSchemaの列数と一致する**：多すぎても少なすぎてもいけない
2. **Tupleの各値の型は、対応する列の型と一致する**：`BigInt`型の列に`Text`値は入らない
3. **NULLは、対応する列が`nullable`のときにだけ許される**：`Value::Null`はどの列の型とも型上は矛盾しないが、`nullable`が`false`の列には論理的に入ってはならない

3つめの条件は、NULLの扱いを考えるうえで要になります。
NULLは「型を持たない値」であり、`BigInt`のNULLと`Text`のNULLを区別する必要はありません。
一方で、列ごとにNULLを許すかどうかは別の関心事です。
`id`列にはNULLを入れたくないが、`nickname`列にはNULLを許したい、という状況は普通に起こります。
NULLが「無型の値」であることと、「NULLを許すかどうか」が列ごとの設定であることを、型の上でも別々の場所に置く必要があります。

## 最小実装

`DataType`から始めます。
このSQLサブセットが対応する型は、`BOOLEAN`、`BIGINT`、`TEXT`の3種類だけです[^double]。

```rust
/// 列が取りうるデータ型。
///
/// このサブセットでは `BOOLEAN` / `BIGINT` / `TEXT` の3種類だけを扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// 真偽値。
    Boolean,
    /// 64bit符号付き整数。
    BigInt,
    /// 可変長文字列。
    Text,
}
```

[^double]: `DOUBLE`は式評価と型変換の基礎が固まった段階で追加します(第1章の対応SQLサブセットを参照)。

次に`Value`です。
`DataType`の各バリアントに対応する値を1つずつ持たせ、さらに`Null`を独立したバリアントとして加えます。

```rust
/// 1つのセルが持つ実際の値。
///
/// `Null` はどの`DataType`にも属さない特別な値であり、列の型とは独立に存在する。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// NULL値。型を持たない。
    Null,
    /// 真偽値。
    Boolean(bool),
    /// 64bit符号付き整数。
    BigInt(i64),
    /// 可変長文字列。
    Text(String),
}
```

`Value`が`DataType`と同じ数だけバリアントを持つのは偶然ではありません。
`Value::Null`を除く各バリアントは、対応する`DataType`のRust表現をそのまま包んでいます。
この対応を、値からたどれるメソッドとして持たせます。

```rust
impl Value {
    /// この値の`DataType`を返す。
    ///
    /// `Value::Null`はどの`DataType`にも対応しないため`None`を返す。
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Boolean(_) => Some(DataType::Boolean),
            Value::BigInt(_) => Some(DataType::BigInt),
            Value::Text(_) => Some(DataType::Text),
        }
    }

    /// この値が`Null`かどうか。
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
```

`data_type()`が`Option<DataType>`を返すのは、`Value::Null`が本当にどの`DataType`にも属さないことを、シグネチャの時点で表明するためです。
`Value::Null`に何らかの`DataType`を割り当てて`DataType`(`Option`なし)を返すようにも書けますが、そうすると「NULLは無型である」という不変条件がコードのどこにも残らなくなります。

列との適合判定も、この`data_type()`の上に組み立てます。

```rust
    /// この値が、指定した列の型・nullable制約に適合するかどうかを判定する。
    ///
    /// `Value::Null`は`nullable`が`true`の列にのみ適合する。
    /// `Null`以外の値は、`data_type()`が列の型と一致する場合にのみ適合する。
    pub fn conforms_to(&self, column: &Column) -> bool {
        match self.data_type() {
            None => column.nullable,
            Some(dt) => dt == column.data_type,
        }
    }
}
```

`conforms_to`の分岐は、先ほど立てた不変条件2と3をそのままコードに落としたものです。
`Value::Null`は`column.nullable`だけを見て適合を決め、それ以外の値は`data_type()`と`column.data_type`の一致だけを見ます。
この2つの分岐が、NULLの扱いと型の扱いを別の関心事として保つ境目です。

`Column`は名前、型、`nullable`をまとめた構造体です。

```rust
/// テーブルの1列を表す。名前、型、NULLを許すかどうかを持つ。
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// 列名。
    pub name: String,
    /// 列の型。
    pub data_type: DataType,
    /// NULLを許すかどうか。`false`なら`Value::Null`を格納できない。
    pub nullable: bool,
}
```

`Schema`は`Column`の並びを保持し、列名から索引を引けるようにします。

```rust
/// テーブルの列構成。列の並び順を保持し、列名から索引を引ける。
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    columns: Vec<Column>,
}

impl Schema {
    /// 列の並びから`Schema`を作る。
    pub fn new(columns: Vec<Column>) -> Self {
        Schema { columns }
    }
```

このほか、列の並びをそのまま返す`columns()`、列数を返す`len()`、空かどうかを返す`is_empty()`、列名から列定義そのものを引く`column()`も持たせています。
列名から列の索引を引く`index_of`が、以降の実装で中心になります。

```rust
    /// 列名から列の索引を引く。見つからなければ`None`を返す。
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
```

`index_of`が`Option<usize>`を返すのは、SQLでは`SELECT does_not_exist FROM users`のように存在しない列名を指定できてしまうためです。
列名解決の失敗は珍しい例外ではなく、SQLを受け取る以上いつでも起こりうる通常の分岐なので、`panic!`ではなく`Option`で呼び出し側に処理を委ねます。

不変条件1と2と3をまとめて検査するのが`validate_tuple`です。

```rust
    /// 与えられた値の並びが、このSchemaが定める列数・型・nullable制約に
    /// すべて適合するかどうかを検査する。
    ///
    /// 列数が一致しない場合、または値が対応する列の型・nullable制約に
    /// 適合しない場合は`DbError::SchemaMismatch`を返す。
    pub fn validate_tuple(&self, values: &[Value]) -> DbResult<()> {
        if values.len() != self.columns.len() {
            return Err(DbError::SchemaMismatch(format!(
                "列数が一致しません: schemaは{}列、valuesは{}列",
                self.columns.len(),
                values.len()
            )));
        }

        for (value, column) in values.iter().zip(self.columns.iter()) {
            if !value.conforms_to(column) {
                return Err(DbError::SchemaMismatch(format!(
                    "列'{}'(型: {:?}, nullable: {})に値{:?}を格納できません",
                    column.name, column.data_type, column.nullable, value
                )));
            }
        }

        Ok(())
    }
}
```

列数の一致を先に検査してから`zip`で1列ずつ`conforms_to`を呼んでいるのは、列数が食い違ったまま`zip`にかけると、短い側に合わせて残りの列が黙って無視されるためです。
列数の不一致自体が呼び出し側の間違いなので、`zip`の前に弾いておきます。

`DbError`には`SchemaMismatch`バリアントを1つ追加しました。

```rust
    /// 値の並びがSchemaの列数・型・nullable制約に適合しないエラー。
    #[error("スキーマ不一致: {0}")]
    SchemaMismatch(String),
```

最後に`Tuple`です。
`Tuple`は`Schema`から独立した型にはせず、生成時に必ず`Schema`との適合を検査するコンストラクタだけを公開します。

```rust
/// `Schema`に従う値の並び。
#[derive(Debug, Clone, PartialEq)]
pub struct Tuple {
    values: Vec<Value>,
}

impl Tuple {
    /// `Schema`に対する妥当性を検査したうえで`Tuple`を作る。
    ///
    /// `values`が`schema`の列数・型・nullable制約に適合しない場合は
    /// `DbError::SchemaMismatch`を返す。
    pub fn new(schema: &Schema, values: Vec<Value>) -> DbResult<Self> {
        schema.validate_tuple(&values)?;
        Ok(Tuple { values })
    }
```

`Tuple`のフィールド`values`はプライベートです。
`Tuple { values }`のような直接構築を外部から封じることで、`Tuple::new`を通らない限り`Schema`に適合しない`Tuple`が作れない状態にしています。
列を名前で取り出す`get_by_name`も、内部では`Schema::index_of`をそのまま使います。

```rust
    /// `Schema`と列名を指定して値を取り出す。
    pub fn get_by_name(&self, schema: &Schema, name: &str) -> Option<&Value> {
        let index = schema.index_of(name)?;
        self.get(index)
    }
}
```

## テストで確認する

不変条件ごとにテストを書きます。
まず型対応と、NULLがnullable列にのみ適合することの確認です。

```rust
    #[test]
    fn null_conforms_only_to_nullable_column() {
        let nullable = Column::new("c", DataType::Text, true);
        let not_nullable = Column::new("c", DataType::Text, false);
        assert!(Value::Null.conforms_to(&nullable));
        assert!(!Value::Null.conforms_to(&not_nullable));
    }
```

続いて、Schema違反のTupleが実際に拒否されることを確認します。
列数の不一致、型の不一致、not-null列へのNULL挿入の3パターンです。

```rust
    #[test]
    fn tuple_new_rejects_wrong_column_count() {
        let schema = users_schema();
        let result = Tuple::new(&schema, vec![Value::BigInt(1)]);
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
    }
```

最後に列名解決です。
存在する列名は正しい索引を返し、存在しない列名は`None`を返すことを確認します。

```rust
    #[test]
    fn schema_index_of_resolves_column_name() {
        let schema = users_schema();
        assert_eq!(schema.index_of("id"), Some(0));
        assert_eq!(schema.index_of("name"), Some(1));
        assert_eq!(schema.index_of("nickname"), Some(2));
        assert_eq!(schema.index_of("does_not_exist"), None);
    }
```

`cargo test`を実行すると、`types`モジュールの9件のテストがすべて通ります。

```console
$ cargo test
running 9 tests
test types::tests::null_conforms_only_to_nullable_column ... ok
test types::tests::tuple_get_by_name_returns_none_for_unknown_column ... ok
test types::tests::schema_index_of_resolves_column_name ... ok
test types::tests::tuple_new_accepts_conforming_values ... ok
test types::tests::tuple_new_rejects_null_in_not_nullable_column ... ok
test types::tests::tuple_new_rejects_type_mismatch ... ok
test types::tests::tuple_new_rejects_wrong_column_count ... ok
test types::tests::value_conforms_to_matching_type_only ... ok
test types::tests::value_data_type_matches_variant ... ok

test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

## 壊して確認する

`validate_tuple`から列数チェックを外すとどうなるか、試しに崩してみます。

```rust
pub fn validate_tuple(&self, values: &[Value]) -> DbResult<()> {
    // 列数チェックを削除したとする
    for (value, column) in values.iter().zip(self.columns.iter()) {
        if !value.conforms_to(column) {
            return Err(DbError::SchemaMismatch(format!(
                "列'{}'に値{:?}を格納できません",
                column.name, value
            )));
        }
    }
    Ok(())
}
```

このコードで`tuple_new_rejects_wrong_column_count`を実行すると失敗します。
`Tuple::new(&schema, vec![Value::BigInt(1)])`は列が1つしかありませんが、`Schema`側の3列と`zip`すると、`zip`は短い側に合わせて`name`列と`nickname`列を静かに無視します。
残った1組(`id`列と`Value::BigInt(1)`)だけが型検査を通過し、本来ならエラーになるべき呼び出しが成功として返ってしまいます。

同様に、`conforms_to`の分岐から`None => column.nullable`を`None => true`に変えると、`tuple_new_rejects_null_in_not_nullable_column`が失敗します。
`nullable`が`false`の列にも`Value::Null`が無条件で通るようになり、不変条件3が保証されなくなるためです。
どちらの変更も、対応するテストを削らない限りコンパイルは通ってもテストの赤で気づけます。

## 演習問題

### 必須課題

1. `Schema`に、列名の重複を検出する`has_duplicate_column_names(&self) -> bool`を追加してください。同名の列が2つ以上あるSchemaは、`index_of`で意図しない列を引いてしまう危険があります。
2. `Tuple`に、値を1つだけ更新した新しい`Tuple`を返す`with_value(&self, schema: &Schema, index: usize, value: Value) -> DbResult<Tuple>`を追加してください。更新後の値がSchemaに適合しない場合はエラーを返すようにしてください。

### 発展課題

1. `Key`(Primary Key)を表す型を設計してください。`Schema`の列のうち、どの列(または列の組)がKeyになるかを保持し、2つの`Tuple`がKeyの値で一致するかどうかを判定できるメソッドを持たせてください。この型は第9章のカタログとDDLで使うことになります。
2. `DataType::Text`の列に最大長を持たせたくなったとします。`DataType`をどう変更すればよいか、Rustのコードとして設計してみてください。`Value::Text`側に最大長のチェックを持たせる案と、`Column`側に持たせる案を比較し、それぞれの得失を書き出してください。
