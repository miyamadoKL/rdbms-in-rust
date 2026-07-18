//! B+Tree(第23章、`crate::btree`)の葉ページ(Leaf Page)と内部ページ(Internal Page)の
//! ページ内レイアウト。
//!
//! `Page`の`payload`(第11章)の中身をどう解釈するかという役割は、第12章の
//! `SlottedPage`と同じである。違うのは、`SlottedPage`が挿入順や`Tombstone`による
//! 再利用を許し、`SlotId`という位置に依存しない安定した識別子を提供していたのに
//! 対し、この章のLeaf PageとInternal Pageは**エントリを常にキーの昇順に並べておく**
//! 必要がある点である。安定した識別子を持たず、常に「0番目に小さいキー、1番目に
//! 小さいキー、……」という位置でエントリを指す。
//!
//! # 常に全エントリを書き直す
//!
//! `SlottedPage`は個々のスロットを1件ずつ`insert`・`update`・`delete`でき、
//! 空いた領域は`compact`が必要になったときだけ回収していた。この章のLeaf
//! PageとInternal Pageは、挿入のたびに「エントリの並びをキー順に保った差し込み」を
//! 行わなければならないため、1件だけを他のエントリに触れず追記する操作自体に
//! 意味がない。そこでこのモジュールは、常にページの全エントリを`Vec`へ取り出し
//! ([`LeafPageRef::entries`]・[`InternalPageRef::entries`])、`Vec`の側で
//! 挿入位置を決めてから、ページ全体を1回で書き直す([`LeafPage::write_entries`]・
//! [`InternalPage::write_entries`])という設計を採る。1ページに収まるエントリ数は
//! 数十〜数百程度(`PAGE_PAYLOAD_SIZE`をキー・RIDのバイト数で割った程度)であり、
//! 挿入のたびに全件をコピーし直すコストは無視できる。
//!
//! この設計により、ページの`payload`のうちDirectoryの直後からデータ領域が
//! 隙間なく詰まっている状態が常に保たれる。`SlottedPage`が持っていた
//! 「Tombstoneの死んだ領域」「`compact`前の断片化」はこの章のページには
//! 存在しない。
//!
//! # Leaf Pageのレイアウト
//!
//! ```text
//! offset 0                 2                    2+4n
//! +------------------------+--------------------+-----------------+
//! | entry_count (2バイト)  | Directory (4n バイト) |   Entry Data   |
//! +------------------------+--------------------+-----------------+
//! ```
//!
//! | フィールド | バイト数 | 内容 |
//! | --- | --- | --- |
//! | `entry_count` | 2 | エントリ数`n`(LE) |
//! | Directory | `4 * n` | `n`個の`(key_offset: u16, key_len: u16)`の並び(LE) |
//!
//! Directoryの`i`番目のエントリが指す`key_offset`から、`key_len`バイトの
//! キー(`crate::btree`がエンコードしたバイト列)、続けて`RecordId`
//! (`page_id: u64` 8バイト + `slot_id: u16` 2バイト、計10バイト、どちらもLE)が
//! 置かれている。エントリはDirectoryの直後から隙間なく、Directoryと同じ
//! キー昇順で並ぶ。
//!
//! # Internal Pageのレイアウト
//!
//! ```text
//! offset 0        2                10                   10+4n
//! +---------------+-----------------+--------------------+-----------------+
//! | entry_count   | leftmost_child  | Directory (4n バイト)|   Entry Data   |
//! | (2バイト)     | (8バイト)       |                      |                |
//! +---------------+-----------------+--------------------+-----------------+
//! ```
//!
//! | フィールド | バイト数 | 内容 |
//! | --- | --- | --- |
//! | `entry_count` | 2 | 区切りキーの本数`n`(LE) |
//! | `leftmost_child` | 8 | 先頭(0番目)の子ページを指す`PageId`(LE) |
//! | Directory | `4 * n` | `n`個の`(key_offset: u16, key_len: u16)`の並び(LE) |
//!
//! `n`本の区切りキーを持つInternal Pageは`n + 1`本の子ページへのポインタを持つ。
//! `leftmost_child`が0番目の子で、Directoryの`i`番目のエントリが指す
//! `key_offset`には、`key_len`バイトのキーに続けて`(i + 1)`番目の子を指す
//! `child_page_id: u64`(8バイト、LE)が置かれている。区切りキー`key_i`は
//! 「`key_i`以上のキーは`(i+1)`番目の子以降にある」という境界を表す
//! (`crate::btree`の探索ロジックを参照)。
//!
//! # 検証
//!
//! [`LeafPageRef::open`]・[`InternalPageRef::open`]は、`SlottedPage::open`
//! (第12章)と同じ理由で`payload`を検証する。Directoryの各エントリが指す
//! バイト範囲が`payload`に収まっていることに加え、キーが昇順に並んでいることまで
//! 確認する。後者は`SlottedPage`には無かった検証だが、この章の探索
//! ([`LeafPageRef::find`]・[`InternalPageRef::find`])がキーの昇順を前提にした
//! 二分探索であり、順序が崩れていると誤った(が`panic`はしない)結果を静かに
//! 返してしまうため、検証しておく価値がある。

