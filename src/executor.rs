//! この章の実行演算子: Insert / Update / Delete。
//!
//! 第18章まではSequential Scan・Filter・Projectionもこのモジュールが
//! `Vec<Tuple>`をまとめて受け取りまとめて返す関数(`seq_scan`・
//! `storage_seq_scan`・`filter`・`project`)として持っていた。第19章で
//! `Executor::next()`が1行ずつ引っ張り出すVolcanoモデルを導入し、この3つは
//! `physical_plan`モジュールの`MemSeqScanExec`・`DiskSeqScanExec`・
//! `FilterExec`・`ProjectionExec`に置き換わった。`INSERT`・`UPDATE`・
//! `DELETE`だけはこのモジュールに残っている。理由は`physical_plan`モジュール
//! 冒頭のドキュメント([`crate::physical_plan`]の「`INSERT`・`UPDATE`・
//! `DELETE`は`Executor`にしない」節)を参照。
//!
//! # 第17章から: 名前解決・型検査は`binder`モジュールへ移した
//!
//! 第10章では、`WHERE`句や`SELECT`の対象式の型検査(`infer_type`・
//! `check_predicate_type`)、`*`の展開(`resolve_items`)はこのモジュールが
//! 担っていた。第17章で`Binder`(`crate::binder`)を導入し、これらをすべて
//! `Database::execute`のBind段階へ移した。`update`・`delete`が受け取る
//! `predicate`・`assignments`は、すでに`Binder`が名前解決・型検査を終えた
//! `BoundExpr`(または、それを含む型)であり、このモジュールは`Expr`という
//! 生のASTには一切触れない。`INSERT`の`VALUES`だけは例外で、列参照を持たない
//! 式(既存の行を参照する構文が無い)なので、引き続き生の`Expr`のまま
//! `eval::eval_expr`で評価する。
//!
//! # 行の供給源が2つある
//!
//! 第16章から、行の供給源は`MemTable`(第10章、プロセスのメモリ上)と
//! `Storage`(第15章、ディスク上のファイル)の2つになった。`update`・
//! `delete`は供給源に直接触れる演算子であり、それぞれに`storage_`を接頭辞に
//! 持つ対の関数(`storage_update`・`storage_delete`)を持つ。`insert`・
//! `storage_insert`も同様である。
//!
//! 1つの関数を`enum`や`trait`で両対応させる案も検討したが、この章でも見送った。
//! `MemTable`は行を`Vec<Tuple>`の添字で直接指すのに対し、`Storage`は
//! `RecordId`(第13章)で指す。`update`・`delete`が「どの行を書き換えるか」を
//! 特定する手段そのものが両者で異なるため、共通化すると分岐だらけの抽象が
//! 必要になる。

use crate::ast::Expr;
use crate::binder::{BoundAssignment, BoundExpr};
use crate::error::{DbError, DbResult};
use crate::eval::{FunctionRegistry, eval_bound_expr, eval_expr};
use crate::ids::{RecordId, TableId};
use crate::storage::Storage;
use crate::storage_mem::MemTable;
use crate::tuple_codec::{decode_tuple, encode_tuple};
use crate::types::{Row, Schema, Tuple, Value};

/// `WHERE`・`SET`の`predicate`が評価された結果を、SQLの三値論理に従って
/// 「その行にマッチしたかどうか」の`bool`へ変換する。
///
/// `TRUE`だけがマッチで、`FALSE`と`NULL`(`UNKNOWN`)はどちらもマッチしない
/// (`physical_plan::FilterExec`・`update`・`delete`が共通して従うべき規則)。`BIGINT`や`TEXT`の
/// ような`BOOLEAN`ではない値が渡された場合は、それを黙って「マッチしない」
/// 側に丸めてしまうと`WHERE 1`のような書き誤りを見逃すことになるため、
/// `DbError::Eval`にする。
///
/// この最後の分岐(`BOOLEAN`・`NULL`以外)は、通常は届かない。`predicate`は
/// `Binder`(第17章)の`bind_predicate`がすでに`BOOLEAN`または型未定の`NULL`
/// であることを検査済みの`BoundExpr`だからである。それでもこの分岐を残して
/// あるのは、`Binder`を経由しない呼び出し経路を想定した保険ではなく、
/// 「`BoundExpr::data_type()`が`Some(Boolean)`または`None`である」という
/// 事実をRustの型システムはコンパイル時に保証しないためである。仮に
/// `Binder`側にバグがあって型検査をすり抜けた場合でも、`executor`が
/// `BOOLEAN`でない値を暗黙に「マッチしない」側へ丸めてしまう(=誤りを
/// 隠してしまう)ことだけは避けたい、という最終防衛線として残している。
pub(crate) fn predicate_matches(value: Value) -> DbResult<bool> {
    match value {
        Value::Boolean(true) => Ok(true),
        Value::Boolean(false) | Value::Null => Ok(false),
        other => {
            let data_type = other
                .data_type()
                .expect("BooleanとNullは既に処理済みなのでdata_type()は必ずSomeを返す");
            Err(DbError::Eval(format!(
                "WHERE句はBOOLEANまたはNULLを返す式である必要があります: {data_type}が渡されました"
            )))
        }
    }
}

