//! Write-Ahead Logging(WAL)。ログレコードの形式と、それをデータファイルとは
//! 別のファイルへ追記する`WalWriter`(第33章)。
//!
//! # 前章までの限界
//!
//! 第30章のトランザクションは、`ROLLBACK`のための逆操作(`UndoRecord`)を
//! プロセスのメモリ上にしか積まなかった。`COMMIT`はその状態を追認するだけで、
//! 変更が実際にディスクへ届いたかどうかには一切関知しない。第13章の
//! `DiskManager::sync`・第14章の`BufferPool::flush_all`はどちらも呼び出し側が
//! 明示的に呼ばない限り実行されず、`Database::execute("COMMIT")`はそのどちらも
//! 呼んでいない。つまりこのクレートは、`COMMIT`が成功を返した直後にプロセスが
//! 死んでも、その変更がディスクに残っているかどうかを何も保証していなかった
//! (本章のテスト`committed_data_is_lost_without_a_sync_before_crash`が、この
//! 状態を実際に再現する)。
//!
//! # WALファースト不変条件
//!
//! この章が導入する中心的な規律は次の1文に尽きる。
//!
//! ```text
//! あるページの変更をディスクへ書き出す前に、
//! その変更を表すログレコードをディスクへ先に書き出す。
//! ```
//!
//! ログはデータファイルとは別の追記専用ファイル(`<db_path>.wal`)に置く。
//! ログレコードは、データページよりもずっと小さく、常に末尾への追記だけで
//! 済む(データページの更新のように、ファイルの途中を書き換える必要がない)ため、
//! 先に安く永続化できる。この不変条件さえ守れば、たとえページの変更がまだ
//! ディスクに届いていなくても、対応するログレコードさえ残っていれば、後から
//! (第34章のCrash Recoveryが)そのログを読んで変更を再現できる。この章では
//! 「ログを先に書く」ところまでを実装し、「ログを読んで再現する」Redoは
//! 第34章に譲る。
//!
//! # ログレコードの粒度: ページ単位ではなくタプル単位
//!
//! 教科書的なARIESの解説は、しばしばPage IDを持つページ単位の物理ログを扱う。
//! このcrateは、`Storage`の書き込みが`SlottedPage`の1スロットに対する
//! 挿入・更新・削除としてすでに表現されている(第12章)ことに合わせ、ログ
//! レコードもテーブル(`TableId`)とタプル(`RecordId`)を対象にする論理寄りの
//! 粒度を選んだ。ページ単位の物理ログ(バイト範囲の差分)にすると、`Storage`の
//! 呼び出し側(`crate::executor`)が新旧の`Tuple`をすでに持っている情報を、
//! わざわざページ内オフセットへ変換し直す必要が生じる。第30章の`UndoRecord`が
//! すでにタプル単位で設計されていたことも踏まえ、この章はその粒度を素直に
//! 引き継いだ。この判断のトレードオフは、1回の`UPDATE`でSlotted Pageの
//! コンパクションが起きても、コンパクションによる他スロットの移動はログに
//! 現れない(移動そのものはCOMMIT前のページ内部の再配置であり、
//! `SlottedPage::update`がそのページ自身のバイト列を作り直す責務を持つため、
//! この章のログはその結果である「このスロットの新しい値」だけを記録すれば足りる)
//! という点である。
//!
//! # レコードの構成
//!
//! - `lsn`: このレコードに割り当てられた、単調増加の**Log Sequence Number**。
//! - `prev_lsn`: 同じトランザクションが直前に書いたレコードの`lsn`。
//!   トランザクションの最初のレコード(`Begin`)は`None`。この連鎖を`Lsn`の
//!   降順にたどると、`ROLLBACK`が適用すべき逆操作を、そのトランザクションが
//!   実行した順とちょうど逆順にたどれる(`crate::transaction::apply_wal_undo_disk`)。
//! - `txn_id`: このレコードを書いたトランザクション。
//! - `record_type`: [`LogRecordType`]。
//! - `table_id`・`rid`: このレコードが対象にする行。`Begin`・`Commit`・`Abort`は
//!   特定の行を持たないため`None`。
//! - `old_rid`: `Update`だけが持つ、更新**前**の`RecordId`。`Storage::update`は
//!   新しい値が元のページに収まらないとき、その行を別のページへ移動させる
//!   (第15章)。移動が起きると`rid`(更新後の位置)と`old_rid`(更新前の位置)が
//!   食い違うため、両方を記録しないと`ROLLBACK`が「どこから」「どこへ」戻すべき
//!   かを特定できない。第30章の`UndoRecord::Update`がすでに`old_rid`・`new_rid`の
//!   両方を持っていたのと同じ理由である(`crate::transaction`のドキュメントを
//!   参照)。
//! - `before_image`・`after_image`: 更新前・更新後のタプルを`crate::tuple_codec`で
//!   エンコードしたバイト列そのもの。`Insert`は`after_image`だけ、`Delete`は
//!   `before_image`だけ、`Update`は両方を持つ。
//!
//! # torn writeの検出と切り捨て
//!
//! ログファイルへの1回の`write`の途中でプロセスやOSが落ちると、ファイルの
//! 末尾に「長さは足りているが中身が壊れている」、あるいは「長さ自体が
//! 足りていない」バイト列が残ることがある(**torn write**)。[`decode_stream`]は、
//! 1レコードずつ長さとchecksumを検証しながら読み進め、どちらかに失敗した
//! 時点で読み込みを止め、それより前の完全なレコード列と、失敗した位置(以降は
//! 捨てるべきバイト数)を返す。[`WalWriter::open`]は、ファイルを開くたびに
//! これを使って末尾のtorn writeを検出し、ファイルをその位置まで切り詰める
//! (`File::set_len`)。既存のレコードを壊さずに保つ一方、ゴミが残ったままの
//! 追記を防ぐ。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::error::DbResult;
use crate::ids::{Lsn, RecordId, TableId, TransactionId};
use crate::page::crc32;

