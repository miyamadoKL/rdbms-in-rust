//! トランザクション境界と、Memoryバックエンド向けのメモリ上Undo(第30章)。
//!
//! この章の`Database`は、`BEGIN`から`COMMIT`または`ROLLBACK`までの間に実行した
//! 複数の文を1つのトランザクションとして扱う。`ROLLBACK`が取り消すべき内容を
//! 覚えておく仕組みが、この章の[`UndoRecord`]と[`TransactionContext`]である。
//!
//! # 第33章での書き直し: `UndoRecord`はMemoryバックエンド専用になった
//!
//! この章を書いた時点([`UndoRecord`]が`Insert`・`Update`・`Delete`という
//! DMLの逆操作を1件ずつ持つ設計)では、Diskバックエンドもこの`UndoRecord`を
//! `RecordId`つきで記録し、`ROLLBACK`のたびに逆順適用していた
//! (`apply_undo_disk`)。この方式はプロセスのメモリ上にしか変更を残さないため、
//! `ROLLBACK`の直後にプロセスがクラッシュすれば実害は無いが、`Active`な
//! トランザクションの途中でクラッシュすれば、それまでの変更が`backend`に
//! どこまで反映されていたかを知る手段が無く、Undoの記録ごと失われた。
//!
//! 第33章は、この欠落をWrite-Ahead Loggingで埋めた。Diskバックエンドの
//! `INSERT`・`UPDATE`・`DELETE`は、もう`UndoRecord`を積まない。代わりに
//! `crate::wal::WalWriter`へBefore/After Imageを持つログレコードを直接書き、
//! `ROLLBACK`はその`prev_lsn`連鎖を逆順にたどって取り消す
//! ([`apply_wal_undo_disk`])。`UndoRecord`という型自体は、ディスクに何も
//! 書かないMemoryバックエンド(`MemStorage`、`RecordId`という概念を持たず、
//! 行は`Vec<Tuple>`の並びでしかない)向けの、この章由来の実装として残した。
//! Memoryバックエンドはそもそも永続化しないデータベースであり、WALを持ち込む
//! 動機(クラッシュをまたいだ復元)自体が無いため、この章の設計をそのまま
//! 維持するという線引きを選んだ。

use std::collections::HashMap;

use crate::ast::IsolationLevel;
use crate::error::DbResult;
use crate::ids::{Lsn, RecordId, TableId, TransactionId};
use crate::storage::Storage;
use crate::storage_mem::MemStorage;
use crate::tuple_codec::decode_tuple;
use crate::types::Tuple;
use crate::wal::LogRecordType;

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

/// 1回のDML操作を打ち消すための逆操作1件(Memoryバックエンド専用、
/// モジュール冒頭「第33章での書き直し」を参照)。
#[derive(Debug, Clone)]
pub enum UndoRecord {
    /// この行が挿入された。取り消すには削除する。
    Insert { table_id: TableId, tuple: Tuple },
    /// この行が削除された。取り消すには再挿入する。
    Delete { table_id: TableId, tuple: Tuple },
    /// この行が`old`から`new`へ更新された。取り消すには`new`を`old`へ戻す。
    Update { table_id: TableId, old: Tuple, new: Tuple },
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
    /// このトランザクションが読み取りロックをどう扱うか(第32章)。
    /// `BEGIN ISOLATION LEVEL ...`で指定しなければ`RepeatableRead`が既定
    /// (`crate::database::execute_begin`のドキュメントを参照)。
    pub isolation_level: IsolationLevel,
    /// `true`なら、この`state`が`Aborted`になった理由はデッドロック検出の
    /// Victim Selection(第32章)である。`false`なら、Statement Error時の
    /// Abort(第30章)、または明示的な`ROLLBACK`によるものである。この
    /// フラグは、`Database`がこのトランザクションへの以後の操作に
    /// `DbError::TransactionAborted`(通常のAbort)と
    /// `DbError::DeadlockDetected`(デッドロックのVictim)のどちらを返すかを
    /// 決める(`crate::database`の該当箇所を参照)。
    pub victim_of_deadlock: bool,
    /// Diskバックエンド(第33章)で、このトランザクションが直近に書いた
    /// WALレコードの`Lsn`。`None`は「まだ1件も書いていない」(`BEGIN`直後、
    /// またはこのトランザクションが一度も書き込みを行っていない)ことを表す。
    /// 次に書くレコードの`prev_lsn`に使うと同時に、`ROLLBACK`が
    /// `crate::transaction::apply_wal_undo_disk`で逆操作をたどる起点にもなる。
    /// Memoryバックエンドでは常に`None`のまま使われない
    /// (`undo_log`がMemoryバックエンド専用であるのと対称的に、こちらは
    /// Diskバックエンド専用のフィールドである)。
    pub wal_last_lsn: Option<Lsn>,
}

