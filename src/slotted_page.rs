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
//! 互いに侵食しない。`init`は空のSlotted Pageとしてこの条件を満たす状態から
//! 始め、`insert`・`update`はいずれも書き込み前にこの条件を保てるかを確認し、
//! 保てない場合は書き込みを行わずに失敗を返す(`compact`は既存のスロットを
//! 動かすだけで新たに確保しないため、そもそも失敗しうる操作ではない)。
//! `open`は、すでにこの条件を満たしている`payload`を読み書きする入口であり、
//! 妥当な初期状態から始めて`insert`・`update`だけを経由してきたバイト列で
//! あればこの条件は常に保たれる。`open`自身は、その前提を無条件には信用しない。
//! ヘッダーがこの条件と`payload`の大きさに対して妥当かどうかに加え、
//! 各スロットの`status`が既知の値であること、Occupiedなスロットの
//! タプルデータ範囲がTuple Data領域に収まり、かつ互いに重なっていないことまで
//! 検証し、破損した(あるいは別のPageTypeの)バイト列を渡された場合は
//! `DbError::CorruptPage`を返す(検証の詳細は`validate_header`を参照)。

use crate::error::{DbError, DbResult};
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
    /// `payload`の大きさ・ヘッダー(`slot_count`、`tuple_data_start`)・
    /// 各スロットの`status`・Occupiedなスロットのタプルデータ範囲を検証し、
    /// いずれかが妥当でなければ`DbError::CorruptPage`を返す(検証の詳細は
    /// `validate_header`を参照)。検証を通過した`payload`に対しては、以後の
    /// メソッドがスライス添字アクセスでpanicすることはない。`init`していない
    /// スライス(全バイト0を含む)に対して呼ぶと、ヘッダーが
    /// `slot_count = 0, tuple_data_start = 0`と解釈され、この検証は通る
    /// (以後の`insert`が常に空き領域不足で失敗する)。
    pub fn open(payload: &'a mut [u8]) -> DbResult<Self> {
        validate_header(payload)?;
        Ok(SlottedPage { payload })
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

    /// Occupiedなスロットが1つも無いかどうか(第39章、`VACUUM`)。
    ///
    /// スロット数が0のページ(一度も使われていないページ)だけでなく、
    /// 全スロットがTombstone化されたページも`true`を返す。`VACUUM`は
    /// この判定でページ全体をFree Page Listへ返せるかどうかを決める。
    pub fn is_empty(&self) -> bool {
        let (slot_count, _) = self.header();
        (0..slot_count).all(|i| !matches!(self.slot_entry(SlotId(i)), Some((_, _, STATUS_OCCUPIED))))
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
        directory_end(slot_count)
    }

    fn header(&self) -> (u16, u16) {
        read_header(self.payload)
    }

    fn set_header(&mut self, slot_count: u16, tuple_data_start: u16) {
        self.payload[0..2].copy_from_slice(&slot_count.to_le_bytes());
        self.payload[2..4].copy_from_slice(&tuple_data_start.to_le_bytes());
    }

    fn slot_entry(&self, slot: SlotId) -> Option<(u16, u16, u8)> {
        read_slot_entry(self.payload, slot)
    }

    fn set_slot_entry(&mut self, slot: SlotId, offset: u16, length: u16, status: u8) {
        let base = SLOTTED_HEADER_SIZE + slot.0 as usize * SLOT_ENTRY_SIZE;
        self.payload[base..base + 2].copy_from_slice(&offset.to_le_bytes());
        self.payload[base + 2..base + 4].copy_from_slice(&length.to_le_bytes());
        self.payload[base + 4] = status;
    }
}