/// ログレコードの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogRecordType {
    /// トランザクションの開始。このトランザクションの`prev_lsn`連鎖の起点になる。
    Begin,
    /// 1行の挿入。`after_image`だけを持つ。
    Insert,
    /// 1行の更新。`before_image`・`after_image`の両方を持つ。
    Update,
    /// 1行の削除。`before_image`だけを持つ。
    Delete,
    /// トランザクションの確定。
    Commit,
    /// トランザクションの取り消し。
    Abort,
}

impl LogRecordType {
    fn to_u8(self) -> u8 {
        match self {
            LogRecordType::Begin => 0,
            LogRecordType::Insert => 1,
            LogRecordType::Update => 2,
            LogRecordType::Delete => 3,
            LogRecordType::Commit => 4,
            LogRecordType::Abort => 5,
        }
    }

    fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(LogRecordType::Begin),
            1 => Some(LogRecordType::Insert),
            2 => Some(LogRecordType::Update),
            3 => Some(LogRecordType::Delete),
            4 => Some(LogRecordType::Commit),
            5 => Some(LogRecordType::Abort),
            _ => None,
        }
    }
}

/// WALの1レコード。モジュール冒頭「レコードの構成」を参照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub lsn: Lsn,
    pub prev_lsn: Option<Lsn>,
    pub txn_id: TransactionId,
    pub record_type: LogRecordType,
    pub table_id: Option<TableId>,
    pub rid: Option<RecordId>,
    pub old_rid: Option<RecordId>,
    pub before_image: Option<Vec<u8>>,
    pub after_image: Option<Vec<u8>>,
}

fn encode_optional_rid(buf: &mut Vec<u8>, rid: Option<RecordId>) {
    match rid {
        Some(rid) => {
            buf.push(1);
            buf.extend_from_slice(&rid.page_id.0.to_le_bytes());
            buf.extend_from_slice(&rid.slot_id.0.to_le_bytes());
        }
        None => buf.push(0),
    }
}

fn decode_optional_rid(bytes: &[u8], pos: &mut usize) -> Option<Option<RecordId>> {
    let present = *bytes.get(*pos)?;
    *pos += 1;
    if present == 0 {
        return Some(None);
    }
    let page_id_bytes: [u8; 8] = bytes.get(*pos..*pos + 8)?.try_into().ok()?;
    *pos += 8;
    let slot_id_bytes: [u8; 2] = bytes.get(*pos..*pos + 2)?.try_into().ok()?;
    *pos += 2;
    Some(Some(RecordId::new(
        crate::ids::PageId(u64::from_le_bytes(page_id_bytes)),
        crate::ids::SlotId(u16::from_le_bytes(slot_id_bytes)),
    )))
}

