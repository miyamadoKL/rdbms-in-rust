//! データベースファイルのオンディスク形式: File Header、Page Header、Checksum。
//!
//! この章では、ファイルへの実際の読み書き(第13章のDisk Manager)を行わない。
//! バイト列と構造体を相互変換するencode/decode、およびその過程で壊れたバイト列を
//! 検出するchecksumだけを扱う。バイト順は全体を通してリトルエンディアンで固定する
//! (`u32`/`u64`の変換に`to_le_bytes`/`from_le_bytes`を使う)。

use crate::error::{DbError, DbResult};
use crate::ids::PageId;

/// 1ページのバイト数。
///
/// 主要なOSの仮想メモリページ、多くのファイルシステムのI/Oブロックサイズは
/// 4KiB(4096バイト)の倍数である。ページサイズをその最小公倍数に合わせておくと、
/// 1回のページI/Oが複数のOSページ・ブロックにまたがりにくくなり、部分的な書き込みが
/// 起きる余地が減る。SQLiteの既定ページサイズも4096バイトであり、この値を採用する。
pub const PAGE_SIZE: usize = 4096;

/// ファイルがminidbの形式であることを示す4バイトの目印。
pub const MAGIC: [u8; 4] = *b"MDB1";

/// オンディスク形式のバージョン。File HeaderやPage Headerのレイアウトを変更する
/// たびに1ずつ増やす。
pub const FORMAT_VERSION: u32 = 1;

/// File Headerのバイト数(`magic` 4 + `format_version` 4 + `page_size` 4 +
/// `page_count` 8 + `checksum` 4)。
pub const FILE_HEADER_SIZE: usize = 24;

/// Page Headerのバイト数(`page_id` 8 + `page_type` 1 + 予約領域 3 + `checksum` 4)。
pub const PAGE_HEADER_SIZE: usize = 16;

/// 1ページのうち、Page Headerを除いた本体のバイト数。
pub const PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

/// ファイル先頭に置かれる、ファイル全体を管理するための固定長領域。
///
/// `page_size`と`page_count`は、これから読み書きしようとしているファイルが
/// このプロセスの想定と矛盾していないかを、実際にページを読みに行く前に確かめる
/// ための値である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHeader {
    /// このファイルの1ページあたりのバイト数。
    pub page_size: u32,
    /// このファイルが持つページの総数(File Header自身を含む)。
    pub page_count: u64,
}

impl FileHeader {
    /// 現在の`PAGE_SIZE`を使い、`page_count`ページを持つ新しいFile Headerを作る。
    pub fn new(page_count: u64) -> Self {
        FileHeader {
            page_size: PAGE_SIZE as u32,
            page_count,
        }
    }

    /// File Headerを`FILE_HEADER_SIZE`バイトの固定長バイト列へ変換する。
    ///
    /// レイアウトは先頭から`magic`(4バイト)、`format_version`(4バイト、LE)、
    /// `page_size`(4バイト、LE)、`page_count`(8バイト、LE)、`checksum`(4バイト、LE)
    /// の順。`checksum`は直前までの20バイトに対する`crc32`である。
    pub fn encode(&self) -> [u8; FILE_HEADER_SIZE] {
        let mut buf = [0u8; FILE_HEADER_SIZE];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf[8..12].copy_from_slice(&self.page_size.to_le_bytes());
        buf[12..20].copy_from_slice(&self.page_count.to_le_bytes());
        let checksum = crc32(&buf[0..20]);
        buf[20..24].copy_from_slice(&checksum.to_le_bytes());
        buf
    }

    /// バイト列からFile Headerを復元し、`magic`・`format_version`・`checksum`を
    /// 検証する。
    ///
    /// いずれかの検証に失敗すると`DbError::CorruptPage`を返す。`page_size`と
    /// `page_count`自体は、このファイルが実際に想定通りのページ数を持つかを
    /// 検証済みの範囲を超えて保証しない(ファイルサイズとの突き合わせは、実際に
    /// ファイルを開く第13章のDisk Managerの仕事である)。
    pub fn decode(bytes: &[u8]) -> DbResult<Self> {
        if bytes.len() != FILE_HEADER_SIZE {
            return Err(DbError::CorruptPage(format!(
                "File Headerのバイト数が不正です: {FILE_HEADER_SIZE}バイトが必要ですが{}バイトでした",
                bytes.len()
            )));
        }

        let magic = &bytes[0..4];
        if magic != MAGIC {
            return Err(DbError::CorruptPage(format!(
                "Magic Numberが一致しません: {:?}が期待されましたが{:?}でした",
                String::from_utf8_lossy(&MAGIC),
                String::from_utf8_lossy(magic)
            )));
        }

        let format_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if format_version != FORMAT_VERSION {
            return Err(DbError::CorruptPage(format!(
                "Format Versionが一致しません: {FORMAT_VERSION}が期待されましたが{format_version}でした"
            )));
        }

        let page_size = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let page_count = u64::from_le_bytes(bytes[12..20].try_into().unwrap());

        let stored_checksum = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
        let actual_checksum = crc32(&bytes[0..20]);
        if stored_checksum != actual_checksum {
            return Err(DbError::CorruptPage(format!(
                "File Headerのchecksumが一致しません: {actual_checksum}が期待されましたが{stored_checksum}が記録されていました"
            )));
        }

        Ok(FileHeader {
            page_size,
            page_count,
        })
    }
}

