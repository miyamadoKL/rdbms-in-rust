//! ARIES-lite Crash Recovery: Analysis・Redo・Undo(第34章)。
//!
//! # 前章の限界: WALは書けるが、読む者がいない
//!
//! 第33章は「ページの変更をディスクへ書き出す前に、その変更を表すログ
//! レコードを先に書き出す」というWALファースト不変条件を実装し、
//! `COMMIT`直後にプロセスが死んでも、その変更を表すログレコード自体は
//! WALファイルに残ることを確認した。だが第33章の`Storage::open`は、
//! WALファイルを読み込んで`WalWriter`を再構築するだけで、そこに残っている
//! レコードを実際にテーブル本体へ**反映し直す**処理を一切持たない。
//! `crate::wal`モジュールドキュメントが「読んで再現するRedoは第34章の
//! 仕事」と書き残していたのが、まさにこの欠落である。この章の
//! [`recover`]が、その欠落を埋める。
//!
//! # ARIESを教材規模へ簡略化した点
//!
//! 本物のARIESに対して、この章が単純化した箇所を以下に列挙する。
//!
//! - **Dirty Page Table(DPT)を持たない**。本来のARIESは、Redoがどの
//!   LSNから始めればよいかをDPTの最小recLSNで絞り込む。このクレートは
//!   テーブルが持つページ数が教材規模(せいぜい数百枚)にとどまるため、
//!   「各レコードの対象ページについて、実際のPage LSNを毎回読んで比較する」
//!   という総当たりの判定で十分に安く済む。DPTを省いた代わりに、
//!   [`Storage::redo_insert`]等がその都度`Page`自身に永続化された
//!   `page_lsn`(第34章、`crate::page::Page::page_lsn`)を直接検査する。
//! - **Repeating Historyは行うが、CLR(Compensation Log Record)は
//!   書かない**。本物のARIESは、Undoの1操作ごとにCLRを書き、Undo自身が
//!   クラッシュしてもどこまでUndo済みかを再起動後に見分けられるようにする。
//!   この章は代わりに、[`recover`]全体を「ディスクへの反映は最後に1回だけ」
//!   というアトミックな操作にすることで同じ問題を解決する(次節「Undo中の
//!   クラッシュへの耐性」を参照)。
//! - **Undoはトランザクション単位で直列に行う**。本物のARIESは、複数の
//!   loserトランザクションのUndoをLSNの降順で1本にまとめ、互いの操作を
//!   インターリーブさせながら進める。この章はそこまでせず、1本の
//!   loserトランザクションのUndoを最後まで終えてから次のloserへ進む。
//!   このクレートはまだシングルスレッドで、Strict 2PL(第31章)のもとでは
//!   複数のloserが同じ行を同時に保持できないため、この単純化は正しさを
//!   損なわない。
//! - **索引はWALと無関係に、丸ごと作り直す**。第33章の限界として、
//!   索引ごとの`BufferPool`にはWALを結線していない。この章は索引ページ
//!   個別の crash-consistency を追求せず、Heap(WALによって正しく復元済み)
//!   から全索引を[`Storage::rebuild_all_indexes_after_recovery`]で作り直す。
//! - **WALの切り詰めは行わない**。`CHECKPOINT`はAnalysisの開始点を
//!   短縮するだけで、それより前のWALレコード自体は削除しない(章末の
//!   演習で扱う)。
//!
//! # Undo中のクラッシュへの耐性
//!
//! [`recover`]は、Analysis・Redo・Undoの全工程が完全に終わるまで、
//! `Storage`が持つ`BufferPool`のdirtyなページを一切ディスクへ書き戻さない
//! (`Storage::flush`・`Storage::sync`を呼ぶのは[`recover`]の最後の1回だけ)。
//! したがって、Redoの途中・Undoの途中のどこで`recover`が失敗しても
//! (この章のテストでは[`crate::failpoint`]で意図的に発生させる)、ディスク上の
//! バイト列は`Storage::open`を呼ぶ直前と一切変わっていない。次に
//! `Storage::open`を呼び直すと、[`recover`]はまったく同じ入力(変化していない
//! WALと変化していないページ)から、Page LSNによる冪等なRedoと
//! トランザクション単位のUndoを最初からやり直し、同じ結果へ確定的に
//! たどり着く。CLRで「どこまでUndo済みか」を細かく記録する代わりに、
//! 「失敗したら全部やり直す」という単純な規律でクラッシュ安全性を得ている。

use std::collections::HashMap;

use crate::error::DbResult;
use crate::ids::{Lsn, TransactionId};
use crate::storage::Storage;
use crate::wal::{LogRecord, LogRecordType, decode_active_transactions};

