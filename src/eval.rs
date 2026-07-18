//! `Expr`を`Value`へ変換する式評価器。
//!
//! 対応するのは、算術演算・比較演算・SQLの三値論理・`IS [NOT] NULL`・`CAST`・
//! Scalar Function呼び出し・列参照(`Expr::ColumnRef`)である。列参照は、
//! `row`引数で渡された行環境(`Row`、第10章で導入)から値を引く。`INSERT`の
//! `VALUES`のように行を伴わない文脈では`row`に`None`を渡し、列参照が現れれば
//! `DbError::Eval`にする。

use std::collections::HashMap;

use crate::ast::{BinaryOperator, Expr, UnaryOperator};
use crate::error::{DbError, DbResult};
use crate::types::{DataType, Row, Value};

/// 式を評価して`Value`を返す。
///
/// `functions`はScalar Function呼び出し(`Expr::FunctionCall`)を解決するために使う。
/// `row`は列参照(`Expr::ColumnRef`)を解決するための行環境で、行を伴わない文脈
/// (`INSERT`の`VALUES`など)では`None`を渡す。
pub fn eval_expr(expr: &Expr, functions: &FunctionRegistry, row: Option<&Row>) -> DbResult<Value> {
    match expr {
        Expr::IntLiteral { value, .. } => Ok(Value::BigInt(*value)),
        Expr::StringLiteral { value, .. } => Ok(Value::Text(value.clone())),
        Expr::BoolLiteral { value, .. } => Ok(Value::Boolean(*value)),
        Expr::NullLiteral { .. } => Ok(Value::Null),
        Expr::ColumnRef { name, .. } => match row {
            Some(row) => row
                .get(name)
                .cloned()
                .ok_or_else(|| DbError::Eval(format!("列'{name}'が見つかりません"))),
            None => Err(DbError::Eval(format!(
                "列参照'{name}'は行を伴わない文脈では使えません"
            ))),
        },
        Expr::Paren { expr, .. } => eval_expr(expr, functions, row),
        Expr::UnaryOp { op, expr, .. } => eval_unary(*op, eval_expr(expr, functions, row)?),
        Expr::BinaryOp { op, lhs, rhs, .. } => eval_binary(*op, lhs, rhs, functions, row),
        Expr::IsNull { expr, negated, .. } => {
            let is_null = eval_expr(expr, functions, row)?.is_null();
            Ok(Value::Boolean(if *negated { !is_null } else { is_null }))
        }
        Expr::Cast {
            expr, type_name, ..
        } => {
            let value = eval_expr(expr, functions, row)?;
            let target = resolve_data_type(&type_name.name)?;
            eval_cast(value, target)
        }
        Expr::FunctionCall { name, args, .. } => {
            let values = args
                .iter()
                .map(|arg| eval_expr(arg, functions, row))
                .collect::<DbResult<Vec<_>>>()?;
            functions.call(name, &values)
        }
    }
}

fn eval_unary(op: UnaryOperator, value: Value) -> DbResult<Value> {
    match op {
        UnaryOperator::Negate => match value {
            Value::Null => Ok(Value::Null),
            Value::BigInt(n) => n
                .checked_neg()
                .map(Value::BigInt)
                .ok_or_else(|| DbError::Eval(format!("整数オーバーフロー: -({n})"))),
            other => {
                // `Value::Null`は直前の分岐で処理済みなので、`data_type()`は必ず
                // `Some`を返す。`Option`を`{:?}`でそのまま表示すると
                // `Some(BigInt)`のようにRustの内部表現が漏れるため、`unwrap`して
                // SQLの型名だけを見せる。
                let data_type = other
                    .data_type()
                    .expect("Nullは既に処理済みなのでdata_type()は必ずSomeを返す");
                Err(DbError::Eval(format!(
                    "単項-はBIGINTに対してのみ使えます: {data_type}が渡されました"
                )))
            }
        },
        UnaryOperator::Not => Ok(tri_to_value(!value_to_tri(&value)?)),
    }
}

