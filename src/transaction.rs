//! トランザクション境界と、メモリ上のUndo(第30章)。
//!
//! この章の`Database`は、`BEGIN`から`COMMIT`または`ROLLBACK`までの間に実行した
//! 複数の文を1つのトランザクションとして扱う。`ROLLBACK`が取り消すべき内容を
//! 覚えておく仕組みが、この章の[`UndoRecord`]と[`TransactionContext`]である。
//!
//! # メモリ上のUndoは、この章限りの実装である
//!
//! `INSERT`・`UPDATE`・`DELETE`が行った変更を、逆操作(挿入の逆は削除、削除の
//! 逆は再挿入、更新の逆は旧値への復元)としてプロセスのメモリ上に積んでおき、
//! `ROLLBACK`が届いたら逆順に適用する。この方式はディスクに何も書かないため、
//! `ROLLBACK`の後にプロセスごとクラッシュすれば、コミットしたつもりの変更も
//! 未コミットの変更も等しく消える。これは第33章で解決する話であり、この章では
//! 触れない。第33章では、この`UndoRecord`は「ページを書き換える前に、その
//! 変更を表すログをディスクへ先に書く」というWrite-Ahead Loggingの仕組みに
//! 置き換わる。具体的には、この章の`UndoRecord`が値として持っている「更新前の
//! 内容」が、WALの**Before Image**として一般化され、ログレコード自身が
//! ディスク上に永続化される。この章の実装は、その最終形に至る前の、書き直しを
//! 前提にした最初の実装である。
//!
//! # `RecordId`はDiskバックエンドだけが持つ
//!
//! Diskバックエンド(`Storage`)は、行の位置を[`RecordId`](第13章)で指す。
//! Memoryバックエンド(`MemStorage`)は`RecordId`という概念を持たず、行は
//! `Vec<Tuple>`の並びでしかない。[`UndoRecord`]の`rid`フィールドが
//! `Option<RecordId>`なのはこのためで、Diskバックエンドの記録では必ず
//! `Some`、Memoryバックエンドの記録では常に`None`になる。Memoryバックエンドの
//! 逆操作は、`RecordId`の代わりに`Tuple`の値そのものの一致で対象行を探す
//! (`apply_undo_memory`)。

use std::collections::HashMap;

use crate::error::DbResult;
use crate::ids::{RecordId, TableId, TransactionId};
use crate::storage::Storage;
use crate::storage_mem::MemStorage;
use crate::tuple_codec::encode_tuple;
use crate::types::Tuple;

/// トランザクションの状態。
///
/// `Active`から`Committed`または`Aborted`への遷移だけを許し、一度
/// `Committed`・`Aborted`になったトランザクションが`Active`へ戻ることはない
/// (`Database`はトランザクションが終わるたびに`tx`フィールドを`None`へ戻す
/// ため、`Committed`・`Aborted`の状態を持つ`TransactionContext`自体、実際には
/// `Database`の中に残らない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionState {
    /// `BEGIN`の直後、まだ`COMMIT`・`ROLLBACK`のどちらも実行していない状態。
    /// この状態の間だけ、`INSERT`・`UPDATE`・`DELETE`・`SELECT`を受け付ける。
    Active,
    /// `COMMIT`によって変更が確定した状態。
    Committed,
    /// `ROLLBACK`によって変更を取り消した状態、または`Active`中に文の実行が
    /// 失敗し、以後`ROLLBACK`しか受け付けなくなった状態(「Statement Error時の
    /// Abort」、本文を参照)。
    Aborted,
}

/// 1回のDML操作を打ち消すための逆操作1件。
///
/// `table_id`はどのテーブルに対する操作かを、`rid`はDiskバックエンドでの
/// 位置(モジュール冒頭を参照)を表す。
#[derive(Debug, Clone)]
pub enum UndoRecord {
    /// この行が挿入された。取り消すには削除する。
    Insert { table_id: TableId, tuple: Tuple, rid: Option<RecordId> },
    /// この行が削除された。取り消すには再挿入する。
    Delete { table_id: TableId, tuple: Tuple, rid: Option<RecordId> },
    /// この行が`old`から`new`へ更新された。取り消すには`new`を`old`へ戻す。
    ///
    /// Diskバックエンドでは、`Storage::update`が新しい値をページに収めきれず
    /// 別のページへ移動させることがある(第15章)。`old_rid`は更新前の位置、
    /// `new_rid`は更新後の位置であり、両方とも記録しておかないと逆操作の
    /// 対象を特定できない(`apply_undo_disk`のドキュメントを参照)。
    Update { table_id: TableId, old: Tuple, new: Tuple, old_rid: Option<RecordId>, new_rid: Option<RecordId> },
}

