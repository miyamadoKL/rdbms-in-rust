//! この章の実行演算子: Values / Sequential Scan / Filter / Projection /
//! Insert / Update / Delete。
//!
//! `Executor::next()`が1行ずつ引っ張り出すVolcanoモデルは第19章で導入する。
//! この章の演算子は、表全体を`Vec<Tuple>`としてまとめて受け取り、まとめて
//! 返す素朴な関数にとどめる。`Database::execute`は、これらの関数を文の種類
//! ごとに正しい順序で呼び出す配線役に徹する。
//!
//! # 行の供給源が2つある
//!
//! 第16章から、行の供給源は`MemTable`(第10章、プロセスのメモリ上)と
//! `Storage`(第15章、ディスク上のファイル)の2つになった。`filter`・
//! `project`は`Vec<Tuple>`だけを受け取る関数のままなので、供給源が
//! どちらであっても変更なく使い回せる。変更が要るのは、供給源に直接触れる
//! 演算子(`seq_scan`・`insert`・`update`・`delete`)だけである。それぞれに
//! `storage_`を接頭辞に持つ対の関数(`storage_seq_scan`・`storage_insert`・
//! `storage_update`・`storage_delete`)を追加し、既存の(接頭辞の無い)関数は
//! `MemTable`向けのまま変えていない。
//!
//! 1つの関数を`enum`や`trait`で両対応させる案も検討したが、この章では見送った。
//! `MemTable`は行を`Vec<Tuple>`の添字で直接指すのに対し、`Storage`は
//! `RecordId`(第13章)で指す。`update`・`delete`が「どの行を書き換えるか」を
//! 特定する手段そのものが両者で異なるため、共通化すると分岐だらけの抽象が
//! 必要になる。将来Volcano Executor(第19章)が演算子をtraitとして抽象化する
//! ときには、この共通化はそちらの設計に沿った形で自然に生まれる。先取りして
//! 今traitを導入する理由はない。

use crate::ast::{Assignment, BinaryOperator, Expr, Ident, SelectItem, UnaryOperator};
use crate::error::{DbError, DbResult};
use crate::eval::{FunctionRegistry, eval_expr};
use crate::ids::{RecordId, TableId};
use crate::storage::Storage;
use crate::storage_mem::MemTable;
use crate::tuple_codec::{decode_tuple, encode_tuple};
use crate::types::{Column, DataType, Row, Schema, Tuple, Value};

/// `WHERE`・`SET`の`predicate`が評価された結果を、SQLの三値論理に従って
/// 「その行にマッチしたかどうか」の`bool`へ変換する。
///
/// `TRUE`だけがマッチで、`FALSE`と`NULL`(`UNKNOWN`)はどちらもマッチしない
/// (`filter`・`update`・`delete`が共通して従うべき規則)。`BIGINT`や`TEXT`の
/// ような`BOOLEAN`ではない値が渡された場合は、それを黙って「マッチしない」
/// 側に丸めてしまうと`WHERE 1`のような書き誤りを見逃すことになるため、
/// `DbError::Eval`にする。
fn predicate_matches(value: Value) -> DbResult<bool> {
    match value {
        Value::Boolean(true) => Ok(true),
        Value::Boolean(false) | Value::Null => Ok(false),
        other => {
            // `Value::Boolean`と`Value::Null`は直前の分岐で処理済みなので、ここに
            // 来る`other`は必ず`data_type()`が`Some`を返す値(`BigInt`/`Text`)である。
            // `Option`を`{:?}`でそのまま表示すると`Some(BigInt)`のようにRustの内部
            // 表現が利用者に漏れてしまうため、`unwrap`してSQLの型名だけを見せる。
            //
            // なお、この分岐へ実際に到達するのは`check_predicate_type`による事前の
            // 静的検査をすり抜けたときだけである(通常は起こらない。`infer_type`の
            // 判定ミスなどに備えた保険)。行の有無に関わらず`WHERE 1`のような式を
            // 拒否する主経路は`check_predicate_type`であり、こちらは行を実際に評価
            // した後の最終防衛線に過ぎない。
            let data_type = other
                .data_type()
                .expect("BooleanとNullは既に処理済みなのでdata_type()は必ずSomeを返す");
            Err(DbError::Eval(format!(
                "WHERE句はBOOLEANまたはNULLを返す式である必要があります: {data_type}が渡されました"
            )))
        }
    }
}

/// `predicate`が`BOOLEAN`を返す式(または型が定まらない`NULL`)であることを、
/// 行を1件も評価せずに`infer_type`で静的に検査する。
///
/// `predicate_matches`は行ごとに評価した`Value`を見て初めて型の誤りに気づくため、
/// テーブルが空で行ループが1度も回らない場合(`SELECT/UPDATE/DELETE ... WHERE 1`を
/// 空テーブルに対して実行するなど)は`predicate_matches`が一度も呼ばれず、
/// `WHERE 1`のような書き誤りを見逃してしまう。`filter`・`update`・`delete`は、
/// 行ループに入る前にこの関数を呼ぶことで、行の有無に関わらず同じ不変条件
/// (「`WHERE`句は`BOOLEAN`か`NULL`を返す式でなければならない」)を保証する。
///
/// `infer_type`は`predicate`の式木全体を再帰的に検査するため、`WHERE 1 AND 2`や
/// `WHERE NOT 1`、`WHERE abs('x') = 1`のように、トップレベルの演算子だけを見ても
/// 気づけない被演算子の型違反も、ここで一度に検出できる。`WHERE NULL`は
/// `infer_type`が`None`(型が定まらない)を返すが、これは`NULL`という有効な
/// `UNKNOWN`述語(0行にマッチする)であって型エラーではないため、`Some(Boolean)`
/// と同じく許可する。
///
/// `pub(crate)`なのは、`FROM`を伴わない`SELECT`(`database`モジュールの
/// `execute_select_without_from`)も、意味を持たないまま構文としてだけ受理する
/// `WHERE`句の型を同じ規則で検査するため。
pub(crate) fn check_predicate_type(
    predicate: &Expr,
    schema: &Schema,
    functions: &FunctionRegistry,
) -> DbResult<()> {
    match infer_type(predicate, schema, functions)? {
        Some(DataType::Boolean) | None => Ok(()),
        Some(other) => Err(DbError::Eval(format!(
            "WHERE句はBOOLEANを返す式である必要があります: 式の型は{other}です"
        ))),
    }
}

