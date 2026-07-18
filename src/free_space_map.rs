//! テーブルの各ページにどれだけ空きがあるかを覚えておく、Free Space Map(FSM)。
//!
//! `HeapFile::insert`(第13章)は、空きのあるページを`page_ids`の先頭から順に
//! 試す線形探索で実装されていた。「線形探索」という言葉が指すコストは、実は
//! ページ数そのものではない。`for &page_id in &self.page_ids { pool.write_page
//! (page_id)?; ... }`の1回1回が、そのページを`BufferPool`からpinし、
//! `SlottedPage`として開いて`insert`を試す、という重い手続きである。ページが
//! すでにフレームに載っていればディスクI/Oは起きないが、それでも`Mutex`のロックや
//! Slot Directoryの走査は毎回発生する。空きがないと判明したページに対しても、
//! そのたびにこの手続き一式を払っている。
//!
//! `FreeSpaceMap`は、「このページには残りおよそ何バイトの空きがあるか」という
//! 整数1つだけをメモリ上に持ち、`BufferPool`にもディスクにも触れずに候補を
//! 絞り込めるようにする。空きが足りないと分かっているページを、pinして開いて
//! から気付くのではなく、その手前で除外できる。
//!
//! # 粒度: バイト単位の実測値
//!
//! この章の`FreeSpaceMap`は、`SlottedPage::free_space`が返すバイト数をそのまま
//! 保持する。PostgreSQLのFree Space Mapは1バイトを256段階に量子化した近似値を
//! 使っており、これは更新の頻度とファイルサイズを抑えるための設計である。この章の
//! `minidb`が扱うページ数・更新頻度はその最適化を要するほど大きくないため、
//! 量子化はせず実測値をそのまま持つ単純な実装を選んだ。バケツ分けによる近似は
//! 章末の演習で扱う。
//!
//! # 更新タイミング: 操作の直後に同期的に反映する
//!
//! `insert`・`update`・`delete`がページを書き換えた直後、そのとき開いていた
//! `SlottedPage`から`free_space()`を読み直すだけで最新値が手に入るため、
//! 追加のページ読み込みなしに更新できる。`Storage::open`(第15章)は、
//! カタログから復元した各テーブルの`page_ids`を1回ずつ読み、実際の
//! `free_space()`から`FreeSpaceMap`を作り直す。`FreeSpaceMap`自身の内容は
//! ディスクへ永続化しない。ページの中身こそが空き容量の一次情報であり、
//! `FreeSpaceMap`はその要約をメモリ上に持つキャッシュにすぎないため、
//! 起動のたびに実測から作り直すほうが、更新のたびに2箇所を同期させ続けるより
//! 単純である。
//!
//! # コンパクションで回収できる空きは見えない
//!
//! `SlottedPage::free_space`(第12章)が返すのは、Slot DirectoryとTuple Data
//! の間にまだ手つかずで残っている隙間だけである。Tombstone化された(削除済みの)
//! タプルが専有したままの領域は、`compact`を呼ぶまで空き領域に数えられない。
//! `FreeSpaceMap`はこの値をそのまま保持するだけなので、あるページで大量の
//! タプルを削除しても、`compact`が実際に呼ばれるまでは空きが増えたとは
//! 見なさない。`insert`は候補が見つからなければ新しいページを確保する側へ
//! 進んでしまい、削除によって本来なら収まるはずだったページを見逃す。
//! この見逃しはデータの正しさには影響しない(そのページは単に候補から
//! 外れるだけで、別のページか新しいページに正しく挿入される)が、ページの
//! 利用効率を落とす。`FreeSpaceMap`の見積もりにコンパクション後の空きを
//! 反映させる改良は、章末の演習で扱う。

use std::collections::HashMap;

use crate::ids::PageId;

/// ページごとの空きバイト数の見積もりを保持する。
#[derive(Debug, Default)]
pub struct FreeSpaceMap {
    free_bytes: HashMap<PageId, u16>,
}