impl TransactionContext {
    pub fn new(id: TransactionId, isolation_level: IsolationLevel) -> Self {
        TransactionContext {
            id,
            state: TransactionState::Active,
            undo_log: Vec::new(),
            isolation_level,
            victim_of_deadlock: false,
            wal_last_lsn: None,
        }
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
            UndoRecord::Insert { table_id, tuple } => {
                if let Some(table) = storage.table_mut(table_id)
                    && let Some(pos) = table.rows().iter().position(|t| t.values() == tuple.values())
                {
                    table.rows_mut().remove(pos);
                }
            }
            UndoRecord::Delete { table_id, tuple } => {
                if let Some(table) = storage.table_mut(table_id) {
                    table.rows_mut().push(tuple);
                }
            }
            UndoRecord::Update { table_id, old, new } => {
                if let Some(table) = storage.table_mut(table_id)
                    && let Some(pos) = table.rows().iter().position(|t| t.values() == new.values())
                {
                    table.rows_mut()[pos] = old;
                }
            }
        }
    }
}

/// Diskバックエンドに対して、WALの`prev_lsn`連鎖を逆順にたどりながら
/// Undoを適用する(第33章)。
///
/// `last_lsn`は`TransactionContext::wal_last_lsn`(このトランザクションが
/// 直近に書いたレコード)。そこから`prev_lsn`を`Begin`レコードに行き着くまで
/// たどり、たどった順(=このトランザクションが実際に書き込んだ順とちょうど
/// 逆順、LIFO)にInsert・Update・Deleteの逆操作を適用する。`Begin`レコード
/// 自身は逆操作を持たないため、そこに行き着いたら止まる。
///
/// # `RecordId`の付け替え(`remap`)が要る理由
///
/// 同じ行を同じトランザクション内で複数回`UPDATE`すると、1回目の更新が
/// `RecordId`を`r1`から`r2`へ動かし、2回目の更新がさらに`r2`から`r3`へ
/// 動かす、ということが起こりうる(`Storage::update`がページに収まりきらない
/// 新しい値を書くたびに、別ページへ移動させるため)。各`Update`レコードは
/// それぞれの更新が起きた**時点の**`old_rid`・`rid`(更新後の位置)しか
/// 知らないので、LIFO順に逆操作を適用していくと、2回目の更新の逆操作
/// (`r3`→`r2`相当の書き戻し)が終わった直後に、1回目の更新の逆操作が
/// 「`r2`から書き戻す」と記録されたとおりに動こうとしても、2回目の逆操作が
/// `r2`をさらに別の`RecordId`へ動かしているかもしれない。`remap`は、
/// 逆操作の適用中に実際に起きた`RecordId`の付け替えを`old_rid → 実際の
/// 適用先`として覚えておき、次の(時系列でより古い)逆操作が参照する
/// `rid`を、適用する直前に`remap`をたどって現在の実際の位置へ解決する。
/// ページ内で書き換えが収まり`RecordId`が変わらなかった場合は、
/// `old_rid == 実際の適用先`になるが、この場合は`remap`へ何も追加しない。
/// 追加してしまうと`rid`が自分自身を指すエントリになり、`resolve`が
/// 無限ループする(第30章の実装時に実際に踏んだバグで、`cargo test`が
/// ハングして初めて気づいた。この章もそのままの`remap`・`resolve`を使う)。
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
/// この失敗経路を閉じない。第15章・第20章がすでに明文化した「正直な
/// ギャップ」と同じ割り切りであり、閉じるには第34章のCrash Recoveryが要る。
pub(crate) fn apply_wal_undo_disk(
    storage: &mut Storage,
    last_lsn: Option<Lsn>,
) -> DbResult<()> {
    // Undo対象のレコードをすべて先に集めてから`storage`を書き換える。
    // `WalWriter`のロックを`storage`の書き換えと同時に握り続けないための
    // 順序であり、意味的な違いは無い(`crate::wal::WalWriter::record`は
    // 参照を返すだけで、これまでに`append`したレコードは書き換わらない)。
    let records = {
        let wal = storage.wal().lock().unwrap_or_else(|p| p.into_inner());
        let mut records = Vec::new();
        let mut current = last_lsn;
        while let Some(lsn) = current {
            let record = wal
                .record(lsn)
                .expect("wal_last_lsn・prev_lsnは常にWalWriterへ記録済みのLsnを指す")
                .clone();
            if record.record_type == LogRecordType::Begin {
                break;
            }
            current = record.prev_lsn;
            records.push(record);
        }
        records
    };

    let mut remap: HashMap<RecordId, RecordId> = HashMap::new();

    fn resolve(remap: &HashMap<RecordId, RecordId>, rid: RecordId) -> RecordId {
        let mut current = rid;
        while let Some(&next) = remap.get(&current) {
            if next == current {
                break;
            }
            current = next;
        }
        current
    }

    for record in records {
        let table_id = record.table_id.expect("Insert/Update/DeleteレコードのWALは必ずtable_idを持つ");
        let schema = storage
            .tables()
            .find(|info| info.id == table_id)
            .expect("Undoの対象テーブルはDROP TABLEされていない前提")
            .schema
            .clone();

        match record.record_type {
            LogRecordType::Insert => {
                let rid = record.rid.expect("InsertレコードのWALは必ずridを持つ");
                let after = record.after_image.expect("InsertレコードのWALは必ずafter_imageを持つ");
                let tuple = decode_tuple(&schema, &after)?;
                let actual = resolve(&remap, rid);
                storage.delete(table_id, actual)?;
                storage.index_delete_row(table_id, &tuple, actual)?;
            }
            LogRecordType::Delete => {
                let rid = record.rid.expect("DeleteレコードのWALは必ずridを持つ");
                let before = record.before_image.expect("DeleteレコードのWALは必ずbefore_imageを持つ");
                let tuple = decode_tuple(&schema, &before)?;
                let new_rid = storage.insert(table_id, &before)?;
                storage.index_insert_row(table_id, &tuple, new_rid)?;
                if new_rid != rid {
                    remap.insert(rid, new_rid);
                }
            }
            LogRecordType::Update => {
                let new_rid = record.rid.expect("UpdateレコードのWALは必ずrid(更新後の位置)を持つ");
                let old_rid = record.old_rid.expect("UpdateレコードのWALは必ずold_rid(更新前の位置)を持つ");
                let before = record.before_image.expect("UpdateレコードのWALは必ずbefore_imageを持つ");
                let after = record.after_image.expect("UpdateレコードのWALは必ずafter_imageを持つ");
                let old_tuple = decode_tuple(&schema, &before)?;
                let new_tuple = decode_tuple(&schema, &after)?;

                let actual = resolve(&remap, new_rid);
                let result_rid = storage.update(table_id, actual, &before)?.unwrap_or(actual);
                storage.index_delete_row(table_id, &new_tuple, actual)?;
                storage.index_insert_row(table_id, &old_tuple, result_rid)?;
                if old_rid != result_rid {
                    remap.insert(old_rid, result_rid);
                }
            }
            LogRecordType::Begin | LogRecordType::Commit | LogRecordType::Abort | LogRecordType::Checkpoint => {
                unreachable!("Begin・Commit・Abort・Checkpointはこのループへ集める前に取り除いている")
            }
        }
    }
    Ok(())
}
