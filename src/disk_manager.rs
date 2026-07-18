//! `Page`(第11章)を実ファイルへ読み書きするDisk Manager。
//!
//! `page`モジュールの`FileHeader`と`Page`は、バイト列と構造体を相互変換できる
//! だけの部品であり、まだファイルシステムに触れない。この章の`DiskManager`が、
//! その変換結果を実際のファイルの決まった位置へ読み書きする。
//!
//! # ファイルレイアウト
//!
//! ファイルは`PAGE_SIZE`バイトごとの区画の並びであり、先頭の1区画(ページ0)が
//! `PageType::Meta`として`FileHeader`を保持する。それ以降のページはすべて
//! `PageType::Data`である。`FileHeader::page_count`はページ0自身を含むページ総数
//! なので、正常なファイルのバイト数は常に`page_count * PAGE_SIZE`と一致する。
//!
//! ```text
//! offset 0                 PAGE_SIZE               2*PAGE_SIZE
//! +------------------------+------------------------+-----
//! | Page 0 (Meta)          | Page 1 (Data)          | ...
//! | payload先頭にFileHeader |                        |
//! +------------------------+------------------------+-----
//! ```
//!
//! # 具象型として実装する理由
//!
//! `DiskManager`をtraitにせず具象の構造体として実装している。この章までの
//! `Page`や`SlottedPage`も、まだ複数の実装を切り替える必要がない部品はすべて
//! 具象型のままであり、`DiskManager`もその流儀を引き継ぐ。抽象化は、それを
//! 呼び出す側が実際に複数の実装を使い分ける必要が生じたときに導入すれば足りる。
//!
//! 第14章のBuffer Poolは、この`DiskManager`をエビクション時の読み書き先として
//! そのまま持つ。第33章のWALで、クラッシュ・リカバリのテストのために書き込みを
//! 失敗させられる偽の実装が要るようになったとしても、その時点で`DiskManager`の
//! 既存のメソッド群からtraitを切り出せば、`DiskManager`自身はそのtraitを実装する
//! だけで済む(呼び出し側のメソッド名は変わらない)。まだ存在しない要求のために
//! traitを先取りして、単一の実装しかないダイナミックディスパッチや型引数を
//! 呼び出し元にまで持ち込む理由はない。
//!
//! # `&self`で読み書きできる理由
//!
//! `read_page`・`write_page`・`allocate_page`はいずれも`&mut self`ではなく
//! `&self`を取る。内部で`std::fs::File`と`page_count`を1本の`Mutex`にまとめて
//! 保持し、各メソッドはそのロックを取ってから読み書きする。ページI/Oのたびに
//! ロックを取る作りは、複数スレッドから同時に呼ばれても安全ではあるものの、
//! 実際には全ての呼び出しを1本のロックで直列化するだけの素朴な実装であり、
//! ページ単位の細かい並行性は持たない。この章の時点では`minidb`はまだ
//! シングルスレッドで動いており(並行実行は第35章のLatchまで登場しない)、
//! この単純さで十分である。`&self`を選んだ理由は、第14章のBuffer Poolが
//! `DiskManager`を`Arc`で共有し、複数のフレームから呼び出せるようにするためで、
//! `&mut self`のままでは呼び出し側が排他的な参照を持ち回る必要が生じてしまう。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

use crate::error::{DbError, DbResult};
use crate::ids::PageId;
use crate::page::{FileHeader, PAGE_SIZE, Page, PageType};

/// `FileHeader`を保持するMetaページは常にページ0に置く。
const META_PAGE_ID: PageId = PageId(0);

struct Inner {
    file: File,
    /// このファイルが持つページの総数(ページ0のMetaページを含む)。
    /// `FileHeader::page_count`と常に一致する値をメモリ上にも保持しておき、
    /// `read_page`・`allocate_page`のたびにMetaページを読み直さずに済ませる。
    page_count: u64,
    /// `read_page`・`write_page`を呼び出した回数の累計。
    ///
    /// ページの中身には影響しない、純粋な観測用のカウンタである。第14章の
    /// Buffer Poolが、キャッシュを挟まずにこの`DiskManager`へ直接タプル参照の
    /// たびにアクセスすると、この値がアクセス回数に比例して増え続けることを示す。
    io_count: u64,
}

/// 1つのファイルへのページ単位の読み書きを担う。
///
/// `open`したファイルは、閉じるまでの間`DiskManager`が排他的に管理する。
/// 同じパスを複数の`DiskManager`で同時に開いた場合の動作は保証しない。
pub struct DiskManager {
    inner: Mutex<Inner>,
}

