//! `Page`の`payload`(第11章)に可変長レコードを詰めるSlotted Page。
//!
//! この章はまだファイルI/Oを行わない。`SlottedPage`は`&mut [u8]`(典型的には
//! `Page::payload_mut()`が返すスライス)を借用してその中身だけを読み書きする、
//! 独立した部品である。Heap Fileとしてページをまたいでテーブルを構成する仕事は
//! 第13章に引き継ぐ。
//!
//! # payload内のレイアウト
//!
//! `payload`は次の3領域に分かれる。Slot Directoryは先頭から後ろへ、
//! Tuple Dataは末尾から前へ向かって、互いに向き合う形で成長する。
//!
//! ```text
//! 0                                                        payload.len()
//! +----------+----------------+---------------+---------------+
//! | Header   | Slot Directory |  Free Space   |   Tuple Data   |
//! | (4バイト)|       →        |               |        ←       |
//! +----------+----------------+---------------+---------------+
//!            ^                                ^
//!            SLOTTED_HEADER_SIZE          tuple_data_start
//! ```
//!
//! **不変条件**: 常に`SLOTTED_HEADER_SIZE + slot_count * SLOT_ENTRY_SIZE <=
//! tuple_data_start`が成り立つ。すなわちSlot DirectoryとTuple Dataは
//! 互いに侵食しない。この章のすべての操作(`insert`・`update`・`compact`)は、
//! 書き込み前にこの条件を満たせるかを確認し、満たせない場合は失敗を返す。

use crate::ids::SlotId;

/// payload先頭に置くヘッダーのバイト数(`slot_count` 2 + `tuple_data_start` 2)。
pub const SLOTTED_HEADER_SIZE: usize = 4;

/// Slot Directoryの1エントリのバイト数
/// (`offset` 2 + `length` 2 + `status` 1 + 予約領域 3)。
///
/// | オフセット | バイト数 | フィールド | 内容 |
/// | --- | --- | --- | --- |
/// | 0 | 2 | `offset` | このスロットが指すTuple Data領域内の開始位置(LE) |
/// | 2 | 2 | `length` | タプルのバイト数(LE) |
/// | 4 | 1 | `status` | `1`(Occupied)または`2`(Tombstone) |
/// | 5 | 3 | (予約領域) | 常に0。将来のスロット拡張用 |
pub const SLOT_ENTRY_SIZE: usize = 8;

const STATUS_OCCUPIED: u8 = 1;
const STATUS_TOMBSTONE: u8 = 2;

/// スロットが指すタプルの生死。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    /// 有効なタプルを指している。
    Occupied,
    /// かつて`Occupied`だったが`delete`済みで、`compact`または`insert`による
    /// 再利用を待っている。
    Tombstone,
}

/// `Page`の`payload`を借用し、Slotted Pageとして読み書きするビュー。
///
/// このビュー自身はどのバイトも所有せず、借用元の`payload`を書き換える。
/// `Page`との組み合わせ方は、呼び出し側が`SlottedPage::init(page.payload_mut())`
/// あるいは`SlottedPage::open(page.payload_mut())`を呼ぶだけでよい。
pub struct SlottedPage<'a> {
    payload: &'a mut [u8],
}

impl<'a> SlottedPage<'a> {
    /// `payload`をSlotted Pageとして初期化する(スロット0件、空き領域は全体)。
    ///
    /// `Page::new`が返す全バイト0の`payload`にも、それ以外の中身が残っている
    /// スライスにも使ってよい。どちらの場合も既存の中身は破棄され、
    /// 空のSlotted Pageとして上書きされる。
    pub fn init(payload: &'a mut [u8]) -> Self {
        let capacity = payload.len();
        assert!(
            capacity <= u16::MAX as usize,
            "payloadがu16に収まらないほど大きいです: {capacity}バイト"
        );
        let mut page = SlottedPage { payload };
        page.set_header(0, capacity as u16);
        page
    }

    /// すでにSlotted Pageとして初期化済みの`payload`をそのまま読み書きする。
    ///
    /// ヘッダーやスロットの内容はいっさい書き換えない。`init`していない
    /// スライス(全バイト0を含む)に対して呼ぶと、ヘッダーが
    /// `slot_count = 0, tuple_data_start = 0`と解釈され、以後の`insert`が
    /// 常に空き領域不足で失敗する。
    pub fn open(payload: &'a mut [u8]) -> Self {
        SlottedPage { payload }
    }