fn eval_binary(
    op: BinaryOperator,
    lhs: &Expr,
    rhs: &Expr,
    functions: &FunctionRegistry,
    row: Option<&Row>,
) -> DbResult<Value> {
    match op {
        BinaryOperator::And => {
            let l = value_to_tri(&eval_expr(lhs, functions, row)?)?;
            let r = value_to_tri(&eval_expr(rhs, functions, row)?)?;
            Ok(tri_to_value(tri_and(l, r)))
        }
        BinaryOperator::Or => {
            let l = value_to_tri(&eval_expr(lhs, functions, row)?)?;
            let r = value_to_tri(&eval_expr(rhs, functions, row)?)?;
            Ok(tri_to_value(tri_or(l, r)))
        }
        BinaryOperator::Add
        | BinaryOperator::Subtract
        | BinaryOperator::Multiply
        | BinaryOperator::Divide => eval_arith(
            op,
            eval_expr(lhs, functions, row)?,
            eval_expr(rhs, functions, row)?,
        ),
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq => eval_compare(
            op,
            eval_expr(lhs, functions, row)?,
            eval_expr(rhs, functions, row)?,
        ),
    }
}

/// 算術演算(`+ - * /`)。`BIGINT`同士にのみ対応する。
///
/// 片方でも`NULL`なら結果は`NULL`(算術演算はNULLを伝播させる)。
/// オーバーフローは`i64`の範囲を静かに折り返す(wrapping)のではなく、
/// `checked_*`系のメソッドでエラーとして検出する。ゼロ除算も同様にエラーにする。
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

/// 比較演算(`= <> < <= > >=`)。`BIGINT`同士、`TEXT`同士、`BOOLEAN`同士にのみ対応する。
///
/// 片方でも`NULL`なら結果は`NULL`(比較演算もNULLを伝播させる。`NULL = NULL`が
/// `TRUE`にならないのはこのためである)。異なる型どうしの比較は暗黙変換せずエラーにする。
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

/// SQLの三値論理における真偽値。`Value::Boolean`/`Value::Null`との対応は
/// `value_to_tri`/`tri_to_value`が持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Unknown,
}

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

fn tri_to_value(tri: Tri) -> Value {
    match tri {
        Tri::True => Value::Boolean(true),
        Tri::False => Value::Boolean(false),
        Tri::Unknown => Value::Null,
    }
}

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

/// `CAST`の型名(テキスト)を`DataType`へ解決する。
///
/// 実際の解決は`DataType::from_sql_name`に委ねる。第9章の`CREATE TABLE`も
/// 同じ関数で列の型名を解決しており、「型名の一覧」がこのクレートに2箇所
/// 存在する事態を避けている。
fn resolve_data_type(type_name: &str) -> DbResult<DataType> {
    DataType::from_sql_name(type_name)
        .ok_or_else(|| DbError::Eval(format!("未知の型名です: {type_name}")))
}

/// `CAST(value AS target)`を評価する。
///
/// 対応する明示的変換は次の表のとおり(`->`の左が`value`の型、右が`target`)。
///
/// | from      | to        | 変換内容                                  |
/// |-----------|-----------|--------------------------------------------|
/// | `NULL`    | 任意      | `NULL`のまま(型を持たないため常に成功)   |
/// | `T`       | `T`       | 恒等変換(常に成功)                        |
/// | `BIGINT`  | `TEXT`    | 10進数の文字列表現                        |
/// | `TEXT`    | `BIGINT`  | `i64`として構文解析。失敗時はエラー       |
/// | `BOOLEAN` | `TEXT`    | `"true"` / `"false"`                      |
/// | `TEXT`    | `BOOLEAN` | 大小文字を無視して`"true"`/`"false"`を解釈。他はエラー |
/// | `BIGINT`  | `BOOLEAN` | 非対応(エラー)                            |
/// | `BOOLEAN` | `BIGINT`  | 非対応(エラー)                            |
///
/// `BIGINT`と`BOOLEAN`を相互変換しないのは、C言語のように整数を真偽値として
/// 解釈する規約をこのSQLサブセットが持たないためである。暗黙の型変換は行わず、
/// この表に無い組み合わせはすべて`DbError::Eval`にする。
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

/// Scalar Functionの実装本体。引数の`Value`列を受け取り、戻り値の`Value`を返す。
type ScalarFn = Box<dyn Fn(&[Value]) -> DbResult<Value> + Send + Sync>;

