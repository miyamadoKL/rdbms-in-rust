//! `Tuple`(第4章)とバイト列との間のencode/decode。
//!
//! `SlottedPage::insert`(第12章)が受け取れるのは`&[u8]`だけであり、
//! `Tuple`をそこへ渡すには、あらかじめこのモジュールでバイト列へ変換しておく
//! 必要がある。逆に`SlottedPage::get`が返すバイト列から`Tuple`を復元するには、
//! そのタプルがどの`Schema`に従うかを知っている必要がある。この対応関係(何列目が
//! どの`DataType`か)はバイト列自身には書き込まず、常に呼び出し側が`Schema`を
//! 渡す前提とする。
//!
//! # バイト列のレイアウト
//!
//! ```text
//! +----------------+----------+----------+-----+----------+
//! | NULLビットマップ | 列0の値  | 列1の値  | ... | 列N-1の値 |
//! +----------------+----------+----------+-----+----------+
//! ```
//!
//! **NULLビットマップ**: 列数を`ceil(schema.len() / 8)`バイトへ切り上げた
//! バイト列。列`i`が`NULL`なら、`i / 8`バイト目の`i % 8`ビット目(最下位ビットを
//! ビット0とする)が1になる。`NULL`の列は値の領域に何も書き込まない
//! (`NULL`はどの`DataType`にも属さないため、書くべきバイト表現がそもそもない)。
//!
//! **各列の値**(`NULL`でない列のみ、Schemaの列順): `DataType`ごとに次の形式で
//! 書き込む。
//!
//! | `DataType` | バイト数 | 形式 |
//! | --- | --- | --- |
//! | `BOOLEAN` | 1 | `0`(false)または`1`(true) |
//! | `BIGINT` | 8 | `i64`のリトルエンディアン表現 |
//! | `TEXT` | `4 + len` | 長さ`len`(`u32`、リトルエンディアン)に続けてUTF-8バイト列 |
//!
//! `TEXT`だけが可変長であり、そのバイト数はタプルごと・列ごとに異なる。
//! 長さプレフィックスがあるおかげで、`decode_tuple`は文字列の終端を
//! 区切り文字の探索なしに一発で求められる。

use crate::error::{DbError, DbResult};
use crate::types::{DataType, Schema, Tuple, Value};

/// NULLビットマップのバイト数(列数を8列単位へ切り上げる)。
fn null_bitmap_len(column_count: usize) -> usize {
    column_count.div_ceil(8)
}

