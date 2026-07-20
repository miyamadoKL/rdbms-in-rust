//! 軽量Parser Fuzzing(第40章)。
//!
//! `cargo-fuzz`(libFuzzer)はコーパスの保存先やASANの有無など環境依存が強く、
//! CIで毎回走らせる前提のテストとは相性が良くない。この章では、依存を
//! 増やさずシードを固定した自作の疑似乱数ファザーを採用する
//! (`common::Xorshift64`、`src/btree.rs`が第23章から使っているものと同じ
//! 実装)。目的はただ1つ、`tokenize`と`parse_statement`がどんな入力に対しても
//! `Err`を返すか値を返すかのどちらかで終わり、`panic`もハングもしないことを
//! 数万パターンにわたって確認することである。
//!
//! 2種類の生成器を使う。
//!
//! - **バイト列ファザー**: 完全にランダムなバイト列を`String::from_utf8_lossy`で
//!   文字列化して食わせる。字句解析器の境界(不正なUTF-8由来の置換文字、
//!   制御文字、突然終わる入力)を狙う。
//! - **トークン列ファザー**: キーワード・識別子・数値・文字列リテラル・
//!   記号を辞書からランダムに選んで並べる。バイト列ファザーよりも構文解析器の
//!   奥(式、`JOIN`、`CAST`、`PREPARE`/`EXECUTE`)まで届きやすい。
//!
//! 入れ子の丸括弧はあえて浅く抑えてある(`MAX_PAREN_DEPTH`)。構文解析器は
//! 再帰下降なので、丸括弧を数万重ねると`panic`ではなくスタックオーバーフローで
//! プロセスごと落ちる。それは`catch_unwind`で捕まえられない種類の異常であり、
//! 混ぜると「見つかったら死ぬテスト」になって本末転倒になる。深い再帰への
//! 対処(明示的な深さ上限を構文解析器に持たせる)は、この章の対象を
//! 「拡充と統合」に絞る方針(章冒頭)から外れるため、演習問題に残す。

mod common;

use std::panic::{self, AssertUnwindSafe};

use common::Xorshift64;
use minidb::{parse_statement, tokenize};

const MAX_PAREN_DEPTH: usize = 12;

const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "CREATE", "TABLE", "DROP", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE",
    "TRUE", "FALSE", "NULL", "NOT", "AND", "OR", "IS", "AS", "CAST", "EXPLAIN", "PRIMARY", "KEY", "UNIQUE", "ORDER",
    "BY", "ASC", "DESC", "LIMIT", "OFFSET", "DISTINCT", "GROUP", "HAVING", "INNER", "JOIN", "ON", "INDEX", "ANALYZE",
    "BEGIN", "COMMIT", "ROLLBACK", "ISOLATION", "LEVEL", "READ", "UNCOMMITTED", "COMMITTED", "REPEATABLE",
    "SERIALIZABLE", "CHECKPOINT", "PREPARE", "EXECUTE", "DEALLOCATE", "SHOW", "DESCRIBE", "VACUUM", "TABLES",
    "INDEXES", "STATS", "BIGINT", "BOOLEAN", "TEXT",
];

const PUNCTUATION: &[&str] = &["(", ")", ",", ";", ".", "*", "+", "-", "/", "=", "<>", "<", "<=", ">", ">=", "$"];

const IDENTS: &[&str] = &["t", "id", "orders", "users", "a", "b1", "列名", "_x", "名前"];

const STRINGS: &[&str] = &["''", "'a'", "'it''s'", "'日本語'", "'\\'", "'  '"];

const NUMBERS: &[&str] = &["0", "1", "42", "-1", "9223372036854775807", "-9223372036854775808", "99999999999999999999999999", "3.14", "1e400", "0x1"];

/// トークン列ファザー: 断片の辞書からランダムに選んでSQL片を組み立てる。
/// 文法として正しい保証は無い(それがFuzzingの狙いである)。
fn generate_token_soup(rng: &mut Xorshift64, len: usize) -> String {
    let mut out = String::new();
    let mut paren_depth = 0usize;
    for i in 0..len {
        if i > 0 {
            out.push(' ');
        }
        let bucket = rng.range(6);
        let piece = match bucket {
            0 => KEYWORDS[rng.range(KEYWORDS.len())],
            1 => {
                if paren_depth < MAX_PAREN_DEPTH && rng.chance(1, 2) {
                    paren_depth += 1;
                    "("
                } else if paren_depth > 0 {
                    paren_depth -= 1;
                    ")"
                } else {
                    PUNCTUATION[rng.range(PUNCTUATION.len())]
                }
            }
            2 => PUNCTUATION[rng.range(PUNCTUATION.len())],
            3 => IDENTS[rng.range(IDENTS.len())],
            4 => STRINGS[rng.range(STRINGS.len())],
            _ => NUMBERS[rng.range(NUMBERS.len())],
        };
        out.push_str(piece);
    }
    // 開いたままの丸括弧は閉じておく(閉じ忘れ自体は「バランスの取れていない
    // 丸括弧」として構文解析器に投げても構わないが、ここでは開き過ぎたぶんを
    // 自己完結させ、意図的な不均衡は独立したコーパスケースに任せる)。
    for _ in 0..paren_depth {
        out.push_str(" )");
    }
    out
}

