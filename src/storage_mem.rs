//! テーブルの行を実際に保持する、プロセスのメモリ上だけの入れ物。
//!
//! `Catalog`(第9章)がテーブルの「定義」(名前・列構成)を持つのに対し、この
//! モジュールはテーブルの「中身」(行の集まり)を持つ。両者を`TableId`で結び
//! つけることで、`Database`はテーブル名を1回だけ`Catalog`で引けば、定義と
//! 中身の両方にたどり着ける。
//!
//! `MemTable`は`Vec<Tuple>`を並び順そのままに保持するだけの構造で、行の
//! 検索・削除はすべて先頭から順に走査する。索引もRIDも持たないため、
//! `UPDATE`・`DELETE`は行の位置(`Vec`の添字)で直接書き換える。この単純さは、
//! ディスクへの永続化(第2部)やB+Tree索引(第3部)が入るまでの、意図した
//! 割り切りである。

use std::collections::HashMap;

use crate::ids::TableId;
use crate::types::Tuple;

/// 1テーブル分の行の集まり。
#[derive(Debug, Default)]
pub struct MemTable {
    rows: Vec<Tuple>,
}

impl MemTable {
    /// 行が1件も無い空のテーブルを作る。
    pub fn new() -> Self {
        MemTable { rows: Vec::new() }
    }

    /// 現在保持している行を返す。
    pub fn rows(&self) -> &[Tuple] {
        &self.rows
    }

    /// 行を書き換えるための可変参照を返す。
    ///
    /// `Insert`・`Update`・`Delete`の各演算子(`executor`モジュール)は、
    /// この`Vec`を直接操作して行を追加・置換・削除する。
    pub fn rows_mut(&mut self) -> &mut Vec<Tuple> {
        &mut self.rows
    }
}

/// カタログに登録された全テーブルの`MemTable`を、`TableId`から引ける対応表。
#[derive(Debug, Default)]
pub struct MemStorage {
    tables: HashMap<TableId, MemTable>,
}

impl MemStorage {
    /// テーブルが1つも無い空のストレージを作る。
    pub fn new() -> Self {
        MemStorage {
            tables: HashMap::new(),
        }
    }

    /// `CREATE TABLE`に対応して、空の`MemTable`を1つ作る。
    ///
    /// `id`がすでに登録済みの場合は、既存の`MemTable`(とその行)を空のものに
    /// 差し替える。`Catalog`が同名テーブルの重複作成をすでに拒否しているため、
    /// `Database::execute`から呼ばれる限りこの上書きが起きることはない。
    pub fn create_table(&mut self, id: TableId) {
        self.tables.insert(id, MemTable::new());
    }

    /// `DROP TABLE`に対応して、`MemTable`とその行をまとめて破棄する。
    pub fn drop_table(&mut self, id: TableId) {
        self.tables.remove(&id);
    }

    /// `TableId`から`MemTable`への参照を引く。
    pub fn table(&self, id: TableId) -> Option<&MemTable> {
        self.tables.get(&id)
    }

    /// `TableId`から`MemTable`への可変参照を引く。
    pub fn table_mut(&mut self, id: TableId) -> Option<&mut MemTable> {
        self.tables.get_mut(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Column, DataType, Schema, Value};

    fn one_row_tuple() -> Tuple {
        let schema = Schema::new(vec![Column::new("id", DataType::BigInt, false)]);
        Tuple::new(&schema, vec![Value::BigInt(1)]).unwrap()
    }

    #[test]
    fn new_table_has_no_rows() {
        let table = MemTable::new();
        assert!(table.rows().is_empty());
    }

    #[test]
    fn rows_mut_can_append_a_row() {
        let mut table = MemTable::new();
        table.rows_mut().push(one_row_tuple());
        assert_eq!(table.rows().len(), 1);
    }

    #[test]
    fn create_table_registers_an_empty_table() {
        let mut storage = MemStorage::new();
        storage.create_table(TableId(0));
        assert!(storage.table(TableId(0)).unwrap().rows().is_empty());
    }

    #[test]
    fn drop_table_removes_the_table_and_its_rows() {
        let mut storage = MemStorage::new();
        storage.create_table(TableId(0));
        storage
            .table_mut(TableId(0))
            .unwrap()
            .rows_mut()
            .push(one_row_tuple());

        storage.drop_table(TableId(0));
        assert!(storage.table(TableId(0)).is_none());
    }

    #[test]
    fn table_returns_none_for_unregistered_id() {
        let storage = MemStorage::new();
        assert!(storage.table(TableId(0)).is_none());
    }
}