    /// 現在のスロット数(Occupied・Tombstoneの両方を含む)。
    pub fn slot_count(&self) -> usize {
        self.header().0 as usize
    }

    /// Slot DirectoryとTuple Dataの間に残っている空きバイト数。
    ///
    /// Tombstone化されたタプルの死んだバイト列は空き領域に含まれない
    /// (`compact`を呼ぶまで回収されない)。
    pub fn free_space(&self) -> usize {
        let (slot_count, tuple_data_start) = self.header();
        tuple_data_start as usize - self.directory_end(slot_count)
    }

    /// 指定したスロットの状態を返す。スロットが存在しなければ`None`。
    pub fn status(&self, slot: SlotId) -> Option<SlotStatus> {
        let (_, _, status) = self.slot_entry(slot)?;
        Some(match status {
            STATUS_OCCUPIED => SlotStatus::Occupied,
            STATUS_TOMBSTONE => SlotStatus::Tombstone,
            other => unreachable!("未知のslot status: {other}"),
        })
    }

    /// スロットが指すタプルのバイト列を返す。
    ///
    /// スロットが存在しない、または`Tombstone`(削除済み)の場合は`None`。
    pub fn get(&self, slot: SlotId) -> Option<&[u8]> {
        let (offset, length, status) = self.slot_entry(slot)?;
        if status != STATUS_OCCUPIED {
            return None;
        }
        let start = offset as usize;
        Some(&self.payload[start..start + length as usize])
    }

    /// `bytes`を新しいタプルとして挿入し、そのタプルを指す`SlotId`を返す。
    ///
    /// まずTombstone化済みのスロット(最小のスロット番号を優先する)を探し、
    /// 見つかればSlot Directoryを増やさずに再利用する。見つからなければ
    /// 新しいスロットをDirectoryの末尾に追加する。どちらの場合も、
    /// 現在の空き領域だけでは足りなければ1度`compact`してから再試行する。
    /// それでも入らなければ`None`を返す(このページには物理的に空きがない)。
    pub fn insert(&mut self, bytes: &[u8]) -> Option<SlotId> {
        if let Some(slot) = self.try_insert(bytes) {
            return Some(slot);
        }
        self.compact();
        self.try_insert(bytes)
    }

    fn try_insert(&mut self, bytes: &[u8]) -> Option<SlotId> {
        let len = bytes.len();
        let (slot_count, tuple_data_start) = self.header();

        let reuse = (0..slot_count)
            .map(SlotId)
            .find(|&s| matches!(self.slot_entry(s), Some((_, _, STATUS_TOMBSTONE))));

        let grows_directory = reuse.is_none();
        let directory_end = self.directory_end(slot_count + u16::from(grows_directory));
        if directory_end + len > tuple_data_start as usize {
            return None;
        }

        let new_start = tuple_data_start as usize - len;
        self.payload[new_start..new_start + len].copy_from_slice(bytes);

        let slot = reuse.unwrap_or(SlotId(slot_count));
        self.set_slot_entry(slot, new_start as u16, len as u16, STATUS_OCCUPIED);
        let new_slot_count = slot_count + u16::from(grows_directory);
        self.set_header(new_slot_count, new_start as u16);
        Some(slot)
    }

    /// 指定したスロットをTombstone化する。
    ///
    /// タプルのバイト列自体は書き換えない(すぐ後で`get`されても困らないよう
    /// 消去する必要はない)。物理的な空き領域としての回収は`compact`が行う。
    /// 対象のスロットが存在しない、またはすでに`Tombstone`なら`false`を返す。
    pub fn delete(&mut self, slot: SlotId) -> bool {
        match self.slot_entry(slot) {
            Some((offset, length, STATUS_OCCUPIED)) => {
                self.set_slot_entry(slot, offset, length, STATUS_TOMBSTONE);
                true
            }
            _ => false,
        }
    }