use crate::error::{DbError, DbResult};
use crate::ids::{PageId, RecordId, SlotId};

const LEAF_HEADER_SIZE: usize = 2;
const INTERNAL_HEADER_SIZE: usize = 2 + 8;
const DIR_ENTRY_SIZE: usize = 4;
const RECORD_ID_SIZE: usize = 8 + 2;
const CHILD_ID_SIZE: usize = 8;

fn read_u16(payload: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(payload[offset..offset + 2].try_into().unwrap())
}

fn read_u64(payload: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(payload[offset..offset + 8].try_into().unwrap())
}

fn dir_entry(payload: &[u8], header_size: usize, index: usize) -> (usize, usize) {
    let base = header_size + index * DIR_ENTRY_SIZE;
    (read_u16(payload, base) as usize, read_u16(payload, base + 2) as usize)
}

/// エントリが占めるバイト範囲`[start, end)`を`payload`の大きさと突き合わせ、
/// はみ出していれば`DbError::CorruptPage`を返す。
fn checked_range(payload_len: usize, start: usize, len: usize, what: &str) -> DbResult<usize> {
    let end = start.checked_add(len).ok_or_else(|| DbError::CorruptPage(format!("{what}のバイト範囲がオーバーフローしています")))?;
    if end > payload_len {
        return Err(DbError::CorruptPage(format!(
            "{what}のバイト範囲[{start}, {end})がpayloadの大きさ({payload_len})からはみ出しています"
        )));
    }
    Ok(end)
}

/// Leaf Pageのエントリ数・Directory・キーの昇順を検証する。
///
/// `header_size`・`trailer_size`(キーの後ろに続く固定長データのバイト数、
/// Leafなら`RECORD_ID_SIZE`、Internalなら`CHILD_ID_SIZE`)を引数化することで、
/// Leaf PageとInternal Pageの両方から共通のロジックとして呼べるようにしてある。
fn validate(payload: &[u8], header_size: usize, trailer_size: usize, what: &str) -> DbResult<usize> {
    if payload.len() < header_size {
        return Err(DbError::CorruptPage(format!(
            "{what}のpayloadがヘッダー({header_size}バイト)より小さいです: {}バイト",
            payload.len()
        )));
    }
    let entry_count = read_u16(payload, 0) as usize;
    let dir_end = checked_range(payload.len(), header_size, entry_count * DIR_ENTRY_SIZE, &format!("{what}のDirectory"))?;
    let _ = dir_end;

    let mut previous_key: Option<Vec<u8>> = None;
    for i in 0..entry_count {
        let (key_offset, key_len) = dir_entry(payload, header_size, i);
        let key_end = checked_range(payload.len(), key_offset, key_len, &format!("{what}のエントリ{i}のキー"))?;
        checked_range(payload.len(), key_end, trailer_size, &format!("{what}のエントリ{i}の付随データ"))?;

        let key = &payload[key_offset..key_end];
        if let Some(prev) = &previous_key {
            // 重複キーを許す設計(`crate::btree`のドキュメント参照)なので、
            // ここで拒否するのは「狭義の降順」だけである。直前のキーと
            // 等しい(重複)場合は許容する。
            if prev.as_slice() > key {
                return Err(DbError::CorruptPage(format!(
                    "{what}のエントリ{i}のキーが、直前のエントリのキーより小さいです(昇順に並んでいる必要があります)"
                )));
            }
        }
        previous_key = Some(key.to_vec());
    }
    Ok(entry_count)
}