/// [`recover`]が何を行ったかを要約する、テスト・観測用のレポート。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// AnalysisがCheckpointレコードを見つけ、その直後からWALを走査したか。
    /// `false`ならWALの先頭(`Lsn(1)`)から走査した。
    pub used_checkpoint: bool,
    /// Analysis・Redoが実際に走査したレコード数
    /// (Checkpoint以前をスキップした分は含まない)。
    pub records_scanned: usize,
    /// Redoが「まだ反映されていない」と判定し、実際に物理的な書き込みを
    /// 行ったレコード数。
    pub records_redone: usize,
    /// UndoがROLLBACKしたloserトランザクションの数
    /// (`Commit`も`Abort`も記録されていなかったトランザクション)。
    pub transactions_undone: usize,
}

/// Analysisが1トランザクションについて追跡する状態。
struct TxState {
    /// このトランザクションが書いた最後のレコードのLSN。Undoの起点になる。
    /// `None`は「まだ1件もInsert/Update/Deleteを書いていない」ことを表す
    /// (`BEGIN`直後にcrashした、または`BEGIN; CHECKPOINT;`のようにCheckpoint
    /// の時点でまだ書き込みが無かったトランザクション)。この場合Undoすべき
    /// 対象が無いので、`apply_wal_undo_disk`には`None`をそのまま渡す
    /// (架空の`Lsn(0)`を作って渡さない、本文「Undo起点の無いトランザクション」
    /// を参照)。
    last_lsn: Option<Lsn>,
    /// `Commit`または`Abort`をすでに見た(=Undo不要)なら`true`。
    resolved: bool,
}

/// `storage`をAnalysis→Redo→Undoの順で復元する(第34章)。
///
/// `Storage::open`が、カタログ・索引・WALを読み込んだ直後に呼ぶ。詳しい
/// 設計はモジュールドキュメントを参照。
pub(crate) fn recover(storage: &mut Storage) -> DbResult<RecoveryReport> {
    let records: Vec<LogRecord> = storage.wal().lock().unwrap_or_else(|p| p.into_inner()).records().to_vec();

    let (start_index, seed, used_checkpoint) = analysis_start(&records);
    let scanned = &records[start_index..];

    let mut table: HashMap<TransactionId, TxState> = HashMap::new();
    for (txn_id, last_lsn) in seed {
        table.insert(txn_id, TxState { last_lsn, resolved: false });
    }
    for record in scanned {
        match record.record_type {
            LogRecordType::Commit | LogRecordType::Abort => {
                table
                    .entry(record.txn_id)
                    .and_modify(|s| {
                        s.last_lsn = Some(record.lsn);
                        s.resolved = true;
                    })
                    .or_insert(TxState { last_lsn: Some(record.lsn), resolved: true });
            }
            LogRecordType::Begin | LogRecordType::Insert | LogRecordType::Update | LogRecordType::Delete => {
                table
                    .entry(record.txn_id)
                    .and_modify(|s| s.last_lsn = Some(record.lsn))
                    .or_insert(TxState { last_lsn: Some(record.lsn), resolved: false });
            }
            LogRecordType::Checkpoint => {}
        }
    }

    let mut records_redone = 0usize;
    for record in scanned {
        if redo_one(storage, record)? {
            records_redone += 1;
        }
        crate::failpoint::hit("recovery_redo_step")?;
    }
    storage.persist_catalog_after_recovery()?;
    // 索引は一時ファイルへ作り直す。既存の索引ファイル(`pending_index_renames`
    // が指す`rename`先)は、Redo・Undo・検証がすべて終わるまで一切変更しない
    // (`Storage::rebuild_all_indexes_after_recovery`のドキュメントを参照)。
    let pending_index_renames = storage.rebuild_all_indexes_after_recovery()?;

    // どの物理的な書き込み(Redo・Undoのどちらも)も、この時点まで
    // `BufferPool`のキャッシュ、または索引の一時ファイルにしか無く、
    // 既存の永続ファイルへは一切書き戻していない。`Abort`レコードも同様に、
    // `WalWriter`のメモリ上のバッファに積むだけで`flush`・`sync`はまだ
    // 呼ばない。これが「Undo中のクラッシュへの耐性」(モジュールドキュメント
    // を参照)の核心である。ここより前でこの関数が失敗すると、`storage`
    // ごと破棄され、ここまでの変更(物理的な書き込みも`Abort`レコードも、
    // 索引の一時ファイルも)はすべて消える。既存の永続ファイルのバイト列は
    // この`recover`を呼ぶ直前と一切変わっていない。
    let mut transactions_undone = 0usize;
    for (txn_id, state) in &table {
        if state.resolved {
            continue;
        }
        crate::transaction::apply_wal_undo_disk(storage, state.last_lsn)?;
        storage.wal().lock().unwrap_or_else(|p| p.into_inner()).append_abort(*txn_id, state.last_lsn);
        transactions_undone += 1;
        crate::failpoint::hit("recovery_undo_step")?;
    }

    // ここに到達して初めて、Analysis・Redo・Undoの全工程が完全に終わった。
    // ここで初めてディスクへ反映する(データページ・カタログ・索引の
    // flush・syncに続けて、Undoが積んだ`Abort`レコードを含むWAL全体を
    // sync)。
    storage.flush()?;
    storage.sync()?;
    // 索引の一時ファイルを、既存の索引ファイルへ`rename`でアトミックに
    // 置き換える。同じディレクトリ内の`rename`はファイルシステムレベルで
    // 単一の操作であり、置き換えの途中の中途半端な状態を外部から観測
    // できない(本文「索引ファイルの入れ替えは一時ファイル経由」を参照)。
    for (temp_path, index_path) in &pending_index_renames {
        std::fs::rename(temp_path, index_path)?;
    }
    storage.wal().lock().unwrap_or_else(|p| p.into_inner()).sync()?;

    Ok(RecoveryReport {
        used_checkpoint,
        records_scanned: scanned.len(),
        records_redone,
        transactions_undone,
    })
}