fn encode_optional_bytes(buf: &mut Vec<u8>, bytes: &Option<Vec<u8>>) {
    match bytes {
        Some(bytes) => {
            buf.push(1);
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        None => buf.push(0),
    }
}

fn decode_optional_bytes(bytes: &[u8], pos: &mut usize) -> Option<Option<Vec<u8>>> {
    let present = *bytes.get(*pos)?;
    *pos += 1;
    if present == 0 {
        return Some(None);
    }
    let len_bytes: [u8; 4] = bytes.get(*pos..*pos + 4)?.try_into().ok()?;
    *pos += 4;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let data = bytes.get(*pos..*pos + len)?.to_vec();
    *pos += len;
    Some(Some(data))
}

impl LogRecord {
    /// 手書きのリトルエンディアン形式へエンコードする。
    ///
    /// レイアウトは`[len: u32][本体][checksum: u32]`。`len`は`本体`と
    /// `checksum`を合わせたバイト数で、[`decode_stream`]が「次のレコードが
    /// 何バイト目まで続くか」を、本体を読む前に知るために使う。`checksum`は
    /// `crc32(本体)`(第11章の`Page::encode`と同じアルゴリズム、
    /// `crate::page::crc32`を再利用する)。
    fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&self.lsn.0.to_le_bytes());
        match self.prev_lsn {
            Some(lsn) => {
                body.push(1);
                body.extend_from_slice(&lsn.0.to_le_bytes());
            }
            None => body.push(0),
        }
        body.extend_from_slice(&self.txn_id.0.to_le_bytes());
        body.push(self.record_type.to_u8());
        match self.table_id {
            Some(id) => {
                body.push(1);
                body.extend_from_slice(&id.0.to_le_bytes());
            }
            None => body.push(0),
        }
        encode_optional_rid(&mut body, self.rid);
        encode_optional_rid(&mut body, self.old_rid);
        encode_optional_bytes(&mut body, &self.before_image);
        encode_optional_bytes(&mut body, &self.after_image);

        let checksum = crc32(&body);
        let mut out = Vec::with_capacity(4 + body.len() + 4);
        out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&checksum.to_le_bytes());
        out
    }

    /// `bytes`の先頭1レコードぶんを復元し、`(レコード, 消費したバイト数)`を返す。
    ///
    /// `bytes`が短すぎる、`len`が示す範囲が`bytes`に収まらない、checksumが
    /// 一致しない、のいずれかであれば`None`を返す。この`None`は「壊れている」
    /// ではなく「まだここまでしか書かれていない(または末尾がtorn write)」を
    /// 意味するものとして、呼び出し側の[`decode_stream`]が扱う。
    fn decode_one(bytes: &[u8]) -> Option<(LogRecord, usize)> {
        let len_bytes: [u8; 4] = bytes.get(0..4)?.try_into().ok()?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let total = 4 + len;
        let record_bytes = bytes.get(4..total)?;
        if len < 4 {
            return None;
        }
        let body = &record_bytes[..len - 4];
        let stored_checksum = u32::from_le_bytes(record_bytes[len - 4..len].try_into().ok()?);
        if crc32(body) != stored_checksum {
            return None;
        }

        let mut pos = 0usize;
        let lsn = Lsn(u64::from_le_bytes(body.get(pos..pos + 8)?.try_into().ok()?));
        pos += 8;
        let prev_present = *body.get(pos)?;
        pos += 1;
        let prev_lsn = if prev_present == 1 {
            let v = Lsn(u64::from_le_bytes(body.get(pos..pos + 8)?.try_into().ok()?));
            pos += 8;
            Some(v)
        } else {
            None
        };
        let txn_id = TransactionId(u64::from_le_bytes(body.get(pos..pos + 8)?.try_into().ok()?));
        pos += 8;
        let record_type = LogRecordType::from_u8(*body.get(pos)?)?;
        pos += 1;
        let table_present = *body.get(pos)?;
        pos += 1;
        let table_id = if table_present == 1 {
            let v = TableId(u64::from_le_bytes(body.get(pos..pos + 8)?.try_into().ok()?));
            pos += 8;
            Some(v)
        } else {
            None
        };
        let rid = decode_optional_rid(body, &mut pos)?;
        let old_rid = decode_optional_rid(body, &mut pos)?;
        let before_image = decode_optional_bytes(body, &mut pos)?;
        let after_image = decode_optional_bytes(body, &mut pos)?;

        Some((
            LogRecord {
                lsn,
                prev_lsn,
                txn_id,
                record_type,
                table_id,
                rid,
                old_rid,
                before_image,
                after_image,
            },
            total,
        ))
    }
}