/// 与えられたエントリ列を書き込むために必要な合計バイト数を計算する。
fn required_len(header_size: usize, entries_key_lens: impl Iterator<Item = usize>, trailer_size: usize) -> usize {
    let mut total = header_size;
    let mut count = 0usize;
    for key_len in entries_key_lens {
        total += DIR_ENTRY_SIZE + key_len + trailer_size;
        count += 1;
    }
    let _ = count;
    total
}

/// `payload`をLeaf Pageとして読み取り専用で開くビュー。
pub struct LeafPageRef<'a> {
    payload: &'a [u8],
}

impl<'a> LeafPageRef<'a> {
    /// すでにLeaf Pageとして初期化済みの`payload`を検証してから開く。
    pub fn open(payload: &'a [u8]) -> DbResult<Self> {
        validate(payload, LEAF_HEADER_SIZE, RECORD_ID_SIZE, "Leaf Page")?;
        Ok(LeafPageRef { payload })
    }

    /// このページが持つエントリ数。
    pub fn entry_count(&self) -> usize {
        read_u16(self.payload, 0) as usize
    }

    /// `index`番目(0始まり、キー昇順)のキーのバイト列。
    pub fn key(&self, index: usize) -> &[u8] {
        let (offset, len) = dir_entry(self.payload, LEAF_HEADER_SIZE, index);
        &self.payload[offset..offset + len]
    }

    /// `index`番目のエントリが指す`RecordId`。
    pub fn record_id(&self, index: usize) -> RecordId {
        let (offset, len) = dir_entry(self.payload, LEAF_HEADER_SIZE, index);
        let base = offset + len;
        let page_id = PageId(read_u64(self.payload, base));
        let slot_id = SlotId(read_u16(self.payload, base + 8));
        RecordId::new(page_id, slot_id)
    }

    /// 全エントリを`(キーのバイト列, RecordId)`の`Vec`として取り出す。
    pub fn entries(&self) -> Vec<(Vec<u8>, RecordId)> {
        (0..self.entry_count()).map(|i| (self.key(i).to_vec(), self.record_id(i))).collect()
    }

    /// `key`を二分探索する。`Ok(i)`は`key(i) == key`である一致、`Err(i)`は
    /// `key`が挿入されるべき位置(その位置より前のキーはすべて`key`未満)を表す。
    /// `key`と等しいキーが複数ある場合、`Ok`が指す`i`はそのいずれか(どれかは
    /// 未規定)であり、重複を漏れなく集めるには`i`から前後に走査する必要がある
    /// (`crate::btree`の呼び出し側を参照)。
    ///
    /// `(0..entry_count()).collect::<Vec<_>>()`のような添字配列をいったん
    /// 確保してから`slice::binary_search_by`に渡す実装は書かない。それでは
    /// 1回の探索がエントリ数に比例した`Vec`確保を伴い、ページ内探索が
    /// `O(log n)`であるという設計の前提(`crate::btree`本文の実測を参照)が
    /// 崩れてしまう。代わりに`lo`・`hi`だけを持つ二分探索を手で書き、
    /// ページの`payload`を読むだけで比較する。
    pub fn find(&self, key: &[u8]) -> Result<usize, usize> {
        let mut lo = 0usize;
        let mut hi = self.entry_count();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.key(mid).cmp(key) {
                std::cmp::Ordering::Equal => return Ok(mid),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Err(lo)
    }
}

/// `payload`をLeaf Pageとして読み書きするビュー。
pub struct LeafPage<'a> {
    payload: &'a mut [u8],
}