/// バイト列ファザー: 完全にランダムなバイト列をUTF-8として(無効な部分は
/// 置換文字へ変換して)解釈する。
fn generate_byte_soup(rng: &mut Xorshift64, len: usize) -> String {
    let bytes: Vec<u8> = (0..len).map(|_| (rng.next() % 256) as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `input`を`tokenize`・`parse_statement`へ渡し、`panic`しないことを確認する。
/// `Err`は正常系(壊れた入力を拒否できた)として扱う。
fn assert_no_panic(input: &str) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = tokenize(input);
        let _ = parse_statement(input);
    }));
    assert!(result.is_ok(), "panicした入力: {input:?}");
}

/// トークン列ファザーを1万ケース走らせる。既定の`cargo test`規模に収まるよう
/// 1ケースあたりの断片数を20までに抑えている。
#[test]
fn token_soup_fuzzing_does_not_panic() {
    let mut rng = Xorshift64::new(0xf0f0_1234_5678_9abc);
    for _ in 0..10_000 {
        let len = 1 + rng.range(20);
        let input = generate_token_soup(&mut rng, len);
        assert_no_panic(&input);
    }
}

/// バイト列ファザーを1万ケース走らせる。1ケースあたり最大64バイト。
#[test]
fn byte_soup_fuzzing_does_not_panic() {
    let mut rng = Xorshift64::new(0x1122_3344_5566_7788);
    for _ in 0..10_000 {
        let len = rng.range(64);
        let input = generate_byte_soup(&mut rng, len);
        assert_no_panic(&input);
    }
}

/// 再発防止コーパス: ランダム生成には頼らず、境界値を固定ケースとして残す。
/// 空入力、丸括弧の過不足、`i64`の上下限をまたぐ数値、閉じない文字列・
/// コメント、`$`パラメータの異常値など、`token_soup_fuzzing_does_not_panic`・
/// `byte_soup_fuzzing_does_not_panic`が生成する確率が低い(または辞書の
/// 組み合わせだけでは作れない)境界を狙って人手で選んである。シードを変えた
/// ランダムファザーは再現性が無いため、今後のレビューで実際に`panic`が
/// 見つかった場合は、シード値ではなく具体的な入力文字列をここへ書き写す。
#[test]
fn regression_corpus_does_not_panic() {
    let cases = [
        "",
        " ",
        ";",
        ";;;;;",
        "SELECT",
        "SELECT *",
        "SELECT * FROM",
        "SELECT 1 FROM t WHERE",
        "SELECT 1 +",
        "SELECT (((((((((((1)))))))))))",
        "SELECT ((((((((((( ",
        "SELECT 1)))))",
        "CAST(1 AS)",
        "CAST(1 AS NOTATYPE)",
        "SELECT 99999999999999999999999999999999",
        "SELECT -9223372036854775808",
        "SELECT -99999999999999999999999999999999999",
        "LIMIT -1",
        "SELECT 1 LIMIT -1 OFFSET -1",
        "SELECT $",
        "SELECT $0",
        "SELECT $99999999999999999999",
        "SELECT $-1",
        "PREPARE",
        "PREPARE p AS",
        "PREPARE p AS SELECT $1",
        "EXECUTE",
        "EXECUTE p(",
        "EXECUTE p(1,2,3,",
        "'",
        "'unterminated",
        "'unterminated\\",
        "\"",
        "SELECT '日本語の識別子と'' escaped quote'",
        "SELECT '\u{0}'",
        "SELECT 1 -- comment without newline",
        "SELECT 1 /* unterminated block comment",
        "/* only a comment */",
        "SELECT 1;;SELECT 2",
        "CREATE TABLE t ()",
        "CREATE TABLE t (id)",
        "INSERT INTO t VALUES ()",
        "SELECT * FROM t JOIN",
        "SELECT * FROM t JOIN u ON",
        "GROUP BY HAVING",
        "SELECT 1 IS IS NULL",
        "SELECT NOT NOT NOT NOT 1",
        "SELECT * FROM t WHERE a = b = c = d = e",
        "\u{feff}SELECT 1",
    ];
    for case in cases {
        assert_no_panic(case);
    }
}