/// 1本のトランザクションが持つ状態。
///
/// `Database`は`Active`なトランザクションを高々1本しか持たない
/// (`BEGIN`の入れ子を許さない設計、本文「BEGINの入れ子をどう扱うか」を参照)。
pub(crate) struct TransactionContext {
    pub id: TransactionId,
    pub state: TransactionState,
    /// この本文で実行した順にUndoレコードを積んだ列。`ROLLBACK`はこれを
    /// 逆順(LIFO)に適用する。
    pub undo_log: Vec<UndoRecord>,
}

impl TransactionContext {
    pub fn new(id: TransactionId) -> Self {
        TransactionContext { id, state: TransactionState::Active, undo_log: Vec::new() }
    }
}

// `insert`・`update`・`delete`(`crate::executor`)が`undo: &mut Vec<UndoRecord>`
// という`pub fn`の引数にこの型を使うため、`UndoRecord`自体も`pub`にしてある
// (`pub(crate)`のままだと「非公開の型がpublicな関数のシグネチャに現れている」
// というlintに引っかかる)。`TransactionContext`はどの`pub fn`のシグネチャにも
// 現れないため、`pub(crate)`のままで良い。

/// Memoryバックエンド(`MemStorage`)に対して、`undo_log`を逆順に適用する。
///
/// 対象行は`Tuple`の値の一致(`Tuple::values()`の比較)で探す。同じ値を持つ
/// 行が複数あっても、`Insert`・`Update`の逆操作はどれか1件を取り消せば
/// 意味的には十分なため、最初に見つかった1件だけを対象にする。
pub(crate) fn apply_undo_memory(storage: &mut MemStorage, undo_log: Vec<UndoRecord>) {
    for record in undo_log.into_iter().rev() {
        match record {
            UndoRecord::Insert { table_id, tuple, .. } => {
                if let Some(table) = storage.table_mut(table_id)
                    && let Some(pos) = table.rows().iter().position(|t| t.values() == tuple.values())
                {
                    table.rows_mut().remove(pos);
                }
            }
            UndoRecord::Delete { table_id, tuple, .. } => {
                if let Some(table) = storage.table_mut(table_id) {
                    table.rows_mut().push(tuple);
                }
            }
            UndoRecord::Update { table_id, old, new, .. } => {
                if let Some(table) = storage.table_mut(table_id)
                    && let Some(pos) = table.rows().iter().position(|t| t.values() == new.values())
                {
                    table.rows_mut()[pos] = old;
                }
            }
        }
    }
}