    /// 指定したスロットが指すタプルを`bytes`へ置き換える。
    ///
    /// 新しいバイト数が元と同じなら、Tuple Data領域の同じ位置へそのまま
    /// 上書きする(Slot Directoryは変更しない)。サイズが変わる場合は、
    /// 新しい領域へ書き直したうえでスロットの`offset`・`length`だけ
    /// 付け替える。元の位置は死んだバイト列として残り、`compact`が回収する。
    /// 現在の空き領域に新しいバイト列が入らなければ1度`compact`してから
    /// 再試行し、それでも入らなければ`false`を返す(対象は元のまま残る)。
    /// 対象のスロットが存在しない、または`Tombstone`なら`false`を返す。
    pub fn update(&mut self, slot: SlotId, bytes: &[u8]) -> bool {
        let Some((offset, length, status)) = self.slot_entry(slot) else {
            return false;
        };
        if status != STATUS_OCCUPIED {
            return false;
        }

        if bytes.len() == length as usize {
            let start = offset as usize;
            self.payload[start..start + bytes.len()].copy_from_slice(bytes);
            return true;
        }

        if self.try_relocate(slot, bytes) {
            return true;
        }
        self.compact();
        self.try_relocate(slot, bytes)
    }

    fn try_relocate(&mut self, slot: SlotId, bytes: &[u8]) -> bool {
        let len = bytes.len();
        let (slot_count, tuple_data_start) = self.header();
        let directory_end = self.directory_end(slot_count);
        if directory_end + len > tuple_data_start as usize {
            return false;
        }
        let new_start = tuple_data_start as usize - len;
        self.payload[new_start..new_start + len].copy_from_slice(bytes);
        self.set_slot_entry(slot, new_start as u16, len as u16, STATUS_OCCUPIED);
        self.set_header(slot_count, new_start as u16);
        true
    }

    /// Tuple Data領域を、Occupiedなタプルだけで隙間なく詰め直す。
    ///
    /// Tombstone化されたタプルの死んだバイト列と、`update`でサイズが
    /// 変わった際に取り残された古いバイト列は、この操作でまとめて回収される。
    /// Slot Directory自体(スロット数、Tombstoneのままのエントリ)は変更しない。
    /// 各`SlotId`が指すタプルの中身は、コンパクションの前後で変わらない。
    pub fn compact(&mut self) {
        let (slot_count, _) = self.header();

        let mut live: Vec<(SlotId, Vec<u8>)> = Vec::new();
        for i in 0..slot_count {
            let slot = SlotId(i);
            if let Some((offset, length, STATUS_OCCUPIED)) = self.slot_entry(slot) {
                let start = offset as usize;
                live.push((slot, self.payload[start..start + length as usize].to_vec()));
            }
        }

        let mut cursor = self.payload.len();
        for (slot, bytes) in &live {
            cursor -= bytes.len();
            self.payload[cursor..cursor + bytes.len()].copy_from_slice(bytes);
            self.set_slot_entry(*slot, cursor as u16, bytes.len() as u16, STATUS_OCCUPIED);
        }
        self.set_header(slot_count, cursor as u16);
    }

    fn directory_end(&self, slot_count: u16) -> usize {
        SLOTTED_HEADER_SIZE + slot_count as usize * SLOT_ENTRY_SIZE
    }

    fn header(&self) -> (u16, u16) {
        let slot_count = u16::from_le_bytes(self.payload[0..2].try_into().unwrap());
        let tuple_data_start = u16::from_le_bytes(self.payload[2..4].try_into().unwrap());
        (slot_count, tuple_data_start)
    }

    fn set_header(&mut self, slot_count: u16, tuple_data_start: u16) {
        self.payload[0..2].copy_from_slice(&slot_count.to_le_bytes());
        self.payload[2..4].copy_from_slice(&tuple_data_start.to_le_bytes());
    }

    fn slot_entry(&self, slot: SlotId) -> Option<(u16, u16, u8)> {
        let (slot_count, _) = self.header();
        if slot.0 >= slot_count {
            return None;
        }
        let base = SLOTTED_HEADER_SIZE + slot.0 as usize * SLOT_ENTRY_SIZE;
        let offset = u16::from_le_bytes(self.payload[base..base + 2].try_into().unwrap());
        let length = u16::from_le_bytes(self.payload[base + 2..base + 4].try_into().unwrap());
        let status = self.payload[base + 4];
        Some((offset, length, status))
    }