/// ページの種類。
///
/// この章で登録するのは、File Header専用ページ(`Meta`)と、それ以外の一般データ
/// ページ(`Data`)の2種類だけである。Slotted Pageとしての内部構造(第12章)や、
/// B+Treeの内部・葉ページの区別(第23章)は、この列挙型にあとから足していく。
/// 第15章では、テーブル定義とFree Page Listを持つCatalogページ(`Catalog`)を
/// 追加する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    /// File Header専用ページ。
    Meta,
    /// 一般のデータページ。
    Data,
    /// `Storage`(第15章)が使う、テーブル定義とFree Page Listを保持するページ。
    Catalog,
}

impl PageType {
    fn to_u8(self) -> u8 {
        match self {
            PageType::Meta => 0,
            PageType::Data => 1,
            PageType::Catalog => 2,
        }
    }

    fn from_u8(byte: u8) -> DbResult<Self> {
        match byte {
            0 => Ok(PageType::Meta),
            1 => Ok(PageType::Data),
            2 => Ok(PageType::Catalog),
            other => Err(DbError::CorruptPage(format!(
                "未知のPage Typeです: {other}"
            ))),
        }
    }
}

/// `PAGE_SIZE`バイト固定長の1ページ。
///
/// 先頭`PAGE_HEADER_SIZE`バイトがPage Header(`page_id`・`page_type`・`checksum`)、
/// 残りが`payload`(本体)である。`payload`のバイト列としての意味付け(Slotted Page
/// としてのレイアウト)は第12章で加わるため、この章では単なるバイト列として扱う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// このページを指す識別子。
    pub page_id: PageId,
    /// このページの種類。
    pub page_type: PageType,
    payload: Vec<u8>,
}

impl Page {
    /// `payload`を全て0で埋めた、新しい空のページを作る。
    pub fn new(page_id: PageId, page_type: PageType) -> Self {
        Page {
            page_id,
            page_type,
            payload: vec![0u8; PAGE_PAYLOAD_SIZE],
        }
    }

    /// ページ本体への参照を返す。
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// ページ本体への可変参照を返す。
    pub fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.payload
    }

    /// ページを`PAGE_SIZE`バイト固定長のバイト列へ変換する。
    ///
    /// レイアウトは先頭から`page_id`(8バイト、LE)、`page_type`(1バイト)、
    /// 予約領域(3バイト、常に0)、`checksum`(4バイト、LE)、`payload`
    /// (`PAGE_PAYLOAD_SIZE`バイト)の順。`checksum`は、`checksum`フィールド自身を
    /// 0で埋めた状態のページ全体(ヘッダーと本体の両方)に対する`crc32`である。
    pub fn encode(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        buf[0..8].copy_from_slice(&self.page_id.0.to_le_bytes());
        buf[8] = self.page_type.to_u8();
        // buf[9..12]は予約領域で、初期化済みの0のままにする。
        // buf[12..16]は次のcrc32計算までchecksum用に0を保つ。
        buf[PAGE_HEADER_SIZE..].copy_from_slice(&self.payload);
        let checksum = crc32(&buf);
        buf[12..16].copy_from_slice(&checksum.to_le_bytes());
        buf
    }

    /// バイト列からページを復元し、`checksum`と`page_type`を検証する。
    ///
    /// いずれかの検証に失敗すると`DbError::CorruptPage`を返す。
    pub fn decode(bytes: &[u8]) -> DbResult<Self> {
        if bytes.len() != PAGE_SIZE {
            return Err(DbError::CorruptPage(format!(
                "ページのバイト数が不正です: {PAGE_SIZE}バイトが必要ですが{}バイトでした",
                bytes.len()
            )));
        }

        let stored_checksum = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let mut zeroed = [0u8; PAGE_SIZE];
        zeroed.copy_from_slice(bytes);
        zeroed[12..16].fill(0);
        let actual_checksum = crc32(&zeroed);
        if stored_checksum != actual_checksum {
            return Err(DbError::CorruptPage(format!(
                "ページのchecksumが一致しません: {actual_checksum}が期待されましたが{stored_checksum}が記録されていました"
            )));
        }

        let page_id = PageId(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
        let page_type = PageType::from_u8(bytes[8])?;
        let payload = bytes[PAGE_HEADER_SIZE..].to_vec();

        Ok(Page {
            page_id,
            page_type,
            payload,
        })
    }
}

