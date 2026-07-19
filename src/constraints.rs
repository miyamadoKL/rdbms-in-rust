//! `PRIMARY KEY`・`UNIQUE`の一意性検査(第20章)。
//!
//! `NOT NULL`はすでに`Tuple::new`(第4章)が、Schemaの`nullable`フラグを見て
//! 検査している。`PRIMARY KEY`は`Column::with_primary_key`が`nullable`を
//! `false`へ強制する(`crate::types`)ため、`NOT NULL`の分はそちらへ乗るだけで
//! よい。この章で新しく要るのは、「同じ値を持つ行が2つ以上無いか」という
//! **一意性**の検査であり、それを担うのがこのモジュールである。
//!
//! # なぜ走査ベースなのか
//!
//! この章の時点で、テーブルの行を高速に検索できる索引(B+Tree)はまだ存在しない
//! (第23・24章で追加する)。そのため、この章の一意性検査は「候補の値を、
//! テーブルの全行と1つずつ比べる」という線形走査で実装する。1回の検査が
//! `O(テーブルの行数)`かかり、複数行の`INSERT`・`UPDATE`ではその検査を対象行数分
//! 繰り返すため、全体では行数にほぼ比例したコストがかかる。第24章で
//! `CREATE INDEX`が使えるようになると、この検査はB+Treeの検索(`O(log n)`)へ
//! 置き換わる。
//!
//! # `NULL`の扱い
//!
//! `UNIQUE`列に対して、SQL標準は「`NULL`同士は重複とみなさない」という規則を
//! 採る。`UNIQUE`な`email`列を持つテーブルに`email`が`NULL`の行を何行入れても、
//! それらは互いに衝突しない。この規則は「値が分からない」という`NULL`の意味論
//! (三値論理でのUNKNOWN)と整合する取り決めであり、この章でもそのまま採用する。
//! `PRIMARY KEY`列は`NOT NULL`を含意するため、この規則が実際に効くのは
//! `UNIQUE`列(かつ`NOT NULL`を伴わないもの)に限られる。

use crate::error::{DbError, DbResult};
use crate::types::{Column, Schema, Tuple, Value};

/// `column`が`PRIMARY KEY`か`UNIQUE`かに応じて、`value`の重複を報告する
/// `DbError`を作る。両方指定されている場合は`PRIMARY KEY`のエラーを優先する
/// (「主キーが重複している」という、より強い制約違反を先に報告するため)。
fn violation_for(column: &Column, value: &Value) -> DbError {
    let message_column = column.name.clone();
    let message_value = value.to_string();
    if column.primary_key {
        DbError::PrimaryKeyViolation { column: message_column, value: message_value }
    } else {
        DbError::UniqueViolation { column: message_column, value: message_value }
    }
}