/// Sequential Scan演算子。テーブルの全行を、格納順のまま複製して返す。
///
/// 索引を持たないこの章では、`WHERE`があってもなくても、まずテーブル全体を
/// 読む以外に行へたどり着く手段が無い。
pub fn seq_scan(table: &MemTable) -> Vec<Tuple> {
    table.rows().to_vec()
}

/// Sequential Scan演算子の`Storage`版。`table_id`のテーブルが使う全ページを
/// 先頭から順に読み、生きている(削除されていない)全タプルを`decode_tuple`
/// (第13章)で復元して返す。
///
/// `Storage::scan`が返すのは`(RecordId, バイト列)`の組だが、ここでは
/// `RecordId`を捨ててバイト列だけを`Tuple`へ復元する。`RecordId`は
/// `storage_update`・`storage_delete`が書き換え・削除の対象を特定するのに
/// 使うが、読み取るだけの`Sequential Scan`にはそもそも要らない。
pub fn storage_seq_scan(storage: &Storage, table_id: TableId, schema: &Schema) -> DbResult<Vec<Tuple>> {
    storage
        .scan(table_id)?
        .map(|entry| entry.and_then(|(_, bytes)| decode_tuple(schema, &bytes)))
        .collect()
}

/// Filter演算子。`predicate`を各行に対して評価し、`TRUE`になった行だけを残す。
///
/// SQLの`WHERE`は三値論理で評価するため、`FALSE`はもちろん`UNKNOWN`(`NULL`)に
/// なった行も、`TRUE`ではないので落ちる。`NULL`の行を「一致しなかった」側に
/// 含めるこの規則、および`BOOLEAN`でも`NULL`でもない値(`WHERE 1`など)を
/// エラーにする規則は、`predicate_matches`が`update`・`delete`とも共通して
/// 適用する。`rows`が空でも`WHERE 1`のような書き誤りを見逃さないよう、行
/// ループへ入る前に`check_predicate_type`で`predicate`の型を静的に検査する。
pub fn filter(
    schema: &Schema,
    functions: &FunctionRegistry,
    rows: Vec<Tuple>,
    predicate: &Expr,
) -> DbResult<Vec<Tuple>> {
    check_predicate_type(predicate, schema, functions)?;
    let mut kept = Vec::with_capacity(rows.len());
    for tuple in rows {
        let row = Row::new(schema, &tuple);
        let value = eval_expr(predicate, functions, Some(&row))?;
        if predicate_matches(value)? {
            kept.push(tuple);
        }
    }
    Ok(kept)
}

/// Projection演算子。`SELECT`の対象式リストを各行に適用し、出力用の
/// `Schema`と行の並びを組み立てる。
///
/// `*`(`SelectItem::Wildcard`)は`table_schema`の全列への列参照へ展開してから
/// 扱うため、以降の処理は「列参照または式のリスト」という1種類の形だけを
/// 見ればよい。
///
/// 出力列の型・nullableは、単純な列参照であれば`table_schema`の定義をそのまま
/// 使うため常に正確である。計算結果(`id + 1`のような式)の型は、行を実際に
/// 評価せず、式のASTと`table_schema`だけから`infer_type`が静的に決める
/// (1行目がNULLだったり、テーブルが空だったりしても型がぶれない)。
/// `infer_type`が型を決められない式(`NULL`単体など、`None`を返す式)は、
/// `DataType::Text`をプレースホルダーとして使う。この場合の`nullable`は、
/// 行ごとに`NULL`になったりならなかったりしうるため、常に`true`とする。
/// 名前解決を伴う本格的な型検査は第17章のBinderで置き換える。
pub fn project(
    table_schema: &Schema,
    functions: &FunctionRegistry,
    rows: &[Tuple],
    items: &[SelectItem],
    sql: &str,
) -> DbResult<(Schema, Vec<Tuple>)> {
    let resolved = resolve_items(table_schema, items, sql);

    let mut out_columns = Vec::with_capacity(resolved.len());
    for (expr, name) in &resolved {
        if let Expr::ColumnRef { name: col_name, .. } = expr
            && let Some(column) = table_schema.column(col_name)
        {
            let mut column = column.clone();
            column.name = name.clone();
            out_columns.push(column);
            continue;
        }

        // `infer_type`が型を決められない(`None`を返す)式は`TEXT`で代用する。
        // このプレースホルダーの理由は`infer_type`のドキュメントコメント参照。
        let data_type = infer_type(expr, table_schema, functions)?.unwrap_or(DataType::Text);
        out_columns.push(Column::new(name.clone(), data_type, true));
    }
    let out_schema = Schema::new(out_columns);

    let mut out_rows = Vec::with_capacity(rows.len());
    for tuple in rows {
        let row = Row::new(table_schema, tuple);
        let mut values = Vec::with_capacity(resolved.len());
        for (expr, _) in &resolved {
            values.push(eval_expr(expr, functions, Some(&row))?);
        }
        out_rows.push(Tuple::new(&out_schema, values)?);
    }
    Ok((out_schema, out_rows))
}

/// エラーメッセージ用に`Option<DataType>`を表示する。`None`(型が定まらない、
/// `NULL`リテラルなど)は`NULL`と表示する。
fn describe_type(data_type: Option<DataType>) -> String {
    match data_type {
        Some(t) => t.to_string(),
        None => "NULL".to_string(),
    }
}