/// Insert演算子。`VALUES`の各行を評価し、`Tuple::new`による
/// スキーマ検査に通ったものだけを`table`へ追加する。
///
/// 検査はテーブルへ1行も書き込む前に、全行に対して済ませる。
/// 3行目の型が`NOT NULL`列に違反していた場合、1・2行目の検査がどれだけ
/// 成功していても、`?`によってその場で打ち切られ、`table`には何も追加されない。
/// 「一部だけ挿入された`INSERT`」という中途半端な状態を避けるこの順序は、
/// `CREATE TABLE`が列定義を1つずつ検査してから最後に1回だけ登録する
/// (第9章)のと同じ考え方である。
///
/// `columns`は、`Binder`(第17章)の`bind_insert`が列名を`schema`上の索引へ
/// 解決した並びである(未知の列名・重複した列名はBind段階ですでに拒否されて
/// いるため、ここでは`schema.len()`の範囲内で重複の無い添字だけが渡ってくる
/// 前提で良い)。
pub fn insert(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    columns: Option<&[usize]>,
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
    columns: Option<&[usize]>,
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
    columns: Option<&[usize]>,
    rows: &[Vec<Expr>],
) -> DbResult<Vec<Tuple>> {
    let mut planned = Vec::with_capacity(rows.len());
    for row_exprs in rows {
        // `VALUES`の各要素は、テーブルの行を伴わない文脈で評価する。
        // `INSERT INTO t VALUES (id + 1)`のように挿入する側の値が既存の行を
        // 参照する構文は無いため、列参照が現れようが無く、`Binder`による
        // 名前解決の対象にもならない。`row`は常に`None`でよい。
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
/// `columns`(列インデックス)の妥当性・重複の無さは、これを組み立てた
/// `Binder`の`bind_insert`がすでに保証しているため、ここでは検査しない。
fn expand_to_schema(schema: &Schema, columns: Option<&[usize]>, values: Vec<Value>) -> DbResult<Vec<Value>> {
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
    for (&index, value) in columns.iter().zip(values) {
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
/// `assignments`の代入先(`column_index`)、`predicate`の型は、いずれも
/// `Binder`の`bind_update`がすでに検査済みである。
pub fn update(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    assignments: &[BoundAssignment],
    predicate: Option<&BoundExpr>,
) -> DbResult<usize> {
    let mut planned = Vec::new();
    for (index, tuple) in table.rows().iter().enumerate() {
        let row = Row::new(schema, tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => predicate_matches(eval_bound_expr(pred, functions, Some(&row))?)?,
        };
        if !matched {
            continue;
        }

        let mut new_values = tuple.values().to_vec();
        for assignment in assignments {
            new_values[assignment.column_index] = eval_bound_expr(&assignment.value, functions, Some(&row))?;
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
pub fn storage_update(
    storage: &mut Storage,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    assignments: &[BoundAssignment],
    predicate: Option<&BoundExpr>,
) -> DbResult<usize> {
    let mut planned: Vec<(RecordId, Tuple)> = Vec::new();
    for entry in storage.scan(table_id)? {
        let (rid, bytes) = entry?;
        let tuple = decode_tuple(schema, &bytes)?;
        let row = Row::new(schema, &tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => predicate_matches(eval_bound_expr(pred, functions, Some(&row))?)?,
        };
        if !matched {
            continue;
        }

        let mut new_values = tuple.values().to_vec();
        for assignment in assignments {
            new_values[assignment.column_index] = eval_bound_expr(&assignment.value, functions, Some(&row))?;
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
pub fn delete(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    predicate: Option<&BoundExpr>,
) -> DbResult<usize> {
    let mut kept = Vec::with_capacity(table.rows().len());
    let mut deleted = 0usize;
    for tuple in table.rows() {
        let row = Row::new(schema, tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => predicate_matches(eval_bound_expr(pred, functions, Some(&row))?)?,
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
pub fn storage_delete(
    storage: &mut Storage,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    predicate: Option<&BoundExpr>,
) -> DbResult<usize> {
    let mut to_delete: Vec<RecordId> = Vec::new();
    for entry in storage.scan(table_id)? {
        let (rid, bytes) = entry?;
        let tuple = decode_tuple(schema, &bytes)?;
        let row = Row::new(schema, &tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => predicate_matches(eval_bound_expr(pred, functions, Some(&row))?)?,
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
    use crate::binder::{Binder, BoundStatement};
    use crate::catalog::Catalog;
    use crate::types::{Column, DataType};

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

    /// テスト用に、`users`(`users_schema()`)を1つ持つカタログを通して`sql`を
    /// 束縛する。`filter`・`project`・`update`が受け取る`BoundExpr`・
    /// `BoundSelectItem`・`BoundAssignment`は、この章からは`Binder`を経由
    /// してしか作れないため、演算子のテストも実際に`Binder`を通す。
    fn users_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog.create_table("users", users_schema()).unwrap();
        catalog
    }

    fn bind_select(sql: &str) -> crate::binder::BoundSelect {
        let catalog = users_catalog();
        let functions = FunctionRegistry::with_builtins();
        let statement = crate::parser::parse_statement(sql).unwrap();
        match Binder::new(&catalog, &functions, sql).bind(statement).unwrap() {
            BoundStatement::Select(select) => select,
            other => panic!("Selectを期待したが{other:?}が返った"),
        }
    }

    /// `WHERE`の1式だけを束縛して取り出す。
    fn bound_predicate(where_sql: &str) -> BoundExpr {
        bind_select(&format!("SELECT id FROM users WHERE {where_sql}"))
            .predicate
            .expect("WHEREを指定したのでpredicateがあるはず")
    }

    fn bound_assignments(update_sql: &str) -> Vec<BoundAssignment> {
        let catalog = users_catalog();
        let functions = FunctionRegistry::with_builtins();
        let sql = format!("UPDATE users SET {update_sql}");
        let statement = crate::parser::parse_statement(&sql).unwrap();
        match Binder::new(&catalog, &functions, &sql).bind(statement).unwrap() {
            BoundStatement::Update(update) => update.assignments,
            other => panic!("Updateを期待したが{other:?}が返った"),
        }
    }

    /// テスト用に、SQLの式1個を`Expr`へ変換する。`INSERT`の`VALUES`は
    /// 束縛されない生の`Expr`のままなので、`insert`系のテストで使う。
    fn expr(sql: &str) -> Expr {
        match crate::parser::parse_statement(&format!("SELECT {sql}")).unwrap() {
            Statement::Select(select) => match select.items.into_iter().next().unwrap() {
                crate::ast::SelectItem::Expr { expr, .. } => expr,
                crate::ast::SelectItem::Wildcard { .. } => panic!("式を期待したがWildcardが返った"),
            },
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
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
        assert!(matches!(predicate_matches(Value::BigInt(1)), Err(DbError::Eval(_))));
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

        // `id`だけを指定する列インデックス(0番)。実際の解決は`Binder`の
        // `bind_insert`が行うが、この関数自体は解決済みの索引を受け取るだけ。
        let columns = vec![0usize];
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
        let rows = vec![vec![expr("1"), expr("'Alice'")], vec![expr("NULL"), expr("'Bob'")]];
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

        let assignments = bound_assignments("name = 'Carol'");
        let predicate = bound_predicate("id = 1");
        let count = update(&mut table, &schema, &functions, &assignments, Some(&predicate)).unwrap();

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

        let assignments = bound_assignments("id = NULL");
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

        let predicate = bound_predicate("id = 1");
        let count = delete(&mut table, &schema, &functions, Some(&predicate)).unwrap();
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

    // ---- Storage版(insert/update/delete) ----

    /// テスト検証専用。`storage_insert`・`storage_update`・`storage_delete`が
    /// 書き込んだ内容を確認するためだけに、`Storage::scan`を`decode_tuple`へ
    /// 通して全行を読み戻す。かつての`executor::storage_seq_scan`はこの手順を
    /// 演算子として公開していたが、この章からSequential Scanは
    /// `physical_plan::DiskSeqScanExec`(Volcano型のPull実行、1件ずつ供給)に
    /// 置き換わったため、`Vec`にまとめて返すこの形はテストの中だけに残す。
    fn scan_all(storage: &Storage, table_id: TableId, schema: &Schema) -> DbResult<Vec<Tuple>> {
        storage.scan(table_id)?.map(|entry| entry.and_then(|(_, bytes)| decode_tuple(schema, &bytes))).collect()
    }

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

        let scanned = scan_all(&storage, table_id, &schema).unwrap();
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
        let rows = vec![vec![expr("1"), expr("'Alice'")], vec![expr("NULL"), expr("'Bob'")]];
        let result = storage_insert(&mut storage, table_id, &schema, &functions, None, &rows);
        assert!(result.is_err());
        assert!(scan_all(&storage, table_id, &schema).unwrap().is_empty());

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
            &[vec![expr("1"), expr("'Alice'")], vec![expr("2"), expr("'Bob'")]],
        )
        .unwrap();

        let assignments = bound_assignments("name = 'Carol'");
        let predicate = bound_predicate("id = 1");
        let count =
            storage_update(&mut storage, table_id, &schema, &functions, &assignments, Some(&predicate)).unwrap();
        assert_eq!(count, 1);

        let scanned = scan_all(&storage, table_id, &schema).unwrap();
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
            &[vec![expr("1"), expr("'Alice'")], vec![expr("2"), expr("'Bob'")]],
        )
        .unwrap();

        let predicate = bound_predicate("id = 1");
        let count = storage_delete(&mut storage, table_id, &schema, &functions, Some(&predicate)).unwrap();
        assert_eq!(count, 1);

        let scanned = scan_all(&storage, table_id, &schema).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].values()[0], Value::BigInt(2));

        std::fs::remove_file(&path).unwrap();
    }
}
