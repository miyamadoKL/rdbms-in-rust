//! minidb 全体で使うエラー型。
//!
//! 各章で機能が増えるにつれて variant を追加していく想定の骨格。

use thiserror::Error;

use crate::types::DataType;

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

    /// `Storage::set_table_stats`(第27章の`ANALYZE`が呼ぶ)に渡された
    /// `TableStats`が、対応するテーブル定義や自分自身と意味的に矛盾している
    /// エラー(第4部レビュー対応)。`CorruptCatalog`と検査項目は同じ
    /// (`validate_stats_metadata`を共有する)だが、こちらは**ディスク上の
    /// データが壊れている**ことを表す`CorruptCatalog`とは異なり、**これから
    /// 書き込もうとした入力そのもの**が不正であることを表す。呼び出し側
    /// (`Database::execute_analyze`)はこの入力検証エラーを、壊れたファイルの
    /// 復旧が必要な`CorruptCatalog`と区別できる。
    #[error("統計情報が不正です: {0}")]
    InvalidStats(String),

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

    /// すでに`Active`なトランザクションの中で`BEGIN`を実行したエラー(第30章)。
    /// このSQLサブセットは`BEGIN`の入れ子を許さない(章の本文で理由を説明する)。
    #[error("すでにトランザクションが開始されています(BEGINの入れ子は未対応です)")]
    TransactionAlreadyActive,

    /// トランザクションの外(Autocommitモード)で`COMMIT`または`ROLLBACK`を
    /// 実行したエラー(第30章)。
    #[error("有効なトランザクションがありません")]
    NoActiveTransaction,

    /// `Active`なトランザクションの中で実行した文がエラーになったあと、
    /// `ROLLBACK`以外の文を実行しようとしたエラー(第30章)。PostgreSQLに
    /// 倣い、一度失敗した文を含むトランザクションはロールバックするまで
    /// 以後の文をすべて拒否する(章の本文「Statement Error時のAbort」を参照)。
    #[error("現在のトランザクションはエラーのため中断されています。ROLLBACKだけ受け付けます")]
    TransactionAborted,

    /// この文が必要とするロックのうち少なくとも1つを、他のトランザクションが
    /// 両立しないモードで保持しているため、今すぐには獲得できなかったエラー
    /// (第31章、`crate::lock_manager::LockResult::Blocked`)。この文は
    /// 一切実行されていない(書き込みはおろか、`undo_log`への記録も無い)ため、
    /// トランザクションは`Active`のまま変化しない
    /// (`Database::finish`が`Aborted`への遷移をこのエラーだけ特別扱いする)。
    /// 呼び出し元は、ロックを塞いでいる側のトランザクションが`COMMIT`・
    /// `ROLLBACK`するのを待ってから、同じ文をもう一度実行し直すことを
    /// 想定している。
    #[error("ロックを獲得できませんでした(他のトランザクションが保持中です)")]
    WouldBlock,

    /// このトランザクションが、他のトランザクションと循環して互いのロックを
    /// 待ち合う**デッドロック**の一部として検出され、Victim Selection(第32章)
    /// によって強制的に`Aborted`へ倒されたエラー。`WouldBlock`と違い、この
    /// トランザクションは(このエラーを受け取った文自身がVictimに選ばれた場合を
    /// 除き)`ROLLBACK`(または`rollback_tx`)以外の操作をこれ以上受け付けない。
    /// 呼び出し元は`WouldBlock`のように同じ文を再試行するのではなく、
    /// トランザクション全体を最初からやり直す必要がある(本文「Victim Selection」
    /// を参照)。
    #[error("デッドロックを検出しました。このトランザクションはVictimとして強制的にABORTされました")]
    DeadlockDetected,

    /// `PREPARE`が、そのSessionにすでに登録済みの名前を指定したエラー(第37章)。
    /// PostgreSQLに倣い、同じ名前への無言の上書きは許さず、先に`DEALLOCATE`
    /// することを要求する。
    #[error("プリペア済み文はすでに存在します: {0}")]
    PreparedStatementAlreadyExists(String),

    /// `EXECUTE`・`DEALLOCATE`が、そのSessionに登録されていない名前を指定した
    /// エラー(第37章)。`PREPARE`していない名前、別のSessionで`PREPARE`した
    /// 名前(Session単位の名前空間、本文「Prepared StatementはSessionの
    /// ものである」を参照)、またはすでに`DEALLOCATE`済みの名前のいずれかが
    /// 当てはまる。
    #[error("プリペア済み文が見つかりません: {0}")]
    PreparedStatementNotFound(String),

    /// `PREPARE`の対象に、`SELECT`・`INSERT INTO`・`UPDATE`・`DELETE FROM`の
    /// いずれでもない文を指定したエラー(第37章)。
    #[error("PREPAREはSELECT・INSERT INTO・UPDATE・DELETE FROMのみ対象にできます")]
    CannotPrepareStatement,

    /// `EXECUTE`に渡した引数の個数が、`PREPARE`本体が使うプレースホルダの
    /// 個数と一致しないエラー(第37章)。
    #[error("EXECUTEの引数の個数が一致しません: {expected}個必要ですが{actual}個渡されました")]
    ParamCountMismatch {
        /// プリペア済み文が使うプレースホルダの個数(`$`の最大番号)。
        expected: usize,
        /// `EXECUTE`に渡された引数の個数。
        actual: usize,
    },
    /// `EXECUTE`に渡した値の型が、`PREPARE`時に文脈から推論した
    /// プレースホルダの型と一致しないエラー(第37章)。`NULL`はどの型の
    /// プレースホルダに対しても許す(通常の列のNULL制約と同じ扱い)。
    #[error("${index}の型が一致しません: {expected}が必要ですが{actual}が渡されました")]
    ParamTypeMismatch {
        /// プレースホルダの番号(`$1`なら`1`)。
        index: u32,
        /// `PREPARE`時に文脈から推論した型。
        expected: DataType,
        /// `EXECUTE`に渡された値の型。
        actual: DataType,
    },
    /// `PREPARE`本体の中で、同じプレースホルダ(`$n`)が矛盾する型で使われて
    /// いるエラー(第37章)。`WHERE a = $1 AND b = $1`で`a`と`b`の型が違う場合
    /// などが該当する。
    #[error("${index}の型が文中で矛盾しています: {first}と{second}")]
    ParamTypeConflict {
        index: u32,
        first: DataType,
        second: DataType,
    },

    /// `PREPARE`本体のプレースホルダ番号(`$n`)が、実装が許容する上限
    /// (`crate::session::MAX_PARAM_INDEX`)を超えているエラー(第6部レビュー
    /// 対応)。字句解析器自体は`$n`を`u32`の範囲でしか制限しないため、
    /// `$4294967295`のような入力自体は短くても、番号をそのまま
    /// `Vec::resize`の引数に使うと桁外れの確保を試みてしまう。この上限は
    /// `resize`する前に検査する。
    #[error("プレースホルダの番号が上限を超えています: ${index}(上限は${max}です)")]
    ParamIndexTooLarge {
        /// SQL文中に現れた`$n`の番号。
        index: u32,
        /// 許容する上限(`crate::session::MAX_PARAM_INDEX`)。
        max: u32,
    },

    /// 実行中の文が、`crate::cancellation::CancellationToken::cancel`による
    /// 明示的なキャンセル要求を受けて打ち切られたエラー(第38章)。クライアントの
    /// 切断検知(`crate::server`)、または`crate::session::Session::cancellation_handle`
    /// 経由の明示的な要求のどちらでも、この同じエラーになる。
    #[error("クエリがキャンセルされました")]
    QueryCancelled,

    /// 実行中の文が、設定された制限時間を超えたため打ち切られたエラー(第38章)。
    /// `crate::cancellation::CancellationToken`が持つ締切を、同期ポイントの
    /// `check`が超過と判定した場合に返る。
    #[error("クエリの実行時間が上限を超えました")]
    QueryTimeout,

    /// `Sort`・Hash JoinのBuild側・Hash Aggregateが子から集める行数が、
    /// 設定された上限(`crate::cancellation::ExecutionContext::max_operator_rows`)を
    /// 超えたエラー(第38章)。
    #[error("{operator}の収集行数が上限を超えました(上限{limit}行)")]
    MemoryLimitExceeded {
        /// 上限を超えた演算子の名前(`"Sort"`・`"Hash Join"`・`"Hash Aggregate"`)。
        operator: &'static str,
        /// 設定されていた上限行数。
        limit: usize,
    },

    /// `SHOW STATS FROM <table>`(第39章)が、`ANALYZE`を一度も実行していない
    /// テーブルを指定したエラー。列ごとの統計は`ANALYZE`(第27章)が集める
    /// ものであり、集めたことのない統計を空の表として黙って返すより、
    /// 「まだ何も集めていない」ことを明示するほうが利用者の勘違いを防げる。
    #[error("テーブル{0}はまだANALYZEが実行されていません")]
    TableNotAnalyzed(String),
}

/// minidb の操作全般で使う `Result` エイリアス。
pub type DbResult<T> = Result<T, DbError>;