/// `candidates`(これから書き込もうとしている行)が、`PRIMARY KEY`・`UNIQUE`の
/// どの列についても、`others`(すでにテーブルにある行、または今回の文で
/// 変更されない行)および`candidates`同士のどちらとも重複しないことを検査する。
///
/// 検査は「値を書き込む前に、書き込もうとしている全行分を済ませる」という
/// All-or-Nothingの順序を保つため、`insert`・`update`の呼び出し側は、この関数を
/// 実際の書き込み(`table.rows_mut().extend`や`storage.insert`)より必ず前に
/// 呼ぶ。
///
/// * `INSERT`: `others`は既存の全行、`candidates`は`VALUES`から組み立てた
///   新しい行(複数行の`INSERT`では、同じ文の中の行同士も`candidates`同士の
///   比較で検査される)。
/// * `UPDATE`: `others`は`WHERE`に一致せず変更されない行、`candidates`は
///   `SET`を適用した後の新しい値。変更される行自身の更新前の値は`others`にも
///   `candidates`にも含めない(自分自身との比較を避けるため)。
///
/// `NULL`は`others`・`candidates`のどちらであっても比較の対象から外す
/// (モジュール冒頭の「`NULL`の扱い」を参照)。
pub fn check_uniqueness<'a>(
    schema: &Schema,
    others: impl Iterator<Item = &'a Tuple> + Clone,
    candidates: &[Tuple],
) -> DbResult<()> {
    for (index, column) in schema.unique_constrained_columns() {
        for candidate in candidates {
            let value = candidate.get(index).expect("candidateはschemaと同じ列数を持つ");
            if value.is_null() {
                continue;
            }
            if others.clone().any(|other| other.get(index) == Some(value)) {
                return Err(violation_for(column, value));
            }
        }

        for i in 0..candidates.len() {
            let vi = candidates[i].get(index).expect("candidateはschemaと同じ列数を持つ");
            if vi.is_null() {
                continue;
            }
            for candidate in &candidates[i + 1..] {
                let vj = candidate.get(index).expect("candidateはschemaと同じ列数を持つ");
                if !vj.is_null() && vj == vi {
                    return Err(violation_for(column, vi));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataType;

    fn schema_with_unique_email() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::BigInt, false).with_primary_key(),
            Column::new("email", DataType::Text, true).with_unique(),
        ])
    }

    fn tuple(schema: &Schema, id: i64, email: Option<&str>) -> Tuple {
        let email = match email {
            Some(s) => Value::Text(s.to_string()),
            None => Value::Null,
        };
        Tuple::new(schema, vec![Value::BigInt(id), email]).unwrap()
    }

    #[test]
    fn detects_primary_key_duplicate_against_existing_rows() {
        let schema = schema_with_unique_email();
        let existing = [tuple(&schema, 1, Some("a@example.com"))];
        let candidates = vec![tuple(&schema, 1, Some("b@example.com"))];
        let err = check_uniqueness(&schema, existing.iter(), &candidates).unwrap_err();
        assert!(matches!(err, DbError::PrimaryKeyViolation { column, .. } if column == "id"));
    }

    #[test]
    fn detects_unique_duplicate_against_existing_rows() {
        let schema = schema_with_unique_email();
        let existing = [tuple(&schema, 1, Some("a@example.com"))];
        let candidates = vec![tuple(&schema, 2, Some("a@example.com"))];
        let err = check_uniqueness(&schema, existing.iter(), &candidates).unwrap_err();
        assert!(matches!(err, DbError::UniqueViolation { column, .. } if column == "email"));
    }

    #[test]
    fn detects_duplicate_among_candidates_themselves() {
        let schema = schema_with_unique_email();
        let candidates = vec![tuple(&schema, 1, Some("a@example.com")), tuple(&schema, 2, Some("a@example.com"))];
        let err = check_uniqueness(&schema, std::iter::empty(), &candidates).unwrap_err();
        assert!(matches!(err, DbError::UniqueViolation { column, .. } if column == "email"));
    }

    #[test]
    fn null_unique_values_never_conflict() {
        let schema = schema_with_unique_email();
        let existing = [tuple(&schema, 1, None)];
        let candidates = vec![tuple(&schema, 2, None), tuple(&schema, 3, None)];
        assert!(check_uniqueness(&schema, existing.iter(), &candidates).is_ok());
    }

    #[test]
    fn distinct_values_do_not_conflict() {
        let schema = schema_with_unique_email();
        let existing = [tuple(&schema, 1, Some("a@example.com"))];
        let candidates = vec![tuple(&schema, 2, Some("b@example.com"))];
        assert!(check_uniqueness(&schema, existing.iter(), &candidates).is_ok());
    }

    #[test]
    fn schema_without_unique_columns_never_conflicts() {
        let schema = Schema::new(vec![Column::new("id", DataType::BigInt, false)]);
        let existing = [Tuple::new(&schema, vec![Value::BigInt(1)]).unwrap()];
        let candidates = vec![Tuple::new(&schema, vec![Value::BigInt(1)]).unwrap()];
        assert!(check_uniqueness(&schema, existing.iter(), &candidates).is_ok());
    }
}