impl FreeSpaceMap {
    /// 空の`FreeSpaceMap`を作る。
    pub fn new() -> Self {
        FreeSpaceMap {
            free_bytes: HashMap::new(),
        }
    }

    /// `page_id`の空きバイト数を`free_bytes`で上書きする。
    ///
    /// `free_bytes`が`u16::MAX`を超える場合は`u16::MAX`に切り詰める
    /// (`PAGE_PAYLOAD_SIZE`は`u16::MAX`よりずっと小さいため、実際には
    /// 起こらない)。
    pub fn update(&mut self, page_id: PageId, free_bytes: usize) {
        self.free_bytes
            .insert(page_id, free_bytes.min(u16::MAX as usize) as u16);
    }

    /// `page_id`の記録を取り除く。
    ///
    /// テーブルが`drop_table`され、そのページがFree Page Listへ戻るとき
    /// (第15章の`Storage::drop_table`)に呼ぶ。次にそのページが別のテーブルへ
    /// 割り当てられたときは、`SlottedPage::init`の直後にあらためて`update`が
    /// 呼ばれるため、この`remove`を省いても誤動作はしない。呼ぶのは、もう
    /// どのテーブルにも属さないページの記録をいつまでも残さないための整理である。
    pub fn remove(&mut self, page_id: PageId) {
        self.free_bytes.remove(&page_id);
    }

    /// `candidates`の中から、`needed`バイト以上の空きが見積もられている
    /// 最初のページを返す。
    ///
    /// `candidates`は呼び出し側(テーブルが所有するページの一覧)が渡す順序を
    /// そのまま使う。まだ`update`されたことのないページ(記録がないページ)は
    /// 候補に含めない。この探索自体は`candidates`の長さに比例する時間がかかる
    /// (漸近的な計算量そのものを`HeapFile`の素朴な線形探索から変えてはいない)。
    /// 変わるのは1候補あたりのコストで、`BufferPool`のpinやSlot Directoryの
    /// 走査を伴わない、メモリ上のハッシュ表参照1回で済む。
    pub fn find_candidate(&self, candidates: &[PageId], needed: usize) -> Option<PageId> {
        candidates
            .iter()
            .copied()
            .find(|page_id| self.free_bytes.get(page_id).is_some_and(|&free| free as usize >= needed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_candidate_returns_none_on_empty_map() {
        let fsm = FreeSpaceMap::new();
        assert_eq!(fsm.find_candidate(&[PageId(1)], 10), None);
    }

    #[test]
    fn find_candidate_skips_pages_without_enough_space() {
        let mut fsm = FreeSpaceMap::new();
        fsm.update(PageId(1), 5);
        fsm.update(PageId(2), 100);
        assert_eq!(fsm.find_candidate(&[PageId(1), PageId(2)], 10), Some(PageId(2)));
    }

    #[test]
    fn find_candidate_prefers_the_first_matching_page_in_candidate_order() {
        let mut fsm = FreeSpaceMap::new();
        fsm.update(PageId(1), 100);
        fsm.update(PageId(2), 200);
        assert_eq!(fsm.find_candidate(&[PageId(1), PageId(2)], 10), Some(PageId(1)));
    }

    #[test]
    fn find_candidate_ignores_pages_that_were_never_updated() {
        let fsm = FreeSpaceMap::new();
        // PageId(1)は一度もupdateされていないので、候補に入っていても無視される。
        assert_eq!(fsm.find_candidate(&[PageId(1)], 0), None);
    }

    #[test]
    fn remove_makes_a_page_ineligible_again() {
        let mut fsm = FreeSpaceMap::new();
        fsm.update(PageId(1), 100);
        fsm.remove(PageId(1));
        assert_eq!(fsm.find_candidate(&[PageId(1)], 10), None);
    }

    #[test]
    fn update_overwrites_the_previous_estimate() {
        let mut fsm = FreeSpaceMap::new();
        fsm.update(PageId(1), 100);
        fsm.update(PageId(1), 5);
        assert_eq!(fsm.find_candidate(&[PageId(1)], 10), None);
    }
}
