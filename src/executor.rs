//! この章の実行演算子: Values / Sequential Scan / Filter / Projection /
//! Insert / Update / Delete。
//!
//! `Executor::next()`が1行ずつ引っ張り出すVolcanoモデルは第19章で導入する。
//! この章の演算子は、表全体を`Vec<Tuple>`としてまとめて受け取り、まとめて
//! 返す素朴な関数にとどめる。`Database::execute`は、これらの関数を文の種類
//! ごとに正しい順序で呼び出す配線役に徹する。第18章からは、その「正しい順序」
//! 自体が`logical_plan::LogicalPlan`という木として明示的な値になり、
//! `Database::eval_query_plan`が木を根から葉へたどりながらこのモジュールの
//! 演算子を呼び出す(`INSERT`・`UPDATE`・`DELETE`は、木を経由しつつも実行の
//! 中身は変わらず、この章より前と同じ`insert`・`update`・`delete`をそのまま呼ぶ)。
//!
//! # 第17章から: 名前解決・型検査は`binder`モジュールへ移した
//!
//! 第10章では、`WHERE`句や`SELECT`の対象式の型検査(`infer_type`・
//! `check_predicate_type`)、`*`の展開(`resolve_items`)はこのモジュールが
//! 担っていた。第17章で`Binder`(`crate::binder`)を導入し、これらをすべて
//! `Database::execute`のBind段階へ移した。`filter`・`project`・`update`・
//! `delete`が受け取る`predicate`・`projection`・`assignments`は、すでに
//! `Binder`が名前解決・型検査を終えた`BoundExpr`(または、それを含む型)であり、
//! このモジュールは`Expr`という生のASTには一切触れない。`INSERT`の`VALUES`
//! だけは例外で、列参照を持たない式(既存の行を参照する構文が無い)なので、
//! 引き続き生の`Expr`のまま`eval::eval_expr`で評価する。
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