/// スロット0件の(空の)`payload`へ、新しいタプルとして挿入できる最大バイト数。
///
/// `SlottedPage::try_insert`が新規スロットを1つ追加する際の判定
/// (`directory_end(1) + len > tuple_data_start`。空の`payload`では
/// `tuple_data_start == payload_len`)を、ページの中身にいっさい触れずに
/// 事前計算するためのヘルパーである。呼び出し側(`HeapFile::insert`・
/// `Storage::insert`)は、これを使って実際にページを確保・初期化する前に
/// `bytes.len()`がそもそも収まりうるかを判定できる。ここで弾いておかないと、
/// 収まらないと分かるのが`SlottedPage::init(..).insert(bytes)`を呼んだ後に
/// なり、その時点ではすでにFree Page Listからページを1枚取り出したり、
/// `BufferPool::allocate_page`でファイルを1ページ伸ばしたりした後である。
pub fn max_len_for_fresh_page(payload_len: usize) -> usize {
    payload_len.saturating_sub(SLOTTED_HEADER_SIZE + SLOT_ENTRY_SIZE)
}

/// Slot DirectoryとTuple Dataの境界(`payload`先頭からのバイト数)。
///
/// `SlottedPage`と`SlottedPageRef`の両方から、`payload`が可変か不変かに
/// 関係なく呼べるよう、`payload`を受け取らない自由関数にしてある。
fn directory_end(slot_count: u16) -> usize {
    SLOTTED_HEADER_SIZE + slot_count as usize * SLOT_ENTRY_SIZE
}

/// `payload`先頭のヘッダー(`slot_count`、`tuple_data_start`)を読む。
///
/// `SlottedPage`と`SlottedPageRef`の両方が使う共通ロジックで、読み取りだけで
/// 完結するため`&[u8]`を受け取る自由関数として`impl`の外に出してある。
fn read_header(payload: &[u8]) -> (u16, u16) {
    let slot_count = u16::from_le_bytes(payload[0..2].try_into().unwrap());
    let tuple_data_start = u16::from_le_bytes(payload[2..4].try_into().unwrap());
    (slot_count, tuple_data_start)
}