/// `bytes`の先頭から、有効なレコードを読めるだけ読む。
///
/// 返り値は`(読めたレコードの列, 有効なバイト数)`。「有効なバイト数」より
/// 後ろに残っているバイト列がtorn writeであり、[`WalWriter::open`]はそこを
/// `File::set_len`で切り詰める。
pub fn decode_stream(bytes: &[u8]) -> (Vec<LogRecord>, usize) {
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        match LogRecord::decode_one(&bytes[offset..]) {
            Some((record, consumed)) => {
                records.push(record);
                offset += consumed;
            }
            None => break,
        }
    }
    (records, offset)
}

/// WALファイルへの追記を担う。
///
/// ログレコードはまず[`WalWriter::append_*`]系メソッドでメモリ上の
/// `buffer`(バイト列)へ溜め、[`WalWriter::flush`]でファイルへ書き渡し、
/// [`WalWriter::sync`]で実ディスクへ同期する。この3段階は
/// `DiskManager`・`BufferPool`が採ってきた「書き込みと同期を分ける」設計
/// (`crate::disk_manager`モジュールドキュメントを参照)をログにも踏襲した
/// もので、`COMMIT`だけが`sync`まで呼ぶ(本文「COMMITの耐久性」を参照)。
///
/// # 書いたレコードをプロセスのメモリ上にも保持し続ける理由
///
/// `records`には、これまでに`append`した(ファイルへ`flush`済みかどうかに
/// かかわらない)全レコードのコピーを保持し続ける。実際のRDBMSはこれを行わず、
/// 必要になったレコードをその都度ディスクから読み直す。この章がメモリに
/// 残す理由は単純で、`ROLLBACK`(`crate::transaction::apply_wal_undo_disk`)が
/// `prev_lsn`の連鎖を*即座に*たどれる必要があり、そのたびにファイルを
/// シークして読み直すコードを書くと、この章の主題(WALファースト不変条件と
/// COMMITの同期順序)から外れた実装の分量が増えてしまうためである。
/// プロセスの寿命を超えてこの連鎖をたどる必要が生じるのが、まさに第34章の
/// Crash Recoveryであり、そこでは実際にファイルを読み直す設計に切り替わる。
pub struct WalWriter {
    file: File,
    /// `append_*`で溜めた、まだ`flush`していないバイト列。
    buffer: Vec<u8>,
    /// これまでに`append`した全レコード(モジュールドキュメントの「書いた
    /// レコードをプロセスのメモリ上にも保持し続ける理由」を参照)。
    records: Vec<LogRecord>,
    /// `lsn`からこの`Vec`の添字への対応。`records`は`Lsn(1)`から欠番なく
    /// 追記されるため、本来は`lsn.0 - 1`という計算だけで引けるが、この
    /// `HashMap`を挟むことで「`Lsn`は連番である」という前提を`record`の
    /// 呼び出し側にまで漏らさずに済む。
    index: HashMap<Lsn, usize>,
    next_lsn: u64,
    /// `sync`済みであることが確定している最大の`Lsn`。`0`は「まだ何も
    /// 同期していない」ことを表す。
    durable_lsn: u64,
}