use crate::ast::Expr;
use crate::binder::{BoundAssignment, BoundExpr, BoundSelectItem};
use crate::error::{DbError, DbResult};
use crate::eval::{FunctionRegistry, eval_bound_expr, eval_expr};
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
fn predicate_matches(value: Value) -> DbResult<bool> {
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
/// 適用する。
///
/// `predicate`が`BOOLEAN`(または型未定の`NULL`)を返す式であることは、
/// `Binder`の`bind_predicate`がすでに検査済みである。第10章の`filter`が
/// 行ループへ入る前に呼んでいた`check_predicate_type`は、この章では不要に
/// なった(`rows`が空でも、Bind段階の検査がすでに`WHERE 1`のような書き誤りを
/// 検出しているため)。
pub fn filter(
    schema: &Schema,
    functions: &FunctionRegistry,
    rows: Vec<Tuple>,
    predicate: &BoundExpr,
) -> DbResult<Vec<Tuple>> {
    let mut kept = Vec::with_capacity(rows.len());
    for tuple in rows {
        let row = Row::new(schema, &tuple);
        let value = eval_bound_expr(predicate, functions, Some(&row))?;
        if predicate_matches(value)? {
            kept.push(tuple);
        }
    }
    Ok(kept)
}

/// Projection演算子。束縛済みの射影対象リスト(`projection`)を各行に適用し、
/// 出力用の`Schema`と行の並びを組み立てる。
///
/// `*`の展開は`Binder`(第17章)がすでに行っているため、この関数は「列参照または
/// 式のリスト」という1種類の形だけを見ればよい。出力列の型・nullableは、
/// 単純な列参照であれば`table_schema`の定義をそのまま使うため常に正確である。
/// 計算結果(`id + 1`のような式)の型は、`item.expr.data_type()`(`Binder`が
/// 構築時に決めた型)をそのまま使う。行を実際に評価しないこの型決定の理由は
/// `binder::Binder::bind_expr`のドキュメントコメントを参照。
pub fn project(
    table_schema: &Schema,
    functions: &FunctionRegistry,
    rows: &[Tuple],
    projection: &[BoundSelectItem],
) -> DbResult<(Schema, Vec<Tuple>)> {
    let mut out_columns = Vec::with_capacity(projection.len());
    for item in projection {
        if let BoundExpr::ColumnRef { column_index, .. } = &item.expr {
            let mut column = table_schema.columns()[*column_index].clone();
            column.name = item.output_name.clone();
            out_columns.push(column);
            continue;
        }

        // `data_type()`が`None`(型が定まらない、`NULL`単体など)を返す式は
        // `TEXT`で代用する。この場合の`nullable`は、行ごとに`NULL`になったり
        // ならなかったりしうるため常に`true`にする。
        let data_type = item.expr.data_type().unwrap_or(DataType::Text);
        out_columns.push(Column::new(item.output_name.clone(), data_type, true));
    }
    let out_schema = Schema::new(out_columns);

    let mut out_rows = Vec::with_capacity(rows.len());
    for tuple in rows {
        let row = Row::new(table_schema, tuple);
        let mut values = Vec::with_capacity(projection.len());
        for item in projection {
            values.push(eval_bound_expr(&item.expr, functions, Some(&row))?);
        }
        out_rows.push(Tuple::new(&out_schema, values)?);
    }
    Ok((out_schema, out_rows))
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

    fn bound_projection(select_sql: &str) -> Vec<BoundSelectItem> {
        bind_select(&format!("SELECT {select_sql} FROM users")).projection
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
            tuple(2, None),        // `name = 'Alice'`はUNKNOWN
            tuple(3, Some("Bob")), // `name = 'Alice'`はFALSE
        ];

        let predicate = bound_predicate("name = 'Alice'");
        let kept = filter(&schema, &functions, rows, &predicate).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].values()[0], Value::BigInt(1));
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

    // ---- Projection ----

    #[test]
    fn project_expands_wildcard_to_all_columns_in_schema_order() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![tuple(1, Some("Alice"))];
        let projection = bound_projection("*");

        let (out_schema, out_rows) = project(&schema, &functions, &rows, &projection).unwrap();
        assert_eq!(
            out_schema.columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
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
        let projection = bound_projection("id, name");

        let (out_schema, out_rows) = project(&schema, &functions, &rows, &projection).unwrap();
        assert!(out_schema.columns()[1].nullable);
        assert_eq!(out_rows.len(), 2);
    }

    #[test]
    fn project_infers_computed_column_type_statically_even_when_first_row_is_null() {
        // `x`はNULLを許すBIGINT列。1行目が`abs(x)`をNULLにする値でも、出力列の
        // 型は実際に1行評価した結果ではなく、`Binder`が式のASTと入力`Schema`
        // だけから静的に決めるため`BigInt`のままになる。もし1行目を評価して
        // `data_type()`(NULLは`None`)から型を決めていたら、ここが`Text`に
        // フォールバックし、2行目の非NULLなBIGINTを`Tuple::new`のスキーマ検査が
        // `SchemaMismatch`として拒否していた。
        let mut catalog = Catalog::new();
        catalog
            .create_table("t", Schema::new(vec![Column::new("x", DataType::BigInt, true)]))
            .unwrap();
        let functions = FunctionRegistry::with_builtins();
        let sql = "SELECT abs(x) FROM t";
        let statement = crate::parser::parse_statement(sql).unwrap();
        let select = match Binder::new(&catalog, &functions, sql).bind(statement).unwrap() {
            BoundStatement::Select(select) => select,
            other => panic!("Selectを期待したが{other:?}が返った"),
        };

        let schema = Schema::new(vec![Column::new("x", DataType::BigInt, true)]);
        let rows = vec![
            Tuple::new(&schema, vec![Value::Null]).unwrap(),
            Tuple::new(&schema, vec![Value::BigInt(-5)]).unwrap(),
        ];

        let (out_schema, out_rows) = project(&schema, &functions, &rows, &select.projection).unwrap();
        assert_eq!(out_schema.columns()[0].data_type, DataType::BigInt);
        assert_eq!(out_rows[0].values()[0], Value::Null);
        assert_eq!(out_rows[1].values()[0], Value::BigInt(5));
    }

    #[test]
    fn project_infers_computed_column_type_on_an_empty_table() {
        // 行が1件も無くても、`Binder`は式のASTだけから型を決められる。
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows: Vec<Tuple> = vec![];
        let projection = bound_projection("id + 1");

        let (out_schema, out_rows) = project(&schema, &functions, &rows, &projection).unwrap();
        assert_eq!(out_schema.columns()[0].data_type, DataType::BigInt);
        assert!(out_rows.is_empty());
    }

    #[test]
    fn project_scalar_function_on_non_null_rows_still_works() {
        let schema = users_schema();
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![tuple(1, Some("Alice")), tuple(2, Some("Bob"))];
        let projection = bound_projection("length(name)");

        let (out_schema, out_rows) = project(&schema, &functions, &rows, &projection).unwrap();
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
        let rows = vec![vec![expr("1"), expr("'Alice'")], vec![expr("NULL"), expr("'Bob'")]];
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
            &[vec![expr("1"), expr("'Alice'")], vec![expr("2"), expr("'Bob'")]],
        )
        .unwrap();

        let assignments = bound_assignments("name = 'Carol'");
        let predicate = bound_predicate("id = 1");
        let count =
            storage_update(&mut storage, table_id, &schema, &functions, &assignments, Some(&predicate)).unwrap();
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
            &[vec![expr("1"), expr("'Alice'")], vec![expr("2"), expr("'Bob'")]],
        )
        .unwrap();

        let predicate = bound_predicate("id = 1");
        let count = storage_delete(&mut storage, table_id, &schema, &functions, Some(&predicate)).unwrap();
        assert_eq!(count, 1);

        let scanned = storage_seq_scan(&storage, table_id, &schema).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].values()[0], Value::BigInt(2));

        std::fs::remove_file(&path).unwrap();
    }
}
