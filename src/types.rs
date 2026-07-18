//! リレーショナルモデルの型: `DataType`、`Value`、`Column`、`Schema`、`Tuple`。
//!
//! この章ではSQLを実行しない。1行のタプルをRustの型として安全に表現できることだけを
//! この章のゴールにする。

use crate::error::{DbError, DbResult};

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

impl DataType {
    /// SQLの型名(大文字小文字を無視)から`DataType`を解決する。
    ///
    /// 対応する型が無ければ`None`を返す。`CAST`の型名解決(第8章の`eval`モジュール)と
    /// `CREATE TABLE`の列定義の型名解決(第9章の`Database::execute`)が、この関数を
    /// 共通の実装として使う。
    pub fn from_sql_name(name: &str) -> Option<DataType> {
        match name.to_ascii_uppercase().as_str() {
            "BIGINT" => Some(DataType::BigInt),
            "TEXT" => Some(DataType::Text),
            "BOOLEAN" => Some(DataType::Boolean),
            _ => None,
        }
    }
}

impl std::fmt::Display for DataType {
    /// `from_sql_name`と対になる、SQLの型名としての表示。
    ///
    /// エラーメッセージなど利用者向けの出力は、Rustの`Debug`表現(`BigInt`)ではなく
    /// SQLの型名(`BIGINT`)を見せるため、この`Display`実装を経由する。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            DataType::Boolean => "BOOLEAN",
            DataType::BigInt => "BIGINT",
            DataType::Text => "TEXT",
        };
        write!(f, "{name}")
    }
}

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

impl Column {
    /// 新しい列定義を作る。
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Column {
            name: name.into(),
            data_type,
            nullable,
        }
    }
}

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

    /// 列の並びを返す。
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// 列数を返す。
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// 列が1つも無いかどうか。
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// 列名から列の索引を引く。見つからなければ`None`を返す。
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// 列名から列定義を引く。見つからなければ`None`を返す。
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

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

    /// 値の並びを返す。
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// 索引を指定して値を取り出す。
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.values.get(index)
    }

    /// `Schema`と列名を指定して値を取り出す。
    pub fn get_by_name(&self, schema: &Schema, name: &str) -> Option<&Value> {
        let index = schema.index_of(name)?;
        self.get(index)
    }
}

/// `Expr::ColumnRef`を解決するための、列名から値を引く行環境。
///
/// `Schema`と`Tuple`を1組にまとめただけの薄いラッパーで、`eval_expr`が
/// `WHERE`句や`SET`の右辺のような「今処理している1行」を必要とする式を
/// 評価するときに使う。式の評価自体に行が要らない場面(`INSERT`の`VALUES`
/// など)では`eval_expr`に`None`を渡し、`Row`を作る必要がない。
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    schema: &'a Schema,
    tuple: &'a Tuple,
}

impl<'a> Row<'a> {
    /// `schema`に従う`tuple`を1行分の環境として包む。
    pub fn new(schema: &'a Schema, tuple: &'a Tuple) -> Self {
        Row { schema, tuple }
    }

    /// 列名から値を引く。この行の`Schema`に無い列名なら`None`を返す。
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.tuple.get_by_name(self.schema, name)
    }

    /// 列インデックスから値を引く。
    ///
    /// `Binder`(第17章)が解決した`BoundExpr::ColumnRef`は列名ではなく索引を
    /// 持つため、実行時の評価(`eval::eval_bound_expr`)はこちらを使う。索引は
    /// 束縛の時点で`Schema`と突き合わせ済みなので、名前を毎回文字列比較で
    /// 探し直す`get`より安く、かつ列名の変化(将来のリネーム等)に影響されない。
    pub fn get_index(&self, index: usize) -> Option<&Value> {
        self.tuple.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::BigInt, false),
            Column::new("name", DataType::Text, false),
            Column::new("nickname", DataType::Text, true),
        ])
    }

    #[test]
    fn from_sql_name_resolves_known_types_case_insensitively() {
        assert_eq!(DataType::from_sql_name("BIGINT"), Some(DataType::BigInt));
        assert_eq!(DataType::from_sql_name("bigint"), Some(DataType::BigInt));
        assert_eq!(DataType::from_sql_name("Text"), Some(DataType::Text));
        assert_eq!(DataType::from_sql_name("boolean"), Some(DataType::Boolean));
    }

    #[test]
    fn from_sql_name_rejects_unknown_type() {
        assert_eq!(DataType::from_sql_name("FLOAT"), None);
    }

    #[test]
    fn value_data_type_matches_variant() {
        assert_eq!(Value::Boolean(true).data_type(), Some(DataType::Boolean));
        assert_eq!(Value::BigInt(1).data_type(), Some(DataType::BigInt));
        assert_eq!(
            Value::Text("a".to_string()).data_type(),
            Some(DataType::Text)
        );
        assert_eq!(Value::Null.data_type(), None);
    }

    #[test]
    fn null_conforms_only_to_nullable_column() {
        let nullable = Column::new("c", DataType::Text, true);
        let not_nullable = Column::new("c", DataType::Text, false);
        assert!(Value::Null.conforms_to(&nullable));
        assert!(!Value::Null.conforms_to(&not_nullable));
    }

    #[test]
    fn value_conforms_to_matching_type_only() {
        let column = Column::new("c", DataType::BigInt, false);
        assert!(Value::BigInt(1).conforms_to(&column));
        assert!(!Value::Text("x".to_string()).conforms_to(&column));
        assert!(!Value::Boolean(true).conforms_to(&column));
    }

    #[test]
    fn schema_index_of_resolves_column_name() {
        let schema = users_schema();
        assert_eq!(schema.index_of("id"), Some(0));
        assert_eq!(schema.index_of("name"), Some(1));
        assert_eq!(schema.index_of("nickname"), Some(2));
        assert_eq!(schema.index_of("does_not_exist"), None);
    }

    #[test]
    fn tuple_new_accepts_conforming_values() {
        let schema = users_schema();
        let tuple = Tuple::new(
            &schema,
            vec![
                Value::BigInt(1),
                Value::Text("Alice".to_string()),
                Value::Null,
            ],
        )
        .expect("schemaに適合するtupleは作成できる");
        assert_eq!(
            tuple.get_by_name(&schema, "name"),
            Some(&Value::Text("Alice".to_string()))
        );
    }

    #[test]
    fn tuple_new_rejects_wrong_column_count() {
        let schema = users_schema();
        let result = Tuple::new(&schema, vec![Value::BigInt(1)]);
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
    }

    #[test]
    fn tuple_new_rejects_type_mismatch() {
        let schema = users_schema();
        let result = Tuple::new(
            &schema,
            vec![
                Value::Text("not a bigint".to_string()),
                Value::Text("Alice".to_string()),
                Value::Null,
            ],
        );
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
    }

    #[test]
    fn tuple_new_rejects_null_in_not_nullable_column() {
        let schema = users_schema();
        let result = Tuple::new(
            &schema,
            vec![Value::Null, Value::Text("Alice".to_string()), Value::Null],
        );
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
    }

    #[test]
    fn tuple_get_by_name_returns_none_for_unknown_column() {
        let schema = users_schema();
        let tuple = Tuple::new(
            &schema,
            vec![
                Value::BigInt(1),
                Value::Text("Alice".to_string()),
                Value::Null,
            ],
        )
        .unwrap();
        assert_eq!(tuple.get_by_name(&schema, "does_not_exist"), None);
    }
}