impl WalWriter {
    /// `path`のWALファイルを開く。存在しなければ新規に作成する。
    ///
    /// 既存のファイルであれば、[`decode_stream`]で内容を読み込み、末尾に
    /// torn writeが見つかればそこまで切り詰める(モジュール冒頭の説明を参照)。
    /// `next_lsn`は読み込んだ最大の`lsn`の次から、`durable_lsn`はファイルに
    /// 実際に存在した(=すでにディスクへ書かれていた)最大の`lsn`から再開する。
    pub fn open<P: AsRef<Path>>(path: P) -> DbResult<Self> {
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let (records, valid_len) = decode_stream(&bytes);
        if valid_len < bytes.len() {
            // 末尾にtorn writeがある。切り詰めて、以後の追記が正しい位置から
            // 始まるようにする。この切り詰め自体も同期しておかないと、
            // 「切り詰めた」という事実自体が次のクラッシュで消えてしまう。
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;

        let max_lsn = records.last().map(|r| r.lsn.0).unwrap_or(0);
        let index = records.iter().enumerate().map(|(i, r)| (r.lsn, i)).collect();

        Ok(WalWriter {
            file,
            buffer: Vec::new(),
            records,
            index,
            next_lsn: max_lsn + 1,
            durable_lsn: max_lsn,
        })
    }

    fn append_record(&mut self, record: LogRecord) -> Lsn {
        let lsn = record.lsn;
        self.buffer.extend_from_slice(&record.encode());
        self.index.insert(lsn, self.records.len());
        self.records.push(record);
        lsn
    }

    fn next_lsn(&mut self) -> Lsn {
        let lsn = Lsn(self.next_lsn);
        self.next_lsn += 1;
        lsn
    }

    /// `Begin`レコードを追記する。
    pub fn append_begin(&mut self, txn_id: TransactionId) -> Lsn {
        let lsn = self.next_lsn();
        self.append_record(LogRecord {
            lsn,
            prev_lsn: None,
            txn_id,
            record_type: LogRecordType::Begin,
            table_id: None,
            rid: None,
            old_rid: None,
            before_image: None,
            after_image: None,
        })
    }

    /// `Insert`レコードを追記する。`after_image`は挿入したタプルの
    /// エンコード済みバイト列(`crate::tuple_codec::encode_tuple`)。
    pub fn append_insert(
        &mut self,
        txn_id: TransactionId,
        prev_lsn: Option<Lsn>,
        table_id: TableId,
        rid: RecordId,
        after_image: Vec<u8>,
    ) -> Lsn {
        let lsn = self.next_lsn();
        self.append_record(LogRecord {
            lsn,
            prev_lsn,
            txn_id,
            record_type: LogRecordType::Insert,
            table_id: Some(table_id),
            rid: Some(rid),
            old_rid: None,
            before_image: None,
            after_image: Some(after_image),
        })
    }

    /// `Update`レコードを追記する。`old_rid`は更新前の位置、`rid`は更新後の
    /// 位置(モジュール冒頭「レコードの構成」を参照)。
    #[allow(clippy::too_many_arguments)]
    pub fn append_update(
        &mut self,
        txn_id: TransactionId,
        prev_lsn: Option<Lsn>,
        table_id: TableId,
        old_rid: RecordId,
        new_rid: RecordId,
        before_image: Vec<u8>,
        after_image: Vec<u8>,
    ) -> Lsn {
        let lsn = self.next_lsn();
        self.append_record(LogRecord {
            lsn,
            prev_lsn,
            txn_id,
            record_type: LogRecordType::Update,
            table_id: Some(table_id),
            rid: Some(new_rid),
            old_rid: Some(old_rid),
            before_image: Some(before_image),
            after_image: Some(after_image),
        })
    }

    /// `Delete`レコードを追記する。`before_image`は削除前のタプルの
    /// エンコード済みバイト列。
    pub fn append_delete(
        &mut self,
        txn_id: TransactionId,
        prev_lsn: Option<Lsn>,
        table_id: TableId,
        rid: RecordId,
        before_image: Vec<u8>,
    ) -> Lsn {
        let lsn = self.next_lsn();
        self.append_record(LogRecord {
            lsn,
            prev_lsn,
            txn_id,
            record_type: LogRecordType::Delete,
            table_id: Some(table_id),
            rid: Some(rid),
            old_rid: None,
            before_image: Some(before_image),
            after_image: None,
        })
    }

    /// `Commit`レコードを追記する。
    pub fn append_commit(&mut self, txn_id: TransactionId, prev_lsn: Option<Lsn>) -> Lsn {
        let lsn = self.next_lsn();
        self.append_record(LogRecord {
            lsn,
            prev_lsn,
            txn_id,
            record_type: LogRecordType::Commit,
            table_id: None,
            rid: None,
            old_rid: None,
            before_image: None,
            after_image: None,
        })
    }

    /// `Abort`レコードを追記する。
    pub fn append_abort(&mut self, txn_id: TransactionId, prev_lsn: Option<Lsn>) -> Lsn {
        let lsn = self.next_lsn();
        self.append_record(LogRecord {
            lsn,
            prev_lsn,
            txn_id,
            record_type: LogRecordType::Abort,
            table_id: None,
            rid: None,
            old_rid: None,
            before_image: None,
            after_image: None,
        })
    }

    /// `buffer`に溜めたバイト列をファイルへ書き渡す(OSのページキャッシュまで)。
    /// 実ディスクへの同期は行わない([`WalWriter::sync`]を参照)。
    pub fn flush(&mut self) -> DbResult<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.file.write_all(&self.buffer)?;
        self.buffer.clear();
        Ok(())
    }