/// `payload`のヘッダーとSlot Directory全体が、この章の不変条件を満たしているかを
/// 検証する。
///
/// `SlottedPage::open`と`SlottedPageRef::open`の両方が使う共通ロジックで、
/// 破損した(あるいは他のPageTypeの)バイト列を`payload`としてそのまま渡された
/// 場合に、以後のスライス添字アクセスでpanicする代わりに`DbError::CorruptPage`
/// を返せるようにする。検証は次の4段階からなる。
///
/// 1. `payload`自体が最低でもヘッダー(`SLOTTED_HEADER_SIZE`バイト)を持つこと。
///    これより短いと、ヘッダーを読む`read_header`自身がスライス添字でpanicする。
/// 2. ヘッダーの`slot_count`・`tuple_data_start`が、この章の不変条件
///    (`SLOTTED_HEADER_SIZE + slot_count * SLOT_ENTRY_SIZE <= tuple_data_start
///    <= payload.len()`)を満たすこと。
/// 3. 各スロットの`status`が`1`(Occupied)か`2`(Tombstone)のどちらかであること。
///    それ以外の値は`status`・`get`の`match`が`unreachable!`でpanicする原因になる。
/// 4. `status`が`Occupied`のスロットについて、`offset`と`length`が指す範囲
///    `[offset, offset + length)`がTuple Data領域(`[tuple_data_start,
///    payload.len())`)に収まっていること。これが無いと`get`・`compact`の
///    スライス添字アクセスがpanicしうる。`Tombstone`のスロットは、`compact`が
///    生きているスロットだけを詰め直す際に古い`offset`・`length`をそのまま
///    残す(このモジュールの`compact`を参照)ため、`tuple_data_start`より手前を
///    指していても壊れているとは言えず、この検証の対象に含めない。実際、
///    `Tombstone`の`offset`・`length`は`get`・`compact`のどちらからも
///    参照されず、`payload`へのスライス添字アクセスには使われない。
///
/// Occupiedなスロット同士のタプルデータ範囲が重なっていないかも、4の検証と
/// 合わせてここで確認する。範囲の重なりはスライス添字アクセスのpanicには
/// つながらない(それぞれの範囲は単独では`payload`に収まっている)が、2つの
/// タプルが同じバイト列を指すという意味の壊れ方であり、後段のロジックが
/// 気づかずに読み進めると誤ったデータを返しかねない。この検査はOccupiedな
/// スロットを`offset`でソートしてから隣接する範囲だけを比べる
/// `O(slot_count log slot_count)`で行う。1ページに収まるスロット数は最大でも
/// 500程度(`PAGE_PAYLOAD_SIZE / SLOT_ENTRY_SIZE`)なので、`open`のたびに
/// 毎回この検査を行ってもコストは無視できる。長さ0のタプル(`start == end`)は
/// どのバイトも占有せず、他のどの範囲とも交差しえないため、この検査に渡す前に
/// 除外する(理由の詳細は下の実装のコメントを参照)。
fn validate_header(payload: &[u8]) -> DbResult<()> {
    if payload.len() < SLOTTED_HEADER_SIZE {
        return Err(DbError::CorruptPage(format!(
            "payloadがヘッダー({SLOTTED_HEADER_SIZE}バイト)より小さいです: {}バイト",
            payload.len()
        )));
    }

    let (slot_count, tuple_data_start) = read_header(payload);
    let tuple_data_start = tuple_data_start as usize;
    if tuple_data_start > payload.len() {
        return Err(DbError::CorruptPage(format!(
            "tuple_data_start({tuple_data_start})がpayloadの大きさ({})を超えています",
            payload.len()
        )));
    }
    if directory_end(slot_count) > tuple_data_start {
        return Err(DbError::CorruptPage(format!(
            "Slot Directoryの終端({})がtuple_data_start({tuple_data_start})を超えています",
            directory_end(slot_count)
        )));
    }

    let mut occupied_ranges: Vec<(usize, usize)> = Vec::new();
    for i in 0..slot_count {
        // iはslot_count未満であり、read_slot_entryはその範囲を`None`にしない。
        let (offset, length, status) = read_slot_entry(payload, SlotId(i))
            .expect("iはslot_count未満なのでread_slot_entryは必ずSomeを返す");
        match status {
            STATUS_OCCUPIED => {
                let start = offset as usize;
                // offset・lengthはどちらもu16なので、この加算はusizeの範囲で
                // 決してoverflowしない。
                let end = start + length as usize;
                if start < tuple_data_start || end > payload.len() {
                    return Err(DbError::CorruptPage(format!(
                        "スロット{i}(Occupied)のタプルデータ範囲[{start}, {end})が\
                         Tuple Data領域[{tuple_data_start}, {})からはみ出しています",
                        payload.len()
                    )));
                }
                // 長さ0のタプル(`start == end`、空の`&[]`をinsertした場合に
                // 実際に起こりうる)は、どのバイトも占有しないため他のどの
                // 範囲とも重なりえない。これを重複検査の対象へそのまま
                // 加えると、同じ`start`を持つ空区間と非空区間(たとえば
                // `[4079, 4079)`と`[4079, 4080)`)が、`start`だけをキーにした
                // ソートの並び順(どちらが先に来るかは不定)次第で誤って
                // 「重なっている」と判定されることがある。空区間はそもそも
                // 交差判定の対象になりえないので、ここで除外しておくのが
                // もっとも単純で取りこぼしのない直し方である。
                if start != end {
                    occupied_ranges.push((start, end));
                }
            }
            STATUS_TOMBSTONE => {}
            other => {
                return Err(DbError::CorruptPage(format!(
                    "スロット{i}のstatusが不正です: {other}\
                     (1(Occupied)か2(Tombstone)である必要があります)"
                )));
            }
        }
    }

    occupied_ranges.sort_unstable_by_key(|&(start, _)| start);
    for pair in occupied_ranges.windows(2) {
        let (_, prev_end) = pair[0];
        let (next_start, _) = pair[1];
        if next_start < prev_end {
            return Err(DbError::CorruptPage(
                "複数のOccupiedスロットのタプルデータ範囲が重なっています".to_string(),
            ));
        }
    }

    Ok(())
}