/// Diskバックエンド(`Storage`)に対して、`undo_log`を逆順に適用する。
///
/// # `RecordId`の付け替え(`remap`)が要る理由
///
/// 同じ行を同じトランザクション内で複数回`UPDATE`すると、1回目の更新が
/// `RecordId`を`r1`から`r2`へ動かし、2回目の更新がさらに`r2`から`r3`へ
/// 動かす、ということが起こりうる(`Storage::update`がページに収まりきらない
/// 新しい値を書くたびに、別ページへ移動させるため)。各`UndoRecord::Update`は
/// それぞれの更新が起きた**時点の**`old_rid`・`new_rid`しか知らないので、
/// LIFO順に逆操作を適用していくと、2回目の更新の逆操作(`r3`→`r2`相当の
/// 書き戻し)が終わった直後に、1回目の更新の逆操作が「`r2`から書き戻す」と
/// 記録されたとおりに動こうとしても、2回目の逆操作が`r2`をさらに別の
/// `RecordId`へ動かしているかもしれない。`remap`は、逆操作の適用中に実際に
/// 起きた`RecordId`の付け替えを`old_rid → 実際の適用先`として覚えておき、
/// 次の(時系列でより古い)逆操作が参照する`rid`を、適用する直前に`remap`を
/// たどって現在の実際の位置へ解決する。ページ内で書き換えが収まり
/// `RecordId`が変わらなかった場合は、`old_rid == 実際の適用先`になるが、
/// この場合は`remap`へ何も追加しない。追加してしまうと`rid`が自分自身を
/// 指すエントリになり、`resolve`が無限ループする(実装時に実際に踏んだ
/// バグで、`cargo test`がハングして初めて気づいた)。
///
/// # 索引の整合性
///
/// `storage.update`・`storage.insert`・`storage.delete`(物理的な書き換え)の
/// 直後に、対応する`index_insert_row`・`index_delete_row`を呼び、Heapと
/// 索引がずれないようにする。呼び出しの順序(物理的な書き換えを先に行い、
/// 索引の更新を後で行う)は、`executor::storage_insert`等の順序をそのまま
/// 踏襲した。逆操作はすでに一度検証を通過した値へ戻すだけなので、
/// `check_indexes_accept_row`のような事前検証はここでは行わない。
///
/// この適用の途中で(たとえば`BufferPool`のI/Oエラーによって)失敗すると、
/// トランザクションの一部だけが取り消された中途半端な状態が残る。この章は
/// この失敗経路を閉じない。第15章・第20章がすでに明文化した「正直なギャップ」
/// と同じ割り切りであり、閉じるには第33章のWrite-Ahead Loggingと第34章の
/// Crash Recoveryが要る。
pub(crate) fn apply_undo_disk(storage: &mut Storage, undo_log: Vec<UndoRecord>) -> DbResult<()> {
    let mut remap: HashMap<RecordId, RecordId> = HashMap::new();

    fn resolve(remap: &HashMap<RecordId, RecordId>, rid: RecordId) -> RecordId {
        let mut current = rid;
        while let Some(&next) = remap.get(&current) {
            // `next == current`(ページ内で収まり`RecordId`が変わらなかった
            // 更新)は付け替えが無かったことを意味する。ここで止めないと、
            // 自己参照のエントリを無限にたどり続けてしまう。
            if next == current {
                break;
            }
            current = next;
        }
        current
    }

    for record in undo_log.into_iter().rev() {
        match record {
            UndoRecord::Insert { table_id, tuple, rid } => {
                let rid = rid.expect("Diskバックエンドの UndoRecord::Insert は必ずridを持つ");
                let actual = resolve(&remap, rid);
                storage.delete(table_id, actual)?;
                storage.index_delete_row(table_id, &tuple, actual)?;
            }
            UndoRecord::Delete { table_id, tuple, rid } => {
                let rid = rid.expect("Diskバックエンドの UndoRecord::Delete は必ずridを持つ");
                let schema = storage
                    .tables()
                    .find(|info| info.id == table_id)
                    .expect("Undoの対象テーブルはDROP TABLEされていない前提")
                    .schema
                    .clone();
                let bytes = encode_tuple(&schema, &tuple);
                let new_rid = storage.insert(table_id, &bytes)?;
                storage.index_insert_row(table_id, &tuple, new_rid)?;
                if new_rid != rid {
                    remap.insert(rid, new_rid);
                }
            }
            UndoRecord::Update { table_id, old, new, old_rid, new_rid } => {
                let old_rid = old_rid.expect("Diskバックエンドの UndoRecord::Update は必ずold_ridを持つ");
                let new_rid = new_rid.expect("Diskバックエンドの UndoRecord::Update は必ずnew_ridを持つ");
                let actual = resolve(&remap, new_rid);
                let schema = storage
                    .tables()
                    .find(|info| info.id == table_id)
                    .expect("Undoの対象テーブルはDROP TABLEされていない前提")
                    .schema
                    .clone();
                let old_bytes = encode_tuple(&schema, &old);
                let result_rid = storage.update(table_id, actual, &old_bytes)?.unwrap_or(actual);
                storage.index_delete_row(table_id, &new, actual)?;
                storage.index_insert_row(table_id, &old, result_rid)?;
                if old_rid != result_rid {
                    remap.insert(old_rid, result_rid);
                }
            }
        }
    }
    Ok(())
}