impl DiskManager {
    /// `path`のファイルを開く。存在しなければ新規に作成し、Metaページを書き込む。
    ///
    /// 既存のファイルを開く場合は、ページ0を読み込んで`FileHeader`を検証し
    /// (Magic Number、Format Version、checksum。いずれも`FileHeader::decode`が
    /// 検証する)、さらに`page_size`が現在の`PAGE_SIZE`と一致することと、
    /// 実際のファイルサイズが`page_count * PAGE_SIZE`と一致することを確認する。
    /// ファイルサイズの不一致は、書き込みが途中で打ち切られた(ページの一部しか
    /// ディスクに届かなかった)ことを示すため、`DbError::CorruptPage`として拒否する。
    pub fn open<P: AsRef<Path>>(path: P) -> DbResult<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let len = file.seek(SeekFrom::End(0))?;
        let page_count = if len == 0 {
            Self::init_new_file(&mut file)?
        } else {
            Self::verify_existing_file(&mut file, len)?
        };

        Ok(DiskManager {
            inner: Mutex::new(Inner {
                file,
                page_count,
                io_count: 0,
            }),
        })
    }

    /// 空のファイルにMetaページを書き込み、ページ数1(Metaページ自身のみ)で初期化する。
    fn init_new_file(file: &mut File) -> DbResult<u64> {
        let page_count = 1;
        let mut meta = Page::new(META_PAGE_ID, PageType::Meta);
        let header = FileHeader::new(page_count);
        meta.payload_mut()[0..crate::page::FILE_HEADER_SIZE].copy_from_slice(&header.encode());

        file.seek(SeekFrom::Start(0))?;
        file.write_all(&meta.encode())?;
        file.sync_all()?;
        Ok(page_count)
    }

    /// 既存のファイルのページ0を読み込み、`FileHeader`を検証してページ数を返す。
    fn verify_existing_file(file: &mut File, len: u64) -> DbResult<u64> {
        file.seek(SeekFrom::Start(0))?;
        let mut buf = [0u8; PAGE_SIZE];
        file.read_exact(&mut buf)?;
        let meta = Page::decode(&buf)?;

        if meta.page_type != PageType::Meta {
            return Err(DbError::CorruptPage(format!(
                "ページ0はMetaページである必要がありますが{:?}でした",
                meta.page_type
            )));
        }

        let header = FileHeader::decode(&meta.payload()[0..crate::page::FILE_HEADER_SIZE])?;
        if header.page_size as usize != PAGE_SIZE {
            return Err(DbError::CorruptPage(format!(
                "page_sizeが一致しません: {PAGE_SIZE}が期待されましたが{}でした",
                header.page_size
            )));
        }

        let expected_len = header.page_count * PAGE_SIZE as u64;
        if len != expected_len {
            return Err(DbError::CorruptPage(format!(
                "ファイルサイズがpage_countと一致しません: page_count={}から期待される{expected_len}バイトに対し、実際のファイルは{len}バイトでした",
                header.page_count
            )));
        }

        Ok(header.page_count)
    }

    /// 指定したページを読み込み、checksumを検証したうえで`Page`として返す。
    ///
    /// `id`が現在の`page_count`以上(まだ`allocate_page`されていない、または
    /// ファイルの範囲外)なら`DbError::PageOutOfRange`を返す。checksumや
    /// Page Typeの検証に失敗した場合は`DbError::CorruptPage`を返す。
    pub fn read_page(&self, id: PageId) -> DbResult<Page> {
        let mut inner = self.lock();
        Self::check_range(id, inner.page_count)?;

        let mut buf = [0u8; PAGE_SIZE];
        inner.file.seek(SeekFrom::Start(Self::offset(id)))?;
        inner.file.read_exact(&mut buf)?;
        inner.io_count += 1;
        Page::decode(&buf)
    }

    /// `page`をそのページIDが指す位置へ書き込む。
    ///
    /// checksumは`Page::encode`が書き込みのたびに計算し直すため、呼び出し側が
    /// 明示的にchecksumを更新する必要はない。この呼び出しはOSへ書き込みを渡す
    /// だけであり、そのバイト列が実際にディスクへ届いたことまでは保証しない
    /// (`sync`を参照)。
    pub fn write_page(&self, page: &Page) -> DbResult<()> {
        let mut inner = self.lock();
        Self::check_range(page.page_id, inner.page_count)?;

        inner.file.seek(SeekFrom::Start(Self::offset(page.page_id)))?;
        inner.file.write_all(&page.encode())?;
        inner.io_count += 1;
        Ok(())
    }

    /// 新しいページを1枚割り当て、そのページを`page_type`で初期化してから
    /// 書き込み、割り当てた`PageId`を返す。
    ///
    /// ページ数が増えたことは、ページ0のMetaページに書き戻す`FileHeader`にも
    /// 反映する。`write_page`と同様、この時点ではまだ`sync`していない。
    pub fn allocate_page(&self, page_type: PageType) -> DbResult<PageId> {
        let mut inner = self.lock();
        let new_id = PageId(inner.page_count);
        let new_page_count = inner.page_count + 1;

        let page = Page::new(new_id, page_type);
        inner
            .file
            .seek(SeekFrom::Start(Self::offset(new_id)))?;
        inner.file.write_all(&page.encode())?;

        let mut meta = Page::new(META_PAGE_ID, PageType::Meta);
        let header = FileHeader::new(new_page_count);
        meta.payload_mut()[0..crate::page::FILE_HEADER_SIZE].copy_from_slice(&header.encode());
        inner.file.seek(SeekFrom::Start(0))?;
        inner.file.write_all(&meta.encode())?;

        inner.page_count = new_page_count;
        Ok(new_id)
    }

    /// 現在のページ数(Metaページを含む)。
    pub fn page_count(&self) -> u64 {
        self.lock().page_count
    }

    /// `read_page`・`write_page`を呼び出した累計回数。
    ///
    /// この`DiskManager`を直接叩く(キャッシュを挟まない)アクセスパターンでは、
    /// 同じページへ何度アクセスしてもこの値は毎回増える。第14章の`BufferPool`を
    /// 経由すると、2回目以降の参照はキャッシュヒットとしてこの値を増やさずに
    /// 済むようになる。
    pub fn io_count(&self) -> u64 {
        self.lock().io_count
    }

    /// これまでの`write_page`・`allocate_page`による変更を、OSのページキャッシュから
    /// ディスクへ実際に書き出す。
    ///
    /// `write_page`が`Ok(())`を返した時点では、そのバイト列はOSのページキャッシュに
    /// 渡っただけであり、電源断やクラッシュに対してまだ安全ではない。OSは通常、
    /// 書き込みを即座にディスクへ反映せず、複数の書き込みをまとめて後から
    /// フラッシュすることで性能を稼いでいる。`sync`は`File::sync_all`を呼び、
    /// OSに対して「今書き込んだ内容を実際のディスクまで届けてから返ってきてほしい」
    /// と要求する。ページの中身に加えてファイルサイズなどのメタデータも
    /// `allocate_page`で変化しうるため、データだけを同期する`sync_data`ではなく
    /// 両方を同期する`sync_all`を使う。第33章のWALは、ログレコードをこの`sync`で
    /// ディスクへ固定してから初めてトランザクションのコミットを応答してよい、
    /// という順序を守ることでcrash-safetyを実現する。
    pub fn sync(&self) -> DbResult<()> {
        let inner = self.lock();
        inner.file.sync_all()?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn check_range(id: PageId, page_count: u64) -> DbResult<()> {
        if id.0 >= page_count {
            return Err(DbError::PageOutOfRange(format!(
                "PageId({})はページ数{page_count}の範囲外です",
                id.0
            )));
        }
        Ok(())
    }

    fn offset(id: PageId) -> u64 {
        id.0 * PAGE_SIZE as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-disk-manager-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        path
    }

    #[test]
    fn open_creates_a_new_file_with_meta_page_only() {
        let path = temp_path("new-file");
        let disk = DiskManager::open(&path).unwrap();
        assert_eq!(disk.page_count(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn allocate_write_and_read_page_round_trips() {
        let path = temp_path("round-trip");
        let disk = DiskManager::open(&path).unwrap();

        let id = disk.allocate_page(PageType::Data).unwrap();
        assert_eq!(id, PageId(1));
        assert_eq!(disk.page_count(), 2);

        let mut page = disk.read_page(id).unwrap();
        page.payload_mut()[0..5].copy_from_slice(b"hello");
        disk.write_page(&page).unwrap();

        let reread = disk.read_page(id).unwrap();
        assert_eq!(&reread.payload()[0..5], b"hello");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn read_page_out_of_range_is_rejected() {
        let path = temp_path("out-of-range");
        let disk = DiskManager::open(&path).unwrap();
        let err = disk.read_page(PageId(5)).unwrap_err();
        assert!(matches!(err, DbError::PageOutOfRange(_)));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reopening_the_same_file_preserves_pages() {
        let path = temp_path("reopen");
        {
            let disk = DiskManager::open(&path).unwrap();
            let id = disk.allocate_page(PageType::Data).unwrap();
            let mut page = disk.read_page(id).unwrap();
            page.payload_mut()[0..5].copy_from_slice(b"world");
            disk.write_page(&page).unwrap();
            disk.sync().unwrap();
        }

        let disk = DiskManager::open(&path).unwrap();
        assert_eq!(disk.page_count(), 2);
        let page = disk.read_page(PageId(1)).unwrap();
        assert_eq!(&page.payload()[0..5], b"world");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn corrupting_a_byte_on_disk_is_detected_on_read() {
        let path = temp_path("corrupt");
        {
            let disk = DiskManager::open(&path).unwrap();
            let id = disk.allocate_page(PageType::Data).unwrap();
            let mut page = disk.read_page(id).unwrap();
            page.payload_mut()[0..5].copy_from_slice(b"alice");
            disk.write_page(&page).unwrap();
            disk.sync().unwrap();
        }

        // ファイルを直接開き、ページ1(2ページ目)の途中のバイトを1つ反転させる。
        {
            let mut file = OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(PAGE_SIZE as u64 + 20)).unwrap();
            file.write_all(&[0xFF]).unwrap();
        }

        let disk = DiskManager::open(&path).unwrap();
        let err = disk.read_page(PageId(1)).unwrap_err();
        assert!(matches!(err, DbError::CorruptPage(_)));

        std::fs::remove_file(&path).unwrap();
    }
}
