//! テーブル定義の唯一の情報源(インメモリカタログ)。
//!
//! `Catalog`は、テーブル名から`TableId`と`Schema`を引ける対応表を持つ。
//! `CREATE TABLE`はここに新しいテーブルを登録し、`DROP TABLE`はここから削除する。
//! `Database::execute`は自分でテーブル定義を保持せず、この`Catalog`だけを信じる。
//! ディスクへの永続化は第15章(永続カタログと空き領域管理)で追加する。この章の
//! `Catalog`はプロセスが終了すれば消える、インメモリだけの実装にとどめる。

use std::collections::HashMap;

use crate::error::{DbError, DbResult};
use crate::ids::TableId;
use crate::types::Schema;

/// カタログに登録された1テーブルの情報。
#[derive(Debug, Clone, PartialEq)]
pub struct TableInfo {
    /// このテーブルを指す識別子。
    pub id: TableId,
    /// テーブル名。
    pub name: String,
    /// 列構成。
    pub schema: Schema,
}

/// テーブル定義の対応表。
///
/// テーブル名の比較は大文字小文字を区別する(`users`と`Users`は別テーブルとして
/// 登録できる)。第6章のLexerは識別子の大文字小文字を畳み込まずに保持する方針を
/// 採ったが、その畳み込みをするかどうかの判断自体は、識別子をカタログに登録する
/// このモジュールへ持ち越されていた。ここでは畳み込まない(=区別する)と決める。
/// Lexerが元のテキストを一切変えずに渡してくる以上、カタログの側だけがこっそり
/// 大文字・小文字を揃えてしまうと、利用者から見て「なぜここだけ特別扱いなのか」が
/// 説明しづらくなる。引用符付き識別子(大文字小文字を保持したまま扱う識別子)を
/// 将来追加する場合も、この区別する方針のほうが素直に拡張できる。
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, TableInfo>,
    next_table_id: u64,
}

impl Catalog {
    /// テーブルが1つも登録されていない空のカタログを作る。
    pub fn new() -> Self {
        Catalog {
            tables: HashMap::new(),
            next_table_id: 0,
        }
    }

    /// 新しいテーブルを登録する。
    ///
    /// 同名のテーブルがすでに存在する場合は`DbError::DuplicateTable`を返す。
    /// 成功した場合、割り当てられた`TableId`を返す。`TableId`は単調増加で払い出され、
    /// `DROP TABLE`で番号が空いても再利用しない。
    pub fn create_table(&mut self, name: &str, schema: Schema) -> DbResult<TableId> {
        if self.tables.contains_key(name) {
            return Err(DbError::DuplicateTable(name.to_string()));
        }

        let id = TableId(self.next_table_id);
        self.next_table_id += 1;
        self.tables.insert(
            name.to_string(),
            TableInfo {
                id,
                name: name.to_string(),
                schema,
            },
        );
        Ok(id)
    }

    /// テーブルを削除する。
    ///
    /// 指定した名前のテーブルが存在しない場合は`DbError::TableNotFound`を返す。
    /// 成功した場合、削除したテーブルの`TableId`を返す。
    pub fn drop_table(&mut self, name: &str) -> DbResult<TableId> {
        self.tables
            .remove(name)
            .map(|info| info.id)
            .ok_or_else(|| DbError::TableNotFound(name.to_string()))
    }

    /// テーブル名から`TableInfo`を引く。見つからなければ`None`を返す。
    pub fn table(&self, name: &str) -> Option<&TableInfo> {
        self.tables.get(name)
    }

    /// 登録されている全テーブルの`TableInfo`を返す(第27章、`ANALYZE`が
    /// テーブル名を省略した場合に使う)。順序は保証しない。
    pub fn tables(&self) -> impl Iterator<Item = &TableInfo> {
        self.tables.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Column, DataType};

    fn users_schema() -> Schema {
        Schema::new(vec![Column::new("id", DataType::BigInt, false)])
    }

    #[test]
    fn create_table_registers_a_new_table() {
        let mut catalog = Catalog::new();
        let id = catalog.create_table("users", users_schema()).unwrap();
        let info = catalog.table("users").unwrap();
        assert_eq!(info.id, id);
        assert_eq!(info.name, "users");
        assert_eq!(info.schema, users_schema());
    }

    #[test]
    fn create_table_assigns_increasing_ids() {
        let mut catalog = Catalog::new();
        let first = catalog.create_table("t1", users_schema()).unwrap();
        let second = catalog.create_table("t2", users_schema()).unwrap();
        assert_eq!(first, TableId(0));
        assert_eq!(second, TableId(1));
    }

    #[test]
    fn create_table_rejects_duplicate_name() {
        let mut catalog = Catalog::new();
        catalog.create_table("users", users_schema()).unwrap();
        let result = catalog.create_table("users", users_schema());
        assert!(matches!(result, Err(DbError::DuplicateTable(name)) if name == "users"));
    }

    #[test]
    fn table_names_are_case_sensitive() {
        let mut catalog = Catalog::new();
        catalog.create_table("users", users_schema()).unwrap();
        // `Users`は`users`とは別名として登録できる。
        catalog.create_table("Users", users_schema()).unwrap();
        assert!(catalog.table("users").is_some());
        assert!(catalog.table("Users").is_some());
        assert!(catalog.table("USERS").is_none());
    }

    #[test]
    fn drop_table_removes_a_registered_table() {
        let mut catalog = Catalog::new();
        let id = catalog.create_table("users", users_schema()).unwrap();
        let dropped_id = catalog.drop_table("users").unwrap();
        assert_eq!(dropped_id, id);
        assert!(catalog.table("users").is_none());
    }

    #[test]
    fn drop_table_rejects_unknown_name() {
        let mut catalog = Catalog::new();
        let result = catalog.drop_table("users");
        assert!(matches!(result, Err(DbError::TableNotFound(name)) if name == "users"));
    }

    #[test]
    fn table_id_is_not_reused_after_drop() {
        let mut catalog = Catalog::new();
        catalog.create_table("users", users_schema()).unwrap();
        catalog.drop_table("users").unwrap();
        let id = catalog.create_table("users", users_schema()).unwrap();
        // 直前に削除した`users`は`TableId(0)`だったが、再作成では`TableId(0)`を
        // 使い回さず、常に単調増加する新しい番号を払い出す。
        assert_eq!(id, TableId(1));
    }

    #[test]
    fn table_returns_none_for_unregistered_name() {
        let catalog = Catalog::new();
        assert!(catalog.table("does_not_exist").is_none());
    }
}