/// 式の出力型を、行を実際に評価せず、式のASTと`schema`だけから静的に決める。
///
/// 戻り値は`Option<DataType>`で、`None`は「型が定まらない」ことを表す。
/// `NULL`リテラル単体だけがこれに当たる(`Value::Null`がどの`DataType`にも
/// 属さない、つまり`Value::data_type`が`None`を返すのと同じ考え方)。
/// それ以外の式は必ず`Some`を返す。`NULL`が被演算子として現れても、演算子
/// 自身の出力の「型」は`NULL`かどうかに関係なく決まる(例えば`1 + NULL`は
/// 実行結果こそ常に`NULL`だが、これを`SELECT`すれば`BIGINT`型の列になり、
/// `Value::Null`は`nullable`な列であればどんな`DataType`にも適合するため
/// 問題は起きない)。`None`まで遡って伝播するのは、`NULL`リテラル自身と、
/// それを素通りさせるだけの`(...)`(括弧)だけである。
///
/// `project`が`SELECT`の出力列の型を決めるのに使うだけでなく、`check_predicate_type`
/// が`WHERE`句の型を検査するのにも使う。単に出力型を決めるだけでなく、各演算子・
/// 関数がその被演算子に課す型制約を式木全体にわたって再帰的に検査し、違反があれば
/// `eval_expr`が実際にその式を評価したときに返すのと同じ文言の`DbError::Eval`を
/// 返す。この検査は行を1件も評価せずに式のASTと`schema`だけから完結するため、
/// テーブルが空でも、行を持っていても、同じ結果になる。
///
/// - 整数・文字列・真偽値リテラルは、そのリテラルが表す型(`Some`)。
/// - `NULL`リテラル単体は型が定まらない(`None`)。
/// - 列参照は、`schema`に定義された、その列の型(`Some`)。列は必ず宣言された
///   型を持つため、`nullable`かどうかに関係なく`Some`になる。
/// - 単項`-`は、被演算子が`BigInt`または`None`でなければエラー。出力は
///   常に`Some(BigInt)`。
/// - 単項`NOT`は、被演算子が`Boolean`または`None`でなければエラー。出力は
///   常に`Some(Boolean)`。
/// - 算術演算(`+ - * /`)は、両辺が`BigInt`または`None`でなければエラー。
///   出力は常に`Some(BigInt)`。
/// - 比較演算(`= <> < <= > >=`)は、両辺が同じ型か、どちらかが`None`でなければ
///   エラー。出力は常に`Some(Boolean)`。
/// - 論理演算(`AND` `OR`)は、両辺が`Boolean`または`None`でなければエラー。
///   出力は常に`Some(Boolean)`。
/// - `IS [NOT] NULL`は、被演算子の型を問わない(ただし被演算子自身の式は
///   再帰的に検査する)。出力は常に`Some(Boolean)`。
/// - `CAST(expr AS type)`は、`expr`自身を再帰的に検査するだけで、`expr`と
///   `type`の組み合わせが`eval_cast`の対応表に載っているかどうかまでは
///   検査しない(この組み合わせの妥当性は値に依存しないため原理的には静的に
///   検査できるが、この章のスコープには含めない)。出力は常に`type`が指す
///   型(`Some`)。`CAST(NULL AS type)`も`type`を返す(`eval_cast`が`NULL`を
///   そのまま`NULL`として通すのと同じ理由で、`CAST`の宣言上の型は入力の
///   `NULL`らしさに影響されない)。
/// - 関数呼び出しは、`functions`に登録された引数の個数・型を検査してから、
///   登録された戻り値の型(`Some`)を返す。各引数は、宣言された型または
///   `None`でなければエラー。
/// - 括弧`(expr)`は中身の式の型・検査をそのまま引き継ぐ。
///
/// `pub(crate)`なのは、`FROM`を伴わない`SELECT`(`database`モジュールの
/// `execute_select_without_from`)も、`FROM`を伴う`SELECT`と同じ型検査を
/// 各射影式に適用するため。`eval_arith`のような実行時の評価関数は`NULL`を
/// 型検査より先に伝播させる(`checked_add`等に辿り着く前に`is_null`で
/// 早期リターンする)ため、`infer_type`による静的検査を経由しない経路では
/// `NULL + 'x'`のような型不正の式でも`NULL`として黙って成功してしまう。
/// `FROM`の有無で成否が変わらないよう、両方の経路で同じ静的検査を先に通す。
pub(crate) fn infer_type(
    expr: &Expr,
    schema: &Schema,
    functions: &FunctionRegistry,
) -> DbResult<Option<DataType>> {
    match expr {
        Expr::IntLiteral { .. } => Ok(Some(DataType::BigInt)),
        Expr::StringLiteral { .. } => Ok(Some(DataType::Text)),
        Expr::BoolLiteral { .. } => Ok(Some(DataType::Boolean)),
        Expr::NullLiteral { .. } => Ok(None),
        Expr::ColumnRef { name, .. } => schema
            .column(name)
            .map(|column| Some(column.data_type))
            .ok_or_else(|| DbError::Eval(format!("列'{name}'が見つかりません"))),
        Expr::UnaryOp { op, expr, .. } => {
            let operand = infer_type(expr, schema, functions)?;
            match op {
                UnaryOperator::Negate => {
                    if let Some(data_type) = operand
                        && data_type != DataType::BigInt
                    {
                        return Err(DbError::Eval(format!(
                            "単項-はBIGINTに対してのみ使えます: {data_type}が渡されました"
                        )));
                    }
                    Ok(Some(DataType::BigInt))
                }
                UnaryOperator::Not => {
                    if let Some(data_type) = operand
                        && data_type != DataType::Boolean
                    {
                        return Err(DbError::Eval(format!(
                            "論理演算はBOOLEANまたはNULLに対してのみ使えます: {data_type}が渡されました"
                        )));
                    }
                    Ok(Some(DataType::Boolean))
                }
            }
        }
        Expr::BinaryOp { op, lhs, rhs, .. } => match op {
            BinaryOperator::Add
            | BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide => {
                let l = infer_type(lhs, schema, functions)?;
                let r = infer_type(rhs, schema, functions)?;
                let l_ok = l.is_none() || l == Some(DataType::BigInt);
                let r_ok = r.is_none() || r == Some(DataType::BigInt);
                if !l_ok || !r_ok {
                    return Err(DbError::Eval(format!(
                        "算術演算はBIGINT同士にのみ使えます: {}と{}",
                        describe_type(l),
                        describe_type(r)
                    )));
                }
                Ok(Some(DataType::BigInt))
            }
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq => {
                let l = infer_type(lhs, schema, functions)?;
                let r = infer_type(rhs, schema, functions)?;
                let ok = match (l, r) {
                    (None, _) | (_, None) => true,
                    (Some(a), Some(b)) => a == b,
                };
                if !ok {
                    return Err(DbError::Eval(format!(
                        "比較演算は同じ型同士にのみ使えます: {}と{}",
                        describe_type(l),
                        describe_type(r)
                    )));
                }
                Ok(Some(DataType::Boolean))
            }
            BinaryOperator::And | BinaryOperator::Or => {
                // 実際の`eval_binary`(`eval`モジュール)がlhsを先に評価してから
                // rhsを評価するのに合わせ、こちらもlhsを先に検査する。
                let l = infer_type(lhs, schema, functions)?;
                if let Some(data_type) = l
                    && data_type != DataType::Boolean
                {
                    return Err(DbError::Eval(format!(
                        "論理演算はBOOLEANまたはNULLに対してのみ使えます: {data_type}が渡されました"
                    )));
                }
                let r = infer_type(rhs, schema, functions)?;
                if let Some(data_type) = r
                    && data_type != DataType::Boolean
                {
                    return Err(DbError::Eval(format!(
                        "論理演算はBOOLEANまたはNULLに対してのみ使えます: {data_type}が渡されました"
                    )));
                }
                Ok(Some(DataType::Boolean))
            }
        },
        Expr::IsNull { expr, .. } => {
            // 被演算子の型は問わないが、被演算子自身が無効な式(未知の関数呼び出し
            // など)でないことは検査する。`eval_expr`もIS NULLを評価する前に
            // 被演算子を評価してエラーを伝播させるのと同じ順序。
            infer_type(expr, schema, functions)?;
            Ok(Some(DataType::Boolean))
        }
        Expr::Cast { expr, type_name, .. } => {
            infer_type(expr, schema, functions)?;
            DataType::from_sql_name(&type_name.name)
                .map(Some)
                .ok_or_else(|| DbError::Eval(format!("未知の型名です: {}", type_name.name)))
        }
        Expr::FunctionCall { name, args, .. } => {
            let arg_types = functions.arg_types(name)?;
            let canonical_name = name.to_ascii_lowercase();
            if args.len() != arg_types.len() {
                return Err(DbError::Eval(format!(
                    "{canonical_name}は引数を{}個取ります(渡されたのは{}個です)",
                    arg_types.len(),
                    args.len()
                )));
            }
            for (arg, expected) in args.iter().zip(arg_types) {
                let actual = infer_type(arg, schema, functions)?;
                if let Some(actual_type) = actual
                    && actual_type != *expected
                {
                    return Err(DbError::Eval(format!(
                        "{canonical_name}は{expected}を引数に取ります: {actual_type}が渡されました"
                    )));
                }
            }
            functions.return_type(name).map(Some)
        }
        Expr::Paren { expr, .. } => infer_type(expr, schema, functions),
    }
}