    /// [`WalWriter::flush`]に続けて`File::sync_all`を呼び、これまでに
    /// `append`した全レコードを実ディスクへ同期する。返り値は同期が完了した
    /// 時点での最大`Lsn`(`durable_lsn`)。
    ///
    /// `Database`の`COMMIT`は、`Commit`レコードを`append`した直後にこれを呼び、
    /// その完了を待ってから利用者へ成功を返す(本文「COMMITの耐久性」を参照)。
    pub fn sync(&mut self) -> DbResult<Lsn> {
        self.flush()?;
        self.file.sync_all()?;
        self.durable_lsn = self.next_lsn - 1;
        Ok(Lsn(self.durable_lsn))
    }

    /// `lsn`までのレコードがすでに同期済みなら何もしない。そうでなければ
    /// [`WalWriter::sync`]を呼び、`lsn`を含むそれ以前の全レコードを同期する。
    ///
    /// これが**WALファースト不変条件**を実際に強制する箇所である。
    /// `crate::buffer_pool::BufferPool`は、dirtyなページをディスクへ書き戻す
    /// 直前に必ずこれを呼ぶ(`crate::buffer_pool`モジュールドキュメントの
    /// 「WALファースト不変条件の強制」を参照)。
    pub fn sync_up_to(&mut self, lsn: Lsn) -> DbResult<()> {
        if lsn.0 <= self.durable_lsn {
            return Ok(());
        }
        self.sync()?;
        Ok(())
    }

    /// これまでに同期済みであることが確定している最大の`Lsn`。
    pub fn durable_lsn(&self) -> Lsn {
        Lsn(self.durable_lsn)
    }

    /// `lsn`のレコードを返す。`append_*`で割り当てた`Lsn`は必ずここで見つかる
    /// (`crate::transaction::apply_wal_undo_disk`が`prev_lsn`の連鎖をたどるのに使う)。
    pub fn record(&self, lsn: Lsn) -> Option<&LogRecord> {
        self.index.get(&lsn).map(|&i| &self.records[i])
    }

    /// これまでに`append`した全レコードを、書いた順に1行1レコードの文字列へ
    /// 整形する。
    ///
    /// この章はCrash Recovery(第34章)をまだ実装しないため、「WALに何が
    /// 書かれているか」を自動で読み戻す経路が無い。この`dump`は、それでも
    /// 開発者がWALの中身を目視で確認できるようにするための、この章の
    /// 開発用ダンプである。
    pub fn dump(&self) -> Vec<String> {
        self.records.iter().map(format_record).collect()
    }
}

fn format_record(record: &LogRecord) -> String {
    let prev = record.prev_lsn.map(|l| l.0.to_string()).unwrap_or_else(|| "-".to_string());
    let target = match (record.table_id, record.rid) {
        (Some(table_id), Some(rid)) => format!("table={} rid=({},{})", table_id.0, rid.page_id.0, rid.slot_id.0),
        _ => "-".to_string(),
    };
    let images = format!(
        "before={}B after={}B",
        record.before_image.as_ref().map(Vec::len).unwrap_or(0),
        record.after_image.as_ref().map(Vec::len).unwrap_or(0),
    );
    format!(
        "lsn={} prev={} txn={} type={:?} {target} {images}",
        record.lsn.0, prev, record.txn_id.0, record.record_type
    )
}