    fn set_slot_entry(&mut self, slot: SlotId, offset: u16, length: u16, status: u8) {
        let base = SLOTTED_HEADER_SIZE + slot.0 as usize * SLOT_ENTRY_SIZE;
        self.payload[base..base + 2].copy_from_slice(&offset.to_le_bytes());
        self.payload[base + 2..base + 4].copy_from_slice(&length.to_le_bytes());
        self.payload[base + 4] = status;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PAGE_PAYLOAD_SIZE;

    fn fresh_payload() -> Vec<u8> {
        vec![0u8; PAGE_PAYLOAD_SIZE]
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let slot = page.insert(b"hello").unwrap();
        assert_eq!(page.get(slot), Some(&b"hello"[..]));
        assert_eq!(page.slot_count(), 1);
    }

    #[test]
    fn multiple_inserts_keep_each_slot_independent() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let s1 = page.insert(b"alice").unwrap();
        let s2 = page.insert(b"bob").unwrap();
        let s3 = page.insert(b"carol").unwrap();

        assert_ne!(s1, s2);
        assert_ne!(s2, s3);
        assert_eq!(page.get(s1), Some(&b"alice"[..]));
        assert_eq!(page.get(s2), Some(&b"bob"[..]));
        assert_eq!(page.get(s3), Some(&b"carol"[..]));
    }

    #[test]
    fn get_on_unknown_slot_returns_none() {
        let mut payload = fresh_payload();
        let page = SlottedPage::init(&mut payload);
        assert_eq!(page.get(SlotId(0)), None);
        assert_eq!(page.get(SlotId(999)), None);
    }

    #[test]
    fn insert_returns_none_when_page_is_full() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        // ページ本体をほぼ埋め尽くす1件を先に入れておく。
        let big = vec![b'x'; PAGE_PAYLOAD_SIZE - SLOTTED_HEADER_SIZE - SLOT_ENTRY_SIZE - 4];
        let slot = page.insert(&big).unwrap();
        assert_eq!(page.get(slot), Some(big.as_slice()));