/// `SelectItem`の並びを、`*`を展開したうえで`(評価する式, 出力列名)`の並びに変換する。
fn resolve_items(table_schema: &Schema, items: &[SelectItem], sql: &str) -> Vec<(Expr, String)> {
    let mut resolved = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard { span } => {
                for column in table_schema.columns() {
                    resolved.push((
                        Expr::ColumnRef {
                            name: column.name.clone(),
                            span: *span,
                        },
                        column.name.clone(),
                    ));
                }
            }
            SelectItem::Expr { expr, span } => {
                resolved.push((expr.clone(), sql[span.start..span.end].to_string()));
            }
        }
    }
    resolved
}

/// Values演算子とInsert演算子。`VALUES`の各行を評価し、`Tuple::new`による
/// スキーマ検査に通ったものだけを`table`へ追加する。
///
/// 検査はテーブルへ1行も書き込む前に、全行に対して済ませる。
/// 3行目の型が`NOT NULL`列に違反していた場合、1・2行目の検査がどれだけ
/// 成功していても、`?`によってその場で打ち切られ、`table`には何も追加されない。
/// 「一部だけ挿入された`INSERT`」という中途半端な状態を避けるこの順序は、
/// `CREATE TABLE`が列定義を1つずつ検査してから最後に1回だけ登録する
/// (第9章)のと同じ考え方である。
pub fn insert(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    columns: Option<&[Ident]>,
    rows: &[Vec<Expr>],
) -> DbResult<usize> {
    let planned = plan_insert_rows(schema, functions, columns, rows)?;
    let count = planned.len();
    table.rows_mut().extend(planned);
    Ok(count)
}

/// Insert演算子の`Storage`版。`plan_insert_rows`で全行を検証してから、
/// 1件ずつ`encode_tuple`(第12章)でバイト列へ変換し、`Storage::insert`
/// (第15章)へ渡す。
///
/// `plan_insert_rows`が全行を検証し終えるまでは`storage`に一切触れないため、
/// 「一部の行だけ挿入されたINSERT」を避けるという不変条件は`insert`
/// (`MemTable`版)と同じ形で保たれる。ただし、検証をすべて通過した後の
/// `storage.insert`自体が個々の呼び出しで失敗する場合(たとえば
/// `DbError::CatalogTooLarge`)は、その手前まで挿入済みの行を巻き戻さない。
/// これは`Storage::insert`自身がすでに採用している割り切り(第15章)を
/// そのまま引き継いだもので、この章のスコープでは新たに解決しない。
pub fn storage_insert(
    storage: &mut Storage,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    columns: Option<&[Ident]>,
    rows: &[Vec<Expr>],
) -> DbResult<usize> {
    let planned = plan_insert_rows(schema, functions, columns, rows)?;
    let count = planned.len();
    for tuple in planned {
        let bytes = encode_tuple(schema, &tuple);
        storage.insert(table_id, &bytes)?;
    }
    Ok(count)
}

/// `VALUES`の各行を評価し、`Tuple::new`によるスキーマ検査に通った`Tuple`の
/// 並びを返す。`insert`・`storage_insert`の両方が使う共通部分で、供給源
/// (`MemTable`か`Storage`か)には一切触れない。
///
/// 検査は`table`または`storage`へ1行も書き込む前に、全行に対して済ませる。
/// 3行目の型が`NOT NULL`列に違反していた場合、1・2行目の検査がどれだけ
/// 成功していても、`?`によってその場で打ち切られ、呼び出し元は何も
/// 書き込まない。「一部だけ挿入された`INSERT`」という中途半端な状態を避ける
/// この順序は、`CREATE TABLE`が列定義を1つずつ検査してから最後に1回だけ
/// 登録する(第9章)のと同じ考え方である。
fn plan_insert_rows(
    schema: &Schema,
    functions: &FunctionRegistry,
    columns: Option<&[Ident]>,
    rows: &[Vec<Expr>],
) -> DbResult<Vec<Tuple>> {
    let mut planned = Vec::with_capacity(rows.len());
    for row_exprs in rows {
        // `VALUES`の各要素は、テーブルの行を伴わない文脈で評価する。
        // `INSERT INTO t VALUES (id + 1)`のように挿入する側の値が既存の行を
        // 参照する構文は無いため、`row`は常に`None`でよい。
        let evaluated = row_exprs
            .iter()
            .map(|expr| eval_expr(expr, functions, None))
            .collect::<DbResult<Vec<_>>>()?;
        let full_values = expand_to_schema(schema, columns, evaluated)?;
        planned.push(Tuple::new(schema, full_values)?);
    }
    Ok(planned)
}

