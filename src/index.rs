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
    /// この索引が、`PRIMARY KEY`・`UNIQUE`列の制約を支えるために
    /// `Database::execute_create_table`が自動生成した索引かどうか
    /// (第3部2巡目レビュー対応)。`true`なら`Storage::drop_index`(SQLの
    /// `DROP INDEX`が呼ぶ入口)は`DbError::CannotDropConstraintIndex`で
    /// 削除を拒否する(テーブルごと削除する`Storage::drop_table`の内部経路は
    /// この制限を受けない)。
    ///
    /// `primary_key`との違い: `primary_key`は`PRIMARY KEY`と`UNIQUE`の
    /// どちらの制約由来かを区別するためのフラグで、`UNIQUE`列由来の制約索引は
    /// `is_constraint = true`かつ`primary_key = false`になる。この場合、
    /// `unique`列だけを見て`CREATE UNIQUE INDEX`(SQL、`is_constraint =
    /// false`)由来の索引と区別することはできないため、この2つは独立した
    /// フィールドとして持つ。
    pub is_constraint: bool,
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
/// 場合は`DbError::CorruptCatalog`を返す。第24章から、
/// `Database::execute_create_table`が`PRIMARY KEY`・`UNIQUE`列に対して必ず
/// `UNIQUE`索引を自動生成するため、この状況は通常起こらない不変条件である。
///
/// 第3部レビュー対応: 以前はこの不変条件が崩れた場合に`unreachable!`で
/// プロセスごと終了させていた。しかし`Database::execute_create_table`が
/// テーブルを永続化した**後**に制約索引の作成へ失敗しうる経路が存在した
/// ため(既存の手動索引名が自動生成名と衝突した場合など、詳しくは
/// `Database::execute_create_table`を参照)、この不変条件は「テーブルは
/// 存在するのに対応する索引が無い」という形でカタログの破損として実際に
/// 観測されうる。1件のクエリの異常な入力・状態が原因でサーバープロセス
/// 全体を巻き添えにする`panic`ではなく、その1件の`INSERT`・`UPDATE`だけを
/// 失敗させる`DbError`として返す。
pub fn check_uniqueness_with_index(
    storage: &Storage,
    table_id: TableId,
    schema: &Schema,
    candidates: &[Tuple],
    exclude: &HashSet<RecordId>,
) -> crate::error::DbResult<()> {
    for (column_index, column) in schema.unique_constrained_columns() {
        let index = storage.unique_index_for_column(table_id, column_index).ok_or_else(|| {
            DbError::CorruptCatalog(format!(
                "PRIMARY KEY・UNIQUE列'{}'に対応するUNIQUE索引が見つかりません(CREATE TABLEが\
                 自動生成するはずの索引が欠落しています)",
                column.name
            ))
        })?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use crate::types::Column;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-index-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        );
        path.push(unique);
        path
    }

    /// 第3部レビュー対応の回帰テスト: `PRIMARY KEY`列に対応するはずの
    /// `UNIQUE`索引が(通常のSQL経路では起こらないはずだが)カタログから
    /// 欠落している状態で`check_uniqueness_with_index`を呼ぶと、
    /// プロセスを巻き添えにする`panic`(旧`unreachable!`)ではなく
    /// `DbError::CorruptCatalog`を返す。
    ///
    /// この状態は、`Database::execute_create_table`が
    /// 索引名の衝突を事前検査するようになった第3部レビュー対応後は、通常の
    /// `CREATE TABLE`経由では作れなくなった。ここでは`Storage`を直接操作し、
    /// 一度自動生成された制約索引を`drop_index`で取り除くことで、その
    /// 「起こらないはずの」状態を意図的に再現する。
    #[test]
    fn check_uniqueness_with_index_reports_a_corrupt_catalog_instead_of_panicking_when_the_constraint_index_is_missing() {
        let path = temp_path("missing-constraint-index");
        let mut storage = Storage::create(&path).unwrap();
        let schema = Schema::new(vec![Column::new("id", DataType::BigInt, false).with_primary_key(), Column::new("name", DataType::Text, true)]);
        let table_id = storage
            .create_table_with_constraint_indexes("users", schema.clone(), &[("id".to_string(), true)])
            .unwrap();

        // 通常のSQL経路ではこの索引は制約索引(`IndexInfo::is_constraint`)
        // なので`Storage::drop_index`(SQLのDROP INDEXが呼ぶ入口)が
        // `DbError::CannotDropConstraintIndex`で拒否する。ここではテストの
        // ために、その制限を受けない内部専用の`drop_index_impl`で直接
        // 取り除き、「テーブルはPRIMARY KEYを持つと申告しているのに、
        // 対応する索引が無い」という不変条件違反を作る。
        storage.drop_index_impl("users_id_idx").unwrap();

        let candidate = Tuple::new(&schema, vec![crate::types::Value::BigInt(1), crate::types::Value::Text("alice".to_string())]).unwrap();
        let err = check_uniqueness_with_index(&storage, table_id, &schema, &[candidate], &HashSet::new()).unwrap_err();
        assert!(matches!(err, DbError::CorruptCatalog(message) if message.contains("id")));

        std::fs::remove_file(&path).unwrap();
    }
}
