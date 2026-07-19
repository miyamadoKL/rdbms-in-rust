//! minidb 全体で使うエラー型。
//!
//! 各章で機能が増えるにつれて variant を追加していく想定の骨格。

use thiserror::Error;

/// minidb の操作全般で返されるエラー。
#[derive(Debug, Error)]
pub enum DbError {
    /// I/O 由来のエラー(ファイル読み書き等)。
    #[error("I/Oエラー: {0}")]
    Io(#[from] std::io::Error),

    /// まだ実装されていない機能を呼び出したときのエラー。
    #[error("未実装: {0}")]
    NotImplemented(String),

    /// 値の並びがSchemaの列数・型・nullable制約に適合しないエラー。
    #[error("スキーマ不一致: {0}")]
    SchemaMismatch(String),

    /// `CREATE TABLE`が、すでにカタログへ登録済みのテーブル名を指定したエラー。
    #[error("テーブルはすでに存在します: {0}")]
    DuplicateTable(String),

    /// `CREATE TABLE`の列定義に、同じ列名が2回以上出てきたエラー。
    #[error("列名が重複しています: {0}")]
    DuplicateColumn(String),

    /// `DROP TABLE`が、カタログに登録されていないテーブル名を指定したエラー。
    #[error("テーブルが存在しません: {0}")]
    TableNotFound(String),

    /// 式の評価に失敗したエラー(型不一致、ゼロ除算、整数オーバーフロー、
    /// `CAST`の失敗、未知の関数・型名など)。
    #[error("評価エラー: {0}")]
    Eval(String),

    /// SQL文字列を構文解析できなかったエラー。発生位置の行・列を持つ。
    ///
    /// `Lex`と表示形式を揃えている(`行N列M: 種別: メッセージ`)。字句解析までは
    /// 成功したが、Token列がこのSQLサブセットの文法に適合しない場合に返す。
    #[error("行{line}列{column}: 構文エラー: {message}")]
    Parse {
        /// エラーの内容。
        message: String,
        /// 発生位置の行番号(1始まり)。
        line: usize,
        /// 発生位置の列番号(1始まり)。
        column: usize,
    },

    /// SQL文字列をToken列へ変換できなかったエラー。発生位置の行・列を持つ。
    #[error("行{line}列{column}: 字句エラー: {message}")]
    Lex {
        /// エラーの内容。
        message: String,
        /// 発生位置の行番号(1始まり)。
        line: usize,
        /// 発生位置の列番号(1始まり)。
        column: usize,
    },

    /// `Binder`(第17章)がASTをBound ASTへ変換できなかったエラー。発生位置の
    /// 行・列を持つ。未知のテーブル・列参照、複数テーブルにまたがる曖昧な
    /// 列参照、式の型検査の失敗がここに当たる。`Lex`・`Parse`と表示形式を
    /// 揃えている(`行N列M: 種別: メッセージ`)ので、利用者はエラーがどの段階
    /// (字句解析・構文解析・名前解決)で起きたかを見分けられる。
    #[error("行{line}列{column}: 名前解決エラー: {message}")]
    Bind {
        /// エラーの内容。
        message: String,
        /// 発生位置の行番号(1始まり)。
        line: usize,
        /// 発生位置の列番号(1始まり)。
        column: usize,
    },

    /// File HeaderまたはPageのバイト列が壊れているエラー(Magic Number不一致、
    /// Format Version不一致、checksum不一致、バイト数不一致、未知のPage Typeなど)。
    #[error("破損したページです: {0}")]
    CorruptPage(String),

    /// Tupleのバイト列が、渡された`Schema`のもとで復元できないエラー
    /// (バイト列がNULLビットマップや値の途中で尽きている、`TEXT`の長さプレフィックス
    /// が実際の残りバイト数を超えているなど)。
    #[error("破損したタプルです: {0}")]
    CorruptTuple(String),

    /// `DiskManager`に、まだ`allocate_page`されていない(または`page_count`の
    /// 範囲外の)`PageId`を渡したエラー。
    #[error("ページ範囲外です: {0}")]
    PageOutOfRange(String),

    /// `HeapFile::insert`に渡したバイト列が、空の1ページにも収まらないほど
    /// 大きいエラー。
    #[error("挿入するデータがページに収まりません: {0}バイト")]
    TupleTooLarge(usize),

    /// `BufferPool`が新しいページを読み込もうとしたが、既存の全フレームがpin中で
    /// evictできる候補が1つもないエラー。
    #[error("バッファプールの全フレームがpin中です: {0}")]
    BufferPoolFull(String),

    /// 永続カタログ(Catalogページ)のバイト列から`Storage`の状態を復元できない
    /// エラー(宣言されたテーブル数・列数・ページ数が実際のバイト列と矛盾している、
    /// 未知の`DataType`コードが書かれている、名前が妥当なUTF-8でないなど)。
    #[error("破損したカタログです: {0}")]
    CorruptCatalog(String),

    /// `Storage`のカタログ(テーブル定義・Free Page List)をエンコードした結果が
    /// Catalogページ1枚(`PAGE_PAYLOAD_SIZE`バイト)に収まらないエラー。
    #[error("カタログがページに収まりません: {0}バイト(上限{1}バイト)")]
    CatalogTooLarge(usize, usize),

    /// `Storage::get`・`update`・`delete`に渡した`RecordId`が、指定した
    /// `TableId`のページ一覧に属していない(別のテーブルのRID、または
    /// Meta/Catalogページを指すRID)エラー。
    #[error("不正なRecordIdです: {0}")]
    InvalidRecordId(String),

    /// `Storage::create_table`が新しい`TableId`を割り当てようとしたが、
    /// `next_table_id`がすでに`u64::MAX`で、これ以上安全に加算できないエラー。
    #[error("これ以上テーブルを作成できません: TableIdの上限(u64::MAX)に達しました")]
    TableIdSpaceExhausted,

    /// `CREATE TABLE`の列定義に、`PRIMARY KEY`が2列以上に指定されたエラー(第20章)。
    /// このSQLサブセットは単一列の`PRIMARY KEY`だけに対応する。複合`PRIMARY KEY`
    /// (複数列の組で一意性を課す構文)は演習課題として読者に残す。
    #[error("PRIMARY KEYは1列にのみ指定できます(複合PRIMARY KEYはこの章の範囲外です)")]
    MultiplePrimaryKeys,

    /// `INSERT`または`UPDATE`が、`PRIMARY KEY`列に既存の行(または同じ文の
    /// 別の行)と同じ値を書き込もうとしたエラー(第20章)。
    #[error("PRIMARY KEY制約違反です: 列'{column}'の値{value}が重複しています")]
    PrimaryKeyViolation {
        /// 違反した列の名前。
        column: String,
        /// 重複していた値の表示(`Value`の利用者向け表示形式)。
        value: String,
    },

    /// `INSERT`または`UPDATE`が、`UNIQUE`列に既存の行(または同じ文の
    /// 別の行)と同じ値を書き込もうとしたエラー(第20章)。`NULL`同士は
    /// 重複とみなさない(`crate::constraints`のドキュメント参照)。
    #[error("UNIQUE制約違反です: 列'{column}'の値{value}が重複しています")]
    UniqueViolation {
        /// 違反した列の名前。
        column: String,
        /// 重複していた値の表示(`Value`の利用者向け表示形式)。
        value: String,
    },

    /// `crate::btree`(第23章)の`insert`・`lookup`に`Value::Null`をキーとして
    /// 渡したエラー。B+Treeは`NULL`をキーとして保持しない(モジュールの
    /// ドキュメント参照)。`NULL`を持つ行をインデックスへ入れない判断は、
    /// 呼び出し側(第24章の`CREATE INDEX`・Index Maintenance)の責務であり、
    /// このエラーはその呼び出し側が誤って`NULL`を渡した場合の防御である。
    #[error("NULLはB+Treeのキーにできません")]
    NullKeyNotAllowed,

    /// `crate::btree`(第23章)の`BTree::create`で決めたキー型と異なる型の
    /// `Value`を`insert`・`lookup`に渡したエラー。
    #[error("B+Treeのキー型が一致しません: {expected}型のツリーに{actual}型の値を渡しました")]
    BTreeKeyTypeMismatch {
        /// `BTree::create`で決めたキー型。
        expected: String,
        /// 実際に渡された値の型。
        actual: String,
    },

    /// `crate::btree`(第23章)の`insert`に渡したキー(またはLeaf Split・
    /// Internal Splitが親へ押し上げようとした区切りキー)1件だけでも、
    /// 空のLeaf PageまたはInternal Pageに収まらないほど大きいエラー。
    /// `HeapFile::insert`(第13章)の`DbError::TupleTooLarge`と同じ理由で、
    /// これ以上分割してもページに収まらない場合の割り切りとして返す。
    #[error("B+Treeのキーがページに収まりません: {0}バイト")]
    BTreeKeyTooLarge(usize),

    /// `crate::btree`(第24章)が`unique`フラグを立てて作られた索引に対して、
    /// すでに存在するキーを`insert`しようとしたエラー。この索引自身は
    /// どの列がPRIMARY KEY・UNIQUEかを知らないため、列名を含まない。
    /// 呼び出し側(`crate::index::check_uniqueness_with_index`、または
    /// `crate::storage::Storage`のIndex Maintenance)が、この章の
    /// `DbError::PrimaryKeyViolation`・`DbError::UniqueViolation`(列名つき)へ
    /// 翻訳してから利用者へ返す。
    #[error("B+Tree索引のunique制約に違反しています")]
    BTreeUniqueViolation,

    /// `CREATE INDEX`が、すでにカタログへ登録済みの索引名を指定したエラー(第24章)。
    #[error("索引はすでに存在します: {0}")]
    DuplicateIndex(String),

    /// `DROP INDEX`が、カタログに登録されていない索引名を指定したエラー(第24章)。
    #[error("索引が存在しません: {0}")]
    IndexNotFound(String),

    /// `DROP INDEX`が、`PRIMARY KEY`・`UNIQUE`列に対応して自動生成された
    /// 制約索引を指定したエラー(第3部2巡目レビュー対応)。この索引を
    /// `DROP INDEX`で消せてしまうと、対応する列の一意性制約を検査する手段が
    /// 失われる。`DROP TABLE`はテーブルごとこの索引も取り除くため、
    /// このエラーの対象にはならない。
    #[error("索引'{0}'はPRIMARY KEY・UNIQUE制約が自動生成した索引のため、DROP INDEXでは削除できません")]
    CannotDropConstraintIndex(String),
}

/// minidb の操作全般で使う `Result` エイリアス。
pub type DbResult<T> = Result<T, DbError>;