/// `tuple`をバイト列へ変換する。
///
/// `tuple`は事前に`schema`へ適合していることを前提とする(`Tuple::new`を
/// 経由して作られたタプルは、この前提を`Tuple::new`自身の検査によって
/// 満たしている)。
pub fn encode_tuple(schema: &Schema, tuple: &Tuple) -> Vec<u8> {
    let values = tuple.values();
    debug_assert_eq!(
        values.len(),
        schema.len(),
        "tupleの列数がschemaと一致しません"
    );
    let mut bitmap = vec![0u8; null_bitmap_len(values.len())];
    let mut body = Vec::new();

    for (i, value) in values.iter().enumerate() {
        match value {
            Value::Null => {
                bitmap[i / 8] |= 1 << (i % 8);
            }
            Value::Boolean(b) => {
                body.push(u8::from(*b));
            }
            Value::BigInt(n) => {
                body.extend_from_slice(&n.to_le_bytes());
            }
            Value::Text(s) => {
                let bytes = s.as_bytes();
                body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }

    let mut out = bitmap;
    out.extend_from_slice(&body);
    out
}

/// バイト列から`schema`に従う`Tuple`を復元する。
///
/// `bytes`が短すぎる、`TEXT`の長さプレフィックスが残りバイト数を超えている、
/// `TEXT`の内容が妥当なUTF-8でないなど、`encode_tuple`が書き出しえない
/// バイト列を受け取った場合は`DbError::CorruptTuple`を返す。
pub fn decode_tuple(schema: &Schema, bytes: &[u8]) -> DbResult<Tuple> {
    let column_count = schema.len();
    let bitmap_len = null_bitmap_len(column_count);
    if bytes.len() < bitmap_len {
        return Err(DbError::CorruptTuple(format!(
            "NULLビットマップのバイト数が不足しています: {bitmap_len}バイトが必要ですが{}バイトしかありません",
            bytes.len()
        )));
    }
    let bitmap = &bytes[0..bitmap_len];
    let mut cursor = bitmap_len;

    let mut values = Vec::with_capacity(column_count);
    for (i, column) in schema.columns().iter().enumerate() {
        let is_null = bitmap[i / 8] & (1 << (i % 8)) != 0;
        if is_null {
            values.push(Value::Null);
            continue;
        }

        let value = match column.data_type {
            DataType::Boolean => {
                let byte = *bytes.get(cursor).ok_or_else(|| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(BOOLEAN)を読む前にバイト列が尽きました",
                        column.name
                    ))
                })?;
                cursor += 1;
                Value::Boolean(byte != 0)
            }
            DataType::BigInt => {
                let end = cursor + 8;
                let slice = bytes.get(cursor..end).ok_or_else(|| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(BIGINT)の8バイトを読む前にバイト列が尽きました",
                        column.name
                    ))
                })?;
                cursor = end;
                Value::BigInt(i64::from_le_bytes(slice.try_into().unwrap()))
            }
            DataType::Text => {
                let len_end = cursor + 4;
                let len_bytes = bytes.get(cursor..len_end).ok_or_else(|| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(TEXT)の長さプレフィックスを読む前にバイト列が尽きました",
                        column.name
                    ))
                })?;
                let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
                cursor = len_end;
                let text_end = cursor + len;
                let text_bytes = bytes.get(cursor..text_end).ok_or_else(|| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(TEXT)の本体を{len}バイト読む前にバイト列が尽きました",
                        column.name
                    ))
                })?;
                cursor = text_end;
                let text = String::from_utf8(text_bytes.to_vec()).map_err(|_| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(TEXT)の内容が妥当なUTF-8ではありません",
                        column.name
                    ))
                })?;
                Value::Text(text)
            }
        };
        values.push(value);
    }

    Tuple::new(schema, values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Column;

    fn mixed_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::BigInt, false),
            Column::new("name", DataType::Text, false),
            Column::new("active", DataType::Boolean, false),
            Column::new("nickname", DataType::Text, true),
        ])
    }

    #[test]
    fn round_trips_all_types_without_null() {
        let schema = mixed_schema();
        let tuple = Tuple::new(
            &schema,
            vec![
                Value::BigInt(42),
                Value::Text("Alice".to_string()),
                Value::Boolean(true),
                Value::Text("Al".to_string()),
            ],
        )
        .unwrap();

        let bytes = encode_tuple(&schema, &tuple);
        let decoded = decode_tuple(&schema, &bytes).unwrap();
        assert_eq!(decoded, tuple);
    }

    #[test]
    fn round_trips_with_null_in_nullable_column() {
        let schema = mixed_schema();
        let tuple = Tuple::new(
            &schema,
            vec![
                Value::BigInt(1),
                Value::Text("Bob".to_string()),
                Value::Boolean(false),
                Value::Null,
            ],
        )
        .unwrap();

        let bytes = encode_tuple(&schema, &tuple);
        let decoded = decode_tuple(&schema, &bytes).unwrap();
        assert_eq!(decoded, tuple);
    }

    #[test]
    fn empty_text_round_trips() {
        let schema = mixed_schema();
        let tuple = Tuple::new(
            &schema,
            vec![
                Value::BigInt(0),
                Value::Text(String::new()),
                Value::Boolean(false),
                Value::Null,
            ],
        )
        .unwrap();

        let bytes = encode_tuple(&schema, &tuple);
        let decoded = decode_tuple(&schema, &bytes).unwrap();
        assert_eq!(decoded, tuple);
    }

    #[test]
    fn multibyte_utf8_text_round_trips() {
        let schema = mixed_schema();
        let tuple = Tuple::new(
            &schema,
            vec![
                Value::BigInt(7),
                Value::Text("東京、大阪".to_string()),
                Value::Boolean(true),
                Value::Null,
            ],
        )
        .unwrap();

        let bytes = encode_tuple(&schema, &tuple);
        let decoded = decode_tuple(&schema, &bytes).unwrap();
        assert_eq!(decoded, tuple);
    }

    #[test]
    fn nine_columns_use_two_bitmap_bytes() {
        // 8列境界をまたぐことを確認するため、9列すべてNULLのSchemaを使う。
        let columns: Vec<Column> = (0..9)
            .map(|i| Column::new(format!("c{i}"), DataType::BigInt, true))
            .collect();
        let schema = Schema::new(columns);
        let values = vec![Value::Null; 9];
        let tuple = Tuple::new(&schema, values).unwrap();

        let bytes = encode_tuple(&schema, &tuple);
        assert_eq!(null_bitmap_len(9), 2);
        // 全列NULLなので値本体は無く、ビットマップの2バイトだけが残る。
        assert_eq!(bytes.len(), 2);

        let decoded = decode_tuple(&schema, &bytes).unwrap();
        assert_eq!(decoded, tuple);
    }

    #[test]
    fn decode_rejects_truncated_bitmap() {
        let schema = mixed_schema();
        let err = decode_tuple(&schema, &[]).unwrap_err();
        assert!(matches!(err, DbError::CorruptTuple(_)));
    }

    #[test]
    fn decode_rejects_truncated_bigint_body() {
        let schema = mixed_schema();
        let tuple = Tuple::new(
            &schema,
            vec![
                Value::BigInt(1),
                Value::Text("x".to_string()),
                Value::Boolean(true),
                Value::Null,
            ],
        )
        .unwrap();
        let mut bytes = encode_tuple(&schema, &tuple);
        // NULLビットマップの直後、BIGINTの8バイトの途中で切り詰める。
        bytes.truncate(1 + 4);
        let err = decode_tuple(&schema, &bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptTuple(_)));
    }

    #[test]
    fn decode_rejects_text_length_prefix_exceeding_remaining_bytes() {
        let schema = mixed_schema();
        let tuple = Tuple::new(
            &schema,
            vec![
                Value::BigInt(1),
                Value::Text("x".to_string()),
                Value::Boolean(true),
                Value::Null,
            ],
        )
        .unwrap();
        let mut bytes = encode_tuple(&schema, &tuple);
        // "x"の長さプレフィックス(1バイト分の直前4バイト)を巨大な値へ書き換える。
        let bitmap_len = null_bitmap_len(schema.len());
        let len_start = bitmap_len + 8;
        bytes[len_start..len_start + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let err = decode_tuple(&schema, &bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptTuple(_)));
    }

    #[test]
    fn decode_rejects_invalid_utf8_text_body() {
        let schema = Schema::new(vec![Column::new("t", DataType::Text, false)]);
        let mut bytes = vec![0u8]; // 1列分のビットマップ、NULLではない。
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xFF, 0xFE]); // 妥当なUTF-8ではない2バイト。
        let err = decode_tuple(&schema, &bytes).unwrap_err();
        assert!(matches!(err, DbError::CorruptTuple(_)));
    }
}
