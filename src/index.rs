//! `CREATE INDEX`が作る2次索引のメタデータと、索引を使った一意性検査(第24章)。
//!
//! `crate::btree::BTree`はキーと`RecordId`の対応だけを持つ、名前もテーブルも
//! 知らないデータ構造だった(第23章)。この章の`crate::storage::Storage`は、
//! 索引に名前をつけ、どのテーブルのどの列を索引化したものかを覚えておく
//! 必要がある。[`IndexInfo`]がその対応を表す値であり、`Storage`はこれを
//! `crate::btree::BTree`本体(索引の実データ)と組にして保持する。
//!
//! このモジュールにはもう1つ、`crate::constraints::check_uniqueness`
//! (第20章の走査ベース一意性検査)の索引版である[`check_uniqueness_with_index`]
//! を置く。第20章の検査は`others`(比較相手の行)をすべて読んで`O(n)`だったが、
//! こちらは`PRIMARY KEY`・`UNIQUE`列に対応する`UNIQUE`索引へ`BTree::lookup`
//! (`O(log n)`)で問い合わせるだけで済む。

use std::collections::HashSet;

use crate::error::DbError;
use crate::ids::{RecordId, TableId};
use crate::storage::Storage;
use crate::types::{Column, DataType, Schema, Tuple};

/// カタログに登録された1つの索引の定義。
///
/// `crate::btree::BTree`自身(索引の実データ、`RecordId`の集まり)は
/// `crate::storage::Storage`が別途保持する。この型が持つのは、その`BTree`が
/// 「どのテーブルの、どの列に対応するか」というメタデータだけである。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexInfo {
    /// 索引名。`CREATE INDEX`・`DROP INDEX`が指定する識別子。
    pub name: String,
    /// 索引化されたテーブル。
    pub table_id: TableId,
    /// 索引化された列の、そのテーブルの`Schema`上の索引(添字)。
    pub column_index: usize,
    /// 索引化された列名(表示用。`column_index`と`Schema`があれば引き直せるが、
    /// エラーメッセージのために持たせてある)。
    pub column_name: String,
    /// `UNIQUE`索引かどうか。`true`なら対応する`BTree`も`unique`フラグを
    /// 立てて作られており、`insert`が重複を拒否する。
    pub unique: bool,
    /// 索引化された列が`PRIMARY KEY`かどうか。`crate::storage::Storage`が
    /// `crate::btree::DbError::BTreeUniqueViolation`を、第20章の
    /// `DbError::PrimaryKeyViolation`(この値が`true`のとき)・
    /// `DbError::UniqueViolation`(`false`のとき)のどちらへ翻訳するかを
    /// 決める。`CREATE INDEX`(SQL構文、常に`false`)と、`PRIMARY KEY`・
    /// `UNIQUE`列に自動生成される索引(`Database::execute_create_table`)を
    /// 区別する。
    pub primary_key: bool,
    /// 索引キーの型(索引化された列の`DataType`と同じ)。
    pub key_type: DataType,
}

/// `column`が`PRIMARY KEY`か`UNIQUE`かに応じて、`value`の重複を報告する
/// `DbError`を作る。`crate::constraints::violation_for`と同じ判断基準
/// (両方指定されていれば`PRIMARY KEY`のエラーを優先する)を、索引経由の
/// 検査からも使えるようにしたもの。
fn violation_for(column: &Column, value: &crate::types::Value) -> DbError {
    let message_column = column.name.clone();
    let message_value = value.to_string();
    if column.primary_key {
        DbError::PrimaryKeyViolation { column: message_column, value: message_value }
    } else {
        DbError::UniqueViolation { column: message_column, value: message_value }
    }
}

/// `candidates`(これから書き込もうとしている行の新しい値)が、`table_id`の
/// `PRIMARY KEY`・`UNIQUE`列に対応する索引と重複しないことを検査する。
///
/// `crate::constraints::check_uniqueness`の「`candidates`と`others`(比較相手)
/// の重複を検査する」部分を、`others`をすべて読む代わりに`UNIQUE`索引への
/// `lookup`(`O(log n)`)で行う索引版である。`candidates`同士の重複
/// (同じ`INSERT`文の中の行同士など)はこの関数では検出しない。呼び出し側
/// (`crate::executor::storage_insert`・`storage_update`)は、この関数に続けて
/// `constraints::check_uniqueness(schema, std::iter::empty(), candidates)`を
/// 呼び、`candidates`同士の重複だけを別途確認する。
///
/// `exclude`に含まれる`RecordId`は、索引上でヒットしても違反として扱わない。
/// `INSERT`では空集合を渡す。`UPDATE`では、これから書き換えようとしている
/// 行(自分自身を含む、同じ`UPDATE`文で書き換えられる全行)の**更新前**の
/// `RecordId`を渡す。そうしないと、値を変えない更新(`UPDATE t SET id = id`)や、
/// 同じ文の中の2行が値を交換するような更新まで、自分自身との衝突として
/// 誤検出してしまう(`crate::executor::update`のコメントを参照)。
///
/// `table_id`の`PRIMARY KEY`・`UNIQUE`列に対応する`UNIQUE`索引が見つからない
/// 場合は`panic`する。第24章から、`Database::execute_create_table`が
/// `PRIMARY KEY`・`UNIQUE`列に対して必ず`UNIQUE`索引を自動生成するため、
/// この状況はディスクバックエンドでは起こらない不変条件である。
pub fn check_uniqueness_with_index(
    storage: &Storage,
    table_id: TableId,
    schema: &Schema,
    candidates: &[Tuple],
    exclude: &HashSet<RecordId>,
) -> crate::error::DbResult<()> {
    for (column_index, column) in schema.unique_constrained_columns() {
        let index = storage.unique_index_for_column(table_id, column_index).unwrap_or_else(|| {
            unreachable!(
                "PRIMARY KEY・UNIQUE列'{}'には第24章からCREATE TABLEが自動でUNIQUE索引を \
                 作るため、対応する索引が必ず見つかるはず",
                column.name
            )
        });
        for candidate in candidates {
            let value = candidate.get(column_index).expect("candidateはschemaと同じ列数を持つ");
            if value.is_null() {
                continue;
            }
            let matches = index.lookup(value)?;
            if matches.iter().any(|rid| !exclude.contains(rid)) {
                return Err(violation_for(column, value));
            }
        }
    }
    Ok(())
}