/// `slot`が指すSlot Directoryの1エントリ(`offset`、`length`、`status`)を読む。
///
/// `read_header`と同じ理由で`&[u8]`を受け取る自由関数にしてあり、
/// `SlottedPage`と`SlottedPageRef`の両方の`get`・`status`から呼ばれる。
fn read_slot_entry(payload: &[u8], slot: SlotId) -> Option<(u16, u16, u8)> {
    let (slot_count, _) = read_header(payload);
    if slot.0 >= slot_count {
        return None;
    }
    let base = SLOTTED_HEADER_SIZE + slot.0 as usize * SLOT_ENTRY_SIZE;
    let offset = u16::from_le_bytes(payload[base..base + 2].try_into().unwrap());
    let length = u16::from_le_bytes(payload[base + 2..base + 4].try_into().unwrap());
    let status = payload[base + 4];
    Some((offset, length, status))
}

/// `Page`の`payload`を読み取り専用で借用し、Slotted Pageとして読むだけのビュー。
///
/// `SlottedPage`との違いは、`&'a [u8]`だけから構築できる点と、`insert`・
/// `delete`・`update`・`compact`のような書き込み系のメソッドを一切持たない点
/// である。第14章の`BufferPool`が`PageReadGuard`(読み取り専用のRAII Guard)を
/// 返すようになったことで、書き込みを一切行わない`get`・`scan`のような経路でも
/// `&mut [u8]`を要求する`SlottedPage::open`を呼べない場面が生まれた。この
/// `SlottedPageRef`はその場面のために追加した、読み取り専用の入口である。
pub struct SlottedPageRef<'a> {
    payload: &'a [u8],
}

impl<'a> SlottedPageRef<'a> {
    /// すでにSlotted Pageとして初期化済みの`payload`を読み取り専用で開く。
    ///
    /// `SlottedPage::open`と同じ検証(`validate_header`を参照)を行い、
    /// 妥当でなければ`DbError::CorruptPage`を返す。それ以外のスロットの内容は
    /// 解釈するだけで書き換えない。
    pub fn open(payload: &'a [u8]) -> DbResult<Self> {
        validate_header(payload)?;
        Ok(SlottedPageRef { payload })
    }

    /// 現在のスロット数(Occupied・Tombstoneの両方を含む)。
    pub fn slot_count(&self) -> usize {
        read_header(self.payload).0 as usize
    }

    /// Slot DirectoryとTuple Dataの間に残っている空きバイト数。
    pub fn free_space(&self) -> usize {
        let (slot_count, tuple_data_start) = read_header(self.payload);
        tuple_data_start as usize - directory_end(slot_count)
    }