/// 最後の`Checkpoint`レコードを探し、Analysis・Redoが走査を始めるべき添字
/// (`records`の中の、そのCheckpointの直後)と、そこへ引き継ぐべき
/// Active Transaction一覧を返す。`Checkpoint`が1つも無ければ`(0, vec![],
/// false)`(先頭から全件走査)を返す。
fn analysis_start(records: &[LogRecord]) -> (usize, Vec<(TransactionId, Option<Lsn>)>, bool) {
    let checkpoint_index = records.iter().rposition(|r| r.record_type == LogRecordType::Checkpoint);
    match checkpoint_index {
        Some(index) => {
            let active = records[index]
                .after_image
                .as_deref()
                .and_then(decode_active_transactions)
                .unwrap_or_default();
            (index + 1, active, true)
        }
        None => (0, Vec::new(), false),
    }
}

/// 1件のログレコードをRedoする。実際に物理的な書き込みを行った(冪等性の
/// 判定により反映済みでスキップしなかった)場合に`true`を返す。
fn redo_one(storage: &mut Storage, record: &LogRecord) -> DbResult<bool> {
    match record.record_type {
        LogRecordType::Insert => {
            let table_id = record.table_id.expect("InsertレコードのWALは必ずtable_idを持つ");
            let rid = record.rid.expect("InsertレコードのWALは必ずridを持つ");
            let after = record.after_image.as_ref().expect("InsertレコードのWALは必ずafter_imageを持つ");
            let before = storage.page_lsn(rid.page_id)?;
            storage.redo_insert(table_id, rid, after, record.lsn)?;
            Ok(before < record.lsn)
        }
        LogRecordType::Delete => {
            let rid = record.rid.expect("DeleteレコードのWALは必ずridを持つ");
            let before = storage.page_lsn(rid.page_id)?;
            storage.redo_delete(rid, record.lsn)?;
            Ok(before < record.lsn)
        }
        LogRecordType::Update => {
            let table_id = record.table_id.expect("UpdateレコードのWALは必ずtable_idを持つ");
            let old_rid = record.old_rid.expect("UpdateレコードのWALは必ずold_rid(更新前の位置)を持つ");
            let new_rid = record.rid.expect("UpdateレコードのWALは必ずrid(更新後の位置)を持つ");
            let after = record.after_image.as_ref().expect("UpdateレコードのWALは必ずafter_imageを持つ");
            if old_rid == new_rid {
                let before = storage.page_lsn(new_rid.page_id)?;
                storage.redo_update_in_place(new_rid, after, record.lsn)?;
                Ok(before < record.lsn)
            } else {
                let before_new = storage.page_lsn(new_rid.page_id)?;
                let before_old = storage.page_lsn(old_rid.page_id)?;
                storage.redo_insert(table_id, new_rid, after, record.lsn)?;
                storage.redo_delete(old_rid, record.lsn)?;
                Ok(before_new < record.lsn || before_old < record.lsn)
            }
        }
        LogRecordType::Begin | LogRecordType::Commit | LogRecordType::Abort | LogRecordType::Checkpoint => Ok(false),
    }
}