impl<'a> LeafPage<'a> {
    /// `payload`を空のLeaf Page(エントリ0件)として初期化する。
    pub fn init(payload: &'a mut [u8]) -> Self {
        payload.fill(0);
        payload[0..2].copy_from_slice(&0u16.to_le_bytes());
        LeafPage { payload }
    }

    /// すでにLeaf Pageとして初期化済みの`payload`を検証してから開く。
    pub fn open(payload: &'a mut [u8]) -> DbResult<Self> {
        validate(payload, LEAF_HEADER_SIZE, RECORD_ID_SIZE, "Leaf Page")?;
        Ok(LeafPage { payload })
    }

    /// 読み取り専用ビューへ変換する(`LeafPageRef`が持つ読み取りメソッドを
    /// そのまま使うためのヘルパー)。
    pub fn as_ref(&self) -> LeafPageRef<'_> {
        LeafPageRef { payload: self.payload }
    }

    /// エントリ数。
    pub fn entry_count(&self) -> usize {
        self.as_ref().entry_count()
    }

    /// `entries`(キー昇順である必要がある)でページの中身を丸ごと置き換える。
    /// `payload`に収まりきらない場合は何も書き換えず`false`を返す。
    pub fn write_entries(&mut self, entries: &[(Vec<u8>, RecordId)]) -> bool {
        let needed = required_len(LEAF_HEADER_SIZE, entries.iter().map(|(k, _)| k.len()), RECORD_ID_SIZE);
        if needed > self.payload.len() {
            return false;
        }

        self.payload.fill(0);
        self.payload[0..2].copy_from_slice(&(entries.len() as u16).to_le_bytes());

        let dir_end = LEAF_HEADER_SIZE + entries.len() * DIR_ENTRY_SIZE;
        let mut cursor = dir_end;
        for (i, (key, rid)) in entries.iter().enumerate() {
            let dir_base = LEAF_HEADER_SIZE + i * DIR_ENTRY_SIZE;
            self.payload[dir_base..dir_base + 2].copy_from_slice(&(cursor as u16).to_le_bytes());
            self.payload[dir_base + 2..dir_base + 4].copy_from_slice(&(key.len() as u16).to_le_bytes());

            self.payload[cursor..cursor + key.len()].copy_from_slice(key);
            cursor += key.len();
            self.payload[cursor..cursor + 8].copy_from_slice(&rid.page_id.0.to_le_bytes());
            cursor += 8;
            self.payload[cursor..cursor + 2].copy_from_slice(&rid.slot_id.0.to_le_bytes());
            cursor += 2;
        }
        true
    }
}

/// `payload`をInternal Pageとして読み取り専用で開くビュー。
pub struct InternalPageRef<'a> {
    payload: &'a [u8],
}

impl<'a> InternalPageRef<'a> {
    /// すでにInternal Pageとして初期化済みの`payload`を検証してから開く。
    pub fn open(payload: &'a [u8]) -> DbResult<Self> {
        validate(payload, INTERNAL_HEADER_SIZE, CHILD_ID_SIZE, "Internal Page")?;
        Ok(InternalPageRef { payload })
    }

    /// 区切りキーの本数(子ページの本数は`entry_count() + 1`)。
    pub fn entry_count(&self) -> usize {
        read_u16(self.payload, 0) as usize
    }

    /// 0番目の子ページ。
    pub fn leftmost_child(&self) -> PageId {
        PageId(read_u64(self.payload, 2))
    }

    /// `index`番目(0始まり、キー昇順)の区切りキーのバイト列。
    pub fn key(&self, index: usize) -> &[u8] {
        let (offset, len) = dir_entry(self.payload, INTERNAL_HEADER_SIZE, index);
        &self.payload[offset..offset + len]
    }

    /// `index`番目の区切りキーの右側にある子ページ(`(index + 1)`番目の子)。
    pub fn child_after(&self, index: usize) -> PageId {
        let (offset, len) = dir_entry(self.payload, INTERNAL_HEADER_SIZE, index);
        PageId(read_u64(self.payload, offset + len))
    }

