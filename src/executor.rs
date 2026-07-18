//! この章の実行演算子: Values / Sequential Scan / Filter / Projection /
//! Insert / Update / Delete。
//!
//! `Executor::next()`が1行ずつ引っ張り出すVolcanoモデルは第19章で導入する。
//! この章の演算子は、表全体を`Vec<Tuple>`としてまとめて受け取り、まとめて
//! 返す素朴な関数にとどめる。`Database::execute`は、これらの関数を文の種類
//! ごとに正しい順序で呼び出す配線役に徹する。

use crate::ast::{Assignment, Expr, Ident, SelectItem};
use crate::error::{DbError, DbResult};
use crate::eval::{FunctionRegistry, eval_expr};
use crate::storage_mem::MemTable;
use crate::types::{Column, DataType, Row, Schema, Tuple, Value};

/// Sequential Scan演算子。テーブルの全行を、格納順のまま複製して返す。
///
/// 索引を持たないこの章では、`WHERE`があってもなくても、まずテーブル全体を
/// 読む以外に行へたどり着く手段が無い。
pub fn seq_scan(table: &MemTable) -> Vec<Tuple> {
    table.rows().to_vec()
}

/// Filter演算子。`predicate`を各行に対して評価し、`TRUE`になった行だけを残す。
///
/// SQLの`WHERE`は三値論理で評価するため、`FALSE`はもちろん`UNKNOWN`(`NULL`)に
/// なった行も、`TRUE`ではないので落ちる。`NULL`の行を「一致しなかった」側に
/// 含めるこの規則は、`eval`モジュールが実装する三値論理(第8章)をそのまま
/// 使うだけで、この関数自身が判定するのは`Value::Boolean(true)`かどうかだけである。
pub fn filter(
    schema: &Schema,
    functions: &FunctionRegistry,
    rows: Vec<Tuple>,
    predicate: &Expr,
) -> DbResult<Vec<Tuple>> {
    let mut kept = Vec::with_capacity(rows.len());
    for tuple in rows {
        let row = Row::new(schema, &tuple);
        if matches!(
            eval_expr(predicate, functions, Some(&row))?,
            Value::Boolean(true)
        ) {
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
/// 使うため常に正確だが、計算結果(`id + 1`のような式)は静的な型検査を
/// まだ持たないため、実際に1行評価してみてその結果から`data_type`を決める
/// (行が1件も無ければ`TEXT`で代用する。第8章で`NULL`リテラルの結果列に
/// 使ったのと同じ折衷案)。この場合の`nullable`は、行ごとに`NULL`になったり
/// ならなかったりしうるため、常に`true`とする。静的な型検査は第17章のBinderで
/// 置き換える。
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

        let data_type = match rows.first() {
            Some(first) => {
                let row = Row::new(table_schema, first);
                eval_expr(expr, functions, Some(&row))?
                    .data_type()
                    .unwrap_or(DataType::Text)
            }
            None => DataType::Text,
        };
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

    let count = planned.len();
    table.rows_mut().extend(planned);
    Ok(count)
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
pub fn update(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    assignments: &[Assignment],
    predicate: Option<&Expr>,
) -> DbResult<usize> {
    let mut planned = Vec::new();
    for (index, tuple) in table.rows().iter().enumerate() {
        let row = Row::new(schema, tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => matches!(
                eval_expr(pred, functions, Some(&row))?,
                Value::Boolean(true)
            ),
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

/// Delete演算子。`predicate`が`TRUE`になった行を`table`から取り除く。
///
/// `Filter`演算子と対になる形で、生き残る行(`predicate`が`TRUE`にならなかった
/// 行)を新しい`Vec`へ集め終えてから`table`へ書き戻す。集め終える前に評価が
/// 失敗すれば、その時点で`table`はまだ元のままなので、失敗した`DELETE`が
/// 一部の行だけ消してしまうことはない。
pub fn delete(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    predicate: Option<&Expr>,
) -> DbResult<usize> {
    let mut kept = Vec::with_capacity(table.rows().len());
    let mut deleted = 0usize;
    for tuple in table.rows() {
        let row = Row::new(schema, tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => matches!(
                eval_expr(pred, functions, Some(&row))?,
                Value::Boolean(true)
            ),
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
}