/// CRC-32(IEEE 802.3、多項式`0xEDB88320`、初期値・最終XOR共に`0xFFFFFFFF`)を計算する。
///
/// 標準的なCRC-32アルゴリズムと同じ値を返すため、他のツールやライブラリで計算した
/// 値とも突き合わせられる。1バイトごとに8回ビットシフトする素朴な実装にとどめ、
/// 高速化のためのテーブル参照は行わない。この章で扱うページ数では速度上の問題に
/// ならない上、テーブル参照を導入すると、学ぶべき対象がCRC-32の計算そのものから
/// テーブルの事前生成手順へずれてしまう。
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vector() {
        // "123456789"に対するCRC-32(IEEE 802.3)の既知の値。
        // <https://www.rfc-editor.org/rfc/rfc3720> Appendix B.4などで確認できる、
        // CRC-32の実装検証によく使われる定番の入力。
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn file_header_round_trip() {
        let header = FileHeader::new(3);
        let bytes = header.encode();
        let decoded = FileHeader::decode(&bytes).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn file_header_decode_rejects_wrong_length() {
        let bytes = vec![0u8; FILE_HEADER_SIZE - 1];
        let err = FileHeader::decode(&bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptPage(_)));
    }

    #[test]
    fn file_header_decode_rejects_bad_magic() {
        let mut bytes = FileHeader::new(1).encode();
        bytes[0] = b'X';
        let err = FileHeader::decode(&bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptPage(_)));
    }

    #[test]
    fn file_header_decode_rejects_bad_version() {
        let mut bytes = FileHeader::new(1).encode();
        bytes[4..8].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let err = FileHeader::decode(&bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptPage(_)));
    }

    #[test]
    fn file_header_decode_rejects_bad_checksum() {
        let mut bytes = FileHeader::new(1).encode();
        // page_countを直接壊すと、checksumはもう合わない。
        bytes[12] ^= 0xFF;
        let err = FileHeader::decode(&bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptPage(_)));
    }

    #[test]
    fn page_round_trip() {
        let mut page = Page::new(PageId(7), PageType::Data);
        page.payload_mut()[0..5].copy_from_slice(b"hello");
        let bytes = page.encode();
        let decoded = Page::decode(&bytes).unwrap();
        assert_eq!(decoded, page);
    }

    #[test]
    fn page_decode_rejects_wrong_length() {
        let bytes = vec![0u8; PAGE_SIZE - 1];
        let err = Page::decode(&bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptPage(_)));
    }

    #[test]
    fn page_decode_rejects_corrupted_payload() {
        let page = Page::new(PageId(1), PageType::Data);
        let mut bytes = page.encode();
        // ヘッダーより後ろ(payload領域)を1バイトだけ壊す。
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let err = Page::decode(&bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptPage(_)));
    }

    #[test]
    fn page_decode_rejects_corrupted_header() {
        let page = Page::new(PageId(1), PageType::Data);
        let mut bytes = page.encode();
        // page_idを壊す(checksumの対象に含まれるため検出できる)。
        bytes[0] ^= 0xFF;
        let err = Page::decode(&bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptPage(_)));
    }

    #[test]
    fn page_decode_rejects_unknown_page_type() {
        let page = Page::new(PageId(1), PageType::Data);
        let mut bytes = page.encode();
        bytes[8] = 0xFF;
        // page_typeを書き換えたのでchecksumも計算し直し、checksum不一致ではなく
        // page_typeの検証で失敗することを確認する。
        let mut zeroed = bytes;
        zeroed[12..16].fill(0);
        let checksum = crc32(&zeroed);
        zeroed[12..16].copy_from_slice(&checksum.to_le_bytes());
        bytes = zeroed;
        let err = Page::decode(&bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptPage(_)));
    }
}