    /// 全区切りキーを`(キーのバイト列, そのキーの右側の子)`の`Vec`として取り出す。
    pub fn entries(&self) -> Vec<(Vec<u8>, PageId)> {
        (0..self.entry_count()).map(|i| (self.key(i).to_vec(), self.child_after(i))).collect()
    }

    /// `key`を探索し、その`key`が属すべき子ページを返す。
    ///
    /// 区切りキー`key_i`は「`key_i`以上のキーは`child_after(i)`以降にある」
    /// という境界を表すため、`key`以下の区切りキーの本数を二分探索で数える
    /// ことで、`key`を含みうる唯一の子が求まる。[`LeafPageRef::find`]と同じ
    /// 理由で、添字配列を確保する実装は避け`lo`・`hi`だけで探索する。
    pub fn child_for(&self, key: &[u8]) -> PageId {
        let mut lo = 0usize;
        let mut hi = self.entry_count();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 { self.leftmost_child() } else { self.child_after(lo - 1) }
    }
}

/// `payload`をInternal Pageとして読み書きするビュー。
pub struct InternalPage<'a> {
    payload: &'a mut [u8],
}

impl<'a> InternalPage<'a> {
    /// `payload`を、`leftmost_child`だけを子に持つ空のInternal Page
    /// (区切りキー0本)として初期化する。
    pub fn init(payload: &'a mut [u8], leftmost_child: PageId) -> Self {
        payload.fill(0);
        payload[0..2].copy_from_slice(&0u16.to_le_bytes());
        payload[2..10].copy_from_slice(&leftmost_child.0.to_le_bytes());
        InternalPage { payload }
    }

    /// すでにInternal Pageとして初期化済みの`payload`を検証してから開く。
    pub fn open(payload: &'a mut [u8]) -> DbResult<Self> {
        validate(payload, INTERNAL_HEADER_SIZE, CHILD_ID_SIZE, "Internal Page")?;
        Ok(InternalPage { payload })
    }