/// Diskバックエンドの1文の実行中、`crate::executor::storage_insert`・
/// `storage_update`・`storage_delete`がWALレコードを書くための窓口(第33章)。
///
/// `prev_lsn`は、呼び出し元(`crate::database`)が持つ
/// `TransactionContext::wal_last_lsn`(または、Autocommitの合成トランザクション
/// 用の一時変数)への可変参照であり、`append_*`のたびに書いたレコードの
/// `Lsn`で更新する。こうしておくと、同じトランザクションの中で複数回
/// `INSERT`・`UPDATE`・`DELETE`を呼んでも、呼び出し側(`executor`)は
/// `prev_lsn`の受け渡しを一切意識せずに済む。
///
/// # `Begin`レコードを遅延して書く
///
/// `prev_lsn`が`None`(このトランザクションがまだ1件もWALへ書いていない)の
/// 状態で最初の`append_*`が呼ばれたとき、その直前に`Begin`レコードを書く。
/// これにより、行を1件も書き換えない`UPDATE`・`DELETE`(`WHERE`に一致する行が
/// 無かった場合)や、読み取りだけで終わるトランザクションは、`Begin`すら
/// WALに残さない。`crate::database::run_disk_dml`は、Autocommitの後始末
/// (Commit・Abortを書くかどうか)をこの「何か書いたかどうか」でそのまま
/// 判断できる。
pub(crate) struct WalCursor<'a> {
    wal: &'a Arc<Mutex<WalWriter>>,
    txn_id: TransactionId,
    prev_lsn: &'a mut Option<Lsn>,
}

impl<'a> WalCursor<'a> {
    pub(crate) fn new(wal: &'a Arc<Mutex<WalWriter>>, txn_id: TransactionId, prev_lsn: &'a mut Option<Lsn>) -> Self {
        WalCursor { wal, txn_id, prev_lsn }
    }

    fn ensure_begin(&mut self, wal: &mut WalWriter) {
        if self.prev_lsn.is_none() {
            let lsn = wal.append_begin(self.txn_id);
            *self.prev_lsn = Some(lsn);
        }
    }

    /// `Insert`レコードを書き、その`Lsn`を返す。
    pub(crate) fn append_insert(&mut self, table_id: TableId, rid: RecordId, after_image: Vec<u8>) -> Lsn {
        let mut wal = self.wal.lock().unwrap_or_else(|p| p.into_inner());
        self.ensure_begin(&mut wal);
        let lsn = wal.append_insert(self.txn_id, *self.prev_lsn, table_id, rid, after_image);
        *self.prev_lsn = Some(lsn);
        lsn
    }