/// 列名指定(`INSERT INTO t (a, b) VALUES ...`)がある場合に、評価済みの値を
/// スキーマの列順へ並べ替える。指定されなかった列は`Value::Null`で埋める
/// (`NOT NULL`列であれば、後段の`Tuple::new`が検査して拒否する)。
///
/// 列名指定が無い場合は、`VALUES`の並びをそのままスキーマの列順とみなす。
fn expand_to_schema(
    schema: &Schema,
    columns: Option<&[Ident]>,
    values: Vec<Value>,
) -> DbResult<Vec<Value>> {
    let Some(columns) = columns else {
        return Ok(values);
    };

    if columns.len() != values.len() {
        return Err(DbError::Eval(format!(
            "列の個数({})とVALUESの個数({})が一致しません",
            columns.len(),
            values.len()
        )));
    }

    let mut full = vec![Value::Null; schema.len()];
    let mut assigned = vec![false; schema.len()];
    for (column, value) in columns.iter().zip(values) {
        let index = schema
            .index_of(&column.name)
            .ok_or_else(|| DbError::Eval(format!("列'{}'が見つかりません", column.name)))?;
        if assigned[index] {
            return Err(DbError::Eval(format!(
                "列'{}'がINSERTの列リストに重複しています",
                column.name
            )));
        }
        assigned[index] = true;
        full[index] = value;
    }
    Ok(full)
}

/// Update演算子。`predicate`が`TRUE`になった行それぞれに`assignments`を適用する。
///
/// `SET`の右辺は、その行の更新前の値を使って評価する。同じ`UPDATE`文の中で
/// 複数の列を書き換える場合でも、後続の代入が直前の代入結果を見ることはない
/// (`SET a = b, b = a`が値を交換する、標準的なSQLの挙動)。
///
/// `Insert`演算子と同様、書き換え後の`Tuple`をすべて`planned`に集め終えてから
/// 一括で`table`へ反映する。行の途中で型検査に失敗した場合、それより前に
/// 検査を通っていた行も含めて、`table`は一切変更されない。
///
/// `table`が空でも`WHERE 1`のような書き誤りを見逃さないよう、行ループへ入る前に
/// `check_predicate_type`で`predicate`の型を静的に検査する(`filter`と同じ理由)。
pub fn update(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    assignments: &[Assignment],
    predicate: Option<&Expr>,
) -> DbResult<usize> {
    if let Some(pred) = predicate {
        check_predicate_type(pred, schema, functions)?;
    }
    let mut planned = Vec::new();
    for (index, tuple) in table.rows().iter().enumerate() {
        let row = Row::new(schema, tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => predicate_matches(eval_expr(pred, functions, Some(&row))?)?,
        };
        if !matched {
            continue;
        }

        let mut new_values = tuple.values().to_vec();
        for assignment in assignments {
            let target = schema.index_of(&assignment.column.name).ok_or_else(|| {
                DbError::Eval(format!("列'{}'が見つかりません", assignment.column.name))
            })?;
            new_values[target] = eval_expr(&assignment.value, functions, Some(&row))?;
        }
        planned.push((index, Tuple::new(schema, new_values)?));
    }

    let count = planned.len();
    for (index, new_tuple) in planned {
        table.rows_mut()[index] = new_tuple;
    }
    Ok(count)
}

/// Update演算子の`Storage`版。`update`(`MemTable`版)が書き換え対象を`Vec`の
/// 添字で直接指すのに対し、こちらは`Storage::scan`(第15章)が返す
/// `(RecordId, バイト列)`から`RecordId`(第13章)で対象を指す。行はページを
/// またいで散らばっているため、`Vec`の添字のような単純な位置情報では
/// そもそも1件を指せない。
///
/// `update`と同じく、`predicate`に一致した行の新しい値をすべて`planned`へ
/// 集め終えてから、`storage.update`で1件ずつ書き戻す。`SET`の右辺は
/// 更新前の値(`decode_tuple`で復元した`tuple`)を使って評価するため、複数の
/// 列を書き換える`UPDATE`でも後続の代入が直前の代入結果を見ることはない
/// (`update`と同じ規則)。
///
/// `table`が空でも`WHERE 1`のような書き誤りを見逃さないよう、行ループへ
/// 入る前に`check_predicate_type`で`predicate`の型を静的に検査する(`filter`・
/// `update`と同じ理由)。
pub fn storage_update(
    storage: &mut Storage,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    assignments: &[Assignment],
    predicate: Option<&Expr>,
) -> DbResult<usize> {
    if let Some(pred) = predicate {
        check_predicate_type(pred, schema, functions)?;
    }

    let mut planned: Vec<(RecordId, Tuple)> = Vec::new();
    for entry in storage.scan(table_id)? {
        let (rid, bytes) = entry?;
        let tuple = decode_tuple(schema, &bytes)?;
        let row = Row::new(schema, &tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => predicate_matches(eval_expr(pred, functions, Some(&row))?)?,
        };
        if !matched {
            continue;
        }

        let mut new_values = tuple.values().to_vec();
        for assignment in assignments {
            let target = schema.index_of(&assignment.column.name).ok_or_else(|| {
                DbError::Eval(format!("列'{}'が見つかりません", assignment.column.name))
            })?;
            new_values[target] = eval_expr(&assignment.value, functions, Some(&row))?;
        }
        planned.push((rid, Tuple::new(schema, new_values)?));
    }

    let count = planned.len();
    for (rid, new_tuple) in planned {
        let bytes = encode_tuple(schema, &new_tuple);
        storage.update(table_id, rid, &bytes)?;
    }
    Ok(count)
}

/// Delete演算子。`predicate`が`TRUE`になった行を`table`から取り除く。
///
/// `Filter`演算子と対になる形で、生き残る行(`predicate`が`TRUE`にならなかった
/// 行)を新しい`Vec`へ集め終えてから`table`へ書き戻す。集め終える前に評価が
/// 失敗すれば、その時点で`table`はまだ元のままなので、失敗した`DELETE`が
/// 一部の行だけ消してしまうことはない。
///
/// `table`が空でも`WHERE 1`のような書き誤りを見逃さないよう、行ループへ入る前に
/// `check_predicate_type`で`predicate`の型を静的に検査する(`filter`と同じ理由)。
pub fn delete(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    predicate: Option<&Expr>,
) -> DbResult<usize> {
    if let Some(pred) = predicate {
        check_predicate_type(pred, schema, functions)?;
    }
    let mut kept = Vec::with_capacity(table.rows().len());
    let mut deleted = 0usize;
    for tuple in table.rows() {
        let row = Row::new(schema, tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => predicate_matches(eval_expr(pred, functions, Some(&row))?)?,
        };
        if matched {
            deleted += 1;
        } else {
            kept.push(tuple.clone());
        }
    }

    *table.rows_mut() = kept;
    Ok(deleted)
}