    /// 読み取り専用ビューへ変換する。
    pub fn as_ref(&self) -> InternalPageRef<'_> {
        InternalPageRef { payload: self.payload }
    }

    /// 区切りキーの本数。
    pub fn entry_count(&self) -> usize {
        self.as_ref().entry_count()
    }

    /// `leftmost_child`と`entries`(区切りキー昇順である必要がある)で
    /// ページの中身を丸ごと置き換える。`payload`に収まりきらない場合は
    /// 何も書き換えず`false`を返す。
    pub fn write_entries(&mut self, leftmost_child: PageId, entries: &[(Vec<u8>, PageId)]) -> bool {
        let needed = required_len(INTERNAL_HEADER_SIZE, entries.iter().map(|(k, _)| k.len()), CHILD_ID_SIZE);
        if needed > self.payload.len() {
            return false;
        }

        self.payload.fill(0);
        self.payload[0..2].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        self.payload[2..10].copy_from_slice(&leftmost_child.0.to_le_bytes());

        let dir_end = INTERNAL_HEADER_SIZE + entries.len() * DIR_ENTRY_SIZE;
        let mut cursor = dir_end;
        for (i, (key, child)) in entries.iter().enumerate() {
            let dir_base = INTERNAL_HEADER_SIZE + i * DIR_ENTRY_SIZE;
            self.payload[dir_base..dir_base + 2].copy_from_slice(&(cursor as u16).to_le_bytes());
            self.payload[dir_base + 2..dir_base + 4].copy_from_slice(&(key.len() as u16).to_le_bytes());

            self.payload[cursor..cursor + key.len()].copy_from_slice(key);
            cursor += key.len();
            self.payload[cursor..cursor + 8].copy_from_slice(&child.0.to_le_bytes());
            cursor += 8;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PAGE_PAYLOAD_SIZE;

    fn fresh_payload() -> Vec<u8> {
        vec![0u8; PAGE_PAYLOAD_SIZE]
    }

    fn rid(page: u64, slot: u16) -> RecordId {
        RecordId::new(PageId(page), SlotId(slot))
    }

    #[test]
    fn leaf_write_then_read_round_trips() {
        let mut payload = fresh_payload();
        let mut page = LeafPage::init(&mut payload);
        let entries = vec![(b"a".to_vec(), rid(1, 0)), (b"b".to_vec(), rid(1, 1)), (b"c".to_vec(), rid(2, 0))];
        assert!(page.write_entries(&entries));
        assert_eq!(page.entry_count(), 3);

        let view = LeafPageRef::open(&payload).unwrap();
        assert_eq!(view.entries(), entries);
        assert_eq!(view.find(b"b"), Ok(1));
        assert_eq!(view.find(b"ab"), Err(1));
        assert_eq!(view.find(b"z"), Err(3));
    }

    #[test]
    fn leaf_write_entries_fails_without_mutating_when_too_large() {
        let mut payload = fresh_payload();
        let mut page = LeafPage::init(&mut payload);
        let original = vec![(b"x".to_vec(), rid(1, 0))];
        assert!(page.write_entries(&original));

        let huge_key = vec![b'k'; PAGE_PAYLOAD_SIZE];
        assert!(!page.write_entries(&[(huge_key, rid(9, 9))]));
        // 失敗した書き込みは既存の中身を変えない。
        assert_eq!(page.as_ref().entries(), original);
    }

    #[test]
    fn leaf_open_rejects_out_of_order_keys() {
        let mut payload = fresh_payload();
        {
            let mut page = LeafPage::init(&mut payload);
            page.write_entries(&[(b"b".to_vec(), rid(1, 0)), (b"a".to_vec(), rid(1, 1))]);
        }
        // write_entriesは呼び出し側が渡した順序をそのまま書くため、昇順でない
        // 配列を渡すとDirectory自体は書けてしまう。openはこれを壊れた
        // ページとして検出する。
        assert!(matches!(LeafPageRef::open(&payload), Err(DbError::CorruptPage(_))));
    }

    #[test]
    fn leaf_open_rejects_a_payload_shorter_than_the_header() {
        assert!(matches!(LeafPageRef::open(&[0u8]), Err(DbError::CorruptPage(_))));
    }

    #[test]
    fn leaf_open_rejects_an_entry_whose_range_overruns_the_payload() {
        let mut payload = fresh_payload();
        payload[0..2].copy_from_slice(&1u16.to_le_bytes());
        // key_offsetをpayloadの外へ出す。
        payload[2..4].copy_from_slice(&(PAGE_PAYLOAD_SIZE as u16 - 1).to_le_bytes());
        payload[4..6].copy_from_slice(&10u16.to_le_bytes());
        assert!(matches!(LeafPageRef::open(&payload), Err(DbError::CorruptPage(_))));
    }

    #[test]
    fn internal_write_then_read_round_trips() {
        let mut payload = fresh_payload();
        let mut page = InternalPage::init(&mut payload, PageId(10));
        let entries = vec![(b"m".to_vec(), PageId(20)), (b"t".to_vec(), PageId(30))];
        assert!(page.write_entries(PageId(10), &entries));

        let view = InternalPageRef::open(&payload).unwrap();
        assert_eq!(view.entries(), entries);
        assert_eq!(view.leftmost_child(), PageId(10));
        // "m"未満はleftmost_child、"m"以上"t"未満は"m"の右側、"t"以上は"t"の右側。
        assert_eq!(view.child_for(b"a"), PageId(10));
        assert_eq!(view.child_for(b"m"), PageId(20));
        assert_eq!(view.child_for(b"q"), PageId(20));
        assert_eq!(view.child_for(b"t"), PageId(30));
        assert_eq!(view.child_for(b"z"), PageId(30));
    }

    #[test]
    fn internal_write_entries_fails_without_mutating_when_too_large() {
        let mut payload = fresh_payload();
        let mut page = InternalPage::init(&mut payload, PageId(1));
        let original = vec![(b"x".to_vec(), PageId(2))];
        assert!(page.write_entries(PageId(1), &original));

        let huge_key = vec![b'k'; PAGE_PAYLOAD_SIZE];
        assert!(!page.write_entries(PageId(1), &[(huge_key, PageId(3))]));
        assert_eq!(page.as_ref().entries(), original);
    }
}