    /// `Update`レコードを書き、その`Lsn`を返す。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_update(
        &mut self,
        table_id: TableId,
        old_rid: RecordId,
        new_rid: RecordId,
        before_image: Vec<u8>,
        after_image: Vec<u8>,
    ) -> Lsn {
        let mut wal = self.wal.lock().unwrap_or_else(|p| p.into_inner());
        self.ensure_begin(&mut wal);
        let lsn = wal.append_update(self.txn_id, *self.prev_lsn, table_id, old_rid, new_rid, before_image, after_image);
        *self.prev_lsn = Some(lsn);
        lsn
    }

    /// `Delete`レコードを書き、その`Lsn`を返す。
    pub(crate) fn append_delete(&mut self, table_id: TableId, rid: RecordId, before_image: Vec<u8>) -> Lsn {
        let mut wal = self.wal.lock().unwrap_or_else(|p| p.into_inner());
        self.ensure_begin(&mut wal);
        let lsn = wal.append_delete(self.txn_id, *self.prev_lsn, table_id, rid, before_image);
        *self.prev_lsn = Some(lsn);
        lsn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{PageId, SlotId};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-wal-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        path
    }

    fn sample_rid() -> RecordId {
        RecordId::new(PageId(3), SlotId(2))
    }

    #[test]
    fn insert_record_round_trips_through_encode_decode() {
        let record = LogRecord {
            lsn: Lsn(1),
            prev_lsn: None,
            txn_id: TransactionId(7),
            record_type: LogRecordType::Insert,
            table_id: Some(TableId(1)),
            rid: Some(sample_rid()),
            old_rid: None,
            before_image: None,
            after_image: Some(b"hello".to_vec()),
        };
        let bytes = record.encode();
        let (decoded, consumed) = LogRecord::decode_one(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, record);
    }

    #[test]
    fn update_record_round_trips_with_both_images_and_both_rids() {
        let record = LogRecord {
            lsn: Lsn(5),
            prev_lsn: Some(Lsn(2)),
            txn_id: TransactionId(1),
            record_type: LogRecordType::Update,
            table_id: Some(TableId(9)),
            rid: Some(RecordId::new(PageId(4), SlotId(0))),
            old_rid: Some(sample_rid()),
            before_image: Some(b"old".to_vec()),
            after_image: Some(b"new-value".to_vec()),
        };
        let bytes = record.encode();
        let (decoded, _) = LogRecord::decode_one(&bytes).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn decode_stream_reads_multiple_records_in_order() {
        let r1 = LogRecord {
            lsn: Lsn(1),
            prev_lsn: None,
            txn_id: TransactionId(1),
            record_type: LogRecordType::Begin,
            table_id: None,
            rid: None,
            old_rid: None,
            before_image: None,
            after_image: None,
        };
        let r2 = LogRecord {
            lsn: Lsn(2),
            prev_lsn: Some(Lsn(1)),
            txn_id: TransactionId(1),
            record_type: LogRecordType::Commit,
            table_id: None,
            rid: None,
            old_rid: None,
            before_image: None,
            after_image: None,
        };
        let mut bytes = r1.encode();
        bytes.extend_from_slice(&r2.encode());

        let (records, consumed) = decode_stream(&bytes);
        assert_eq!(consumed, bytes.len());
        assert_eq!(records, vec![r1, r2]);
    }

    #[test]
    fn decode_stream_stops_before_a_torn_tail() {
        let r1 = LogRecord {
            lsn: Lsn(1),
            prev_lsn: None,
            txn_id: TransactionId(1),
            record_type: LogRecordType::Insert,
            table_id: Some(TableId(1)),
            rid: Some(sample_rid()),
            old_rid: None,
            before_image: None,
            after_image: Some(b"payload".to_vec()),
        };
        let mut bytes = r1.encode();
        let full_len = bytes.len();
        // 途中(次のレコードのつもり)で書き込みが打ち切られた状況を再現する。
        bytes.extend_from_slice(&[0xAB, 0xCD, 0xEF]);

        let (records, consumed) = decode_stream(&bytes);
        assert_eq!(records, vec![r1]);
        assert_eq!(consumed, full_len);
    }

    #[test]
    fn decode_stream_rejects_a_flipped_byte_in_the_last_record() {
        let r1 = LogRecord {
            lsn: Lsn(1),
            prev_lsn: None,
            txn_id: TransactionId(1),
            record_type: LogRecordType::Delete,
            table_id: Some(TableId(2)),
            rid: Some(sample_rid()),
            old_rid: None,
            before_image: Some(b"alice".to_vec()),
            after_image: None,
        };
        let mut bytes = r1.encode();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;

        let (records, consumed) = decode_stream(&bytes);
        assert!(records.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn open_truncates_a_torn_tail_left_on_disk() {
        let path = temp_path("torn-tail");
        {
            let mut writer = WalWriter::open(&path).unwrap();
            writer.append_begin(TransactionId(1));
            writer.sync().unwrap();
        }

        // 2回目のレコードが途中までしか書かれなかった状況を、ファイルへ
        // 直接ゴミバイト列を追記して再現する。
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&[1, 2, 3, 4, 5, 6, 7]).unwrap();
        }
        let torn_len = std::fs::metadata(&path).unwrap().len();

        let writer = WalWriter::open(&path).unwrap();
        assert_eq!(writer.records.len(), 1);
        let truncated_len = std::fs::metadata(&path).unwrap().len();
        assert!(truncated_len < torn_len, "torn tailが切り詰められていません");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn sync_up_to_only_syncs_when_the_target_lsn_is_not_yet_durable() {
        let path = temp_path("sync-up-to");
        let mut writer = WalWriter::open(&path).unwrap();
        let lsn = writer.append_begin(TransactionId(1));
        assert_eq!(writer.durable_lsn(), Lsn(0));

        writer.sync_up_to(lsn).unwrap();
        assert_eq!(writer.durable_lsn(), lsn);

        // すでに同期済みのlsnを指定しても、追加の書き込みは起こらない
        // (バッファが空のままなので`flush`が早期リターンする)。
        writer.sync_up_to(lsn).unwrap();
        assert_eq!(writer.durable_lsn(), lsn);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reopening_preserves_records_and_continues_lsn_numbering() {
        let path = temp_path("reopen");
        {
            let mut writer = WalWriter::open(&path).unwrap();
            writer.append_begin(TransactionId(1));
            writer.append_commit(TransactionId(1), Some(Lsn(1)));
            writer.sync().unwrap();
        }

        let mut writer = WalWriter::open(&path).unwrap();
        assert_eq!(writer.records.len(), 2);
        let lsn = writer.append_begin(TransactionId(2));
        assert_eq!(lsn, Lsn(3));

        std::fs::remove_file(&path).unwrap();
    }
}
