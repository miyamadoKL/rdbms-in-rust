//! 統合テスト共通のヘルパー。
//!
//! `tests/`配下の各ファイルは別々のクレートとしてコンパイルされるため、
//! 一時DBを組み立てる処理をここに集約し、`mod common;`で読み込んで使う。

use minidb::{Database, DbResult, QueryResult};

/// インメモリのDatabaseを1つ作る。各テストはこの関数から独立したDBを得る。
#[allow(dead_code)]
pub fn temp_db() -> Database {
    Database::memory()
}

/// インメモリDBを1つ作り、SQLを1文実行した結果を返す。
///
/// 1本のSQLを実行して結果だけを確認したいテストのための近道。
#[allow(dead_code)]
pub fn execute_sql(sql: &str) -> DbResult<QueryResult> {
    temp_db().execute(sql)
}

/// 永続モードの`Database`(`Database::open`、第16章)のテストで使う、一意な
/// 一時ファイルパスを返す。パス自体を作るだけで、ファイルは作らない
/// (`Database::open`がまだ存在しないパスに対して新規作成することを確認する
/// テストが、あらかじめファイルを作らずに済むようにするため)。
#[allow(dead_code)]
pub fn temp_db_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    let unique = format!(
        "minidb-persistence-test-{name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    path.push(unique);
    path
}

/// テスト専用の決定的な疑似乱数生成器(xorshift64)。`src/btree.rs`・
/// `src/slotted_page.rs`のテストモジュールが使っているものと同じ実装であり、
/// 依存クレートを増やさずシードを固定して再現できることを理由に、この章
/// (第40章)で追加するFuzzing・Property-based Test・Crash Injection Loop・
/// 決定的ランダムインターリーブの4つのテストファイルが共通で使うぶんを
/// ここへ集約した。
#[allow(dead_code)]
pub struct Xorshift64(pub u64);

#[allow(dead_code)]
impl Xorshift64 {
    pub fn new(seed: u64) -> Self {
        // 種が0だとxorshiftは0を返し続けて壊れるため、0除けの奇数へ倒す。
        Xorshift64(seed | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// `[0, bound)`の範囲の値を返す。`bound == 0`は呼び出し禁止。
    pub fn range(&mut self, bound: usize) -> usize {
        (self.next() as usize) % bound
    }

    /// `true`/`false`を確率`num/den`で返す。
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.next() % den < num
    }
}