/// レジストリに登録された1個のScalar Function。実装本体に加えて、戻り値の
/// `DataType`を静的に持つ。`project`(第10章の`executor`モジュール)が、
/// 実際に1行評価する前に`SELECT`の出力列の型を決めるのに使う。
struct FunctionEntry {
    func: ScalarFn,
    return_type: DataType,
}

/// Scalar Functionの名前とその実装を対応づけるレジストリ。
///
/// 名前解決は大文字小文字を区別しない(`ABS`と`abs`は同じ関数を指す)。
pub struct FunctionRegistry {
    functions: HashMap<String, FunctionEntry>,
}

impl FunctionRegistry {
    /// 関数が1つも登録されていない空のレジストリを作る。
    pub fn new() -> Self {
        FunctionRegistry {
            functions: HashMap::new(),
        }
    }

    /// 組み込み関数(`abs`、`length`)だけを登録したレジストリを作る。
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register("abs", DataType::BigInt, builtin_abs);
        registry.register("length", DataType::BigInt, builtin_length);
        registry
    }

    /// 関数を1つ登録する。同名の関数がすでにあれば上書きする。
    ///
    /// `return_type`は、この関数がNULL以外の入力に対して返す`Value`の型。
    /// 静的な型検査(`executor::infer_type`)がこの値を使う。
    pub fn register(
        &mut self,
        name: &str,
        return_type: DataType,
        f: impl Fn(&[Value]) -> DbResult<Value> + Send + Sync + 'static,
    ) {
        self.functions.insert(
            name.to_ascii_lowercase(),
            FunctionEntry {
                func: Box::new(f),
                return_type,
            },
        );
    }

    /// 名前と引数から関数を呼び出す。登録されていない名前は`DbError::Eval`にする。
    pub fn call(&self, name: &str, args: &[Value]) -> DbResult<Value> {
        match self.functions.get(&name.to_ascii_lowercase()) {
            Some(entry) => (entry.func)(args),
            None => Err(DbError::Eval(format!("未知の関数です: {name}"))),
        }
    }

    /// 名前から、この関数の戻り値の`DataType`を引く。登録されていない名前は
    /// `DbError::Eval`にする。
    pub fn return_type(&self, name: &str) -> DbResult<DataType> {
        match self.functions.get(&name.to_ascii_lowercase()) {
            Some(entry) => Ok(entry.return_type),
            None => Err(DbError::Eval(format!("未知の関数です: {name}"))),
        }
    }
}

impl Default for FunctionRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// 引数を1個取る関数を作るための共通の引数検査。
fn expect_one_arg<'a>(name: &str, args: &'a [Value]) -> DbResult<&'a Value> {
    match args {
        [arg] => Ok(arg),
        _ => Err(DbError::Eval(format!(
            "{name}は引数を1個取ります(渡されたのは{}個です)",
            args.len()
        ))),
    }
}

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