    /// 指定したスロットの状態を返す。スロットが存在しなければ`None`。
    pub fn status(&self, slot: SlotId) -> Option<SlotStatus> {
        let (_, _, status) = read_slot_entry(self.payload, slot)?;
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
        let (offset, length, status) = read_slot_entry(self.payload, slot)?;
        if status != STATUS_OCCUPIED {
            return None;
        }
        let start = offset as usize;
        Some(&self.payload[start..start + length as usize])
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
    fn open_rejects_a_corrupt_header_instead_of_panicking() {
        // tuple_data_startがpayload自体の大きさを超えている(壊れた)ヘッダー。
        let mut payload = fresh_payload();
        payload[0..2].copy_from_slice(&0u16.to_le_bytes()); // slot_count
        payload[2..4].copy_from_slice(&u16::MAX.to_le_bytes()); // tuple_data_start
        assert!(matches!(
            SlottedPage::open(&mut payload),
            Err(DbError::CorruptPage(_))
        ));
        assert!(matches!(
            SlottedPageRef::open(&payload),
            Err(DbError::CorruptPage(_))
        ));
    }

    #[test]
    fn open_rejects_a_slot_directory_that_overruns_tuple_data_start() {
        // slot_countが大きすぎて、Slot Directoryの終端がtuple_data_startを
        // 超えている(構造としては読めるが意味をなさない)ヘッダー。
        let mut payload = fresh_payload();
        payload[0..2].copy_from_slice(&u16::MAX.to_le_bytes()); // slot_count
        payload[2..4].copy_from_slice(&0u16.to_le_bytes()); // tuple_data_start
        assert!(matches!(
            SlottedPage::open(&mut payload),
            Err(DbError::CorruptPage(_))
        ));
    }

    #[test]
    fn open_rejects_a_payload_shorter_than_the_header_instead_of_panicking() {
        // 空のpayload(ヘッダーの4バイトすら無い)。read_headerがそのまま
        // 読もうとするとスライス添字でpanicする。
        assert!(matches!(
            SlottedPageRef::open(&[]),
            Err(DbError::CorruptPage(_))
        ));
        let mut empty: Vec<u8> = Vec::new();
        assert!(matches!(
            SlottedPage::open(&mut empty),
            Err(DbError::CorruptPage(_))
        ));
    }

    #[test]
    fn open_rejects_an_occupied_slot_whose_range_overruns_the_payload() {
        // ヘッダー自体(slot_count・tuple_data_startの関係)は妥当だが、
        // スロット0がOccupiedのまま、その[offset, offset+length)がpayloadの
        // 末尾をはみ出している。openを通してしまうと、後続のgetがスライス
        // 添字アクセスでpanicする。
        let mut payload = fresh_payload();
        payload[0..2].copy_from_slice(&1u16.to_le_bytes()); // slot_count = 1
        let tuple_data_start = (PAGE_PAYLOAD_SIZE - 10) as u16;
        payload[2..4].copy_from_slice(&tuple_data_start.to_le_bytes());
        // スロット0: offset=tuple_data_start、length=10のはずが、lengthを
        // 大きく偽ってpayloadの外へはみ出させる。
        let base = SLOTTED_HEADER_SIZE;
        payload[base..base + 2].copy_from_slice(&tuple_data_start.to_le_bytes()); // offset
        payload[base + 2..base + 4].copy_from_slice(&100u16.to_le_bytes()); // length
        payload[base + 4] = 1; // status = Occupied

        assert!(matches!(
            SlottedPageRef::open(&payload),
            Err(DbError::CorruptPage(_))
        ));
        assert!(matches!(
            SlottedPage::open(&mut payload),
            Err(DbError::CorruptPage(_))
        ));
    }

    #[test]
    fn open_rejects_a_slot_with_an_unknown_status_instead_of_panicking() {
        // ヘッダーもタプルデータ範囲も妥当だが、スロット0のstatusが
        // 1(Occupied)でも2(Tombstone)でもない未知の値。これを素通りさせると
        // statusやgetのmatchがunreachable!でpanicする。
        let mut payload = fresh_payload();
        payload[0..2].copy_from_slice(&1u16.to_le_bytes()); // slot_count = 1
        let tuple_data_start = (PAGE_PAYLOAD_SIZE - 10) as u16;
        payload[2..4].copy_from_slice(&tuple_data_start.to_le_bytes());
        let base = SLOTTED_HEADER_SIZE;
        payload[base..base + 2].copy_from_slice(&tuple_data_start.to_le_bytes()); // offset
        payload[base + 2..base + 4].copy_from_slice(&10u16.to_le_bytes()); // length
        payload[base + 4] = 3; // status = 未知の値

        assert!(matches!(
            SlottedPageRef::open(&payload),
            Err(DbError::CorruptPage(_))
        ));
        assert!(matches!(
            SlottedPage::open(&mut payload),
            Err(DbError::CorruptPage(_))
        ));
    }

    #[test]
    fn open_rejects_overlapping_occupied_ranges() {
        // 2つのOccupiedスロットが、それぞれ単独ではpayloadに収まる範囲だが、
        // 互いに重なっている(同じバイト列を指している)。
        let mut payload = fresh_payload();
        payload[0..2].copy_from_slice(&2u16.to_le_bytes()); // slot_count = 2
        let tuple_data_start = (PAGE_PAYLOAD_SIZE - 20) as u16;
        payload[2..4].copy_from_slice(&tuple_data_start.to_le_bytes());

        let slot0_base = SLOTTED_HEADER_SIZE;
        payload[slot0_base..slot0_base + 2].copy_from_slice(&tuple_data_start.to_le_bytes());
        payload[slot0_base + 2..slot0_base + 4].copy_from_slice(&15u16.to_le_bytes());
        payload[slot0_base + 4] = 1; // status = Occupied

        let slot1_base = SLOTTED_HEADER_SIZE + SLOT_ENTRY_SIZE;
        let overlapping_offset = tuple_data_start + 5; // slot0の範囲の途中から始まる。
        payload[slot1_base..slot1_base + 2].copy_from_slice(&overlapping_offset.to_le_bytes());
        payload[slot1_base + 2..slot1_base + 4].copy_from_slice(&10u16.to_le_bytes());
        payload[slot1_base + 4] = 1; // status = Occupied

        assert!(matches!(
            SlottedPageRef::open(&payload),
            Err(DbError::CorruptPage(_))
        ));
    }

    #[test]
    fn open_accepts_a_page_containing_zero_length_tuples_in_any_insertion_order() {
        // 回帰テスト: 長さ0のタプル(空の`&[]`)を挿入すると、その範囲は
        // start == endの空区間になる。以前の実装は、この空区間と別のスロットの
        // 非空区間がたまたま同じstartを持つ場合(例: 非空区間[4079, 4080)の
        // 直後に空タプルをinsertすると、その空区間は[4079, 4079)になる)、
        // 「startだけをキーにしたソート」の並び順が不定なせいで、両者を
        // 誤って「重なっている」と判定しopenをCorruptPageで失敗させることが
        // あった。非空→空、空→非空、空→空という3つの挿入順のどれでも、
        // insert直後・compact後のどちらでも再openできることを確認する。
        let cases: [(&[u8], &[u8]); 3] = [(b"x", b""), (b"", b"x"), (b"", b"")];
        for (first, second) in cases {
            let mut payload = fresh_payload();
            let (s1, s2) = {
                let mut page = SlottedPage::init(&mut payload);
                let s1 = page.insert(first).unwrap();
                let s2 = page.insert(second).unwrap();
                (s1, s2)
            };

            assert!(SlottedPageRef::open(&payload).is_ok());
            {
                let mut page = SlottedPage::open(&mut payload).unwrap();
                assert_eq!(page.get(s1), Some(first));
                assert_eq!(page.get(s2), Some(second));
                page.compact();
                assert_eq!(page.get(s1), Some(first));
                assert_eq!(page.get(s2), Some(second));
            }

            // compactの後も、この`payload`をあらためて開き直せる。
            assert!(SlottedPageRef::open(&payload).is_ok());
            assert!(SlottedPage::open(&mut payload).is_ok());
        }
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
    fn slotted_page_ref_reads_the_same_bytes_as_slotted_page() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);
        let slot = page.insert(b"hello").unwrap();
        let gone = page.insert(b"gone").unwrap();
        page.delete(gone);
        let free_space_via_mut = page.free_space();

        // 同じ`payload`を、書き込みを一切行わない`SlottedPageRef`から読む。
        let view = SlottedPageRef::open(&payload).unwrap();
        assert_eq!(view.get(slot), Some(&b"hello"[..]));
        assert_eq!(view.slot_count(), 2);
        assert_eq!(view.status(slot), Some(SlotStatus::Occupied));
        assert_eq!(view.free_space(), free_space_via_mut);
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
    fn is_empty_is_true_only_when_no_slot_is_occupied() {
        let mut payload = fresh_payload();
        let mut page = SlottedPage::init(&mut payload);
        assert!(page.is_empty(), "スロットが1つも無いページは空");

        let s1 = page.insert(b"alice").unwrap();
        let s2 = page.insert(b"bob").unwrap();
        assert!(!page.is_empty());

        page.delete(s1);
        assert!(!page.is_empty(), "s2がまだOccupiedなので空ではない");

        page.delete(s2);
        assert!(page.is_empty(), "全スロットがTombstone化されれば空");
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