        // 残りの空きより大きいタプルは入らない。
        assert_eq!(page.insert(b"12345678"), None);
        // 既存のタプルは無事なまま。
        assert_eq!(page.get(slot), Some(big.as_slice()));
    }

    #[test]
    fn delete_marks_tombstone_and_hides_the_tuple() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let slot = page.insert(b"gone soon").unwrap();
        assert!(page.delete(slot));

        assert_eq!(page.get(slot), None);
        assert_eq!(page.status(slot), Some(SlotStatus::Tombstone));
        // Tombstone化してもスロット自体はDirectoryに残る。
        assert_eq!(page.slot_count(), 1);
    }

    #[test]
    fn delete_twice_fails_the_second_time() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let slot = page.insert(b"x").unwrap();
        assert!(page.delete(slot));
        assert!(!page.delete(slot));
    }

    #[test]
    fn insert_reuses_a_tombstoned_slot_id() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let s1 = page.insert(b"first").unwrap();
        let _s2 = page.insert(b"second").unwrap();
        page.delete(s1);
        assert_eq!(page.slot_count(), 2);

        let s3 = page.insert(b"third").unwrap();
        // Directoryを増やさず、Tombstone化済みのs1をそのまま使い回す。
        assert_eq!(s3, s1);
        assert_eq!(page.slot_count(), 2);
        assert_eq!(page.get(s3), Some(&b"third"[..]));
    }

    #[test]
    fn compact_preserves_tuple_contents_by_slot_id() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let s1 = page.insert(b"alice").unwrap();
        let s2 = page.insert(b"bob").unwrap();
        let s3 = page.insert(b"carol").unwrap();
        page.delete(s2);

        let free_before = page.free_space();
        page.compact();
        let free_after = page.free_space();

        // 死んだbobの領域が回収され、空き領域が増えている。
        assert!(free_after > free_before);
        // 生きているタプルの中身はスロットIDを介して変わらず読める。
        assert_eq!(page.get(s1), Some(&b"alice"[..]));
        assert_eq!(page.get(s3), Some(&b"carol"[..]));
        assert_eq!(page.get(s2), None);
        assert_eq!(page.status(s2), Some(SlotStatus::Tombstone));
    }

    #[test]
    fn insert_triggers_compaction_when_fragmented_space_is_enough() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        // 大きめのタプルで埋めたあと、真ん中を消して断片化させる。
        let chunk = vec![b'a'; 800];
        let s1 = page.insert(&chunk).unwrap();
        let s2 = page.insert(&chunk).unwrap();
        let s3 = page.insert(&chunk).unwrap();
        page.delete(s2);

        // 直後の空き領域(Directoryとs3の間)だけでは入らないが、
        // s2の死んだ領域まで回収すれば入るサイズのタプルを挿入する。
        let recovered = vec![b'b'; 700];
        let slot = page.insert(&recovered).unwrap();

        assert_eq!(page.get(slot), Some(recovered.as_slice()));
        assert_eq!(page.get(s1), Some(chunk.as_slice()));
        assert_eq!(page.get(s3), Some(chunk.as_slice()));
    }

    #[test]
    fn update_with_same_size_overwrites_in_place() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let slot = page.insert(b"aaaaa").unwrap();
        let slot_count_before = page.slot_count();
        assert!(page.update(slot, b"bbbbb"));

        assert_eq!(page.get(slot), Some(&b"bbbbb"[..]));
        assert_eq!(page.slot_count(), slot_count_before);
    }

    #[test]
    fn update_with_larger_size_relocates_the_tuple() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let slot = page.insert(b"short").unwrap();
        assert!(page.update(slot, b"a much longer replacement value"));

        assert_eq!(
            page.get(slot),
            Some(&b"a much longer replacement value"[..])
        );
        // SlotIdは変わらない(RIDの安定性)。
        assert_eq!(page.slot_count(), 1);
    }

    #[test]
    fn update_with_smaller_size_shrinks_in_place_via_relocation() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let slot = page.insert(b"a much longer original value").unwrap();
        assert!(page.update(slot, b"short"));
        assert_eq!(page.get(slot), Some(&b"short"[..]));
    }

    #[test]
    fn update_on_tombstoned_slot_fails() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let slot = page.insert(b"x").unwrap();
        page.delete(slot);
        assert!(!page.update(slot, b"y"));
    }

    #[test]
    fn update_returns_false_and_leaves_tuple_intact_when_page_stays_full() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);

        let big = vec![b'x'; PAGE_PAYLOAD_SIZE - SLOTTED_HEADER_SIZE - SLOT_ENTRY_SIZE * 2 - 16];
        let s1 = page.insert(&big).unwrap();
        let s2 = page.insert(b"tiny").unwrap();

        // s1を大幅に大きくしようとしても、空きがないので失敗し、元の値のまま残る。
        let too_big = vec![b'y'; PAGE_PAYLOAD_SIZE];
        assert!(!page.update(s1, &too_big));
        assert_eq!(page.get(s1), Some(big.as_slice()));
        assert_eq!(page.get(s2), Some(&b"tiny"[..]));
    }

    /// テスト専用の決定的な疑似乱数生成器(xorshift64)。
    ///
    /// `proptest`のような依存を増やさず、シードを固定した単純な乱数で
    /// ランダム挿入/削除の性質を検証する。シードを固定するのは、
    /// テストが失敗したときに同じ操作列を再現できるようにするためである。
    struct Xorshift64(u64);

    impl Xorshift64 {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn range(&mut self, bound: usize) -> usize {
            (self.next() as usize) % bound
        }
    }

    #[test]
    fn random_insert_delete_keeps_slotted_page_consistent() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);
        let mut rng = Xorshift64(0x1234_5678_9abc_def1);

        // (slot, 挿入したバイト列)のうち、まだ削除していないものを追跡する。
        let mut alive: Vec<(SlotId, Vec<u8>)> = Vec::new();

        for i in 0..500 {
            let insert_bias = alive.len() < 4; // 空になりすぎて操作が偏らないようにする。
            let do_insert = insert_bias || rng.range(3) != 0;

            if do_insert {
                let len = 1 + rng.range(64);
                let byte = (i % 251) as u8;
                let bytes: Vec<u8> = vec![byte; len];
                if let Some(slot) = page.insert(&bytes) {
                    alive.retain(|(s, _)| *s != slot);
                    alive.push((slot, bytes));
                }
                // 挿入が失敗する(ページが本当に満杯)のは許容し、以降の操作を続ける。
            } else {
                let idx = rng.range(alive.len());
                let (slot, _) = alive.remove(idx);
                assert!(page.delete(slot));
            }

            // 生きていると思っているスロットは、常にその通りのバイト列を返す。
            for (slot, bytes) in &alive {
                assert_eq!(page.get(*slot), Some(bytes.as_slice()));
            }
        }

        // 最後にコンパクションしても、生きているタプルの中身は変わらない。
        page.compact();
        for (slot, bytes) in &alive {
            assert_eq!(page.get(*slot), Some(bytes.as_slice()));
        }
    }
}
