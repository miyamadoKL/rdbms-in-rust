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

use std::collections::HashSet;

use crate::ast::Expr;
use crate::binder::{BoundAssignment, BoundExpr};
use crate::constraints;
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
    constraints::check_uniqueness(schema, table.rows().iter(), &planned)?;
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
///
/// # 第24章での変更: 一意性検査と索引の更新
///
/// `PRIMARY KEY`・`UNIQUE`列を持つテーブルは、第24章から`CREATE TABLE`が
/// 必ず対応する`UNIQUE`索引を自動生成している(`Database::execute_create_table`)。
/// 第20章の`constraints::check_uniqueness`(既存の全行を読んで`O(n)`で比較する)
/// は、その「既存行との比較」の部分を`crate::index::check_uniqueness_with_index`
/// (`UNIQUE`索引への`lookup`、`O(log n)`)へ置き換えた。同じ`INSERT`文の中の
/// 行同士の重複(`candidates`同士)は索引に無いキーの衝突なので
/// `check_uniqueness_with_index`では検出できず、引き続き
/// `constraints::check_uniqueness`(`others`を空にした呼び出し)へ残す。
///
/// 検証をすべて通過したら、`storage.insert`で行を書き込んだ直後に
/// `storage.index_insert_row`で**その行が対象になる全索引**(`UNIQUE`・
/// 非`UNIQUE`の両方)を更新する(Index Maintenance)。
///
/// # 第3部レビュー対応: Heapと索引の不整合を防ぐ
///
/// `storage.insert`(Heapへの書き込み)と`storage.index_insert_row`(索引への
/// 反映)は別々の呼び出しであり、間に他の操作を挟まないとはいえ、後者が
/// 個々の索引で`DbError::BTreeKeyTooLarge`を返して失敗する余地は残る
/// (たとえば`TEXT`列の値がHeapの1ページには収まるが、その列を索引化した
/// B+Treeの1ページには収まらないほど長い場合)。何もしなければ、Heapには
/// 存在するがどの索引にも登録されていない行が残り、Seq ScanとIndex Scanの
/// 結果が食い違う。
///
/// これを2段構えで防ぐ。まず、`planned`の全行について
/// `storage.check_indexes_accept_row`で「対象となる全索引にキーが収まるか」を
/// Heapへの書き込みより前に検証する(**主防御**)。`crate::btree::BTree::insert`は、
/// この検査を通過したキーに対する多段Split伝播が構造的に
/// `DbError::BTreeKeyTooLarge`にならないことを保証しているため
/// (`crate::btree`モジュールドキュメントの「Split中の伝播が安全である
/// 理由」を参照)、通常の失敗はここで`storage`に一切触れずに検出できる。
/// それでも`index_insert_row`が失敗した場合(`BufferPool`のI/Oエラーの
/// ような無関係な理由による、まれな経路)は、その行のために書き込んだ
/// Heap行を`storage.delete`で取り除いてからエラーを返す(**保険**、
/// `index_insert_row`自身がそれより前に成功していた索引への反映を巻き戻す
/// 処理と対になる)。この行より前に処理した行(同じ`INSERT`文の中の他の行)は、
/// モジュールドキュメントに書いた既存の割り切りのとおり巻き戻さない。
pub fn storage_insert(
    storage: &mut Storage,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    columns: Option<&[usize]>,
    rows: &[Vec<Expr>],
) -> DbResult<usize> {
    let planned = plan_insert_rows(schema, functions, columns, rows)?;
    if schema.unique_constrained_columns().next().is_some() {
        crate::index::check_uniqueness_with_index(storage, table_id, schema, &planned, &HashSet::new())?;
        constraints::check_uniqueness(schema, std::iter::empty(), &planned)?;
    }
    for tuple in &planned {
        storage.check_indexes_accept_row(table_id, tuple)?;
    }

    let count = planned.len();
    for tuple in planned {
        let bytes = encode_tuple(schema, &tuple);
        let rid = storage.insert(table_id, &bytes)?;
        if let Err(err) = storage.index_insert_row(table_id, &tuple, rid) {
            let _ = storage.delete(table_id, rid);
            return Err(err);
        }
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

    // 一意性は、更新される行の新しい値(`candidates`)が、更新されない行
    // (`others`)および更新される行同士のどちらとも重複しないことで検査する。
    // 更新される行自身の更新前の値は`others`に含めない。含めてしまうと、
    // `UPDATE users SET id = id WHERE id = 1`のような「値を変えない更新」まで
    // 自分自身との衝突として誤検出してしまう。
    let planned_indices: HashSet<usize> = planned.iter().map(|(index, _)| *index).collect();
    let candidates: Vec<Tuple> = planned.iter().map(|(_, tuple)| tuple.clone()).collect();
    let others = table
        .rows()
        .iter()
        .enumerate()
        .filter(|(index, _)| !planned_indices.contains(index))
        .map(|(_, tuple)| tuple);
    constraints::check_uniqueness(schema, others, &candidates)?;

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
/// # 第24章での変更: 一意性検査と索引の更新
///
/// `storage_insert`と同じ理由で、既存行との一意性検査は
/// `crate::index::check_uniqueness_with_index`(索引への`lookup`)へ置き換えた。
/// `UPDATE`は`INSERT`と違い「更新される行自身の更新前の値」を比較対象から
/// 除く必要がある(`UPDATE users SET id = id`のような値を変えない更新や、
/// 同じ文の中で2行が値を交換する更新を、自分自身との衝突として誤検出
/// しないため)。この除外を、`others`(更新されない行だけを集めた`Vec`)を
/// 作る代わりに、`planned`(更新される全行)の**更新前**の`RecordId`の
/// 集合(`exclude`)として`check_uniqueness_with_index`へ渡す形で行う。
///
/// Index Maintenanceは、`storage.update`が返す新しい`RecordId`(ページ内に
/// 収まれば元と同じ、収まらずページをまたいで移動すれば別の値になる、
/// `Storage::update`のドキュメントを参照)をそのまま使い、更新前の値を
/// `storage.index_delete_row`で取り除いてから、更新後の値を
/// `storage.index_insert_row`で挿入し直す。値が変わらない列でも、行が
/// 別のページへ移動していれば`RecordId`は変わるため、この削除→挿入を
/// 省略すると索引が古い`RecordId`を指したまま残ってしまう。
///
/// # 第3部レビュー対応: Heapと索引の不整合を防ぐ
///
/// `storage_insert`と同じ理由([`storage_insert`]のドキュメントを参照)で、
/// `storage.update`(Heapの書き換え)・`storage.index_delete_row`(旧索引の
/// 削除)・`storage.index_insert_row`(新索引への挿入)という3段階のどこかで
/// エラーが起きると、Heapと索引が食い違ったまま残る余地がある。
///
/// まず`planned`の全行について、更新後の値(`new_tuple`)が対象となる
/// 全索引に収まるかを`storage.check_indexes_accept_row`でHeapの書き換えより
/// 前に検証する(主防御)。`index_delete_row`は、`NULL`でも型不一致でもない
/// 既存のキーを取り除くだけなので通常は失敗しない(`BTree::delete`が
/// 返しうるエラーはどちらもすでに除外済みの入力にしか起こらない)。
/// `index_insert_row`は、事前検査を通過していれば`crate::btree::BTree::insert`の
/// 多段Split伝播が構造的に安全であることにより(`crate::btree`モジュール
/// ドキュメントの「Split中の伝播が安全である理由」を参照)通常は失敗しないが、
/// `BufferPool`のI/Oエラーのような無関係な理由でなお失敗する余地は残る
/// (まれな経路の保険)。いずれの段階が失敗しても、Heap・索引を更新前の
/// 内容(`old_tuple`、ただし物理的な位置は`new_rid`)へ戻してからエラーを返す。
pub fn storage_update(
    storage: &mut Storage,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    assignments: &[BoundAssignment],
    predicate: Option<&BoundExpr>,
) -> DbResult<usize> {
    let mut planned: Vec<(RecordId, Tuple, Tuple)> = Vec::new();
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
        let new_tuple = Tuple::new(schema, new_values)?;
        planned.push((rid, tuple, new_tuple));
    }

    if schema.unique_constrained_columns().next().is_some() {
        let exclude: HashSet<RecordId> = planned.iter().map(|(rid, _, _)| *rid).collect();
        let candidates: Vec<Tuple> = planned.iter().map(|(_, _, new_tuple)| new_tuple.clone()).collect();
        crate::index::check_uniqueness_with_index(storage, table_id, schema, &candidates, &exclude)?;
        constraints::check_uniqueness(schema, std::iter::empty(), &candidates)?;
    }
    for (_, _, new_tuple) in &planned {
        storage.check_indexes_accept_row(table_id, new_tuple)?;
    }

    let count = planned.len();
    for (old_rid, old_tuple, new_tuple) in planned {
        let old_bytes = encode_tuple(schema, &old_tuple);
        let new_bytes = encode_tuple(schema, &new_tuple);
        let new_rid = storage.update(table_id, old_rid, &new_bytes)?.unwrap_or(old_rid);

        if let Err(err) = storage.index_delete_row(table_id, &old_tuple, old_rid) {
            // 通常は起こらない(このコメントの上、`storage_update`ドキュメントの
            // 「第3部レビュー対応」を参照)。万一に備え、Heapだけでも
            // 更新前の内容へ戻す。
            let _ = storage.update(table_id, new_rid, &old_bytes);
            return Err(err);
        }

        if let Err(err) = storage.index_insert_row(table_id, &new_tuple, new_rid) {
            // 事前検査(check_indexes_accept_row)を通過していれば、
            // `crate::btree::BTree::insert`の多段Split伝播が構造的に安全である
            // ことにより(`crate::btree`モジュールドキュメントの「Split中の
            // 伝播が安全である理由」を参照)、通常はここに到達しない。万一
            // `BufferPool`のI/Oエラーのような無関係な理由で失敗しても、
            // 旧索引・旧Heapへ戻す。`index_insert_row`はここまでに成功していた
            // (無かった)索引への反映をすでに自身で巻き戻し済みなので、ここでは
            // 直前に削除した旧索引エントリを`new_rid`向けに挿入し直すだけでよい。
            let _ = storage.index_insert_row(table_id, &old_tuple, new_rid);
            let _ = storage.update(table_id, new_rid, &old_bytes);
            return Err(err);
        }
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
/// 第24章から、削除する行ごとに`storage.index_delete_row`を呼び、
/// `table_id`の全索引からその行のエントリを取り除く(Index Maintenance)。
pub fn storage_delete(
    storage: &mut Storage,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    predicate: Option<&BoundExpr>,
) -> DbResult<usize> {
    let mut to_delete: Vec<(RecordId, Tuple)> = Vec::new();
    for entry in storage.scan(table_id)? {
        let (rid, bytes) = entry?;
        let tuple = decode_tuple(schema, &bytes)?;
        let row = Row::new(schema, &tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => predicate_matches(eval_bound_expr(pred, functions, Some(&row))?)?,
        };
        if matched {
            to_delete.push((rid, tuple));
        }
    }

    let count = to_delete.len();
    for (rid, tuple) in to_delete {
        storage.delete(table_id, rid)?;
        storage.index_delete_row(table_id, &tuple, rid)?;
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
            BoundStatement::Select(select) => *select,
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

    // ---- 第3部レビュー対応: storage_insert/storage_updateとIndex Maintenanceの不整合 ----

    /// `id: BIGINT`(8バイト)を使う`users_schema`では、「Heapの1行としては
    /// 収まるが、対応するB+Treeの空のLeaf Page1枚には収まらない」という
    /// 値の幅が存在しない(`crate::storage`のテスト
    /// `index_insert_row_rolls_back_earlier_indexes_when_a_later_index_rejects_the_key`
    /// のコメントを参照)。1バイトで符号化される`flag: BOOLEAN`を使い、
    /// この幅を作れるスキーマにする。
    fn flag_schema() -> Schema {
        Schema::new(vec![Column::new("flag", DataType::Boolean, false), Column::new("payload", DataType::Text, true)])
    }

    fn flag_tuple(flag: bool, payload: Option<&str>) -> Tuple {
        let schema = flag_schema();
        let payload = match payload {
            Some(s) => Value::Text(s.to_string()),
            None => Value::Null,
        };
        Tuple::new(&schema, vec![Value::Boolean(flag), payload]).unwrap()
    }

    fn flag_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog.create_table("items", flag_schema()).unwrap();
        catalog
    }

    fn flag_bound_predicate(where_sql: &str) -> BoundExpr {
        let catalog = flag_catalog();
        let functions = FunctionRegistry::with_builtins();
        let sql = format!("SELECT flag FROM items WHERE {where_sql}");
        let statement = crate::parser::parse_statement(&sql).unwrap();
        match Binder::new(&catalog, &functions, &sql).bind(statement).unwrap() {
            BoundStatement::Select(select) => select.predicate.expect("WHEREを指定したのでpredicateがあるはず"),
            other => panic!("Selectを期待したが{other:?}が返った"),
        }
    }

    fn flag_bound_assignments(update_sql: &str) -> Vec<BoundAssignment> {
        let catalog = flag_catalog();
        let functions = FunctionRegistry::with_builtins();
        let sql = format!("UPDATE items SET {update_sql}");
        let statement = crate::parser::parse_statement(&sql).unwrap();
        match Binder::new(&catalog, &functions, &sql).bind(statement).unwrap() {
            BoundStatement::Update(update) => update.assignments,
            other => panic!("Updateを期待したが{other:?}が返った"),
        }
    }

    /// `flag_schema`のテーブル`items`を1つ持つ`Storage`を作る。
    fn items_storage(name: &str) -> (std::path::PathBuf, Storage, TableId) {
        let path = temp_path(name);
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("items", flag_schema()).unwrap();
        (path, storage, table_id)
    }

    /// Heapには収まるが、`payload`列を索引化したB+Treeの空のLeaf Page1枚には
    /// 収まらないほど長い`TEXT`値(`crate::storage`のテストと同じ幅の計算)。
    fn oversized_payload() -> String {
        "x".repeat(crate::page::PAGE_PAYLOAD_SIZE - 20)
    }

    /// `table_id`が持つ全索引について、索引が指す`RecordId`の集合が、
    /// Seq Scan(Heap)側でその索引化列がNULLでない行の`RecordId`の集合と
    /// 一致することを確認する(第3部レビューが要求する「エラー後もSeq Scan
    /// とIndex Scanが一致する」の検証そのもの)。
    fn assert_every_index_matches_seq_scan(storage: &Storage, table_id: TableId, schema: &Schema) {
        for info in storage.indexes_for_table(table_id).cloned().collect::<Vec<_>>() {
            let btree = storage.index_btree(&info.name).unwrap();
            let mut index_rids: Vec<RecordId> = btree
                .range(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
                .unwrap()
                .map(|entry| entry.unwrap().1)
                .collect();
            index_rids.sort_by_key(|r| (r.page_id.0, r.slot_id.0));

            let mut expected_rids: Vec<RecordId> = storage
                .scan(table_id)
                .unwrap()
                .map(|entry| entry.unwrap())
                .filter(|(_, bytes)| !decode_tuple(schema, bytes).unwrap().get(info.column_index).unwrap().is_null())
                .map(|(rid, _)| rid)
                .collect();
            expected_rids.sort_by_key(|r| (r.page_id.0, r.slot_id.0));

            assert_eq!(index_rids, expected_rids, "索引{}がSeq Scanの結果と一致しない", info.name);
        }
    }

    /// レビュー指摘5番の再現条件: `INSERT`文の2行目の`payload`が、Heapには
    /// 収まるが対応する索引には収まらない。事前検査
    /// (`Storage::check_indexes_accept_row`)がHeapへの書き込みより前に
    /// 全行を検査するため、1行目もまったく書き込まれず、既存の行・索引も
    /// 変化しない。
    #[test]
    fn storage_insert_leaves_heap_and_every_index_consistent_when_a_row_does_not_fit_an_index() {
        let (path, mut storage, table_id) = items_storage("insert-index-mismatch");
        let schema = flag_schema();
        let functions = FunctionRegistry::with_builtins();
        storage.create_index("flag_idx", "items", "flag", false).unwrap();
        storage.create_index("payload_idx", "items", "payload", false).unwrap();

        // 事前に妥当な行を1件入れておく(既存の行・索引が変化しないことを
        // 確認する対象)。
        storage_insert(&mut storage, table_id, &schema, &functions, None, &[vec![expr("true"), expr("'seed'")]]).unwrap();

        let huge_payload = oversized_payload();
        let rows = vec![
            vec![expr("false"), expr("'ok'")],
            vec![expr("true"), expr(&format!("'{huge_payload}'"))],
        ];
        let err = storage_insert(&mut storage, table_id, &schema, &functions, None, &rows).unwrap_err();
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));

        let scanned = scan_all(&storage, table_id, &schema).unwrap();
        assert_eq!(scanned.len(), 1, "全行を検査してから書き込むため、1行目も含めて一切挿入されていないはず");
        assert_eq!(scanned[0].values(), flag_tuple(true, Some("seed")).values());
        assert_every_index_matches_seq_scan(&storage, table_id, &schema);

        std::fs::remove_file(&path).unwrap();
        storage.drop_index("flag_idx").unwrap();
        storage.drop_index("payload_idx").unwrap();
    }

    /// レビュー指摘6番の再現条件(ページ内更新、`RecordId`は変わらない版):
    /// 対象の行が単独でページを占有しており、更新後の値がそのページの
    /// 残り容量に収まる(索引の制約さえ無ければ、`RecordId`を変えずに
    /// ページ内で書き換えられる)状況を作る。この場合でも、更新後の値が
    /// 索引に収まらなければ`storage_update`はエラーを返し、Heap・索引とも
    /// 更新前の内容のまま変化しない。
    #[test]
    fn storage_update_leaves_heap_and_every_index_consistent_when_the_new_value_does_not_fit_an_index_in_place() {
        let (path, mut storage, table_id) = items_storage("update-index-mismatch-in-place");
        let schema = flag_schema();
        let functions = FunctionRegistry::with_builtins();
        storage.create_index("flag_idx", "items", "flag", false).unwrap();
        storage.create_index("payload_idx", "items", "payload", false).unwrap();

        // このテーブルの唯一の行にする。ページには他の行が無いため、
        // `payload`をそのページの残り容量いっぱいまで書き換えても
        // (索引の制約さえ無ければ)`RecordId`を変えずに収まる。
        storage_insert(&mut storage, table_id, &schema, &functions, None, &[vec![expr("true"), expr("'alice'")]]).unwrap();
        let before = scan_all(&storage, table_id, &schema).unwrap();

        let huge_payload = oversized_payload();
        let assignments = flag_bound_assignments(&format!("payload = '{huge_payload}'"));
        let predicate = flag_bound_predicate("flag = TRUE");
        let err = storage_update(&mut storage, table_id, &schema, &functions, &assignments, Some(&predicate)).unwrap_err();
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));

        let after = scan_all(&storage, table_id, &schema).unwrap();
        assert_eq!(after, before, "更新前の行がそのまま残っているはず");
        assert_every_index_matches_seq_scan(&storage, table_id, &schema);

        std::fs::remove_file(&path).unwrap();
        storage.drop_index("flag_idx").unwrap();
        storage.drop_index("payload_idx").unwrap();
    }

    /// レビュー指摘6番の再現条件(`RecordId`移動版): 更新対象の行と同じページに
    /// 別の行(`filler`)を先に詰めておき、更新後の値がそのページに収まらず
    /// 別ページへ移動する状況を作る。移動が起きるかどうかに関わらず、更新後の
    /// 値が索引に収まらなければ`storage_update`は失敗し、Heap・索引とも
    /// 更新前の内容のまま変化しない。
    #[test]
    fn storage_update_leaves_heap_and_every_index_consistent_when_the_new_value_does_not_fit_an_index_and_the_row_would_move() {
        let (path, mut storage, table_id) = items_storage("update-index-mismatch-move");
        let schema = flag_schema();
        let functions = FunctionRegistry::with_builtins();
        storage.create_index("flag_idx", "items", "flag", false).unwrap();
        storage.create_index("payload_idx", "items", "payload", false).unwrap();

        // fillerでページの大半を埋めてから、更新対象の行を同じページへ入れる。
        // filler自身はpayload_idxに挿入できる大きさ(BTree::max_key_lenの
        // 上限以下)に収めつつ、更新後の値(oversized_payload)は、たとえ
        // 索引の制約が無かったとしてもこのページには収まらず、別ページへ
        // 移動せざるをえない大きさである。
        let filler_payload = "y".repeat(2_000);
        let rows = vec![
            vec![expr("true"), expr(&format!("'{filler_payload}'"))],
            vec![expr("false"), expr("'small'")],
        ];
        storage_insert(&mut storage, table_id, &schema, &functions, None, &rows).unwrap();
        let before = scan_all(&storage, table_id, &schema).unwrap();

        let huge_payload = oversized_payload();
        let assignments = flag_bound_assignments(&format!("payload = '{huge_payload}'"));
        let predicate = flag_bound_predicate("flag = FALSE");
        let err = storage_update(&mut storage, table_id, &schema, &functions, &assignments, Some(&predicate)).unwrap_err();
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));

        let after = scan_all(&storage, table_id, &schema).unwrap();
        assert_eq!(after, before, "更新前の全行がそのまま残っているはず(行の移動も起きていないはず)");
        assert_every_index_matches_seq_scan(&storage, table_id, &schema);

        std::fs::remove_file(&path).unwrap();
        storage.drop_index("flag_idx").unwrap();
        storage.drop_index("payload_idx").unwrap();
    }
}