/// Delete演算子の`Storage`版。`storage_update`と同じ理由で、削除対象を
/// `Vec`の添字ではなく`Storage::scan`が返す`RecordId`で指す。
///
/// `predicate`に一致した行の`RecordId`をすべて`to_delete`へ集め終えてから、
/// `storage.delete`で1件ずつ削除する。`delete`(`MemTable`版)が生き残る行を
/// 新しい`Vec`へ集めるのとは集め方が逆(こちらは消す側を集める)だが、
/// どちらも「対象を確定させてから初めて書き換える」という順序は同じであり、
/// 集め終える前に評価が失敗すれば`storage`はまだ何も変更されていない。
///
/// `table`が空でも`WHERE 1`のような書き誤りを見逃さないよう、行ループへ
/// 入る前に`check_predicate_type`で`predicate`の型を静的に検査する(`filter`・
/// `delete`と同じ理由)。
pub fn storage_delete(
    storage: &mut Storage,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    predicate: Option<&Expr>,
) -> DbResult<usize> {
    if let Some(pred) = predicate {
        check_predicate_type(pred, schema, functions)?;
    }

    let mut to_delete: Vec<RecordId> = Vec::new();
    for entry in storage.scan(table_id)? {
        let (rid, bytes) = entry?;
        let tuple = decode_tuple(schema, &bytes)?;
        let row = Row::new(schema, &tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => predicate_matches(eval_expr(pred, functions, Some(&row))?)?,
        };
        if matched {
            to_delete.push(rid);
        }
    }

    let count = to_delete.len();
    for rid in to_delete {
        storage.delete(table_id, rid)?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Statement;
    use crate::lexer::Span;

    fn users_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::BigInt, false),
            Column::new("name", DataType::Text, true),
        ])
    }

    fn tuple(id: i64, name: Option<&str>) -> Tuple {
        let schema = users_schema();
        let name = match name {
            Some(s) => Value::Text(s.to_string()),
            None => Value::Null,
        };
        Tuple::new(&schema, vec![Value::BigInt(id), name]).unwrap()
    }

    /// テスト用に、SQLの式1個を`Expr`へ変換する。`SELECT`に埋め込んで
    /// パースするのは、`eval`モジュールのテストと同じやり方。
    fn expr(sql: &str) -> Expr {
        match crate::parser::parse_statement(&format!("SELECT {sql}")).unwrap() {
            Statement::Select(select) => match select.items.into_iter().next().unwrap() {
                SelectItem::Expr { expr, .. } => expr,
                SelectItem::Wildcard { .. } => panic!("式を期待したがWildcardが返った"),
            },
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
    }

    fn ident(name: &str) -> Ident {
        Ident {
            name: name.to_string(),
            span: Span::new(0, 0),
        }
    }

    // ---- Sequential Scan ----

    #[test]
    fn seq_scan_clones_all_rows_in_insertion_order() {
        let mut table = MemTable::new();
        table.rows_mut().push(tuple(1, Some("Alice")));
        table.rows_mut().push(tuple(2, Some("Bob")));

        let scanned = seq_scan(&table);
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned[0].values(), tuple(1, Some("Alice")).values());
        assert_eq!(scanned[1].values(), tuple(2, Some("Bob")).values());
    }

    // ---- Filter ----

    #[test]
    fn filter_keeps_only_true_rows() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![
            tuple(1, Some("Alice")),
            tuple(2, None),  // `name = 'Alice'`はUNKNOWN
            tuple(3, Some("Bob")), // `name = 'Alice'`はFALSE
        ];

        let kept = filter(&schema, &functions, rows, &expr("name = 'Alice'")).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].values()[0], Value::BigInt(1));
    }

    #[test]
    fn filter_rejects_a_non_boolean_predicate_instead_of_silently_dropping_rows() {
        // `WHERE 1`はBIGINTを返す式であり、暗黙にBOOLEANへ変換したり
        // 「一致しなかった」側へ黙って丸めたりせず、エラーにする。
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![tuple(1, Some("Alice"))];

        let result = filter(&schema, &functions, rows, &expr("1"));
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    #[test]
    fn filter_rejects_a_non_boolean_predicate_even_on_an_empty_table() {
        // 行が1件も無いと`predicate_matches`は一度も呼ばれないため、
        // `check_predicate_type`による事前の静的検査が無いと`WHERE 1`のような
        // 書き誤りが空テーブルに対しては素通りしてしまう。
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows: Vec<Tuple> = Vec::new();

        let result = filter(&schema, &functions, rows, &expr("1"));
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    // ---- predicate_matches ----

    #[test]
    fn predicate_matches_true_only_for_boolean_true() {
        assert!(predicate_matches(Value::Boolean(true)).unwrap());
        assert!(!predicate_matches(Value::Boolean(false)).unwrap());
        assert!(!predicate_matches(Value::Null).unwrap());
    }

    #[test]
    fn predicate_matches_rejects_non_boolean_values() {
        assert!(matches!(
            predicate_matches(Value::BigInt(1)),
            Err(DbError::Eval(_))
        ));
        assert!(matches!(
            predicate_matches(Value::Text("x".to_string())),
            Err(DbError::Eval(_))
        ));
    }

    #[test]
    fn predicate_matches_error_message_shows_the_sql_type_name_not_rust_debug_output() {
        // `Value::data_type()`は`Option<DataType>`を返すため、`{:?}`でそのまま
        // 表示すると`Some(BigInt)`のようにRustの内部表現が利用者に漏れてしまう。
        // ここではSQLの型名(`BIGINT`)だけが出ることを固定する。
        let err = predicate_matches(Value::BigInt(1)).unwrap_err();
        let DbError::Eval(message) = err else {
            panic!("DbError::Evalを期待したが{err:?}が返った");
        };
        assert!(message.contains("BIGINTが渡されました"));
        assert!(!message.contains("Some("));
    }

    // ---- Projection ----

    #[test]
    fn project_expands_wildcard_to_all_columns_in_schema_order() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![tuple(1, Some("Alice"))];
        let items = vec![SelectItem::Wildcard { span: Span::new(0, 0) }];

        let (out_schema, out_rows) =
            project(&schema, &functions, &rows, &items, "SELECT *").unwrap();
        assert_eq!(
            out_schema
                .columns()
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "name"]
        );
        assert_eq!(out_rows[0].values(), tuple(1, Some("Alice")).values());
    }

    #[test]
    fn project_computed_column_is_always_nullable() {
        // 1行目の`id + 1`はNULLにならないが、出力列の`nullable`は保守的に
        // 常に`true`にする(`project`のコメント参照)。この保守化のおかげで、
        // 2行目以降に実際にNULLを含む行が来ても、Tuple::newのスキーマ検査に
        // 引っかからない。
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![tuple(1, Some("Alice")), tuple(2, None)];
        let sql = "SELECT id, name";
        let items = match crate::parser::parse_statement(sql).unwrap() {
            Statement::Select(select) => select.items,
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        };

        let (out_schema, out_rows) = project(&schema, &functions, &rows, &items, sql).unwrap();
        assert!(out_schema.columns()[1].nullable);
        assert_eq!(out_rows.len(), 2);
    }

    /// テスト用に、`SELECT`文のSQLから`items`(対象式リスト)だけを取り出す。
    fn select_items(sql: &str) -> Vec<SelectItem> {
        match crate::parser::parse_statement(sql).unwrap() {
            Statement::Select(select) => select.items,
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn project_infers_computed_column_type_statically_even_when_first_row_is_null() {
        // `x`はNULLを許すBIGINT列。1行目が`abs(x)`をNULLにする値でも、出力列の
        // 型は実際に1行評価した結果ではなく、式のASTと入力`Schema`だけから
        // 静的に決まるため`BigInt`のままになる。もし旧実装のように1行目を
        // 評価して`data_type()`(NULLは`None`)から型を決めていたら、ここが
        // `Text`にフォールバックし、2行目の非NULLなBIGINTを`Tuple::new`の
        // スキーマ検査が`SchemaMismatch`として拒否していた。
        let schema = Schema::new(vec![Column::new("x", DataType::BigInt, true)]);
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![
            Tuple::new(&schema, vec![Value::Null]).unwrap(),
            Tuple::new(&schema, vec![Value::BigInt(-5)]).unwrap(),
        ];
        let items = select_items("SELECT abs(x)");

        let (out_schema, out_rows) =
            project(&schema, &functions, &rows, &items, "SELECT abs(x)").unwrap();
        assert_eq!(out_schema.columns()[0].data_type, DataType::BigInt);
        assert_eq!(out_rows[0].values()[0], Value::Null);
        assert_eq!(out_rows[1].values()[0], Value::BigInt(5));
    }

    #[test]
    fn project_infers_computed_column_type_on_an_empty_table() {
        // 行が1件も無くても、`infer_type`は式のASTだけから型を決められる。
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows: Vec<Tuple> = vec![];
        let items = select_items("SELECT id + 1");

        let (out_schema, out_rows) =
            project(&schema, &functions, &rows, &items, "SELECT id + 1").unwrap();
        assert_eq!(out_schema.columns()[0].data_type, DataType::BigInt);
        assert!(out_rows.is_empty());
    }

    #[test]
    fn project_scalar_function_on_non_null_rows_still_works() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![tuple(1, Some("Alice")), tuple(2, Some("Bob"))];
        let items = select_items("SELECT length(name)");

        let (out_schema, out_rows) =
            project(&schema, &functions, &rows, &items, "SELECT length(name)").unwrap();
        assert_eq!(out_schema.columns()[0].data_type, DataType::BigInt);
        assert_eq!(out_rows[0].values()[0], Value::BigInt(5));
        assert_eq!(out_rows[1].values()[0], Value::BigInt(3));
    }

    // ---- Insert ----

    #[test]
    fn insert_appends_all_rows_at_once() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let mut table = MemTable::new();

        let rows = vec![vec![expr("1"), expr("'Alice'")], vec![expr("2"), expr("'Bob'")]];
        let count = insert(&mut table, &schema, &functions, None, &rows).unwrap();
        assert_eq!(count, 2);
        assert_eq!(table.rows().len(), 2);
    }

    #[test]
    fn insert_with_explicit_columns_fills_omitted_columns_with_null() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let mut table = MemTable::new();

        let columns = vec![ident("id")];
        let rows = vec![vec![expr("1")]];
        insert(&mut table, &schema, &functions, Some(&columns), &rows).unwrap();
        assert_eq!(table.rows()[0].values(), &[Value::BigInt(1), Value::Null]);
    }

    #[test]
    fn insert_is_all_or_nothing_when_a_row_fails_validation() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let mut table = MemTable::new();

        // 1行目は妥当だが、2行目は`id`(NOT NULL)にNULLを渡していて失敗する。
        let rows = vec![
            vec![expr("1"), expr("'Alice'")],
            vec![expr("NULL"), expr("'Bob'")],
        ];
        let result = insert(&mut table, &schema, &functions, None, &rows);
        assert!(result.is_err());
        assert!(table.rows().is_empty());
    }

    // ---- Update ----

    #[test]
    fn update_changes_only_matched_rows() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let mut table = MemTable::new();
        table.rows_mut().push(tuple(1, Some("Alice")));
        table.rows_mut().push(tuple(2, Some("Bob")));

        let assignments = vec![Assignment {
            column: ident("name"),
            value: expr("'Carol'"),
            span: Span::new(0, 0),
        }];
        let count = update(
            &mut table,
            &schema,
            &functions,
            &assignments,
            Some(&expr("id = 1")),
        )
        .unwrap();

        assert_eq!(count, 1);
        assert_eq!(table.rows()[0].values()[1], Value::Text("Carol".to_string()));
        assert_eq!(table.rows()[1].values()[1], Value::Text("Bob".to_string()));
    }

    #[test]
    fn update_leaves_the_table_untouched_when_a_row_fails_validation() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let mut table = MemTable::new();
        table.rows_mut().push(tuple(1, Some("Alice")));
        table.rows_mut().push(tuple(2, Some("Bob")));

        let assignments = vec![Assignment {
            column: ident("id"),
            value: expr("NULL"),
            span: Span::new(0, 0),
        }];
        let result = update(&mut table, &schema, &functions, &assignments, None);
        assert!(result.is_err());
        // 1行目の検査で失敗したので、2行目まで検査が進んでいても`table`は
        // 一切変更されていない。
        assert_eq!(table.rows()[0].values()[0], Value::BigInt(1));
        assert_eq!(table.rows()[1].values()[0], Value::BigInt(2));
    }

    #[test]
    fn update_rejects_a_non_boolean_predicate_even_on_an_empty_table() {
        // `filter`と同じ理由で、`table`が空だと行ループが1度も回らないため、
        // `check_predicate_type`による事前検査が無いと`WHERE 1`が素通りする。
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let mut table = MemTable::new();

        let assignments = vec![Assignment {
            column: ident("name"),
            value: expr("'Carol'"),
            span: Span::new(0, 0),
        }];
        let result = update(
            &mut table,
            &schema,
            &functions,
            &assignments,
            Some(&expr("1")),
        );
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    // ---- Delete ----

    #[test]
    fn delete_removes_only_matched_rows() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let mut table = MemTable::new();
        table.rows_mut().push(tuple(1, Some("Alice")));
        table.rows_mut().push(tuple(2, Some("Bob")));

        let count = delete(&mut table, &schema, &functions, Some(&expr("id = 1"))).unwrap();
        assert_eq!(count, 1);
        assert_eq!(table.rows().len(), 1);
        assert_eq!(table.rows()[0].values()[0], Value::BigInt(2));
    }

    #[test]
    fn delete_without_predicate_removes_every_row() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let mut table = MemTable::new();
        table.rows_mut().push(tuple(1, Some("Alice")));
        table.rows_mut().push(tuple(2, Some("Bob")));

        let count = delete(&mut table, &schema, &functions, None).unwrap();
        assert_eq!(count, 2);
        assert!(table.rows().is_empty());
    }

    #[test]
    fn delete_rejects_a_non_boolean_predicate_even_on_an_empty_table() {
        // `filter`と同じ理由で、`table`が空だと行ループが1度も回らないため、
        // `check_predicate_type`による事前検査が無いと`WHERE 1`が素通りする。
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let mut table = MemTable::new();

        let result = delete(&mut table, &schema, &functions, Some(&expr("1")));
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    // ---- Storage版(seq_scan/insert/update/delete) ----

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-executor-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        path
    }

    /// `users_schema`のテーブルを1つ持つ`Storage`を作り、その`TableId`と
    /// 一緒に返す。呼び出し側はテスト終了時に`std::fs::remove_file`で
    /// 一時ファイルを片付ける。
    fn users_storage(name: &str) -> (std::path::PathBuf, Storage, TableId) {
        let path = temp_path(name);
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();
        (path, storage, table_id)
    }

    #[test]
    fn storage_insert_then_storage_seq_scan_round_trips() {
        let (path, mut storage, table_id) = users_storage("insert-scan");
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();

        let rows = vec![vec![expr("1"), expr("'Alice'")], vec![expr("2"), expr("'Bob'")]];
        let count = storage_insert(&mut storage, table_id, &schema, &functions, None, &rows).unwrap();
        assert_eq!(count, 2);

        let scanned = storage_seq_scan(&storage, table_id, &schema).unwrap();
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned[0].values(), tuple(1, Some("Alice")).values());
        assert_eq!(scanned[1].values(), tuple(2, Some("Bob")).values());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn storage_insert_is_all_or_nothing_when_a_row_fails_validation() {
        let (path, mut storage, table_id) = users_storage("insert-validation");
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();

        // 1行目は妥当だが、2行目は`id`(NOT NULL)にNULLを渡していて失敗する。
        let rows = vec![
            vec![expr("1"), expr("'Alice'")],
            vec![expr("NULL"), expr("'Bob'")],
        ];
        let result = storage_insert(&mut storage, table_id, &schema, &functions, None, &rows);
        assert!(result.is_err());
        assert!(storage_seq_scan(&storage, table_id, &schema).unwrap().is_empty());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn storage_update_changes_only_matched_rows_and_keeps_the_record_id() {
        let (path, mut storage, table_id) = users_storage("update");
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        storage_insert(
            &mut storage,
            table_id,
            &schema,
            &functions,
            None,
            &[
                vec![expr("1"), expr("'Alice'")],
                vec![expr("2"), expr("'Bob'")],
            ],
        )
        .unwrap();

        let assignments = vec![Assignment {
            column: ident("name"),
            value: expr("'Carol'"),
            span: Span::new(0, 0),
        }];
        let count = storage_update(
            &mut storage,
            table_id,
            &schema,
            &functions,
            &assignments,
            Some(&expr("id = 1")),
        )
        .unwrap();
        assert_eq!(count, 1);

        let scanned = storage_seq_scan(&storage, table_id, &schema).unwrap();
        assert_eq!(scanned.len(), 2);
        assert!(scanned.contains(&tuple(1, Some("Carol"))));
        assert!(scanned.contains(&tuple(2, Some("Bob"))));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn storage_delete_removes_only_matched_rows() {
        let (path, mut storage, table_id) = users_storage("delete");
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        storage_insert(
            &mut storage,
            table_id,
            &schema,
            &functions,
            None,
            &[
                vec![expr("1"), expr("'Alice'")],
                vec![expr("2"), expr("'Bob'")],
            ],
        )
        .unwrap();

        let count = storage_delete(&mut storage, table_id, &schema, &functions, Some(&expr("id = 1"))).unwrap();
        assert_eq!(count, 1);

        let scanned = storage_seq_scan(&storage, table_id, &schema).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].values()[0], Value::BigInt(2));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn storage_update_and_storage_delete_reject_a_non_boolean_predicate_even_on_an_empty_table() {
        // `update`・`delete`(MemTable版)と同じ理由で、行ループへ入る前の
        // `check_predicate_type`が無いと、テーブルが空の間は`WHERE 1`のような
        // 書き誤りを見逃してしまう。
        let (path, mut storage, table_id) = users_storage("empty-predicate");
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();

        let assignments = vec![Assignment {
            column: ident("name"),
            value: expr("'Carol'"),
            span: Span::new(0, 0),
        }];
        let update_result = storage_update(
            &mut storage,
            table_id,
            &schema,
            &functions,
            &assignments,
            Some(&expr("1")),
        );
        assert!(matches!(update_result, Err(DbError::Eval(_))));

        let delete_result = storage_delete(&mut storage, table_id, &schema, &functions, Some(&expr("1")));
        assert!(matches!(delete_result, Err(DbError::Eval(_))));

        std::fs::remove_file(&path).unwrap();
    }
}