fn builtin_length(args: &[Value]) -> DbResult<Value> {
    match expect_one_arg("length", args)? {
        Value::Null => Ok(Value::Null),
        Value::Text(s) => Ok(Value::BigInt(s.chars().count() as i64)),
        other => {
            // `Value::Null`は直前の分岐で処理済みなので、`data_type()`は必ず
            // `Some`を返す。
            let data_type = other
                .data_type()
                .expect("Nullは既に処理済みなのでdata_type()は必ずSomeを返す");
            Err(DbError::Eval(format!(
                "lengthはTEXTを引数に取ります: {data_type}が渡されました"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Span;
    use crate::parser::parse_statement;

    fn eval_sql(sql: &str) -> DbResult<Value> {
        let statement = parse_statement(&format!("SELECT {sql}")).unwrap();
        let expr = match statement {
            crate::ast::Statement::Select(select) => match select.items.into_iter().next().unwrap() {
                crate::ast::SelectItem::Expr { expr, .. } => expr,
                crate::ast::SelectItem::Wildcard { .. } => panic!("式を期待したがWildcardが返った"),
            },
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        };
        eval_expr(&expr, &FunctionRegistry::with_builtins(), None)
    }

    fn dummy_span() -> Span {
        Span::new(0, 0)
    }

    fn lit_int(v: i64) -> Expr {
        Expr::IntLiteral {
            value: v,
            span: dummy_span(),
        }
    }

    fn lit_null() -> Expr {
        Expr::NullLiteral { span: dummy_span() }
    }

    // ---- 算術演算 ----

    #[test]
    fn adds_two_bigints() {
        assert_eq!(eval_sql("1 + 2").unwrap(), Value::BigInt(3));
    }

    #[test]
    fn evaluates_all_arithmetic_operators() {
        assert_eq!(eval_sql("7 - 2").unwrap(), Value::BigInt(5));
        assert_eq!(eval_sql("3 * 4").unwrap(), Value::BigInt(12));
        assert_eq!(eval_sql("7 / 2").unwrap(), Value::BigInt(3));
    }

    #[test]
    fn division_by_zero_is_an_error() {
        assert!(matches!(eval_sql("1 / 0"), Err(DbError::Eval(_))));
    }

    #[test]
    fn evaluates_i64_min_and_i64_max_literals() {
        assert_eq!(eval_sql("-9223372036854775808").unwrap(), Value::BigInt(i64::MIN));
        assert_eq!(eval_sql("9223372036854775807").unwrap(), Value::BigInt(i64::MAX));
    }

    #[test]
    fn arithmetic_overflow_is_an_error() {
        let sql = format!("{} + 1", i64::MAX);
        assert!(matches!(eval_sql(&sql), Err(DbError::Eval(_))));
    }

    #[test]
    fn unary_negate_overflow_is_an_error() {
        // SQLの`-9223372036854775808`は、Parserが符号込みで`Expr::IntLiteral`に
        // 変換するため`UnaryOp`を経由しない(第7章のParserのテスト参照)。
        // `eval_unary`自身の`checked_neg`によるオーバーフロー検出は、`Expr`を
        // 直接組み立てて`i64::MIN`を単項`-`に渡すことで確かめる。
        // (`i64::MIN`を単項`-`するとオーバーフローするのは、`i64`の範囲が
        // `-9223372036854775808..=9223372036854775807`と非対称なため。)
        let expr = Expr::UnaryOp {
            op: UnaryOperator::Negate,
            expr: Box::new(Expr::IntLiteral {
                value: i64::MIN,
                span: dummy_span(),
            }),
            span: dummy_span(),
        };
        let result = eval_expr(&expr, &FunctionRegistry::with_builtins(), None);
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    #[test]
    fn arithmetic_propagates_null() {
        assert_eq!(eval_sql("1 + NULL").unwrap(), Value::Null);
        assert_eq!(eval_sql("NULL / 1").unwrap(), Value::Null);
    }

    #[test]
    fn arithmetic_on_non_bigint_is_an_error() {
        assert!(matches!(eval_sql("1 + 'a'"), Err(DbError::Eval(_))));
    }

    // ---- 比較演算 ----

    #[test]
    fn compares_bigints() {
        assert_eq!(eval_sql("1 = 1").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("1 <> 2").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("1 < 2").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("2 <= 2").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("3 > 2").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("2 >= 2").unwrap(), Value::Boolean(true));
    }

    #[test]
    fn compares_texts() {
        assert_eq!(eval_sql("'a' = 'a'").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("'a' < 'b'").unwrap(), Value::Boolean(true));
    }

    #[test]
    fn compares_booleans() {
        assert_eq!(eval_sql("true = true").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("false <> true").unwrap(), Value::Boolean(true));
    }

    #[test]
    fn null_equals_null_is_null_not_true() {
        // SQLの`NULL = NULL`はUnknown(NULL)であり、TRUEではない。
        assert_eq!(eval_sql("NULL = NULL").unwrap(), Value::Null);
    }

    #[test]
    fn comparison_with_null_propagates() {
        assert_eq!(eval_sql("1 = NULL").unwrap(), Value::Null);
    }

    #[test]
    fn comparison_between_different_types_is_an_error() {
        assert!(matches!(eval_sql("1 = 'a'"), Err(DbError::Eval(_))));
        assert!(matches!(eval_sql("true = 1"), Err(DbError::Eval(_))));
    }

    // ---- 三値論理: AND ----

    #[test]
    fn and_truth_table() {
        assert_eq!(eval_sql("true AND true").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("true AND false").unwrap(), Value::Boolean(false));
        assert_eq!(eval_sql("true AND NULL").unwrap(), Value::Null);
        assert_eq!(eval_sql("false AND true").unwrap(), Value::Boolean(false));
        assert_eq!(eval_sql("false AND false").unwrap(), Value::Boolean(false));
        // FALSEはNULLに勝つ: 片方が確定してFALSEなら、もう片方が未知でも結果はFALSE。
        assert_eq!(eval_sql("false AND NULL").unwrap(), Value::Boolean(false));
        assert_eq!(eval_sql("NULL AND true").unwrap(), Value::Null);
        assert_eq!(eval_sql("NULL AND false").unwrap(), Value::Boolean(false));
        assert_eq!(eval_sql("NULL AND NULL").unwrap(), Value::Null);
    }

    // ---- 三値論理: OR ----

    #[test]
    fn or_truth_table() {
        assert_eq!(eval_sql("true OR true").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("true OR false").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("true OR NULL").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("false OR true").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("false OR false").unwrap(), Value::Boolean(false));
        assert_eq!(eval_sql("false OR NULL").unwrap(), Value::Null);
        // TRUEはNULLに勝つ。
        assert_eq!(eval_sql("NULL OR true").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("NULL OR false").unwrap(), Value::Null);
        assert_eq!(eval_sql("NULL OR NULL").unwrap(), Value::Null);
    }

    // ---- 三値論理: NOT ----

    #[test]
    fn not_truth_table() {
        assert_eq!(eval_sql("NOT true").unwrap(), Value::Boolean(false));
        assert_eq!(eval_sql("NOT false").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("NOT NULL").unwrap(), Value::Null);
    }

    #[test]
    fn logical_op_on_non_boolean_is_an_error() {
        assert!(matches!(eval_sql("1 AND true"), Err(DbError::Eval(_))));
    }

    // ---- IS [NOT] NULL ----

    #[test]
    fn is_null_never_returns_null_itself() {
        assert_eq!(eval_sql("NULL IS NULL").unwrap(), Value::Boolean(true));
        assert_eq!(eval_sql("1 IS NULL").unwrap(), Value::Boolean(false));
        assert_eq!(eval_sql("NULL IS NOT NULL").unwrap(), Value::Boolean(false));
        assert_eq!(eval_sql("1 IS NOT NULL").unwrap(), Value::Boolean(true));
    }

    // ---- CAST ----

    #[test]
    fn cast_bigint_to_text() {
        assert_eq!(
            eval_sql("CAST(42 AS TEXT)").unwrap(),
            Value::Text("42".to_string())
        );
    }

    #[test]
    fn cast_text_to_bigint() {
        assert_eq!(eval_sql("CAST('42' AS BIGINT)").unwrap(), Value::BigInt(42));
    }

    #[test]
    fn cast_boolean_to_text() {
        assert_eq!(
            eval_sql("CAST(true AS TEXT)").unwrap(),
            Value::Text("true".to_string())
        );
    }

    #[test]
    fn cast_text_to_boolean_is_case_insensitive() {
        assert_eq!(
            eval_sql("CAST('TRUE' AS BOOLEAN)").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn cast_identity_always_succeeds() {
        assert_eq!(eval_sql("CAST(1 AS BIGINT)").unwrap(), Value::BigInt(1));
    }

    #[test]
    fn cast_null_stays_null_for_any_target_type() {
        assert_eq!(eval_sql("CAST(NULL AS BIGINT)").unwrap(), Value::Null);
    }

    #[test]
    fn cast_invalid_text_to_bigint_is_an_error() {
        assert!(matches!(
            eval_sql("CAST('abc' AS BIGINT)"),
            Err(DbError::Eval(_))
        ));
    }

    #[test]
    fn cast_between_bigint_and_boolean_is_an_error() {
        assert!(matches!(eval_sql("CAST(1 AS BOOLEAN)"), Err(DbError::Eval(_))));
        assert!(matches!(eval_sql("CAST(true AS BIGINT)"), Err(DbError::Eval(_))));
    }

    #[test]
    fn cast_unknown_type_name_is_an_error() {
        assert!(matches!(
            eval_sql("CAST(1 AS FLOAT)"),
            Err(DbError::Eval(_))
        ));
    }

    // ---- Scalar Function ----

    #[test]
    fn calls_abs() {
        assert_eq!(eval_sql("abs(-5)").unwrap(), Value::BigInt(5));
        assert_eq!(eval_sql("abs(5)").unwrap(), Value::BigInt(5));
    }

    #[test]
    fn calls_length() {
        assert_eq!(eval_sql("length('hello')").unwrap(), Value::BigInt(5));
    }

    #[test]
    fn function_name_lookup_is_case_insensitive() {
        assert_eq!(eval_sql("ABS(-5)").unwrap(), Value::BigInt(5));
    }

    #[test]
    fn unknown_function_is_an_error() {
        assert!(matches!(eval_sql("no_such_fn(1)"), Err(DbError::Eval(_))));
    }

    #[test]
    fn wrong_arity_is_an_error() {
        assert!(matches!(eval_sql("abs(1, 2)"), Err(DbError::Eval(_))));
        assert!(matches!(eval_sql("abs()"), Err(DbError::Eval(_))));
    }

    #[test]
    fn function_arg_type_mismatch_is_an_error() {
        assert!(matches!(eval_sql("abs('a')"), Err(DbError::Eval(_))));
    }

    // ---- 列参照 ----

    #[test]
    fn column_ref_without_a_row_is_an_error() {
        let expr = Expr::ColumnRef {
            name: "id".to_string(),
            span: dummy_span(),
        };
        let result = eval_expr(&expr, &FunctionRegistry::with_builtins(), None);
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    #[test]
    fn column_ref_resolves_from_a_row() {
        use crate::types::{Column, DataType, Row, Schema, Tuple};

        let schema = Schema::new(vec![Column::new("id", DataType::BigInt, false)]);
        let tuple = Tuple::new(&schema, vec![Value::BigInt(42)]).unwrap();
        let row = Row::new(&schema, &tuple);

        let expr = Expr::ColumnRef {
            name: "id".to_string(),
            span: dummy_span(),
        };
        let result = eval_expr(&expr, &FunctionRegistry::with_builtins(), Some(&row)).unwrap();
        assert_eq!(result, Value::BigInt(42));
    }

    #[test]
    fn column_ref_to_unknown_column_is_an_error() {
        use crate::types::{Column, DataType, Row, Schema, Tuple};

        let schema = Schema::new(vec![Column::new("id", DataType::BigInt, false)]);
        let tuple = Tuple::new(&schema, vec![Value::BigInt(42)]).unwrap();
        let row = Row::new(&schema, &tuple);

        let expr = Expr::ColumnRef {
            name: "does_not_exist".to_string(),
            span: dummy_span(),
        };
        let result = eval_expr(&expr, &FunctionRegistry::with_builtins(), Some(&row));
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    // ---- FunctionRegistry ----

    #[test]
    fn custom_function_can_be_registered() {
        let mut registry = FunctionRegistry::new();
        registry.register("answer", DataType::BigInt, |_args| Ok(Value::BigInt(42)));
        assert_eq!(registry.call("answer", &[]).unwrap(), Value::BigInt(42));
    }

    #[test]
    fn return_type_looks_up_a_registered_functions_declared_return_type() {
        let registry = FunctionRegistry::with_builtins();
        assert_eq!(registry.return_type("abs").unwrap(), DataType::BigInt);
        assert_eq!(registry.return_type("LENGTH").unwrap(), DataType::BigInt);
    }

    #[test]
    fn return_type_of_unknown_function_is_an_error() {
        let registry = FunctionRegistry::with_builtins();
        assert!(matches!(
            registry.return_type("no_such_fn"),
            Err(DbError::Eval(_))
        ));
    }

    #[test]
    fn eval_expr_can_be_called_directly_on_an_ast_node() {
        // `eval_sql`はSQL文字列から`Expr`を組み立てる経由だが、
        // `eval_expr`自体はASTノードを直接受け取る公開APIでもある。
        let expr = Expr::BinaryOp {
            op: BinaryOperator::Add,
            lhs: Box::new(lit_int(1)),
            rhs: Box::new(lit_null()),
            span: dummy_span(),
        };
        let result = eval_expr(&expr, &FunctionRegistry::with_builtins(), None).unwrap();
        assert_eq!(result, Value::Null);
    }
}
