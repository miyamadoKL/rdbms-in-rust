//! SQL文字列を受け取り、結果を返す実行の入口。
//!
//! 第19章から、`Database::execute`は5段階のパイプラインになった。
//!
//! 1. **構文解析**(`parser::parse_statement`): SQL文字列を`Statement`(AST)へ変換する。
//! 2. **名前解決**(`binder::Binder::bind`): `Statement`をカタログと突き合わせ、
//!    テーブル名・列名を解決し、式の型を検査した`BoundStatement`(Bound AST)へ
//!    変換する。未知のテーブル・列、曖昧な列参照、型不一致は、この段階で
//!    位置情報付きの`DbError::Bind`として検出される。
//! 3. **論理計画**(`logical_plan::build_*`): `BoundStatement`(`Select`・`Insert`・
//!    `Update`・`Delete`)を、関係代数の演算子木である[`LogicalPlan`]へ変換する。
//! 4. **ルールベース最適化**(`rules::optimize`、第26章): `SELECT`が組み立てた
//!    `LogicalPlan`を、意味を変えない書き換え(Constant Folding、Boolean
//!    Simplification、Filter Merge、Predicate Pushdown、Projection Pruning)が
//!    固定点まで反復して書き換える。`INSERT`・`UPDATE`・`DELETE`はこの段階を
//!    経由しない(対象行が変わりうる書き換えはまだ無いので、今のところ通しても
//!    意味が無い)。
//! 5. **物理計画**(`physical_plan::optimize`): 書き換え後の`LogicalPlan`を、
//!    実行アルゴリズムを確定した[`crate::physical_plan::PhysicalPlan`]へ変換する。
//! 6. **実行**: `CREATE TABLE`・`DROP TABLE`はどちらの計画も経由せず、テーブル
//!    定義を直接登録・削除する(`CREATE TABLE`が`Binder`を素通りするのと同じ理由。
//!    モジュール冒頭の説明は[`crate::binder`]を参照)。`SELECT`は
//!    `PhysicalPlan`を`Box<dyn Executor>`の木へ組み立て(`build_query_executor`)、
//!    `next()`を1行ずつ呼ぶループで結果を集める([`crate::physical_plan`]の
//!    Volcanoモデル)。`INSERT`・`UPDATE`・`DELETE`は`Executor`を経由せず、
//!    第18章までと同じ`executor`モジュールの一括関数(`insert`・`update`・
//!    `delete`等)を呼ぶ(理由は[`crate::physical_plan`]冒頭の「`INSERT`・
//!    `UPDATE`・`DELETE`は`Executor`にしない」節を参照)。`EXPLAIN`は実行せず、
//!    `PhysicalPlan`の木を文字列化した`QueryResult`を返す(`execute_explain`)。
//!
//! テーブル定義と行を実際にどこへ持つかは[`Backend`]が決める。
//!
//! # `Backend`: メモリとディスクの切り替え
//!
//! 第16章から、`Database`は2つの姿を持つ。`Database::memory`が作る
//! インメモリのDatabase(第9・10章由来)は`Catalog`と`MemStorage`にテーブル
//! 定義と行を分けて持ち、プロセスの終了とともに消える。`Database::open`が作る
//! 永続モードのDatabaseは、その両方を1つの`Storage`(第15章)にまとめて持ち、
//! ファイルへ書き出す。
//!
//! この2つを`Backend`という`enum`で切り替える設計を選んだのは、`Database`の
//! 呼び出し側(`execute`とその先のSQL文ごとの実行関数)に「今どちらのバックエンド
//! を使っているか」を毎回意識させないためである。`execute`自身はバックエンドに
//! 触れず、各`execute_*`関数だけが`match &self.backend`で分岐する。`trait`で
//! 抽象化する案も検討したが、バックエンドは`Database::memory`か`Database::open`
//! かで起動時に1回だけ決まり、実行中に差し替わることはないため、動的ディスパッチ
//! や型引数を持ち込むほどの可変性が無い。`enum`の分岐のほうが、2つの実装を
//! 並べて読み比べられる分、この章の分量では見通しがよい。
//!
//! # ロックの粒度(第31章)
//!
//! `SELECT`・`INSERT`・`UPDATE`・`DELETE`は、実行の前に
//! [`crate::lock_manager::LockManager`]からロックを獲得する
//! (`acquire_scan_locks`)。獲得するロックの**粒度**(テーブル単位か、行
//! (`RecordId`)単位か)は`Backend`によって異なる。
//!
//! `Backend::Memory`は`RecordId`という概念を持たない(第30章の
//! `crate::transaction`モジュールの説明のとおり、行は`Vec<Tuple>`の並びで
//! しかない)ため、`LockKey::Table(table_id)`だけを使う。`SELECT`はテーブル
//! 全体にSharedを、`INSERT`・`UPDATE`・`DELETE`はテーブル全体にExclusiveを
//! 掛ける。この粒度はテーブルまるごとを1個の対象として扱うぶん粗いが、
//! 正しさは疑いようがない。`INSERT`もテーブル全体のExclusiveを取るため、
//! 他のトランザクションが同じテーブルにSharedを持っている間は新しい行を
//! 差し込めず、結果としてPhantom(第30章の説明を参照)も起こらない。
//!
//! `Backend::Disk`は`RecordId`を持つため、`LockKey::Tuple(table_id, rid)`を
//! 使う、より細かい粒度に切り替える。`SELECT`はその時点でテーブルに
//! **存在する行**の`RecordId`をすべて列挙し、それぞれにSharedを掛ける
//! (`WHERE`による絞り込みの前に、テーブル全体の現存する行を対象にする。
//! この単純化については本文「タプルロックの対象をどこまで絞るか」を参照)。
//! `UPDATE`・`DELETE`は`SELECT`より対象を絞り、`WHERE`に一致した行だけに
//! Exclusiveを掛けてから書き換えに入る(`acquire_write_locks`、
//! `crate::executor::storage_matching_rids`)。この絞り込みのおかげで、
//! 異なる行を書き換える2つの`UPDATE`は互いにブロックし合わない
//! (`tests/interleave_disk.rs`を参照)。**`INSERT`は何もロックしない。**
//! 新しく挿入される行の`RecordId`は、挿入が終わるまで存在しないため、
//! そもそもロックする対象が無い。この非対称性(既存の行は守られるが、まだ
//! 存在しない行は誰も守らない)が、Tuple Lockの下でもPhantomが起き続ける
//! 理由である(本文「Phantomはなぜ生き残るか」を参照)。

use std::path::Path;

use std::collections::HashMap;

use crate::ast::{
    AnalyzeStatement, BeginStatement, CheckpointStatement, CommitStatement, CreateTableStatement, DropIndexStatement,
    DropTableStatement, IsolationLevel, RollbackStatement, Statement,
};
use crate::binder::{Binder, BoundCreateIndex, BoundExpr, BoundStatement};
use crate::catalog::{Catalog, TableInfo};
use crate::error::{DbError, DbResult};
use crate::eval::FunctionRegistry;
use crate::executor;
use crate::ids::{Lsn, TableId, TransactionId};
use crate::lock_manager::{LockKey, LockManager, LockMode, LockResult};
use crate::logical_plan::{self, DeleteNode, InsertNode, LogicalPlan, UpdateNode};
use crate::physical_plan::{
    self, CounterNode, CountingExec, DiskSeqScanExec, DistinctExec, Executor, FilterExec, HashAggregateExec, HashJoinExec,
    IndexNestedLoopJoinExec, IndexScanExec, LimitExec, MemSeqScanExec, NestedLoopJoinExec, PhysicalPlan, ProjectionExec,
    SeqScanNode, SortExec, StatsLookup, ValuesExec, explain_text,
};
use crate::rules;
use crate::statistics::{StatsCollector, TableStats};
use crate::storage::Storage;
use crate::storage_mem::MemStorage;
use crate::transaction::{self, TransactionContext, TransactionState};
use crate::types::{Column, DataType, Schema, Tuple, Value};
use crate::wal::WalCursor;

/// テーブル定義と行を実際に保持する場所。
///
/// `Memory`はテーブル定義を`Catalog`(第9章)、行を`MemStorage`(第10章)という
/// プロセスのメモリ上だけの2つの部品に分けて持つ。`Disk`は両方を1つの
/// `Storage`(第15章)にまとめて持ち、ファイルへ永続化する。モジュール冒頭の
/// 説明も参照。
///
/// `Disk`の`storage`は`Box<Storage>`にしてある。第24章で`Storage`が索引の
/// メタデータ・ファイルパス(`path: PathBuf`、`indexes: HashMap<...>`)を
/// 追加で持つようになり、`Memory`variant(`Catalog` + `MemStorage`)より
/// かなり大きくなった。`Backend`全体のサイズは一番大きいvariantに合わせて
/// 確保されるため、`Box`で間接化しないと`Memory`を使うとき(インメモリDBの
/// テストなど、この教材で最も頻繁な使い方)にも`Disk`分の大きさを毎回
/// スタックに載せることになる。
enum Backend {
    Memory {
        catalog: Catalog,
        storage: MemStorage,
        /// `ANALYZE`(第27章)が集めた統計情報。`Catalog`・`MemStorage`と同じく
        /// プロセスのメモリ上だけに保持し、永続化しない
        /// (`crate::catalog`がメモリオンリーである既存方針と一貫させる)。
        stats: HashMap<TableId, TableStats>,
    },
    Disk {
        storage: Box<Storage>,
    },
}

/// minidbのデータベース1つを表す。
///
/// Scalar Functionのレジストリと、テーブル定義・行を実際に保持する
/// [`Backend`]を持つ。
pub struct Database {
    functions: FunctionRegistry,
    backend: Backend,
    /// `BEGIN`で開始した、明示的なトランザクション(第30章)。`None`は
    /// Autocommit(明示的な`BEGIN`を伴わない文を、1文ごとに独立した
    /// トランザクションとして扱うモード)を意味する。`Database`はこの1本しか
    /// 持てない(`BEGIN`の入れ子を許さない設計、本文「BEGINの入れ子をどう
    /// 扱うか」を参照)。第37章で複数セッションに分かれるまでは、1つの
    /// `Database`が持てるActiveなトランザクションは高々1本である。
    tx: Option<TransactionContext>,
    /// 次に`BEGIN`(または`begin_tx`)が割り当てる`TransactionId`(第30章)。
    /// `0`から単調増加させるだけの採番で、`Storage`側には永続化しない。
    /// プロセスを再起動すれば`0`から採番し直すが、`TransactionId`はプロセス内
    /// でのUndoの帳簿以上の役割を持たないため、再起動をまたいで一意である
    /// 必要がない。`tx`と`harness_contexts`はどちらもこの採番を共有する。
    next_txn_id: u64,
    /// 決定的インターリーブテストハーネス専用の、複数トランザクションの
    /// 対応表(第30章、後述の「決定的インターリーブテストハーネス専用の
    /// 内部API」を参照)。通常のSQL経路(`execute`)はこのフィールドに一切
    /// 触れない。
    harness_contexts: HashMap<TransactionId, TransactionContext>,
    /// SELECT・DMLが取得するShared/Exclusiveロックを管理する(第31章)。
    /// 通常のSQL経路(`execute`・`tx`)とハーネス経路(`begin_tx`・
    /// `harness_contexts`)は、この1個の`LockManager`を共有する。
    /// `TransactionId`はどちらの経路でも同じ`next_txn_id`から採番されるため、
    /// 両者が同時に同じ行・テーブルへ触れれば、この`lock_manager`を通じて
    /// 正しく衝突する(ロックの粒度は`crate::database`モジュール冒頭の
    /// 「ロックの粒度」節、統合の詳細は`execute_bound_statement`・
    /// `acquire_scan_locks`を参照)。
    lock_manager: LockManager<LockKey>,
}

/// [`Database::begin_tx`]が返す、1本のトランザクションを指す不透明な識別子。
///
/// 中身(`TransactionId`)はハーネスのコード自身も直接読む必要が無いため、
/// フィールドは非公開にしてある。`Database::execute_in_tx`・`commit_tx`・
/// `rollback_tx`へ渡す以外の使い道を持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxHandle(TransactionId);

impl Database {
    /// インメモリのDatabaseを作る。組み込みのScalar Function(`abs`、`length`)は
    /// 最初から登録済みの状態で始まり、カタログとストレージはどちらも空の
    /// 状態で始まる。
    ///
    /// 第1部から続く、テストと使い捨てのSQL実行のための入口。プロセスを
    /// 終了すればテーブルも行もすべて消える。
    pub fn memory() -> Self {
        Database {
            functions: FunctionRegistry::with_builtins(),
            backend: Backend::Memory {
                catalog: Catalog::new(),
                storage: MemStorage::new(),
                stats: HashMap::new(),
            },
            tx: None,
            next_txn_id: 0,
            harness_contexts: HashMap::new(),
            lock_manager: LockManager::new(),
        }
    }

    /// `path`のファイルに永続化されたDatabaseを開く。
    ///
    /// `path`がまだ存在しなければ、新しいデータベースファイルとして初期化する
    /// (`Storage::create`)。すでに存在すれば、その内容を読み込んで復元する
    /// (`Storage::open`)。この2つの区別は、呼び出し側に「これは新規作成か、
    /// 再オープンか」を明示的に選ばせるのではなく、パスの存在だけから自動的に
    /// 決める。第1章の冒頭で示した`Database::open("example.db")?`という
    /// コード例は、初回の実行では新規作成、2回目以降の実行では再オープンとして
    /// 動くことをこの振る舞いが支えている。
    ///
    /// キャッシュされた変更をファイルへ書き戻すには、以後[`Database::flush`]を
    /// 明示的に呼ぶ必要がある(`Storage`・`BufferPool`が第14・15章から一貫して
    /// 採っている、dirtyなページを明示的にflushするまで書き戻さない方針を
    /// 引き継ぐ)。
    pub fn open<P: AsRef<Path>>(path: P) -> DbResult<Self> {
        let path = path.as_ref();
        let storage = if path.exists() {
            Storage::open(path)?
        } else {
            Storage::create(path)?
        };
        Ok(Database {
            functions: FunctionRegistry::with_builtins(),
            backend: Backend::Disk { storage: Box::new(storage) },
            tx: None,
            next_txn_id: 0,
            harness_contexts: HashMap::new(),
            lock_manager: LockManager::new(),
        })
    }

    /// キャッシュされている変更をすべてディスクへ書き戻し、実ディスクへ同期する。
    ///
    /// インメモリのDatabase(`Database::memory`)に対しては何もしない(書き戻す
    /// 先となるファイルがそもそも無い)。永続モードのDatabase(`Database::open`)
    /// に対しては`Storage::flush`(`BufferPool`のキャッシュをOSへ書き渡す)に
    /// 続けて`Storage::sync`(OSに実ディスクへの反映を要求する)を呼ぶ。
    ///
    /// `BufferPool`は`DiskManager`をprivateフィールドとして所有しており、
    /// 呼び出し側が`sync`だけを別途呼べる経路はない。この章では「`flush`を
    /// 呼べば耐久化まで完了する」という単純な契約に揃え、`flush`と`sync`を
    /// 呼び分ける余地(グループコミットなど)は第33章のWALに譲る。`Database`
    /// 自体は`Drop`で自動的にflushしない。`HeapFile`(第13章)・`Storage`
    /// (第15章)がすでに採っている、書き戻しのタイミングを呼び出し側の
    /// `unwrap`可能な操作として明示させる設計をここでも踏襲する。
    pub fn flush(&self) -> DbResult<()> {
        match &self.backend {
            Backend::Memory { .. } => Ok(()),
            Backend::Disk { storage } => {
                storage.flush()?;
                storage.sync()
            }
        }
    }

    /// これまでにWALへ書いた全レコードを、開発者が目視で確認できる文字列へ
    /// 整形して返す(第33章)。Memoryバックエンドは常に空の`Vec`を返す
    /// (WALを持たない)。`Storage::wal_dump`の薄い委譲であり、詳しくは
    /// そちらのドキュメントを参照。
    pub fn wal_dump(&self) -> Vec<String> {
        match &self.backend {
            Backend::Memory { .. } => Vec::new(),
            Backend::Disk { storage } => storage.wal_dump(),
        }
    }

    /// `Database::open`が起動時に実行したCrash Recovery(第34章)の要約。
    /// Memoryバックエンド、または新規作成した(既存ファイルが無かった)
    /// Diskバックエンドでは`None`。
    pub fn last_recovery_report(&self) -> Option<crate::recovery::RecoveryReport> {
        match &self.backend {
            Backend::Memory { .. } => None,
            Backend::Disk { storage } => storage.last_recovery_report(),
        }
    }

    /// 現在のトランザクション状態(第30章)。`None`はAutocommit、つまり
    /// `BEGIN`していない状態を表す。
    pub fn transaction_state(&self) -> Option<TransactionState> {
        self.tx.as_ref().map(|tx| tx.state)
    }

    /// 通常のSQL経路(`execute`)が`BEGIN`で開始した、現在`Active`または
    /// `Aborted`なトランザクションの`TransactionId`(第30章)。`None`は
    /// Autocommitを表す。決定的インターリーブテストハーネス(`begin_tx`等)が
    /// 作る`TransactionId`とは別の採番だが、`Database`の中では同じカウンタ
    /// (`next_txn_id`)を共有する。
    pub fn current_transaction_id(&self) -> Option<TransactionId> {
        self.tx.as_ref().map(|tx| tx.id)
    }

    /// 現在のカタログへの参照。
    ///
    /// `Database::memory`で作ったDatabaseでのみ使える。永続モードの
    /// Database(`Database::open`)は`Catalog`という型そのものを経由せず、
    /// `Storage`(第15章)が独自にテーブル定義を保持しているため、このメソッドを
    /// 呼ぶとpanicする。
    pub fn catalog(&self) -> &Catalog {
        match &self.backend {
            Backend::Memory { catalog, .. } => catalog,
            Backend::Disk { .. } => {
                panic!("catalog()はDatabase::memory()で作ったDatabaseでのみ使えます")
            }
        }
    }

    /// AST(`Statement`)を`Binder`へ渡し、`BoundStatement`へ変換する。
    ///
    /// `Backend`のどちらであっても、`binder::CatalogLookup`を実装した
    /// `Catalog`または`Storage`をそのまま`Binder`へ渡せる(モジュール冒頭の
    /// 説明のとおり、`Binder`はどちらのバックエンドかを意識しない)。
    fn bind(&self, statement: Statement, sql: &str) -> DbResult<BoundStatement> {
        match &self.backend {
            Backend::Memory { catalog, .. } => Binder::new(catalog, &self.functions, sql).bind(statement),
            Backend::Disk { storage } => Binder::new(storage.as_ref(), &self.functions, sql).bind(statement),
        }
    }

    /// [`Database::bind`]の公開版(第37章)。
    ///
    /// `Session`が`PREPARE`を実行するときに使う。`Database::execute`は
    /// 構文解析・束縛・実行をひと続きに行うが、`PREPARE`は束縛までを
    /// 先に済ませて`BoundStatement`をSessionの名前空間に残す必要がある
    /// (`crate::session`モジュールのドキュメント参照)ため、束縛だけを
    /// 独立に呼べる入口が要る。
    pub fn bind_statement(&self, statement: Statement, sql: &str) -> DbResult<BoundStatement> {
        self.bind(statement, sql)
    }

    /// SQL文字列を1本実行し、結果を返す。
    ///
    /// 構文解析(`parser::parse_statement`)→名前解決(`Binder::bind`)→計画
    /// (`logical_plan::build_*`)→実行という4段階を順に通す。構文解析の失敗
    /// (`DbError::Lex`・`DbError::Parse`)、名前解決の失敗(`DbError::Bind`)は、
    /// どちらもそのまま呼び出し元に伝わる。`LogicalPlan`への変換自体は失敗しない
    /// (`Binder`がすでに名前・型を確定させているため、`BoundStatement`から
    /// `LogicalPlan`への変換は形を組み替えるだけで、新たに検出すべき誤りが無い)。
    /// 第30章から、この関数は`BEGIN`・`COMMIT`・`ROLLBACK`という3つの
    /// トランザクション境界文をここで直接振り分ける。それ以外の文(`SELECT`
    /// 以下、これまでどおりの9種類)は`execute_bound_statement`へ委ね、
    /// 実行結果に応じて`finish`が「Active中の失敗はAbortedへ遷移させる」
    /// (本文「Statement Error時のAbort」)を適用する。`BEGIN`・`COMMIT`・
    /// `ROLLBACK`自身の失敗(たとえば`BEGIN`の入れ子)は`finish`を経由しない。
    /// トランザクション制御文の成否とトランザクションの状態遷移は、
    /// `execute_begin`・`execute_commit`・`execute_rollback`自身がすでに
    /// 一貫した形で管理しているため、ここでさらに“失敗したら状態を変える”
    /// という規則を重ねると、たとえば入れ子`BEGIN`のエラーが進行中の
    /// トランザクションまで巻き込んでAbortedにしてしまう。
    pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
        let statement = match crate::parser::parse_statement(sql) {
            Ok(statement) => statement,
            Err(err) => return self.finish(Err(err)),
        };

        match statement {
            Statement::Begin(begin) => self.execute_begin(begin),
            Statement::Commit(commit) => self.execute_commit(commit),
            Statement::Rollback(rollback) => self.execute_rollback(rollback),
            Statement::Checkpoint(checkpoint) => self.execute_checkpoint(checkpoint),
            statement => {
                if let Some(tx) = &self.tx
                    && tx.state == TransactionState::Aborted
                {
                    return Err(aborted_error(tx.victim_of_deadlock));
                }
                let result = self.execute_bound_statement(statement, sql);
                self.finish(result)
            }
        }
    }

    /// `Active`なトランザクション中に実行した通常の文(`BEGIN`・`COMMIT`・
    /// `ROLLBACK`以外)が失敗したら、トランザクションを`Aborted`へ遷移させる。
    /// 以後は`ROLLBACK`だけを受け付ける(本文「Statement Error時のAbort」を
    /// 参照)。Autocommit(`self.tx`が`None`)の場合は何もしない。
    ///
    /// 第31章から、`DbError::WouldBlock`(ロックを獲得できなかった)だけは
    /// この遷移の対象外にする。この文は一切実行されていない(書き込みも
    /// `undo_log`への記録も無い)ため、`Aborted`へ倒す理由が無い。呼び出し元は
    /// あとで同じ文をもう一度試せる(本文「`WouldBlock`は失敗ではない」を参照)。
    fn finish(&mut self, result: DbResult<QueryResult>) -> DbResult<QueryResult> {
        if matches!(result, Err(DbError::WouldBlock)) {
            return result;
        }
        if result.is_err()
            && let Some(tx) = &mut self.tx
            && tx.state == TransactionState::Active
        {
            tx.state = TransactionState::Aborted;
        }
        result
    }

    /// この文のロックを持つべきトランザクション(第31章)。`Active`な
    /// トランザクション(`self.tx`、通常のSQL経路の`BEGIN`、またはハーネスの
    /// `execute_in_tx`が差し替えた値)があればその`TransactionId`をそのまま
    /// 使う。無ければ(Autocommit)、この1文だけのために新しい`TransactionId`を
    /// 割り当てる。Autocommitで割り当てたIDは、この文の実行が終わったら
    /// `execute_bound_statement`がその場で`lock_manager.release_all`する
    /// (本文「Autocommit文のロックは文の間だけ」を参照)。
    fn lock_owner(&mut self) -> TransactionId {
        match &self.tx {
            Some(tx) => tx.id,
            None => {
                let id = TransactionId(self.next_txn_id);
                self.next_txn_id += 1;
                id
            }
        }
    }

    /// 構文解析済みの`Statement`(`BEGIN`・`COMMIT`・`ROLLBACK`以外)を束縛し、
    /// 実行する。第16〜29章の`execute`本体そのものであり、この章が新設した
    /// トランザクション境界の判定・遷移はすべて呼び出し元(`execute`)が担う。
    ///
    /// 第31章から、実行の前に`lock_owner`でこの文のロック保持者を決める。
    /// `SELECT`・`UPDATE`・`DELETE`はそれぞれの実行関数の中でこの
    /// `TransactionId`を使ってロックを獲得する(`INSERT`がロックを取らない
    /// 理由は`crate::database`モジュール冒頭「ロックの粒度」を参照)。
    /// Autocommit(`self.tx`が`None`)であれば、文の実行が成功・失敗の
    /// どちらであっても、終わった時点でこの文のために取ったロックをすべて
    /// 手放す。`Active`なトランザクションの中であれば手放さず、
    /// `COMMIT`・`ROLLBACK`(`execute_commit`・`execute_rollback`・
    /// `commit_tx`・`rollback_tx`)まで持ち越す。これがStrict 2PLの
    /// Growing Phase(獲得だけを行い、独立したShrinking Phaseを持たない)を
    /// この実装で表している部分である。
    fn execute_bound_statement(&mut self, statement: Statement, sql: &str) -> DbResult<QueryResult> {
        let bound = self.bind(statement, sql)?;
        self.run_bound_statement(bound)
    }

    /// すでに束縛済みの文を、束縛をやり直さずに直接実行する(第37章)。
    ///
    /// `EXECUTE`(`Session::execute`)が、`PREPARE`時に確定した`BoundStatement`
    /// へパラメータを差し込んだ結果を渡すために使う。`bind`を経由しない点を
    /// 除けば[`Database::execute_bound_statement`]と全く同じロック・実行の
    /// 規律(`lock_owner`・Autocommitのロック解放)に従う。
    pub fn execute_bound_statement_prebound(&mut self, bound: BoundStatement) -> DbResult<QueryResult> {
        self.run_bound_statement(bound)
    }

    /// [`Database::execute_bound_statement`]・[`Database::execute_bound_statement_prebound`]
    /// が共有する、束縛済みの文を実際に実行する本体。
    fn run_bound_statement(&mut self, bound: BoundStatement) -> DbResult<QueryResult> {
        let owner = self.lock_owner();
        let result = match bound {
            BoundStatement::Select(select) => self.execute_select(logical_plan::build_select(*select), owner),
            BoundStatement::CreateTable(create) => self.execute_create_table(&create),
            BoundStatement::DropTable(drop) => self.execute_drop_table(&drop),
            BoundStatement::CreateIndex(create) => self.execute_create_index(create),
            BoundStatement::DropIndex(drop) => self.execute_drop_index(&drop),
            BoundStatement::Insert(insert) => self.execute_insert(logical_plan::build_insert(insert), owner),
            BoundStatement::Update(update) => self.execute_update(logical_plan::build_update(update), owner),
            BoundStatement::Delete(delete) => self.execute_delete(logical_plan::build_delete(delete), owner),
            BoundStatement::Explain { inner, analyze } => self.execute_explain(*inner, analyze, owner),
            BoundStatement::Analyze(analyze) => self.execute_analyze(analyze),
            BoundStatement::Begin(_) | BoundStatement::Commit(_) | BoundStatement::Rollback(_) | BoundStatement::Checkpoint(_) => {
                unreachable!("BEGIN・COMMIT・ROLLBACK・CHECKPOINTはexecuteの先頭ですでに処理済み")
            }
        };
        if self.tx.is_none() {
            self.lock_manager.release_all(owner);
        }
        result
    }

    /// `BEGIN`を実行する。すでに`Active`なトランザクションがあれば、その
    /// 入れ子を許さずエラーにする(本文「BEGINの入れ子をどう扱うか」を参照)。
    ///
    /// # 分離レベルの既定値(第32章)
    ///
    /// `BEGIN ISOLATION LEVEL ...`を省略した`BEGIN`単体は`RepeatableRead`を
    /// 既定にする。PostgreSQLの既定(`Read Committed`)とは異なる選択だが、
    /// このクレートは第31章の時点ですでにStrict 2PL(Shared LockもCOMMITまで
    /// 保持する、`RepeatableRead`相当の規律)で動いていた。`Read Committed`を
    /// 既定にすると、第31章までに書いた`BEGIN`を伴うテスト・本文の例すべてが
    /// (読み取りロックを文の終わりで解放する挙動へ)無言で意味を変えてしまう。
    /// 明示的に`BEGIN ISOLATION LEVEL ...`と書いた場合にだけ、その分離レベルの
    /// 規律に従う。
    fn execute_begin(&mut self, begin: BeginStatement) -> DbResult<QueryResult> {
        if self.tx.is_some() {
            return Err(DbError::TransactionAlreadyActive);
        }
        let id = TransactionId(self.next_txn_id);
        self.next_txn_id += 1;
        let level = begin.isolation_level.unwrap_or(IsolationLevel::RepeatableRead);
        self.tx = Some(TransactionContext::new(id, level));
        Ok(QueryResult::command("BEGIN"))
    }

    /// `COMMIT`を実行する。トランザクションが無ければ`DbError::NoActiveTransaction`、
    /// `Aborted`状態であれば`DbError::TransactionAborted`を返す。PostgreSQLは
    /// `Aborted`状態への`COMMIT`を暗黙の`ROLLBACK`として受理するが、この章では
    /// 採らない(本文「Statement Error時のAbort」で理由を説明する)。積んだ
    /// `undo_log`はここでは適用せず、ただ捨てる。`Active`の間に行った書き込みは
    /// すでに`backend`に反映済みであり、`COMMIT`はその状態を追認するだけで
    /// 良い。
    fn execute_commit(&mut self, _commit: CommitStatement) -> DbResult<QueryResult> {
        match &self.tx {
            None => Err(DbError::NoActiveTransaction),
            Some(tx) if tx.state == TransactionState::Aborted => Err(aborted_error(tx.victim_of_deadlock)),
            Some(_) => {
                let tx = self.tx.take().expect("直前のmatchでSomeを確認済み");
                wal_commit_if_disk(&self.backend, tx.id, tx.wal_last_lsn)?;
                self.lock_manager.release_all(tx.id);
                Ok(QueryResult::command("COMMIT"))
            }
        }
    }

    /// `ROLLBACK`を実行する。`Active`・`Aborted`のどちらの状態でも受理し、
    /// `BEGIN`以降に積んだ`undo_log`を逆順に適用してから`tx`を手放す
    /// (`crate::transaction::apply_undo_memory`・`apply_undo_disk`)。
    fn execute_rollback(&mut self, _rollback: RollbackStatement) -> DbResult<QueryResult> {
        let Some(tx) = self.tx.take() else {
            return Err(DbError::NoActiveTransaction);
        };
        match &mut self.backend {
            Backend::Memory { storage, .. } => transaction::apply_undo_memory(storage, tx.undo_log),
            Backend::Disk { storage } => wal_rollback_if_disk(storage, tx.id, tx.wal_last_lsn)?,
        }
        self.lock_manager.release_all(tx.id);
        Ok(QueryResult::command("ROLLBACK"))
    }

    /// `CHECKPOINT`を実行する(第34章)。
    ///
    /// Memoryバックエンドはそもそも永続化しない(WALを持たない)ため何もしない。
    /// Diskバックエンドは、現在Activeなトランザクションを全部、Active
    /// Transaction一覧として`Storage::checkpoint`へ渡す。通常のSQL経路の
    /// `self.tx`(高々1本)だけでなく、決定的インターリーブテストハーネス
    /// (`harness_contexts`)が持つトランザクションも含める。`self.tx`と
    /// `harness_contexts`は同じ`Database`が同時に持つ、対等なActive
    /// トランザクションの集合であり(採番自体も共有している、`begin_tx`の
    /// ドキュメントを参照)、`CHECKPOINT`の視点からハーネス経由かどうかを
    /// 区別する理由が無い。これを省くと、`begin_tx`で開始したトランザクション
    /// が`INSERT`したあと`CHECKPOINT`をまたいで再起動したとき、Analysisが
    /// そのトランザクションの存在自体を知らないままRedoだけ行い、`Commit`
    /// レコードの無い未確定の行がUndoされずに残ってしまう(この章のレビューで
    /// 実際に指摘された不具合)。トランザクションの境界文ではないため、
    /// `self.tx`・`harness_contexts`のどちらの状態も変えない。
    fn execute_checkpoint(&mut self, _checkpoint: CheckpointStatement) -> DbResult<QueryResult> {
        let mut active: Vec<(TransactionId, Option<Lsn>)> =
            self.tx.as_ref().map(|tx| vec![(tx.id, tx.wal_last_lsn)]).unwrap_or_default();
        active.extend(self.harness_contexts.values().map(|ctx| (ctx.id, ctx.wal_last_lsn)));
        match &mut self.backend {
            Backend::Memory { .. } => {}
            Backend::Disk { storage } => {
                storage.checkpoint(&active)?;
            }
        }
        Ok(QueryResult::command("CHECKPOINT"))
    }

    /// `undo`を、`Active`なトランザクションがあればその`undo_log`へ積む。
    /// Autocommit(`self.tx`が`None`)であれば、この文の変更を取り消す先が
    /// 無い(Statement Rollbackがすでに文単位のAll-or-Nothingを保証している
    /// ため、そもそも積む必要が無い)ので、そのまま捨てる。
    fn record_undo(&mut self, undo: Vec<transaction::UndoRecord>) {
        if undo.is_empty() {
            return;
        }
        if let Some(tx) = &mut self.tx {
            tx.undo_log.extend(undo);
        }
    }

    // ---- 決定的インターリーブテストハーネス専用の内部API(第30章) ----
    //
    // 通常のSQL経路(`execute`)は、`Database`が`Active`なトランザクションを
    // 高々1本しか持てない設計である(`tx: Option<TransactionContext>`)。
    // ところがトランザクションのインターリーブを検証するテストは、複数の
    // 未コミットトランザクションを、1つの`Database`の上で交互に進める必要が
    // ある。Buffer PoolとB+Treeがスレッドセーフになるのは第35章であり、実際に
    // 複数スレッドを立てて競合させることはまだできないため、「単一スレッド上で、
    // 複数のトランザクションの文を指定した順序で交互に実行する」という形で
    // インターリーブを再現する。
    //
    // `harness_contexts`は、`begin_tx`が作った`TransactionContext`を
    // `TransactionId`ごとに保持する対応表である。`execute_in_tx`は、対象の
    // `TransactionContext`を対応表から取り出して一時的に`self.tx`へ差し替え、
    // 通常のSQL経路と共有の`execute_bound_statement`・`finish`をそのまま呼んだ
    // あと、変化した`TransactionContext`(`undo_log`が伸びている、または
    // `Aborted`へ遷移している)を対応表へ戻す。実行ロジック自体
    // (`INSERT`・`UPDATE`・`DELETE`・`SELECT`のBind・実行・Undo記録・Abort遷移)
    // は通常のSQL経路と完全に共有され、ハーネスのために複製しない。
    //
    // このAPIはSQLの構文(`BEGIN`・`COMMIT`・`ROLLBACK`)を経由しない。
    // `TxHandle`は`Database`の外からは中身の見えない不透明な識別子であり、
    // SQL文字列として`BEGIN`を書く通常の経路とは独立している(モジュール冒頭の
    // 「SQL/REPL経路は単一トランザクションのまま」という設計判断のとおり)。

    /// 新しいトランザクションを開始し、以後`execute_in_tx`・`commit_tx`・
    /// `rollback_tx`で参照する`TxHandle`を返す。通常のSQL経路の`self.tx`には
    /// 触れないため、`execute`(`BEGIN`を含む)と`begin_tx`は互いに独立している。
    ///
    /// 分離レベルは`RepeatableRead`が既定になる(`execute_begin`が`BEGIN`単体に
    /// 対して選ぶ既定と同じ、理由も同じ)。他の分離レベルで開始したい場合は
    /// [`Database::begin_tx_with_isolation`]を使う。
    pub fn begin_tx(&mut self) -> TxHandle {
        self.begin_tx_with_isolation(IsolationLevel::RepeatableRead)
    }

    /// [`Database::begin_tx`]と同じだが、分離レベルを明示的に指定できる
    /// (第32章)。ハーネスのテストが`READ UNCOMMITTED`・`READ COMMITTED`・
    /// `SERIALIZABLE`でのインターリーブを組み立てるときに使う。
    pub fn begin_tx_with_isolation(&mut self, isolation_level: IsolationLevel) -> TxHandle {
        let id = TransactionId(self.next_txn_id);
        self.next_txn_id += 1;
        self.harness_contexts.insert(id, TransactionContext::new(id, isolation_level));
        TxHandle(id)
    }

    /// `handle`が指すトランザクションの中で、`sql`を1文実行する。
    ///
    /// `handle`が`Aborted`状態であれば、`ROLLBACK`と同じ文言でしか区別できない
    /// `DbError::TransactionAborted`を返す(通常のSQL経路の`execute`が
    /// `Aborted`中に他の文を拒否するのと同じ規則)。`handle`が指す
    /// トランザクションがすでに`commit_tx`・`rollback_tx`で終わっている場合は
    /// panicする(ハーネスの使い方の誤りであり、SQLの実行時エラーではない)。
    ///
    /// 第31章から、この文が必要とするロックのどれかを他の`handle`が
    /// 両立しないモードで持っていれば、`Err(DbError::WouldBlock)`を返す。
    /// この場合、`handle`のトランザクションは`Active`のまま変化せず
    /// (`finish`がこのエラーだけ`Aborted`への遷移から除外する)、`sql`は
    /// 一切実行されていない。呼び出し元(ハーネスを使うテストコード)は、
    /// ロックを塞いでいる側の`commit_tx`・`rollback_tx`を呼んだあとで、
    /// 同じ`sql`を引数にもう一度`execute_in_tx`を呼び直す。この再試行を
    /// 自動的に行うスケジューラは無い(モジュール`crate::lock_manager`冒頭の
    /// 説明を参照)。
    pub fn execute_in_tx(&mut self, handle: &TxHandle, sql: &str) -> DbResult<QueryResult> {
        self.run_in_tx(handle, |db| {
            let statement = crate::parser::parse_statement(sql)?;
            db.execute_bound_statement(statement, sql)
        })
    }

    /// [`Database::execute_in_tx`]の、すでに束縛済みの文を渡す版(第37章)。
    ///
    /// `EXECUTE`(`Session::execute`)が、`PREPARE`時に確定した`BoundStatement`
    /// へパラメータを差し込んだ結果を、`BEGIN`で開始済みのトランザクションの
    /// 中で実行するために使う。構文解析・束縛のどちらもやり直さない点だけが
    /// [`Database::execute_in_tx`]と異なり、ロック待ちでの再試行(`WouldBlock`)・
    /// `Aborted`状態での拒否・`finish`によるAbort遷移は共通の`run_in_tx`が
    /// 同じ規律で扱う。
    pub fn execute_in_tx_bound(&mut self, handle: &TxHandle, bound: BoundStatement) -> DbResult<QueryResult> {
        self.run_in_tx(handle, |db| db.run_bound_statement(bound))
    }

    /// `execute_in_tx`・`execute_in_tx_bound`が共有する、`harness_contexts`との
    /// 出し入れ・`Aborted`検査・`finish`適用をまとめた本体。`run`には
    /// 「構文解析(必要なら)して実行する」処理を渡す。`run`の実行前に`sql`の
    /// 構文解析だけが失敗した場合でも、`finish`によるAbort遷移とcontextの
    /// 復元を他の失敗と同じ経路で行う(第30章の元の`execute_in_tx`が
    /// 持っていた挙動をそのまま引き継ぐ)。
    fn run_in_tx(
        &mut self,
        handle: &TxHandle,
        run: impl FnOnce(&mut Self) -> DbResult<QueryResult>,
    ) -> DbResult<QueryResult> {
        let ctx = self
            .harness_contexts
            .remove(&handle.0)
            .expect("TxHandleはすでにcommit_tx・rollback_tx済み、または他のDatabaseのものです");
        if ctx.state == TransactionState::Aborted {
            let err = aborted_error(ctx.victim_of_deadlock);
            self.harness_contexts.insert(handle.0, ctx);
            return Err(err);
        }

        // 通常のSQL経路が使う`self.tx`を、この文の間だけ`ctx`に差し替える。
        // ハーネスのテストは`execute`(`db.execute("BEGIN")`等)を併用しない
        // 前提なので、差し替え前の`self.tx`は常に`None`のはずだが、`Option`の
        // まま保存して差し替え後に戻すことで、その前提が破られても値を失わない。
        let previous = self.tx.replace(ctx);
        let result = run(self);
        let result = self.finish(result);
        let ctx = self.tx.take().expect("runはself.txを取り除かない");
        self.tx = previous;
        self.harness_contexts.insert(handle.0, ctx);
        result
    }

    /// `handle`が指すトランザクションを確定する。`Aborted`状態であれば
    /// `DbError::TransactionAborted`を返し、`ROLLBACK`しか受け付けない
    /// (`execute_commit`と同じ規則)。
    pub fn commit_tx(&mut self, handle: TxHandle) -> DbResult<()> {
        let ctx = self
            .harness_contexts
            .remove(&handle.0)
            .expect("TxHandleはすでにcommit_tx・rollback_tx済み、または他のDatabaseのものです");
        if ctx.state == TransactionState::Aborted {
            let err = aborted_error(ctx.victim_of_deadlock);
            self.harness_contexts.insert(handle.0, ctx);
            return Err(err);
        }
        wal_commit_if_disk(&self.backend, ctx.id, ctx.wal_last_lsn)?;
        self.lock_manager.release_all(ctx.id);
        Ok(())
    }

    /// `handle`が指すトランザクションが積んだ`undo_log`(Memoryバックエンド)
    /// またはWALの`prev_lsn`連鎖(Diskバックエンド、第33章)を逆順に適用し、
    /// `BEGIN`(`begin_tx`)以降の変更を取り消す。
    pub fn rollback_tx(&mut self, handle: TxHandle) -> DbResult<()> {
        let ctx = self
            .harness_contexts
            .remove(&handle.0)
            .expect("TxHandleはすでにcommit_tx・rollback_tx済み、または他のDatabaseのものです");
        match &mut self.backend {
            Backend::Memory { storage, .. } => transaction::apply_undo_memory(storage, ctx.undo_log),
            Backend::Disk { storage } => wal_rollback_if_disk(storage, ctx.id, ctx.wal_last_lsn)?,
        }
        self.lock_manager.release_all(ctx.id);
        Ok(())
    }

    /// `CREATE TABLE`を実行し、列定義を`Schema`へ変換したうえで`Catalog`に登録し、
    /// `MemStorage`に空のテーブルを作る。
    ///
    /// `CREATE TABLE`は`Binder`による名前解決を経ない(`BoundStatement::CreateTable`
    /// はASTをそのまま持ち回す)。`Binder`が解決するのは既存のカタログエントリを
    /// 指す名前であり、`CREATE TABLE`が持つ名前(テーブル名・列名)はこれから
    /// 新しく作る名前だからである。列の型名(`BIGINT`等)をここで解決するのも
    /// 同じ理由で、`schema.column(name)`のような既存の列への参照ではなく、
    /// 新しい`Schema`を組み立てる作業の一部にすぎない。
    ///
    /// 列名の重複検査(`DbError::DuplicateColumn`)は`Schema::new`自体ではなく、
    /// ここ(`CREATE TABLE`の実行経路)で行う。`Schema`は`SELECT`の出力列を
    /// 表すのにも使われ(`executor::project`)、`SELECT a, a FROM t`のように
    /// 計算結果の列名が重複するのはSQLとして正当なので、`Schema`という型
    /// そのものに「列名は必ず一意」という不変条件を持たせることはできない。
    /// 一意性が必要なのは「実表の列定義」という文脈に限られるため、検査は
    /// その文脈を知っているこの関数に置く。列名の比較は、`Schema::index_of`
    /// や`Catalog`のテーブル名比較と同じく大文字小文字を区別する(`id`と
    /// `ID`は別の列として許す)。
    fn execute_create_table(&mut self, create: &CreateTableStatement) -> DbResult<QueryResult> {
        let mut columns = Vec::with_capacity(create.columns.len());
        let mut seen_names = std::collections::HashSet::with_capacity(create.columns.len());
        let mut primary_key_count = 0;
        for column_def in &create.columns {
            if !seen_names.insert(column_def.name.name.as_str()) {
                return Err(DbError::DuplicateColumn(column_def.name.name.clone()));
            }
            let data_type = DataType::from_sql_name(&column_def.type_name.name).ok_or_else(
                || DbError::Eval(format!("未知の型名です: {}", column_def.type_name.name)),
            )?;
            let nullable = !column_def.not_null;
            let mut column = Column::new(column_def.name.name.clone(), data_type, nullable);
            if column_def.primary_key {
                primary_key_count += 1;
                column = column.with_primary_key();
            }
            if column_def.unique {
                column = column.with_unique();
            }
            columns.push(column);
        }
        if primary_key_count > 1 {
            return Err(DbError::MultiplePrimaryKeys);
        }

        let schema = Schema::new(columns);
        // 第24章: `PRIMARY KEY`・`UNIQUE`列に対応するUNIQUE索引を自動生成する
        // ために、`Schema`(この後`storage.create_table`へ`move`する)から
        // 先に必要な情報だけを複製しておく。
        let constraint_columns: Vec<(String, bool)> = schema
            .columns()
            .iter()
            .filter(|c| c.primary_key || c.unique)
            .map(|c| (c.name.clone(), c.primary_key))
            .collect();

        match &mut self.backend {
            Backend::Memory { catalog, storage, .. } => {
                let id = catalog.create_table(&create.table.name, schema)?;
                storage.create_table(id);
            }
            Backend::Disk { storage } => {
                // 第3部2巡目レビュー対応: テーブルの登録と、対応する制約索引
                // 全部の作成を、`Storage::create_table_with_constraint_indexes`
                // 1回の呼び出しにまとめる。索引名の衝突だけでなく、索引ファイルの
                // 作成に伴うI/Oエラーなど、`create_constraint_index`が返しうる
                // どんな理由の失敗であっても、テーブルと(途中まで作った)索引の
                // 両方をきれいに戻す(詳しくは`Storage::create_table_with_constraint_indexes`
                // のドキュメントを参照)。テーブルと索引を別々の呼び出しで
                // 登録し、索引名の衝突だけを個別に事前検査していた以前の実装は、
                // それ以外の理由での失敗(たとえばテーブル名が長すぎて索引
                // ファイルのパスがOSの上限を超えるI/Oエラー)を防げなかった。
                storage.create_table_with_constraint_indexes(&create.table.name, schema, &constraint_columns)?;
            }
        }
        Ok(QueryResult::command("CREATE TABLE"))
    }

    /// `CREATE INDEX` / `CREATE UNIQUE INDEX`を実行する(第24章)。
    ///
    /// メモリバックエンド(`Database::memory`)は`crate::storage::Storage`を
    /// 持たず、索引を作る先が無いため`DbError::NotImplemented`を返す。
    /// `Binder`の`bind_create_index`は、`CatalogLookup::index_exists`が
    /// メモリバックエンドで常に`false`を返すために索引名の重複を検出できず、
    /// ここまで束縛が素通りしてくる(モジュール`crate::binder`のドキュメント
    /// を参照)。
    fn execute_create_index(&mut self, create: BoundCreateIndex) -> DbResult<QueryResult> {
        match &mut self.backend {
            Backend::Memory { .. } => Err(DbError::NotImplemented(
                "CREATE INDEXはDatabase::open(ディスクバックエンド)でのみサポートされています".to_string(),
            )),
            Backend::Disk { storage } => {
                storage.create_index(&create.index_name, &create.table_name, &create.column_name, create.unique)?;
                Ok(QueryResult::command("CREATE INDEX"))
            }
        }
    }

    /// `DROP INDEX`を実行する(第24章)。`execute_create_index`と同じ理由で、
    /// メモリバックエンドでは`DbError::NotImplemented`を返す(実際には
    /// `bind_drop_index`が「索引が見つかりません」で先に拒むため、通常の
    /// `Database::execute`経由ではここに到達しない)。
    fn execute_drop_index(&mut self, drop: &DropIndexStatement) -> DbResult<QueryResult> {
        match &mut self.backend {
            Backend::Memory { .. } => Err(DbError::NotImplemented(
                "DROP INDEXはDatabase::open(ディスクバックエンド)でのみサポートされています".to_string(),
            )),
            Backend::Disk { storage } => {
                storage.drop_index(&drop.index.name)?;
                Ok(QueryResult::command("DROP INDEX"))
            }
        }
    }

    /// `DROP TABLE`を実行し、テーブル定義とその行をまとめて削除する。
    ///
    /// テーブルが存在することは`Binder`(`bind_drop_table`)がすでに位置情報付きの
    /// `DbError::Bind`として検査済みである。ここで呼ぶ`Catalog::drop_table`・
    /// `Storage::drop_table`自身も未知のテーブル名を`DbError::TableNotFound`
    /// として検出するが、これは二重検査というより、`Catalog`・`Storage`という
    /// データ構造自身が持つべき不変条件(登録されていない名前は削除できない)を
    /// 手放さずに残しているだけである。`Binder`とbindしてから実行するまでの間に
    /// 状態が変わる余地はこの章には無いため、実際には後者が発火することはない。
    fn execute_drop_table(&mut self, drop: &DropTableStatement) -> DbResult<QueryResult> {
        match &mut self.backend {
            Backend::Memory { catalog, storage, stats } => {
                let id = catalog.drop_table(&drop.table.name)?;
                storage.drop_table(id);
                // 統計情報(第27章)も、もう存在しないテーブルの分を残さない。
                stats.remove(&id);
            }
            Backend::Disk { storage } => {
                storage.drop_table(&drop.table.name)?;
            }
        }
        Ok(QueryResult::command("DROP TABLE"))
    }

    /// `LogicalPlan`に組み立てた`SELECT`を実行する。
    ///
    /// `logical_plan::build_select`が返す木をまず`rules::optimize`(第26章)に
    /// 通し、意味を変えない書き換えを固定点まで適用してから、`physical_plan::optimize`
    /// で[`PhysicalPlan`]へ変換し、`build_query_executor`で`Box<dyn Executor>`の
    /// 木を組み立てる。`Executor::next()`を`None`が返るまで呼び続け、返った
    /// タプルを`rows`に集める。
    ///
    /// 第18章までの`eval_query_plan`は、`Filter`・`Projection`の各段が子の
    /// 結果を`Vec<Tuple>`としてまるごと受け取ってから、まるごと新しい`Vec`を
    /// 作って返す再帰関数だった。テーブルが100,000行あり`WHERE`が1行しか
    /// 残さない`SELECT`でも、`Filter`が返す前の中間結果は100,000行分の
    /// `Tuple`を一度にメモリへ載せていた。この章の`next()`ループは、`Filter`・
    /// `Projection`が子から1行ずつ引いて1行ずつ返す(`physical_plan`モジュール
    /// 冒頭の説明を参照)ため、`rows`へ最終的に集まる行数だけがメモリに載り、
    /// 木の中間段階に100,000行分の`Vec`が生まれることはない(`database`モジュールの
    /// テスト`select_does_not_materialize_the_whole_table_in_a_single_vec`で
    /// この性質を確認する)。
    ///
    /// `rows`という1つの`Vec`に最終結果を集めているのは、`QueryResult`が
    /// `rows()`で`&[Tuple]`を返す型になっているためであり、この`Vec`自体は
    /// 「最終的にクライアントへ返す結果の件数」に比例する。ストリーミング
    /// 実行が効くのは、あくまで計画の中間段階(`Filter`を通過する前の
    /// 候補行、`WHERE`に一致しなかった行)がメモリに残らないという点である。
    ///
    /// 第31章から、計画を組み立てたあと・実行するより前に、この`SELECT`が
    /// 走査するテーブルすべてに対して`owner`名義でSharedロックを獲得する
    /// (`acquire_scan_locks`)。獲得できなければ`Err(DbError::WouldBlock)`を
    /// 返し、`Executor`は一切組み立てない(ロックを取れなかった`SELECT`は
    /// 1行も読まない)。
    fn execute_select(&mut self, plan: LogicalPlan, owner: TransactionId) -> DbResult<QueryResult> {
        let mut scanned = Vec::new();
        collect_scan_tables(&plan, &mut scanned);
        self.acquire_scan_locks(owner, &scanned, LockMode::Shared)?;

        let plan = rules::optimize(plan, &self.functions);
        let physical = physical_plan::optimize(plan, self.index_storage(), self);
        let schema = physical.output_schema();
        let mut executor = self.build_query_executor(&physical, None)?;

        let mut rows = Vec::new();
        while let Some(tuple) = executor.next()? {
            rows.push(tuple);
        }
        Ok(QueryResult { schema, rows, command_tag: None })
    }

    /// `owner`の分離レベル(第32章)。通常のSQL経路の`self.tx`(`owner`と
    /// `TransactionId`が一致すれば)、無ければハーネスの`harness_contexts`を
    /// 見る。どちらにも無ければ`owner`はAutocommit用に`lock_owner`が
    /// その場で割り当てた一時IDであり(`TransactionContext`自体が存在しない)、
    /// `RepeatableRead`を返す。Autocommitの1文はそれ自体が完結したトランザク
    /// ションであり、文の終わりに`execute_bound_statement`がロックを一括で
    /// 手放す(`RepeatableRead`か`ReadCommitted`かで、文の**途中**の解放
    /// タイミングに違いは出ない)。
    fn isolation_level_of(&self, owner: TransactionId) -> IsolationLevel {
        if let Some(tx) = &self.tx
            && tx.id == owner
        {
            return tx.isolation_level;
        }
        if let Some(ctx) = self.harness_contexts.get(&owner) {
            return ctx.isolation_level;
        }
        IsolationLevel::RepeatableRead
    }

    /// `owner`名義で`key`に`mode`のロックを1つ獲得する。`Blocked`になった場合は
    /// [`Database::detect_deadlock`]でWait-for Graphを調べ、循環を検出できれば
    /// Victimを強制的に`Aborted`へ倒してから再試行する(第32章、本文
    /// 「デッドロックの検出と解決」を参照)。
    ///
    /// # 3つの結果
    ///
    /// 1. `Granted`(またはBlockedを検出・解決できて再試行が`Granted`): `Ok(())`。
    /// 2. 循環が見つからない(単に他のトランザクションが保持中): `Err(WouldBlock)`。
    /// 3. 循環が見つかり、`owner`自身がVictimに選ばれた: `Err(DeadlockDetected)`。
    ///    この場合`owner`のトランザクションはすでに`Aborted`へ遷移済みである。
    fn acquire_lock_or_detect_deadlock(&mut self, owner: TransactionId, key: LockKey, mode: LockMode) -> DbResult<()> {
        if self.lock_manager.acquire(owner, key, mode) == LockResult::Granted {
            return Ok(());
        }
        match self.detect_deadlock(owner)? {
            None => Err(DbError::WouldBlock),
            Some(victim) if victim == owner => Err(DbError::DeadlockDetected),
            Some(_) => {
                // 別のトランザクションをVictimとして倒したことで、`owner`が
                // 待ち行列の中ですでに昇格しているかもしれない
                // (`LockManager::release_all`の`promote_waiters`を参照)。
                // 昇格していれば`acquire`は`Granted`をその場で返す。まだ
                // 昇格していなければ(循環は解けたが、循環に含まれない
                // 別のトランザクションがまだ`key`を保持している場合)、
                // 通常の`WouldBlock`として呼び出し元に再試行を委ねる。
                if self.lock_manager.acquire(owner, key, mode) == LockResult::Granted {
                    Ok(())
                } else {
                    Err(DbError::WouldBlock)
                }
            }
        }
    }

    /// `owner`が新しく作った待ち要求を起点に、Wait-for Graphへ循環が生じて
    /// いないか調べる(第32章)。
    ///
    /// # Wait-for Graphの組み立てとVictim Selection
    ///
    /// `LockManager::wait_for_edges`が返す「誰が誰を待っているか」の辺から
    /// 隣接表を作り、`owner`を起点にDFSで`owner`自身へ戻ってくる経路を探す。
    /// 見つかった経路が閉路であり、この実装が検出する循環はすべて`owner`を
    /// 含む(`owner`を経由しない、無関係な部分にある循環までは探索しない。
    /// この単純化を選んだ理由は本文「検出のタイミング」を参照)。
    ///
    /// 循環が見つかったら、その中で最も**新しい**`TransactionId`(最若、
    /// 最後に`BEGIN`したトランザクション)をVictimに選ぶ
    /// (`crate::transaction`モジュールの`TransactionId`は単調増加で採番される、
    /// 本文「Victim Selection: 最若TxIDを選ぶ」で理由を説明する)。選んだ
    /// Victimは[`Database::abort_transaction`]で即座に強制Abortし、循環を
    /// 物理的に断ち切ってから`Some(victim)`を返す。循環が見つからなければ
    /// `None`を返す(呼び出し元は通常の`WouldBlock`を返す)。
    fn detect_deadlock(&mut self, owner: TransactionId) -> DbResult<Option<TransactionId>> {
        let edges = self.lock_manager.wait_for_edges();
        let mut adjacency: HashMap<TransactionId, Vec<TransactionId>> = HashMap::new();
        for (waiter, holder) in edges {
            adjacency.entry(waiter).or_default().push(holder);
        }
        for neighbors in adjacency.values_mut() {
            neighbors.sort_by_key(|t| t.0);
            neighbors.dedup();
        }

        let Some(cycle) = find_cycle_containing(&adjacency, owner) else {
            return Ok(None);
        };
        let victim = cycle.into_iter().max_by_key(|t| t.0).expect("循環は少なくとも1つの要素を持つ");
        self.abort_transaction(victim)?;
        Ok(Some(victim))
    }

    /// `victim`を強制的に`Aborted`へ倒す(第32章のVictim Selection、または
    /// 将来の章がタイムアウト等の理由で呼ぶことを想定した共通経路)。
    ///
    /// `victim`のトランザクションコンテキストは、通常のSQL経路の`self.tx`
    /// (`id`が一致する場合)か、ハーネスの`harness_contexts`のどちらかに
    /// ある。見つけた側から`undo_log`を取り出して`ROLLBACK`と同じ逆順適用を
    /// 行い、`state`を`Aborted`、`victim_of_deadlock`を`true`にする。
    ///
    /// `TransactionContext`自体は`self.tx`・`harness_contexts`のどちらの
    /// スロットからも取り除かない(`Option`を`None`にしたり`HashMap`から
    /// 取り除いたりしない)。呼び出し元がすでに`self.tx`や`harness_contexts`の
    /// 該当エントリを前提にした後始末(`execute_in_tx`が`self.tx.take()`で
    /// 対応表へ戻す、など)を書いているため、ここでスロットの形を変えると
    /// その前提が壊れる。この関数は中身(`state`・`undo_log`)だけを書き換える。
    fn abort_transaction(&mut self, victim: TransactionId) -> DbResult<()> {
        // `undo_log`だけでなく`wal_last_lsn`も、ここで使ったら
        // (`std::mem::take`で)消費してしまう。理由は本文
        // 「二重巻き戻しを防ぐ」を参照: この関数はVictim Selectionの結果として
        // 呼ばれるが、呼び出し元のトランザクションは、その後で改めて
        // `rollback_tx`・`execute_rollback`(`ROLLBACK`はAborted状態でも
        // 唯一受け付けられる文である)を呼ぶことを想定している。もし
        // `wal_last_lsn`をここで消費せず残したままにすると、その後の
        // `rollback_tx`がこの`Some(lsn)`をそのまま使って**同じWALレコードを
        // 二重に**Undoしてしまう。1回目のUndoで書き戻した値を、その後
        // 別のトランザクションが書き換えていたとしても、2回目のUndoは
        // それを気にせず古いBefore Imageで踏みつぶす。`undo_log`
        // (Memoryバックエンド)はすでに`mem::take`で空にしていたため
        // この事故を免れていたが、`wal_last_lsn`(Diskバックエンド)は
        // `Option<Lsn>`をコピーして使うだけだったため、この章の統合テストで
        // 実スレッドが`DeadlockDetected`を受けたあと`rollback_tx`を呼ぶという
        // (ごく自然な)後始末をするまで、この二重巻き戻しは表面化しなかった。
        let (undo_log, wal_last_lsn) = if let Some(tx) = &mut self.tx
            && tx.id == victim
        {
            (std::mem::take(&mut tx.undo_log), tx.wal_last_lsn.take())
        } else if let Some(ctx) = self.harness_contexts.get_mut(&victim) {
            (std::mem::take(&mut ctx.undo_log), ctx.wal_last_lsn.take())
        } else {
            return Ok(());
        };

        match &mut self.backend {
            Backend::Memory { storage, .. } => transaction::apply_undo_memory(storage, undo_log),
            Backend::Disk { storage } => wal_rollback_if_disk(storage, victim, wal_last_lsn)?,
        }
        self.lock_manager.release_all(victim);

        if let Some(tx) = &mut self.tx
            && tx.id == victim
        {
            tx.state = TransactionState::Aborted;
            tx.victim_of_deadlock = true;
        } else if let Some(ctx) = self.harness_contexts.get_mut(&victim) {
            ctx.state = TransactionState::Aborted;
            ctx.victim_of_deadlock = true;
        }
        Ok(())
    }

    /// `table_ids`が指すテーブルのうち、`SELECT`が実際にロックすべき対象の
    /// 鍵を列挙する(第31・32章)。`Backend::Memory`は`LockKey::Table`を、
    /// `Backend::Disk`はその時点でテーブルに**存在する**行の`LockKey::Tuple`を
    /// 返す(絞り込みの単純化は本文「タプルロックの対象をどこまで絞るか」を
    /// 参照)。`&self`だけで完結させているのは、`Backend::Disk`の`storage`への
    /// 借用を先に終わらせ、続く`acquire_lock_or_detect_deadlock`(`&mut self`が
    /// 要る)の呼び出しと衝突させないためである。
    fn scan_lock_keys(&self, table_ids: &[TableId]) -> DbResult<Vec<LockKey>> {
        match &self.backend {
            Backend::Memory { .. } => Ok(table_ids.iter().map(|&id| LockKey::Table(id)).collect()),
            Backend::Disk { storage } => {
                let mut keys = Vec::new();
                for &table_id in table_ids {
                    for entry in storage.scan(table_id)? {
                        let (rid, _) = entry?;
                        keys.push(LockKey::Tuple(table_id, rid));
                    }
                }
                Ok(keys)
            }
        }
    }

    /// `owner`名義で、`table_ids`が指すテーブルに対して`mode`のロックを
    /// 獲得する(第31章)。`execute_select`(`SELECT`はテーブル全体を読みうる)と、
    /// `run_insert`のMemory分岐(`INSERT`はテーブル全体にExclusiveを取る)が
    /// 使う。`Backend`によってロックの粒度を切り替える理由は`crate::database`
    /// モジュール冒頭「ロックの粒度」を参照。
    ///
    /// # 分離レベルによる読み取りロックの規律(第32章)
    ///
    /// `mode`が`Shared`(=読み取り)のときだけ、`owner`の分離レベルに応じて
    /// 次のように振る舞いを変える。`mode`が`Exclusive`(書き込み)のときは
    /// 分離レベルを見ない。書き込みロックの規律(Strict 2PL、COMMITまで保持)は
    /// 4つの分離レベルすべてで共通であり、変えているのは常に「読み取りに
    /// ロックをどこまで効かせるか」だけである(本文「分離レベルが変えるのは
    /// 読み取りの規律だけ」を参照)。
    ///
    /// - `ReadUncommitted`: 読み取りロックを一切取らない(Dirty Readを許す)。
    ///   `Ok(())`を即座に返し、`LockManager`にすら触れない。
    /// - `ReadCommitted`: 通常どおり獲得したうえで、この関数を抜ける直前に
    ///   [`LockManager::release_keys`]で**この文で新規に取得したShared
    ///   ロックだけ**を即座に手放す(Non-repeatable Readを許す)。`owner`が
    ///   この文より前から(たとえば先行する`UPDATE`によって)同じ鍵にすでに
    ///   Exclusive・Sharedロックを持っていた場合、その鍵はここでは解放しない
    ///   (`owner`が他に持っている書き込みロック等の鍵に触れないのはもちろん、
    ///   **同じ鍵であっても**元から持っていたロックはCOMMIT・ABORTまで保持する。
    ///   さもないと、`UPDATE`で確定前の変更をExclusiveロックで守っていたはず
    ///   の行が、直後の`SELECT`が同じ行をなぞっただけで解放されてしまい、
    ///   他のトランザクションがその未確定の行を書き換えられてしまう。この
    ///   章のレビューで実際に指摘された不具合)。「新規に取得した」かどうかは
    ///   [`LockManager::take_pending_shared_grants`]がその都度教えてくれる
    ///   (この関数が自前で判定しない理由は同メソッドのドキュメントを参照。
    ///   `WouldBlock`で一度待たされたあとの再試行でも正しく判定できることが
    ///   この委譲の要点である)。
    /// - `RepeatableRead`: 何もせず、獲得したロックをそのまま`COMMIT`まで
    ///   保持させる(第31章から変わらない挙動)。
    /// - `Serializable`(`Backend::Disk`のみ): 上の`RepeatableRead`と同じ
    ///   Tuple Lockに加え、`LockKey::Table(table_id)`にも`Shared`を取る。この
    ///   追加の1本が、Phantomを起こす`INSERT`(`run_insert`が同じ分離レベルで
    ///   取る`LockKey::Table`への`Exclusive`)と衝突する(本文「Serializableは
    ///   どうPhantomを防ぐか」を参照)。`Backend::Memory`は元から`LockKey::Table`
    ///   だけを使うため、この追加は不要である。
    fn acquire_scan_locks(&mut self, owner: TransactionId, table_ids: &[TableId], mode: LockMode) -> DbResult<()> {
        let level = self.isolation_level_of(owner);
        if level == IsolationLevel::ReadUncommitted && mode == LockMode::Shared {
            return Ok(());
        }

        let mut keys = self.scan_lock_keys(table_ids)?;
        if level == IsolationLevel::Serializable && mode == LockMode::Shared && matches!(&self.backend, Backend::Disk { .. })
        {
            keys.extend(table_ids.iter().map(|&id| LockKey::Table(id)));
        }

        for &key in &keys {
            self.acquire_lock_or_detect_deadlock(owner, key, mode)?;
        }

        if level == IsolationLevel::ReadCommitted && mode == LockMode::Shared {
            let newly_acquired = self.lock_manager.take_pending_shared_grants(owner);
            self.lock_manager.release_keys(owner, &newly_acquired);
        }
        Ok(())
    }

    /// `table_id`のうち`predicate`に一致する行の鍵を列挙する(第31・32章、
    /// `acquire_write_locks`が使う)。`scan_lock_keys`と同じ理由で`&self`だけで
    /// 完結させている。
    fn write_lock_keys(
        &self,
        table_id: TableId,
        schema: &Schema,
        predicate: Option<&BoundExpr>,
    ) -> DbResult<Vec<LockKey>> {
        match &self.backend {
            Backend::Memory { .. } => Ok(vec![LockKey::Table(table_id)]),
            Backend::Disk { storage } => {
                let rids = executor::storage_matching_rids(storage, table_id, schema, &self.functions, predicate)?;
                Ok(rids.into_iter().map(|rid| LockKey::Tuple(table_id, rid)).collect())
            }
        }
    }

    /// `owner`名義で、`table_id`のうち`predicate`に一致する行にExclusive
    /// ロックを獲得する(第31章、`run_update`・`run_delete`が使う)。書き込み
    /// ロックの規律は分離レベルに関係なく常にStrict 2PLであるため
    /// (`acquire_scan_locks`のドキュメントを参照)、この関数は`isolation_level_of`
    /// を見ない。
    ///
    /// `Backend::Memory`はテーブル全体のExclusiveを取る(`acquire_scan_locks`と
    /// 同じ粒度)。`Backend::Disk`は`crate::executor::storage_matching_rids`で
    /// `predicate`に一致した行の`RecordId`だけを先に確定させ、その行だけを
    /// ロックする。この絞り込みのおかげで、`id`の異なる行を書き換える2つの
    /// `UPDATE`は、`Backend::Disk`のもとでは互いにブロックし合わない
    /// (`tests/interleave_disk.rs`の`different_rows_do_not_block_each_other_under_tuple_lock`
    /// を参照)。`acquire_scan_locks`(`SELECT`が使う、絞り込み前の全行を
    /// ロックする関数)より細かい粒度になっているのは、対象が`SELECT`より
    /// 単純(常にただ1個のテーブル)だからである。
    fn acquire_write_locks(
        &mut self,
        owner: TransactionId,
        table_id: TableId,
        schema: &Schema,
        predicate: Option<&BoundExpr>,
    ) -> DbResult<()> {
        let keys = self.write_lock_keys(table_id, schema, predicate)?;
        for key in keys {
            self.acquire_lock_or_detect_deadlock(owner, key, LockMode::Exclusive)?;
        }
        Ok(())
    }

    /// `PhysicalPlan`の木を根から葉へたどり、対応する[`Executor`]を組み立てる。
    ///
    /// `SeqScan`・`Values`が行を生成する葉であり、`Filter`・`Projection`は
    /// 子の`Executor`を`Box<dyn Executor>`として持つ中間ノードである。この
    /// 関数自体は木を1回だけたどって`Executor`の入れ子を組み立てるだけで、
    /// 行を実際に読みに行くのは呼び出し側が`next()`を呼んだときである(木の
    /// 組み立てと実行が分離しているのがVolcanoモデルの特徴で、`eval_query_plan`
    /// (第18章)が組み立てと実行を1回の再帰呼び出しで同時に行っていたのとは
    /// 異なる)。
    ///
    /// `SeqScan`だけが`&self.backend`(`Memory`か`Disk`か)を見る。`Filter`・
    /// `Projection`は供給源を意識せず、`Box<dyn Executor>`という共通の
    /// インターフェースだけを相手にする。`Insert`・`Update`・`Delete`は
    /// この関数を経由しない(`crate::physical_plan`冒頭の説明を参照)ため、
    /// ここに渡ってくることはない。
    ///
    /// `counters`が`Some`(`EXPLAIN ANALYZE`、第27章)なら、組み立てた
    /// `Executor`を[`CountingExec`]でラップしたうえで返す。`counters`の木は
    /// `plan.children()`と同じ形(`CounterNode::build`が複製したもの)を
    /// 持つため、子へ再帰するたびに`counters.map(|n| &n.children[i])`で
    /// 対応する子のカウンタへ降りていける。`None`(通常の`SELECT`・
    /// `ANALYZE`)なら計測のオーバーヘッドを一切かけない。
    fn build_query_executor<'a>(
        &'a self,
        plan: &'a PhysicalPlan,
        counters: Option<&CounterNode>,
    ) -> DbResult<Box<dyn Executor + 'a>> {
        let exec = self.build_query_executor_inner(plan, counters)?;
        Ok(match counters {
            Some(node) => Box::new(CountingExec::new(exec, node.count.clone())),
            None => exec,
        })
    }

    fn build_query_executor_inner<'a>(
        &'a self,
        plan: &'a PhysicalPlan,
        counters: Option<&CounterNode>,
    ) -> DbResult<Box<dyn Executor + 'a>> {
        let child = |index: usize| counters.map(|n| &n.children[index]);
        match plan {
            PhysicalPlan::SeqScan(scan) => {
                let exec: Box<dyn Executor + 'a> = match &self.backend {
                    Backend::Memory { storage, .. } => {
                        let mem_table = storage
                            .table(scan.table_id)
                            .expect("catalogに登録されたテーブルはstorageにも必ず存在する");
                        Box::new(MemSeqScanExec::new(&scan.schema, mem_table))
                    }
                    Backend::Disk { storage } => Box::new(DiskSeqScanExec::new(storage, scan.table_id, &scan.schema)?),
                };
                Ok(exec)
            }
            PhysicalPlan::IndexScan(scan) => {
                // `optimize`が`IndexScan`を選ぶのは`self.index_storage()`が
                // `Some`を返した(=`Backend::Disk`の)ときだけである
                // (`physical_plan::optimize`のドキュメントを参照)。
                let Backend::Disk { storage } = &self.backend else {
                    unreachable!("IndexScanはBackend::Diskのときにしかoptimizeが選ばない")
                };
                let exec = IndexScanExec::new(storage, scan.table_id, &scan.schema, &scan.index_name, &scan.kind)?;
                Ok(Box::new(exec))
            }
            PhysicalPlan::Values(values) => {
                let exec = ValuesExec::new(values.schema.clone(), &self.functions, &values.rows)?;
                Ok(Box::new(exec))
            }
            PhysicalPlan::Filter(filter) => {
                let input = self.build_query_executor(&filter.input, child(0))?;
                Ok(Box::new(FilterExec::new(input, &filter.predicate, &self.functions)))
            }
            PhysicalPlan::NestedLoopJoin(join) => {
                let left = self.build_query_executor(&join.left, child(0))?;
                let right = self.build_query_executor(&join.right, child(1))?;
                let exec = NestedLoopJoinExec::new(left, right, &join.condition, &self.functions)?;
                Ok(Box::new(exec))
            }
            PhysicalPlan::HashJoin(join) => {
                let left = self.build_query_executor(&join.left, child(0))?;
                let right = self.build_query_executor(&join.right, child(1))?;
                let exec = HashJoinExec::new(left, right, &join.keys, &self.functions)?;
                Ok(Box::new(exec))
            }
            PhysicalPlan::IndexNestedLoopJoin(join) => {
                // `IndexScan`と同じ理由で、`optimize`がこのノードを選ぶのは
                // 常に`Backend::Disk`のときだけである。
                let Backend::Disk { storage } = &self.backend else {
                    unreachable!("IndexNestedLoopJoinはBackend::Diskのときにしかoptimizeが選ばない")
                };
                let left = self.build_query_executor(&join.left, child(0))?;
                let exec = IndexNestedLoopJoinExec::new(
                    left,
                    storage,
                    join.table_id,
                    join.schema.clone(),
                    &join.index_name,
                    &join.outer_key,
                    &self.functions,
                );
                Ok(Box::new(exec))
            }
            PhysicalPlan::Aggregate(aggregate) => {
                let input = self.build_query_executor(&aggregate.input, child(0))?;
                let exec = HashAggregateExec::new(
                    input,
                    &aggregate.group_by,
                    &aggregate.calls,
                    aggregate.schema.clone(),
                    &self.functions,
                )?;
                Ok(Box::new(exec))
            }
            PhysicalPlan::Projection(projection) => {
                let input = self.build_query_executor(&projection.input, child(0))?;
                Ok(Box::new(ProjectionExec::new(input, &projection.projection, &self.functions)))
            }
            PhysicalPlan::Distinct(distinct) => {
                let input = self.build_query_executor(&distinct.input, child(0))?;
                Ok(Box::new(DistinctExec::new(input)))
            }
            PhysicalPlan::Sort(sort) => {
                let input = self.build_query_executor(&sort.input, child(0))?;
                Ok(Box::new(SortExec::new(input, &sort.keys, &self.functions)?))
            }
            PhysicalPlan::Limit(limit) => {
                let input = self.build_query_executor(&limit.input, child(0))?;
                Ok(Box::new(LimitExec::new(input, limit.limit, limit.offset)))
            }
            PhysicalPlan::Insert(_) | PhysicalPlan::Update(_) | PhysicalPlan::Delete(_) => {
                unreachable!("Insert/Update/DeleteはSELECTの計画に現れない(logical_plan::build_selectは作らない)")
            }
        }
    }

    /// `EXPLAIN [ANALYZE]`を実行する。対象の文を`LogicalPlan`・`PhysicalPlan`へ
    /// 変換し、各演算子に推定行数(`rows=`)を添えた木を文字列化した
    /// `QueryResult`を返す。
    ///
    /// `SELECT`は`execute_select`と同じく`rules::optimize`(第26章)を経由する。
    /// `EXPLAIN`が見せる計画は、実際に実行される計画そのものでなければならない
    /// (ここだけルールベース最適化を素通りすると、`EXPLAIN`の表示と実際の
    /// 実行計画が食い違ってしまう)。
    ///
    /// `inner`は`Parser`(第19章)がすでに`SELECT`・`INSERT INTO`・`UPDATE`・
    /// `DELETE FROM`の4種類に絞っているため、`CreateTable`・`DropTable`・
    /// `CreateIndex`・`DropIndex`(第24章)・`ANALYZE`(第27章)・入れ子の
    /// `Explain`はここに渡ってこない。
    ///
    /// `analyze`が`true`(第27章、PostgreSQLの`EXPLAIN ANALYZE`に相当)なら、
    /// 対象の文を実際に実行し、実測行数(`actual=`)も併記する。
    ///
    /// **`SELECT`と`INSERT`/`UPDATE`/`DELETE`とで、`actual=`を添えられる
    /// 範囲が異なる**。`SELECT`は`Executor`の木をそのまま実行できる
    /// (`crate::physical_plan`冒頭の説明)ため、[`CountingExec`]で全ノードを
    /// ラップし、演算子ごとの実測行数を集められる。`INSERT`/`UPDATE`/`DELETE`
    /// は`Executor`を経由しない一括処理(`run_insert`等)であり、演算子ごとの
    /// 内訳を計測する手段が無い。そのためこの章では、`INSERT`/`UPDATE`/
    /// `DELETE`の`EXPLAIN ANALYZE`は**根のノード1行にだけ**`actual=`
    /// (実際に書き込まれた行数)を添え、`input`側(`Values`・`Scan`)の
    /// サブツリーは推定行数のみを表示する。
    ///
    /// もう1つ明記しておく必要があるのは、`EXPLAIN ANALYZE INSERT`/`UPDATE`/
    /// `DELETE`は**実際に書き込みを行う**という点である。PostgreSQLの
    /// `EXPLAIN (ANALYZE, ...)`は計測後に自動でロールバックするが、この教材は
    /// そこまでしない。`run_insert`等を直接呼ぶため、`Active`なトランザクション
    /// の中で実行すれば第30章のUndoにも通常どおり乗り(そのトランザクションを
    /// `ROLLBACK`すれば計測ぶんの書き込みも一緒に消える)、Autocommitで
    /// 実行すればそのまま確定する。どちらの場合も、`EXPLAIN ANALYZE INSERT`を
    /// 実行した時点でテーブルの行は実際に増える。
    ///
    /// **この章のロックは、`EXPLAIN`の実行経路には組み込んでいない。**
    /// `EXPLAIN`(非`ANALYZE`)は`SELECT`・`Executor`を実行せず計画を文字列化
    /// するだけなので、そもそもロックを取る理由が無い。`EXPLAIN ANALYZE`は
    /// 実際に`Executor`を実行する(`SELECT`の分岐)か`run_insert`等を呼ぶ
    /// (`INSERT`/`UPDATE`/`DELETE`の分岐)ため本来はロックが必要だが、
    /// `SELECT`の分岐は`execute_select`を経由せず独自に`Executor`を組み立てて
    /// おり、ここに`acquire_scan_locks`を差し込むには`execute_select`と
    /// ほぼ同じ配線をもう1箇所複製する必要がある。この章はその複製を見送り、
    /// `EXPLAIN ANALYZE`をロックの対象外として残す(演習課題)。
    fn execute_explain(&mut self, inner: BoundStatement, analyze: bool, owner: TransactionId) -> DbResult<QueryResult> {
        match inner {
            BoundStatement::Select(select) => {
                let logical = rules::optimize(logical_plan::build_select(*select), &self.functions);
                let physical = physical_plan::optimize(logical, self.index_storage(), self);
                if analyze {
                    let counters = CounterNode::build(&physical);
                    let mut executor = self.build_query_executor(&physical, Some(&counters))?;
                    while executor.next()?.is_some() {}
                    Ok(QueryResult::explain(explain_text(&physical, self, self.index_storage(), Some(&counters))))
                } else {
                    Ok(QueryResult::explain(explain_text(&physical, self, self.index_storage(), None)))
                }
            }
            BoundStatement::Insert(insert) => {
                let physical = physical_plan::optimize(logical_plan::build_insert(insert.clone()), self.index_storage(), self);
                let text = explain_text(&physical, self, self.index_storage(), None);
                if analyze {
                    let count = self.run_insert(logical_plan::build_insert(insert), owner)?;
                    Ok(QueryResult::explain(append_actual_to_root_line(&text, count)))
                } else {
                    Ok(QueryResult::explain(text))
                }
            }
            BoundStatement::Update(update) => {
                let physical = physical_plan::optimize(logical_plan::build_update(update.clone()), self.index_storage(), self);
                let text = explain_text(&physical, self, self.index_storage(), None);
                if analyze {
                    let count = self.run_update(logical_plan::build_update(update), owner)?;
                    Ok(QueryResult::explain(append_actual_to_root_line(&text, count)))
                } else {
                    Ok(QueryResult::explain(text))
                }
            }
            BoundStatement::Delete(delete) => {
                let physical = physical_plan::optimize(logical_plan::build_delete(delete.clone()), self.index_storage(), self);
                let text = explain_text(&physical, self, self.index_storage(), None);
                if analyze {
                    let count = self.run_delete(logical_plan::build_delete(delete), owner)?;
                    Ok(QueryResult::explain(append_actual_to_root_line(&text, count)))
                } else {
                    Ok(QueryResult::explain(text))
                }
            }
            BoundStatement::CreateTable(_)
            | BoundStatement::DropTable(_)
            | BoundStatement::CreateIndex(_)
            | BoundStatement::DropIndex(_)
            | BoundStatement::Explain { .. }
            | BoundStatement::Analyze(_)
            | BoundStatement::Begin(_)
            | BoundStatement::Commit(_)
            | BoundStatement::Rollback(_)
            | BoundStatement::Checkpoint(_) => {
                unreachable!(
                    "ParserがEXPLAINの対象をSELECT・INSERT INTO・UPDATE・DELETE FROMに制限している"
                )
            }
        }
    }

    /// `physical_plan::optimize`にPoint/Range Index Scan・Index Nested Loop
    /// Joinのアクセスパスを検討させてよい`Storage`を返す(第25章)。
    ///
    /// 索引は`Backend::Disk`だけが持てる(第24章、`Database::memory`は
    /// `CREATE INDEX`自体を拒否する)ため、`Backend::Memory`では常に`None`を
    /// 返し、`optimize`は`Scan`を`SeqScan`のまま(索引を検討せず)変換する。
    fn index_storage(&self) -> Option<&Storage> {
        match &self.backend {
            Backend::Memory { .. } => None,
            Backend::Disk { storage } => Some(storage.as_ref()),
        }
    }

    /// `INSERT INTO`を実行する。`executor::insert`(または`executor::storage_insert`)
    /// が、`VALUES`の評価から書き込みまでを行う。
    ///
    /// `plan`は`logical_plan::build_insert`が組み立てた`Insert(Values)`の木で、
    /// 根が`LogicalPlan::Insert`、その唯一の子が`LogicalPlan::Values`である
    /// ことは`build_insert`の作り方から保証されている。`Values`の`rows`は
    /// まだ評価していない`Expr`のままなので、`executor::insert`・
    /// `storage_insert`にそのまま渡す(第17章までの`BoundInsert::rows`と
    /// 同じ扱い)。
    ///
    /// テーブル名・明示された列名の解決は`Binder`の`bind_insert`が済ませて
    /// いるため、`InsertNode`は`table_id`と`schema`を独立に(カタログからの
    /// 借用ではなく複製として)持っている。第16章まではこの複製を`execute_insert`
    /// 自身が`table_info.clone()`という形で行っていたが、`Binder`が返す時点で
    /// 複製済みになったことで、その回避策はここでは要らなくなった。
    ///
    /// **第31章から: `INSERT`のロックはバックエンドで非対称になる。**
    /// `Backend::Memory`ではテーブル全体にExclusiveを取る(Table Lockしか
    /// 粒度が無いため、`SELECT`・`UPDATE`・`DELETE`と同じ対象を奪い合う)。
    /// `Backend::Disk`では**何もロックしない**。理由は`crate::database`
    /// モジュール冒頭「ロックの粒度」を参照(挿入する行の`RecordId`は挿入が
    /// 終わるまで存在せず、ロックする対象が無い)。
    fn execute_insert(&mut self, plan: LogicalPlan, owner: TransactionId) -> DbResult<QueryResult> {
        let count = self.run_insert(plan, owner)?;
        Ok(QueryResult::command_with_count("INSERT", count))
    }

    /// `execute_insert`の中身のうち、実際に書き込んで影響行数を返す部分。
    /// `EXPLAIN ANALYZE INSERT`(第27章、`execute_explain`)も、`QueryResult`
    /// ではなく実測行数そのものを必要とするため、この部分だけを共有する。
    fn run_insert(&mut self, plan: LogicalPlan, owner: TransactionId) -> DbResult<usize> {
        let LogicalPlan::Insert(InsertNode { table_id, schema, columns, input, .. }) = plan else {
            unreachable!("logical_plan::build_insertは常にLogicalPlan::Insertを返す")
        };
        let LogicalPlan::Values(values) = *input else {
            unreachable!("logical_plan::build_insertはInsertの子に常にValuesを積む")
        };
        if matches!(&self.backend, Backend::Memory { .. }) {
            self.acquire_scan_locks(owner, &[table_id], LockMode::Exclusive)?;
        } else if self.isolation_level_of(owner) == IsolationLevel::Serializable {
            // `Backend::Disk`の`INSERT`は通常どこもロックしない(モジュール
            // 冒頭「ロックの粒度」を参照、新しい行の`RecordId`は挿入が終わる
            // までロックする対象自体が無い)。`Serializable`のときだけ例外で、
            // `acquire_scan_locks`が同じ分離レベルの`SELECT`に取らせる
            // `LockKey::Table`のSharedと衝突させるため、`Exclusive`を先に
            // 取る(本文「SerializableはどうPhantomを防ぐか」を参照)。
            self.acquire_lock_or_detect_deadlock(owner, LockKey::Table(table_id), LockMode::Exclusive)?;
        }

        match &mut self.backend {
            Backend::Memory { storage, .. } => {
                let mut undo = Vec::new();
                let mem_table =
                    storage.table_mut(table_id).expect("catalogに登録されたテーブルはstorageにも必ず存在する");
                let result = executor::insert(
                    mem_table,
                    table_id,
                    &schema,
                    &self.functions,
                    columns.as_deref(),
                    &values.rows,
                    &mut undo,
                );
                self.record_undo(undo);
                result
            }
            Backend::Disk { storage } => run_disk_dml(storage, &mut self.tx, &mut self.next_txn_id, |storage, wal| {
                executor::storage_insert(storage, table_id, &schema, &self.functions, columns.as_deref(), &values.rows, wal)
            }),
        }
    }

    /// `UPDATE`を実行する。`executor::update`(または`executor::storage_update`)
    /// が、`WHERE`に一致した行への`SET`の適用までを行う。テーブル名・`SET`の
    /// 対象列・`WHERE`の名前解決と型検査は、いずれも`Binder`の`bind_update`が
    /// 済ませている。
    ///
    /// `UpdateNode::input`(`Scan`)は、書き換え対象のテーブルを演算子木として
    /// 表すために持っているが、`executor::update`自身が「走査しながら
    /// `predicate`を評価し、一致した行だけ書き換える」という1回の走査に
    /// まとめて行うため、ここでは`input`を実際にたどらず`table_id`・`schema`
    /// だけを取り出す(`logical_plan::build_update`のドキュメント参照)。
    fn execute_update(&mut self, plan: LogicalPlan, owner: TransactionId) -> DbResult<QueryResult> {
        let count = self.run_update(plan, owner)?;
        Ok(QueryResult::command_with_count("UPDATE", count))
    }

    /// `execute_update`と`EXPLAIN ANALYZE UPDATE`が共有する、実際に書き込む部分。
    ///
    /// 第31章から、書き換えに入る前に`owner`名義で対象行へExclusiveロックを
    /// 獲得する(`acquire_write_locks`)。獲得できなければ
    /// `Err(DbError::WouldBlock)`を返し、`executor::update`・`storage_update`は
    /// 一切呼ばない(ロックを取れなかった`UPDATE`は1行も書き換えない)。
    fn run_update(&mut self, plan: LogicalPlan, owner: TransactionId) -> DbResult<usize> {
        let LogicalPlan::Update(UpdateNode { table_id, schema, assignments, predicate, .. }) = plan else {
            unreachable!("logical_plan::build_updateは常にLogicalPlan::Updateを返す")
        };
        self.acquire_write_locks(owner, table_id, &schema, predicate.as_ref())?;

        match &mut self.backend {
            Backend::Memory { storage, .. } => {
                let mut undo = Vec::new();
                let mem_table =
                    storage.table_mut(table_id).expect("catalogに登録されたテーブルはstorageにも必ず存在する");
                let result = executor::update(
                    mem_table,
                    table_id,
                    &schema,
                    &self.functions,
                    &assignments,
                    predicate.as_ref(),
                    &mut undo,
                );
                self.record_undo(undo);
                result
            }
            Backend::Disk { storage } => run_disk_dml(storage, &mut self.tx, &mut self.next_txn_id, |storage, wal| {
                executor::storage_update(storage, table_id, &schema, &self.functions, &assignments, predicate.as_ref(), wal)
            }),
        }
    }

    /// `DELETE FROM`を実行する。`executor::delete`(または`executor::storage_delete`)
    /// が、`WHERE`に一致した行の削除までを行う。テーブル名・`WHERE`の名前解決と
    /// 型検査は`Binder`の`bind_delete`が済ませている。`DeleteNode::input`を
    /// 実際にたどらない理由は`execute_update`と同じ。
    fn execute_delete(&mut self, plan: LogicalPlan, owner: TransactionId) -> DbResult<QueryResult> {
        let count = self.run_delete(plan, owner)?;
        Ok(QueryResult::command_with_count("DELETE", count))
    }

    /// `execute_delete`と`EXPLAIN ANALYZE DELETE`が共有する、実際に書き込む部分。
    /// ロックの獲得は`run_update`と同じ(`acquire_write_locks`を呼ぶ)。
    fn run_delete(&mut self, plan: LogicalPlan, owner: TransactionId) -> DbResult<usize> {
        let LogicalPlan::Delete(DeleteNode { table_id, schema, predicate, .. }) = plan else {
            unreachable!("logical_plan::build_deleteは常にLogicalPlan::Deleteを返す")
        };
        self.acquire_write_locks(owner, table_id, &schema, predicate.as_ref())?;

        match &mut self.backend {
            Backend::Memory { storage, .. } => {
                let mut undo = Vec::new();
                let mem_table =
                    storage.table_mut(table_id).expect("catalogに登録されたテーブルはstorageにも必ず存在する");
                let result = executor::delete(mem_table, table_id, &schema, &self.functions, predicate.as_ref(), &mut undo);
                self.record_undo(undo);
                result
            }
            Backend::Disk { storage } => run_disk_dml(storage, &mut self.tx, &mut self.next_txn_id, |storage, wal| {
                executor::storage_delete(storage, table_id, &schema, &self.functions, predicate.as_ref(), wal)
            }),
        }
    }

    /// `ANALYZE [テーブル名]`を実行する(第27章)。
    ///
    /// 対象テーブルを`SeqScan`相当の全件走査(`build_query_executor`が
    /// 組み立てる`Executor`、`Backend::Memory`・`Backend::Disk`のどちらでも
    /// 同じ経路)で1回走査し、[`StatsCollector`]へ1行ずつ渡して統計を集める。
    /// テーブル名が省略されていれば、カタログに登録されている全テーブルが
    /// 対象になる。
    fn execute_analyze(&mut self, analyze: AnalyzeStatement) -> DbResult<QueryResult> {
        let targets: Vec<(TableId, String, Schema)> = match &analyze.table {
            Some(ident) => {
                let info = self.table_info(&ident.name).ok_or_else(|| DbError::TableNotFound(ident.name.clone()))?;
                vec![(info.id, info.name.clone(), info.schema.clone())]
            }
            None => self.all_table_infos().map(|info| (info.id, info.name.clone(), info.schema.clone())).collect(),
        };

        for (table_id, table_name, schema) in &targets {
            let stats = self.collect_table_stats(*table_id, table_name, schema)?;
            match &mut self.backend {
                Backend::Memory { stats: table_stats, .. } => {
                    table_stats.insert(*table_id, stats);
                }
                Backend::Disk { storage } => {
                    storage.set_table_stats(*table_id, stats)?;
                }
            }
        }

        Ok(QueryResult::command_with_count("ANALYZE", targets.len()))
    }

    /// `table_id`を`SeqScan`で全件走査し、[`StatsCollector`]で統計を集める。
    fn collect_table_stats(&self, table_id: TableId, table_name: &str, schema: &Schema) -> DbResult<TableStats> {
        let plan = PhysicalPlan::SeqScan(SeqScanNode {
            table_id,
            table_name: table_name.to_string(),
            schema: schema.clone(),
        });
        let mut executor = self.build_query_executor(&plan, None)?;
        let mut collector = StatsCollector::new(schema);
        while let Some(tuple) = executor.next()? {
            collector.add_row(&tuple);
        }
        Ok(collector.finish())
    }

    /// テーブル名から`TableInfo`相当(`id`・`name`・`schema`)を引く。
    /// `Backend::Memory`・`Backend::Disk`のどちらでも使えるよう、`catalog()`
    /// のようにpanicするのではなく`Option`で返す(第27章、`ANALYZE`が最初の
    /// 利用者)。
    fn table_info(&self, name: &str) -> Option<TableInfo> {
        match &self.backend {
            Backend::Memory { catalog, .. } => catalog.table(name).cloned(),
            Backend::Disk { storage } => storage.table(name).cloned(),
        }
    }

    /// 登録されている全テーブルの`TableInfo`を返す(第27章、`ANALYZE`が
    /// テーブル名を省略した場合に使う)。
    fn all_table_infos(&self) -> Box<dyn Iterator<Item = TableInfo> + '_> {
        match &self.backend {
            Backend::Memory { catalog, .. } => Box::new(catalog.tables().cloned()),
            Backend::Disk { storage } => Box::new(storage.tables().cloned()),
        }
    }
}

/// 複数の実スレッドから同じ`Database`を安全に共有するための最小限のラッパー
/// (第35章)。
///
/// `Database`自身のフィールド(`Catalog`・`Backend`・`LockManager`等)は
/// スレッドセーフになっていない。このラッパーは`Mutex<Database>`1本で
/// `Database`全体を丸ごと直列化し、「複数スレッドから同じ`Database`に
/// 安全に触れる」という最小限の目標だけを満たす。SQL実行エンジンの内部
/// (Catalog・Lock Manager・実行計画の組み立て)そのものを細粒度にロック
/// フリー化し、セッションごとに独立させるのは第37章の仕事であり、この章の
/// 範囲ではない。
///
/// 一方、`Mutex`の外にある**Buffer PoolとB+Treeは、この章で本物のLatchを
/// 持つようになった**([`crate::buffer_pool`]・[`crate::btree`]を参照)ため、
/// `Arc<BTree>`のように`Database`を経由せず直接複数スレッドから共有すれば、
/// ページ単位の細かい並行性をそのまま使える。この2つの粒度(`Database`は
/// トランザクション単位で粗く、Buffer Pool・B+Treeはページ単位で細かい)が
/// 併存している状態が、この章の到達点である。
///
/// # `Blocked`を実スレッドの「待機」に変える
///
/// [`LockManager::acquire`]自体は第31章から変わっていない。`Blocked`だと
/// 判断したら`DbError::WouldBlock`という**値**を返すだけで、呼び出し元の
/// スレッドを止めはしない。決定的インターリーブテストハーネス(第30章)は、
/// この値を受け取って「今は再試行しない」と判断する側に回ることで、
/// 単一スレッドのままインターリーブを制御していた。
///
/// このラッパーは、その`WouldBlock`を受け取ったら`Condvar::wait`で
/// スレッドを実際に眠らせ、他のどこかで`release_all`が呼ばれるたびに
/// 起こして同じ文を再試行する。`LockManager`本体を書き換えず、その外側に
/// 「値を受け取って待機に変える」薄い層を1枚重ねただけであり、決定的
/// ハーネスを使う既存のテスト(第30〜34章)は一切変更していない。この
/// 二層構成(下: 値を返すだけの`LockManager`、上: それを待機に変える
/// このラッパー)を保つことで、同じ`LockManager`を単一スレッドの決定的
/// テストと複数スレッドの実行時の両方で使い回せる。
pub struct SharedDatabase {
    db: std::sync::Mutex<Database>,
    cvar: std::sync::Condvar,
}

impl SharedDatabase {
    /// `db`を包んで、複数スレッドから共有できるようにする。
    pub fn new(db: Database) -> Self {
        SharedDatabase { db: std::sync::Mutex::new(db), cvar: std::sync::Condvar::new() }
    }

    /// 新しいトランザクションを開始する([`Database::begin_tx`]を参照)。
    pub fn begin_tx(&self) -> TxHandle {
        self.lock().begin_tx()
    }

    /// 分離レベルを指定して新しいトランザクションを開始する
    /// ([`Database::begin_tx_with_isolation`]を参照)。
    pub fn begin_tx_with_isolation(&self, isolation_level: IsolationLevel) -> TxHandle {
        self.lock().begin_tx_with_isolation(isolation_level)
    }

    /// [`Database::execute_in_tx`]のブロッキング版。
    ///
    /// `DbError::WouldBlock`を受け取ったら、このスレッドを`Condvar`で
    /// 眠らせ、起こされるたびに同じ`sql`をもう一度試す。他の結果
    /// (`Ok`、`WouldBlock`以外の`Err`)はそのまま呼び出し元へ返す。
    /// `DbError::DeadlockDetected`はここでは特別扱いしない。Victimに
    /// 選ばれたトランザクションは`WouldBlock`を返さずこのエラーを返す
    /// ([`Database::acquire_lock_or_detect_deadlock`]を参照)ため、この
    /// メソッドはループを継続せずそのまま呼び出し元へ伝える。
    ///
    /// 試行のたびに(結果によらず)`Condvar::notify_all`を呼ぶ。この試行が
    /// デッドロック解決のために別のトランザクションを強制Abortしていたら
    /// (`Database::abort_transaction`、`lock_manager.release_all`)、その
    /// Victim自身のスレッドが別に眠っているかもしれない。通知を怠ると、
    /// そのスレッドは自分がAbort済みになったことに気付けないまま永久に
    /// 眠り続ける。過剰な通知(何も変わっていない試行のあとの通知)は
    /// 起こされたスレッドが条件を再確認して再び眠るだけで安全だが、通知の
    /// 欠落は起こすべきスレッドを永久に眠らせたままにする。安全側に倒し、
    /// 毎回無条件に通知する。
    pub fn execute_in_tx(&self, handle: &TxHandle, sql: &str) -> DbResult<QueryResult> {
        let mut guard = self.lock();
        loop {
            let outcome = guard.execute_in_tx(handle, sql);
            self.cvar.notify_all();
            match outcome {
                Err(DbError::WouldBlock) => {
                    guard = self.cvar.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                other => return other,
            }
        }
    }

    /// [`Database::bind_statement`]のブロッキング版(第37章)。`Session`が
    /// `PREPARE`のときに使う。束縛はロックを待つ必要が無い(ロック取得は
    /// 実行の時点で行う)ため、`execute_in_tx`のような再試行ループは持たない。
    pub fn bind_statement(&self, statement: Statement, sql: &str) -> DbResult<BoundStatement> {
        self.lock().bind_statement(statement, sql)
    }

    /// [`Database::execute_in_tx_bound`]のブロッキング版(第37章)。
    /// `bound`を`&BoundStatement`で受け取り、`WouldBlock`で再試行するたびに
    /// `clone`する([`Database::execute_in_tx_bound`]は所有権を取るが、この文は
    /// 一切実行されていないため同じ`bound`をそのまま渡し直せる、`execute_in_tx`
    /// が同じ`sql`をもう一度渡すのと同じ理由)。
    pub fn execute_in_tx_bound(&self, handle: &TxHandle, bound: &BoundStatement) -> DbResult<QueryResult> {
        let mut guard = self.lock();
        loop {
            let outcome = guard.execute_in_tx_bound(handle, bound.clone());
            self.cvar.notify_all();
            match outcome {
                Err(DbError::WouldBlock) => {
                    guard = self.cvar.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                other => return other,
            }
        }
    }

    /// `handle`が指すトランザクションを確定する([`Database::commit_tx`]を
    /// 参照)。ロックを解放するため、成功・失敗によらず`notify_all`する。
    pub fn commit_tx(&self, handle: TxHandle) -> DbResult<()> {
        let mut guard = self.lock();
        let result = guard.commit_tx(handle);
        drop(guard);
        self.cvar.notify_all();
        result
    }

    /// `handle`が指すトランザクションを取り消す([`Database::rollback_tx`]を
    /// 参照)。`commit_tx`と同じ理由で`notify_all`する。
    pub fn rollback_tx(&self, handle: TxHandle) -> DbResult<()> {
        let mut guard = self.lock();
        let result = guard.rollback_tx(handle);
        drop(guard);
        self.cvar.notify_all();
        result
    }

    /// [`Database::flush`]のブロッキング版(第37章)。REPL(`src/main.rs`)が
    /// `Session`経由で`Database`を直接持たなくなったため、終了時の
    /// flushを`SharedDatabase`越しに呼べるようにする。
    pub fn flush(&self) -> DbResult<()> {
        self.lock().flush()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Database> {
        self.db.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl StatsLookup for Database {
    /// `EXPLAIN`/`EXPLAIN ANALYZE`(第27章)が推定行数を計算するための
    /// `table_stats`の実装。`Backend::Memory`はプロセスのメモリ上に持つ
    /// `HashMap`をそのまま引き、`Backend::Disk`は`Storage`がCatalogページから
    /// 復元した統計情報を引く。
    fn table_stats(&self, table_id: TableId) -> Option<&TableStats> {
        match &self.backend {
            Backend::Memory { stats, .. } => stats.get(&table_id),
            Backend::Disk { storage } => storage.table_stats(table_id),
        }
    }
}

/// `text`(`explain_text`が組み立てた複数行のEXPLAIN出力)の先頭行(根の
/// ノード)にだけ` actual=<count>`を追記する(第27章)。
///
/// `INSERT`/`UPDATE`/`DELETE`の`EXPLAIN ANALYZE`(`Database::execute_explain`)が
/// 使う。根のノードは常に1行目に現れる(`explain_text`・`write_tree`の
/// 深さ0の行はインデント無しの1行になる)ため、改行までの部分文字列に
/// 追記するだけでよい。
fn append_actual_to_root_line(text: &str, count: usize) -> String {
    match text.split_once('\n') {
        Some((first_line, rest)) => format!("{first_line} actual={count}\n{rest}"),
        None => format!("{text} actual={count}"),
    }
}

/// `Database::execute`の結果。
///
/// `SELECT`は列構成(`Schema`)と、それに従う行の並びを持つ。`CREATE TABLE`・
/// `DROP TABLE`のようなDDL文と、`INSERT`・`UPDATE`・`DELETE`のようなDML文は
/// 返す行を持たないため、`schema`は空、`rows`も空のベクタになり、代わりに
/// `command_tag`が完了した文の種類を持つ。DDL文は`"CREATE TABLE"`のように
/// 種類の名前だけ、DML文は`"INSERT 2"`のように影響を受けた行数を添えた形式に
/// なる(psqlの`INSERT 0 2`のような追加情報は持たない、この教材の簡略形式)。
/// 行を1件も返さない`SELECT`と区別するためにフィールドを分けている。
#[derive(Debug)]
pub struct QueryResult {
    schema: Schema,
    rows: Vec<Tuple>,
    command_tag: Option<String>,
}

impl QueryResult {
    /// DDL文が完了したことを表す`QueryResult`を作る。
    ///
    /// `pub(crate)`にしているのは、`crate::server`の`Session`が`BEGIN`・
    /// `COMMIT`・`ROLLBACK`をSQLの構文解析を経由せず自前で処理する際に
    /// (`SharedDatabase`の`TxHandle`API越しに実行するため、`execute_begin`
    /// 等のSQL経路を通らない、`crate::server`モジュールドキュメント参照)、
    /// 同じ形のコマンドタグを組み立てる必要があるため。
    pub(crate) fn command(tag: &'static str) -> Self {
        QueryResult {
            schema: Schema::new(Vec::new()),
            rows: Vec::new(),
            command_tag: Some(tag.to_string()),
        }
    }

    /// DML文が完了したことを表す`QueryResult`を作る。`count`は影響を受けた行数。
    fn command_with_count(tag: &'static str, count: usize) -> Self {
        QueryResult {
            schema: Schema::new(Vec::new()),
            rows: Vec::new(),
            command_tag: Some(format!("{tag} {count}")),
        }
    }

    /// `EXPLAIN`が完了したことを表す`QueryResult`を作る。`plan_text`は
    /// `PhysicalPlan`の`Display`実装(木を表示した複数行の文字列)。
    ///
    /// PostgreSQLの`EXPLAIN`にならい、`QUERY PLAN`という1列の結果として返す
    /// (`SELECT`の結果と同じ形で表示できるようにするため、`command_tag`は
    /// 使わない)。木の1行が結果の1行になる。
    fn explain(plan_text: String) -> Self {
        let schema = Schema::new(vec![Column::new("QUERY PLAN", DataType::Text, false)]);
        let rows = plan_text
            .lines()
            .map(|line| {
                Tuple::new(&schema, vec![Value::Text(line.to_string())])
                    .expect("QUERY PLAN列はTEXTなので必ず成功する")
            })
            .collect();
        QueryResult { schema, rows, command_tag: None }
    }

    /// 結果の列構成を返す。DDL・DML文の完了では列を持たない空の`Schema`を返す。
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// 結果の行を返す。DDL・DML文の完了では常に空のスライスを返す。
    pub fn rows(&self) -> &[Tuple] {
        &self.rows
    }
}

impl std::fmt::Display for QueryResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(tag) = &self.command_tag {
            return write!(f, "{tag}");
        }

        let header = self
            .schema
            .columns()
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        writeln!(f, "{header}")?;
        writeln!(f, "{}", "-".repeat(header.chars().count().max(1)))?;

        for tuple in &self.rows {
            let row = tuple
                .values()
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(" | ");
            writeln!(f, "{row}")?;
        }

        let row_word = if self.rows.len() == 1 { "row" } else { "rows" };
        write!(f, "({} {row_word})", self.rows.len())
    }
}

/// `plan`が読みに行くテーブルの`TableId`を、木をたどって集める(第31章、
/// `execute_select`がロックの対象を決めるために使う)。同じテーブルが
/// (自己結合などで)複数回現れても構わない。`LockManager::acquire`は同じ
/// `(txn, key)`への再要求を無害に素通りさせる(`crate::lock_manager`を参照)
/// ため、ここで重複を取り除く必要は無い。
/// `adjacency`(Wait-for Graphの隣接表)の中で、`start`を含む循環を1つ探す
/// (第32章、`Database::detect_deadlock`が使う)。
///
/// `start`からDFSで辺をたどり、`start`へ戻ってくる経路が見つかれば、その
/// 経路(`start`を含む、循環を構成するノードの列)を返す。`start`を経由しない
/// 循環(たとえば`start`から到達できる先で、`start`とは無関係などうしが
/// 待ち合っている場合)は探索しない。探索中に同じノードを2回訪れそうになったら
/// (`start`自身への到達を除く)、そこで探索を打ち切る(無関係な循環に迷い込んで
/// 無限に回り続けないための番人)。`adjacency`の各隣接リストは
/// `detect_deadlock`が`TransactionId`の昇順にソート済みであり、複数の循環が
/// 存在する場合でもこの関数は毎回同じ経路を決定的に返す。
fn find_cycle_containing(
    adjacency: &HashMap<TransactionId, Vec<TransactionId>>,
    start: TransactionId,
) -> Option<Vec<TransactionId>> {
    fn dfs(
        node: TransactionId,
        start: TransactionId,
        adjacency: &HashMap<TransactionId, Vec<TransactionId>>,
        path: &mut Vec<TransactionId>,
        on_path: &mut std::collections::HashSet<TransactionId>,
    ) -> Option<Vec<TransactionId>> {
        let neighbors = adjacency.get(&node)?;
        for &next in neighbors {
            if next == start {
                return Some(path.clone());
            }
            if on_path.contains(&next) {
                continue;
            }
            on_path.insert(next);
            path.push(next);
            if let Some(cycle) = dfs(next, start, adjacency, path, on_path) {
                return Some(cycle);
            }
            path.pop();
            on_path.remove(&next);
        }
        None
    }

    let mut path = vec![start];
    let mut on_path = std::collections::HashSet::from([start]);
    dfs(start, start, adjacency, &mut path, &mut on_path)
}

/// `Aborted`状態のトランザクションへ以後の操作を拒むときのエラーを選ぶ
/// (第32章)。デッドロックのVictimとして強制的に`Aborted`へ倒された場合は
/// `DbError::DeadlockDetected`を、それ以外(Statement Error時のAbort・
/// 明示的な`ROLLBACK`後)は従来どおり`DbError::TransactionAborted`を返す。
fn aborted_error(victim_of_deadlock: bool) -> DbError {
    if victim_of_deadlock { DbError::DeadlockDetected } else { DbError::TransactionAborted }
}

/// Diskバックエンドの`INSERT`・`UPDATE`・`DELETE`を、WALのトランザクション
/// 境界で挟んで実行する(第33章)。
///
/// `tx`が`Some`(`BEGIN`済みの明示的トランザクション)であれば、この関数は
/// `Commit`・`Abort`のどちらも書かない(`COMMIT`・`ROLLBACK`自体のWAL処理は
/// `Database::execute_commit`・`execute_rollback`が別途行う)。`Begin`は、
/// 実際に1件でも書き込みが起きた時点で[`WalCursor`]が遅延して書く
/// (`crate::wal::WalCursor`のドキュメントを参照)。
///
/// `tx`が`None`(Autocommit)であれば、この1文だけのための使い捨て
/// トランザクションIDを`next_txn_id`から採番する。`f`が1件でも書き込んで
/// いれば(`prev_lsn`が`None`のままでなければ)、成功時は`Commit`レコードを
/// 書いてから[`crate::wal::WalWriter::sync`]で同期し、それが終わるまで
/// `run_disk_dml`自体が返らない。これが「`COMMIT`応答前にログを同期する」と
/// いう規律を、明示的な`BEGIN`を伴わない1文にも及ぼす部分である
/// (本文「Autocommitの1文も、それ自体が耐久性を持つ」を参照)。失敗時は
/// `Abort`レコードを書くだけで同期はしない(失敗した文の変更を耐久化する
/// 意味が無いため)。
fn run_disk_dml<F>(
    storage: &mut Storage,
    tx: &mut Option<TransactionContext>,
    next_txn_id: &mut u64,
    f: F,
) -> DbResult<usize>
where
    F: FnOnce(&mut Storage, &mut WalCursor) -> DbResult<usize>,
{
    let wal = storage.wal().clone();
    let autocommit = tx.is_none();
    let txn_id = tx.as_ref().map(|ctx| ctx.id).unwrap_or_else(|| {
        let id = TransactionId(*next_txn_id);
        *next_txn_id += 1;
        id
    });

    let mut local_prev_lsn: Option<Lsn> = None;
    let prev_lsn: &mut Option<Lsn> = match tx.as_mut() {
        Some(ctx) => &mut ctx.wal_last_lsn,
        None => &mut local_prev_lsn,
    };

    let result = {
        let mut cursor = WalCursor::new(&wal, txn_id, prev_lsn);
        f(storage, &mut cursor)
    };

    if autocommit && let Some(last_lsn) = *prev_lsn {
        let mut w = wal.lock().unwrap_or_else(|p| p.into_inner());
        match &result {
            Ok(_) => {
                w.append_commit(txn_id, Some(last_lsn));
                w.sync()?;
            }
            Err(_) => {
                w.append_abort(txn_id, Some(last_lsn));
            }
        }
    }
    result
}

/// `tx.wal_last_lsn`が`Some`(=このトランザクションが1件でもWALへ書いて
/// いた)なら、`Commit`レコードを書いて同期する(第33章、`execute_commit`・
/// `commit_tx`が使う)。`None`(読み取りだけで終わったトランザクション)なら
/// 何もしない。
fn wal_commit_if_disk(backend: &Backend, tx_id: TransactionId, wal_last_lsn: Option<Lsn>) -> DbResult<()> {
    let Backend::Disk { storage } = backend else { return Ok(()) };
    let Some(last_lsn) = wal_last_lsn else { return Ok(()) };
    let mut w = storage.wal().lock().unwrap_or_else(|p| p.into_inner());
    w.append_commit(tx_id, Some(last_lsn));
    w.sync()?;
    Ok(())
}

/// `ROLLBACK`(および、Victim SelectionによるAbort)がDiskバックエンドの
/// WALに対して行う後始末(第33章)。`wal_last_lsn`が指す連鎖を
/// `crate::transaction::apply_wal_undo_disk`で逆順に適用してから、
/// `Abort`レコードを書く(`Some`のときだけ。`None`なら何も書いていないので
/// 取り消す変更も無い)。`Abort`は`COMMIT`と違って同期を待たない
/// (本文「ROLLBACKの同期は待たない」を参照)。
fn wal_rollback_if_disk(storage: &mut Storage, tx_id: TransactionId, wal_last_lsn: Option<Lsn>) -> DbResult<()> {
    transaction::apply_wal_undo_disk(storage, wal_last_lsn)?;
    if let Some(last_lsn) = wal_last_lsn {
        let mut w = storage.wal().lock().unwrap_or_else(|p| p.into_inner());
        w.append_abort(tx_id, Some(last_lsn));
        w.flush()?;
    }
    Ok(())
}

fn collect_scan_tables(plan: &LogicalPlan, tables: &mut Vec<TableId>) {
    match plan {
        LogicalPlan::Scan(scan) => tables.push(scan.table_id),
        LogicalPlan::Values(_) => {}
        LogicalPlan::Filter(filter) => collect_scan_tables(&filter.input, tables),
        LogicalPlan::Join(join) => {
            collect_scan_tables(&join.left, tables);
            collect_scan_tables(&join.right, tables);
        }
        LogicalPlan::Aggregate(aggregate) => collect_scan_tables(&aggregate.input, tables),
        LogicalPlan::Projection(projection) => collect_scan_tables(&projection.input, tables),
        LogicalPlan::Distinct(distinct) => collect_scan_tables(&distinct.input, tables),
        LogicalPlan::Sort(sort) => collect_scan_tables(&sort.input, tables),
        LogicalPlan::Limit(limit) => collect_scan_tables(&limit.input, tables),
        LogicalPlan::Insert(_) | LogicalPlan::Update(_) | LogicalPlan::Delete(_) => {
            unreachable!("logical_plan::build_selectが組み立てる木にInsert/Update/Deleteは現れない")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_model;
    use crate::error::DbError;
    use crate::join_order;
    use crate::parser;

    #[test]
    fn executes_integer_literal() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1;").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(1)]);
        assert_eq!(result.schema().columns()[0].name, "1");
    }

    #[test]
    fn executes_addition() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 + 2;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(3)]);
    }

    #[test]
    fn executes_boolean_literal() {
        let mut db = Database::memory();
        let result = db.execute("SELECT true;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::Boolean(true)]);
    }

    #[test]
    fn executes_chained_addition_left_to_right() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 + 2 + 3;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(6)]);
    }

    #[test]
    fn multiplication_now_evaluates() {
        // `parser`は`1 + 2 * 3`を`1 + (2 * 3)`という正しい木に組み立てる
        // (`parser`のテストで確認済み)。前章まではこの木の乗算部分を評価できず
        // `NotImplemented`になっていたが、`eval`モジュールが揃ったこの章からは
        // 最後まで評価できる。
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 + 2 * 3;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(7)]);
    }

    #[test]
    fn executes_comparison() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 = 1;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::Boolean(true)]);
    }

    #[test]
    fn executes_three_valued_logic() {
        let mut db = Database::memory();
        let result = db.execute("SELECT NULL AND FALSE;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::Boolean(false)]);
    }

    #[test]
    fn executes_cast() {
        let mut db = Database::memory();
        let result = db.execute("SELECT CAST(42 AS TEXT);").unwrap();
        assert_eq!(
            result.rows()[0].values(),
            &[Value::Text("42".to_string())]
        );
    }

    #[test]
    fn select_null_literal_is_a_nullable_text_column() {
        // `Value::Null`はどの`DataType`にも属さないため、結果列の型は
        // プレースホルダーとして`TEXT`を選ぶ(`execute_select`のコメント参照)。
        // 値そのものは`Value::Null`のままである。
        let mut db = Database::memory();
        let result = db.execute("SELECT NULL;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::Null]);
        assert_eq!(result.schema().columns()[0].data_type, DataType::Text);
        assert!(result.schema().columns()[0].nullable);
    }

    #[test]
    fn division_by_zero_is_an_eval_error() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 / 0;");
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    #[test]
    fn calls_builtin_function() {
        let mut db = Database::memory();
        let result = db.execute("SELECT abs(-5);").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(5)]);
    }

    #[test]
    fn column_ref_without_from_is_a_bind_error() {
        // `FROM`が無いので`id`を解決できるテーブルが1つも無い。`Binder`が
        // 未知の列参照として位置情報付きで拒否する。
        let mut db = Database::memory();
        let result = db.execute("SELECT id;");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn null_plus_text_is_rejected_the_same_way_with_and_without_from() {
        // `eval_arith`は両辺の型を検査するより先に`NULL`を伝播させて早期リターン
        // するため、`infer_type`による静的検査を経由しない経路のままだと
        // `SELECT NULL + 'x'`(FROMなし)は`NULL`として黙って成功してしまい、
        // `SELECT NULL + 'x' FROM t`(`executor::project`がすでに`infer_type`で
        // 検査する)は拒否される、というFROMの有無による非対称が生じる。
        // `execute_select_without_from`にも同じ静的検査を通すことで、両方の経路が
        // 同じ文言の`エラー`になることを確認する。
        let without_from_message =
            expect_eval_error_message(&mut Database::memory(), "SELECT NULL + 'x'");

        let mut empty_db = users_db();
        let empty_from_message =
            expect_eval_error_message(&mut empty_db, "SELECT NULL + 'x' FROM users");

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_from_message =
            expect_eval_error_message(&mut populated_db, "SELECT NULL + 'x' FROM users");

        assert_eq!(without_from_message, "算術演算はBIGINT同士にのみ使えます: NULLとTEXT");
        assert_eq!(without_from_message, empty_from_message);
        assert_eq!(without_from_message, populated_from_message);
    }

    #[test]
    fn null_plus_text_is_null_is_rejected_the_same_way_with_and_without_from() {
        // `IS NULL`は被演算子の型を問わないが、被演算子自身(`NULL + 'x'`)は
        // 再帰的に検査されるため、この式全体もFROMの有無に関係なく同じ
        // エラーになる。
        let without_from_message =
            expect_eval_error_message(&mut Database::memory(), "SELECT (NULL + 'x') IS NULL");

        let mut empty_db = users_db();
        let empty_from_message = expect_eval_error_message(
            &mut empty_db,
            "SELECT (NULL + 'x') IS NULL FROM users",
        );

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_from_message = expect_eval_error_message(
            &mut populated_db,
            "SELECT (NULL + 'x') IS NULL FROM users",
        );

        assert_eq!(without_from_message, empty_from_message);
        assert_eq!(without_from_message, populated_from_message);
    }

    #[test]
    fn select_without_from_where_true_returns_the_one_row() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 WHERE TRUE;").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(1)]);
    }

    #[test]
    fn select_without_from_where_false_returns_no_rows() {
        // `FROM`が無い`SELECT`は、列を持たない空の`Schema`に対するちょうど1件の
        // タプルを暗黙の入力とみなす。`WHERE FALSE`はその1件を除外するので、
        // 結果は0行になる(以前は`WHERE`が全く評価されず常に1行返っていた)。
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 WHERE FALSE;").unwrap();
        assert!(result.rows().is_empty());
    }

    #[test]
    fn select_without_from_where_null_returns_no_rows() {
        // `NULL`(UNKNOWN)も`FALSE`と同じく「一致しなかった」側に含まれるので、
        // 0行になる。
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 WHERE NULL;").unwrap();
        assert!(result.rows().is_empty());
    }

    #[test]
    fn select_without_from_where_non_boolean_is_a_bind_error() {
        // `WHERE 1`のような`BOOLEAN`でも`NULL`でもない述語は、`FROM`を伴う
        // `SELECT`と同じく`Binder`が`DbError::Bind`で拒否する(黙って0行や
        // 1行にはしない)。
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 WHERE 1;");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn select_without_from_where_false_skips_evaluating_the_projection() {
        // `WHERE`が`TRUE`にならなかった1件は結果に含まれないため、射影式は
        // 評価しない。`1 / 0`は値に依存するエラーだが、この行がそもそも結果に
        // 含まれないので表面化しない(`executor::project`がフィルタ後の行だけを
        // 評価するのと同じ理由)。
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 / 0 WHERE FALSE;").unwrap();
        assert!(result.rows().is_empty());
    }

    #[test]
    fn null_plus_bigint_column_type_matches_with_and_without_from() {
        // 以前は`FROM`が無い経路の結果列の型を、実際に評価した`Value`の
        // `data_type()`(`NULL`なら`None`)から決めていたため、`NULL + 1`の列の
        // 型が`FROM`が無ければ`TEXT`、`FROM`があれば(`infer_type`が静的に
        // `BigInt`と決めるので)`BIGINT`という食い違いが起きていた。
        // `infer_type`の`Some(DataType)`をそのまま列の型に使うことで一致する。
        let without_from = Database::memory().execute("SELECT NULL + 1;").unwrap();

        let mut empty_db = users_db();
        let empty_from = empty_db.execute("SELECT NULL + 1 FROM users").unwrap();

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_from = populated_db.execute("SELECT NULL + 1 FROM users").unwrap();

        assert_eq!(without_from.schema().columns()[0].data_type, DataType::BigInt);
        assert_eq!(
            without_from.schema().columns()[0].data_type,
            empty_from.schema().columns()[0].data_type
        );
        assert_eq!(
            without_from.schema().columns()[0].data_type,
            populated_from.schema().columns()[0].data_type
        );
    }

    #[test]
    fn abs_of_null_column_type_matches_with_and_without_from() {
        let without_from = Database::memory().execute("SELECT abs(NULL);").unwrap();

        let mut empty_db = users_db();
        let empty_from = empty_db.execute("SELECT abs(NULL) FROM users").unwrap();

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_from = populated_db.execute("SELECT abs(NULL) FROM users").unwrap();

        assert_eq!(without_from.schema().columns()[0].data_type, DataType::BigInt);
        assert_eq!(
            without_from.schema().columns()[0].data_type,
            empty_from.schema().columns()[0].data_type
        );
        assert_eq!(
            without_from.schema().columns()[0].data_type,
            populated_from.schema().columns()[0].data_type
        );
    }

    #[test]
    fn bare_null_column_type_matches_with_and_without_from() {
        // `NULL`単体は`infer_type`が`None`(型が定まらない)を返す唯一のケースで、
        // `FROM`の有無に関係なく`TEXT`のプレースホルダーに揃う。
        let without_from = Database::memory().execute("SELECT NULL;").unwrap();

        let mut empty_db = users_db();
        let empty_from = empty_db.execute("SELECT NULL FROM users").unwrap();

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_from = populated_db.execute("SELECT NULL FROM users").unwrap();

        assert_eq!(without_from.schema().columns()[0].data_type, DataType::Text);
        assert_eq!(
            without_from.schema().columns()[0].data_type,
            empty_from.schema().columns()[0].data_type
        );
        assert_eq!(
            without_from.schema().columns()[0].data_type,
            populated_from.schema().columns()[0].data_type
        );
    }

    #[test]
    fn null_plus_bigint_still_propagates_null_without_from() {
        // `NULL + 1`はどちらも`BigInt`か`None`(型未定の`NULL`)であり、
        // `infer_type`の検査を正しく通過する。型として正しい式に対する実行時の
        // `NULL`伝播(`eval_arith`)は、この静的検査の変更後もそのまま働く。
        let mut db = Database::memory();
        let result = db.execute("SELECT NULL + 1;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::Null]);
    }

    #[test]
    fn executes_multiple_select_items() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1, 2 + 3;").unwrap();
        assert_eq!(
            result.rows()[0].values(),
            &[Value::BigInt(1), Value::BigInt(5)]
        );
        assert_eq!(result.schema().columns()[0].name, "1");
        assert_eq!(result.schema().columns()[1].name, "2 + 3");
    }

    #[test]
    fn propagates_parse_error() {
        let mut db = Database::memory();
        let result = db.execute("this is not sql");
        assert!(matches!(result, Err(DbError::Parse { .. })));
    }

    #[test]
    fn propagates_lex_error_with_position() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 'abc");
        match result {
            Err(DbError::Lex { line, column, .. }) => assert_eq!((line, column), (1, 8)),
            Err(e) => panic!("DbError::Lexを期待したがDbError::Parse等が返った: {e}"),
            Ok(_) => panic!("DbError::Lexを期待したがOkが返った"),
        }
    }

    #[test]
    fn each_execute_call_is_independent() {
        let mut db = Database::memory();
        let first = db.execute("SELECT 1;").unwrap();
        let second = db.execute("SELECT 2;").unwrap();
        assert_eq!(first.rows()[0].values(), &[Value::BigInt(1)]);
        assert_eq!(second.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn display_formats_header_row_and_footer() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1;").unwrap();
        assert_eq!(result.to_string(), "1\n-\n1\n(1 row)");
    }

    #[test]
    fn select_with_from_rejects_unknown_table() {
        // テーブル名の解決は`Binder`が行うため、未知のテーブル名は
        // 位置情報付きの`DbError::Bind`になる。
        let mut db = Database::memory();
        let result = db.execute("SELECT id FROM users");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn insert_rejects_unknown_table() {
        let mut db = Database::memory();
        let result = db.execute("INSERT INTO users VALUES (1)");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    // ---- CREATE TABLE ----

    #[test]
    fn create_table_registers_the_table_in_the_catalog() {
        let mut db = Database::memory();
        let result = db
            .execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)")
            .unwrap();
        assert_eq!(result.to_string(), "CREATE TABLE");

        let info = db.catalog().table("users").unwrap();
        assert_eq!(info.schema.columns().len(), 2);
        assert_eq!(info.schema.columns()[0].name, "id");
        assert_eq!(info.schema.columns()[0].data_type, DataType::BigInt);
        assert!(!info.schema.columns()[0].nullable);
        assert_eq!(info.schema.columns()[1].name, "name");
        assert_eq!(info.schema.columns()[1].data_type, DataType::Text);
        assert!(info.schema.columns()[1].nullable);
    }

    #[test]
    fn create_table_result_has_no_rows() {
        let mut db = Database::memory();
        let result = db
            .execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        assert!(result.rows().is_empty());
        assert!(result.schema().is_empty());
    }

    #[test]
    fn create_table_rejects_duplicate_name() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        let result = db.execute("CREATE TABLE users (id BIGINT NOT NULL)");
        assert!(matches!(result, Err(DbError::DuplicateTable(name)) if name == "users"));
    }

    #[test]
    fn create_table_rejects_unknown_type_name() {
        let mut db = Database::memory();
        let result = db.execute("CREATE TABLE users (id FLOAT)");
        assert!(matches!(result, Err(DbError::Eval(_))));
        // 型名の解決に失敗した時点でカタログには何も登録されない。
        assert!(db.catalog().table("users").is_none());
    }

    #[test]
    fn create_table_rejects_duplicate_column_name() {
        let mut db = Database::memory();
        let result = db.execute("CREATE TABLE dup (id BIGINT, id TEXT)");
        assert!(matches!(result, Err(DbError::DuplicateColumn(name)) if name == "id"));
        // 列名の重複を検出した時点でカタログには何も登録されない。
        assert!(db.catalog().table("dup").is_none());
    }

    #[test]
    fn create_table_column_names_are_case_sensitive() {
        // `id`と`ID`は別列として許す。`Catalog`のテーブル名比較(第9章)と
        // 揃えた方針。
        let mut db = Database::memory();
        let result = db.execute("CREATE TABLE t (id BIGINT, ID TEXT)");
        assert!(result.is_ok());
    }

    // ---- DROP TABLE ----

    #[test]
    fn drop_table_removes_a_registered_table() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        let result = db.execute("DROP TABLE users").unwrap();
        assert_eq!(result.to_string(), "DROP TABLE");
        assert!(db.catalog().table("users").is_none());
    }

    #[test]
    fn drop_table_rejects_unknown_table() {
        // `Binder`の`bind_drop_table`が、実行(`Catalog::drop_table`)より先に
        // 位置情報付きで存在を確認する。
        let mut db = Database::memory();
        let result = db.execute("DROP TABLE users");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn create_drop_create_cycle_succeeds() {
        // 削除したテーブル名は再利用できる: 削除→同名で再作成が通ることを確認する。
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        db.execute("DROP TABLE users").unwrap();
        let result = db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)");
        assert!(result.is_ok());
        assert_eq!(db.catalog().table("users").unwrap().schema.columns().len(), 2);
    }

    // ---- INSERT ----

    fn users_db() -> Database {
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)")
            .unwrap();
        db
    }

    /// `sql`を実行し、`DbError::Eval`または`DbError::Bind`のメッセージ文字列を
    /// 取り出す。値に依存する検査(ゼロ除算など)は実行時に`DbError::Eval`のまま
    /// だが、式の型検査は第17章から`Binder`が`DbError::Bind`として検出する。
    /// どちらの経路でも文言そのものは変わらないことを確認したいテスト
    /// (`..._is_rejected_with_the_same_error_on_empty_and_non_empty_tables`)が
    /// 共通して使うため、この関数はどちらのバリアントからもメッセージだけを
    /// 取り出す。
    fn expect_eval_error_message(db: &mut Database, sql: &str) -> String {
        match db.execute(sql) {
            Err(DbError::Eval(message)) | Err(DbError::Bind { message, .. }) => message,
            Ok(_) => panic!("{sql:?}は失敗するはずだったが成功した"),
            Err(other) => panic!("DbError::EvalまたはDbError::Bindを期待したが{other}が返った"),
        }
    }

    #[test]
    fn insert_adds_a_row() {
        let mut db = users_db();
        let result = db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        assert_eq!(result.to_string(), "INSERT 1");

        let selected = db.execute("SELECT * FROM users").unwrap();
        assert_eq!(selected.rows().len(), 1);
        assert_eq!(
            selected.rows()[0].values(),
            &[Value::BigInt(1), Value::Text("Alice".to_string())]
        );
    }

    #[test]
    fn insert_accepts_multiple_rows_in_one_statement() {
        let mut db = users_db();
        let result = db
            .execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        assert_eq!(result.to_string(), "INSERT 2");
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 2);
    }

    #[test]
    fn insert_with_explicit_columns_fills_omitted_columns_with_null() {
        let mut db = users_db();
        db.execute("INSERT INTO users (id) VALUES (1)").unwrap();

        let selected = db.execute("SELECT * FROM users").unwrap();
        assert_eq!(
            selected.rows()[0].values(),
            &[Value::BigInt(1), Value::Null]
        );
    }

    #[test]
    fn insert_with_explicit_columns_in_any_order() {
        let mut db = users_db();
        db.execute("INSERT INTO users (name, id) VALUES ('Alice', 1)")
            .unwrap();

        let selected = db.execute("SELECT * FROM users").unwrap();
        assert_eq!(
            selected.rows()[0].values(),
            &[Value::BigInt(1), Value::Text("Alice".to_string())]
        );
    }

    #[test]
    fn insert_rejects_not_null_violation() {
        let mut db = users_db();
        let result = db.execute("INSERT INTO users (name) VALUES ('Alice')");
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
        // 検査に失敗した行は1件も挿入されない。
        assert!(db.execute("SELECT * FROM users").unwrap().rows().is_empty());
    }

    #[test]
    fn insert_is_all_or_nothing_across_rows() {
        let mut db = users_db();
        // 1行目は妥当だが、2行目が`id`のNOT NULLに違反する。
        let result = db.execute("INSERT INTO users VALUES (1, 'Alice'), (NULL, 'Bob')");
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
        assert!(db.execute("SELECT * FROM users").unwrap().rows().is_empty());
    }

    // ---- SELECT (FROM/WHERE/*) ----

    #[test]
    fn select_star_returns_all_columns_in_schema_order() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        let result = db.execute("SELECT * FROM users").unwrap();
        assert_eq!(
            result
                .schema()
                .columns()
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "name"]
        );
    }

    #[test]
    fn select_projects_a_subset_of_columns() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        let result = db.execute("SELECT name FROM users").unwrap();
        assert_eq!(result.schema().columns().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::Text("Alice".to_string())]);
    }

    #[test]
    fn select_evaluates_expressions_over_columns() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        let result = db.execute("SELECT id + 1 FROM users").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn select_where_filters_rows() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("SELECT id FROM users WHERE id = 2").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn select_where_drops_unknown_rows() {
        // `name`が`NULL`の行は、`name = 'Alice'`がUNKNOWNになるため落ちる
        // (FALSEになる場合と同じ扱い)。
        let mut db = users_db();
        db.execute("INSERT INTO users (id) VALUES (1)").unwrap();
        db.execute("INSERT INTO users VALUES (2, 'Alice')").unwrap();
        let result = db
            .execute("SELECT id FROM users WHERE name = 'Alice'")
            .unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn select_from_empty_table_returns_no_rows() {
        let mut db = users_db();
        let result = db.execute("SELECT * FROM users").unwrap();
        assert!(result.rows().is_empty());
        // 行が1件も無くても、列参照の出力Schemaは`table_schema`から正確に決まる。
        assert_eq!(result.schema().columns()[0].data_type, DataType::BigInt);
    }

    #[test]
    fn select_where_1_is_rejected_even_on_an_empty_table() {
        // `users`が空だと`filter`の行ループが1度も回らないため、行を評価して
        // 初めて気づく検査だけでは`WHERE 1`のような書き誤りを見逃してしまう。
        // `Binder`の`bind_predicate`による束縛時の静的検査がその穴を塞ぐ。
        let mut db = users_db();
        let result = db.execute("SELECT id FROM users WHERE 1");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn select_where_null_succeeds_with_no_rows() {
        // `WHERE NULL`は型エラーではない。`NULL`はUNKNOWNであり、`TRUE`にならない
        // という理由で正しく「0行」に絞り込まれるべきで、`WHERE 1`のような
        // 型違反とは区別しなければならない。`infer_type`が`NULL`リテラルに対して
        // `None`(型が定まらない)を返し、`check_predicate_type`が`None`を
        // `Some(Boolean)`と同じく許可するのはこのため。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("SELECT id FROM users WHERE NULL").unwrap();
        assert!(result.rows().is_empty());
    }

    #[test]
    fn select_where_1_and_2_is_rejected_with_the_same_error_on_empty_and_non_empty_tables() {
        // `1 AND 2`はトップレベルの演算子(`AND`)だけを見ると`Boolean`を返す形を
        // しているため、`AND`の被演算子の型まで再帰的に検査しないと、空テーブルでは
        // 素通りしてしまう(非空テーブルでは`eval_expr`が行ごとに`1`をBOOLEANとして
        // 扱えず実行時エラーになる、という非対称が生じる)。`infer_type`が`AND`の
        // 両辺を再帰的に検査するようになったことで、空・非空どちらでも同じ文言の
        // エラーになることを確認する。
        let mut empty_db = users_db();
        let empty_message = expect_eval_error_message(&mut empty_db, "SELECT id FROM users WHERE 1 AND 2");

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_message =
            expect_eval_error_message(&mut populated_db, "SELECT id FROM users WHERE 1 AND 2");

        assert_eq!(empty_message, populated_message);
    }

    #[test]
    fn select_where_not_1_is_rejected_with_the_same_error_on_empty_and_non_empty_tables() {
        let mut empty_db = users_db();
        let empty_message = expect_eval_error_message(&mut empty_db, "SELECT id FROM users WHERE NOT 1");

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_message =
            expect_eval_error_message(&mut populated_db, "SELECT id FROM users WHERE NOT 1");

        assert_eq!(empty_message, populated_message);
    }

    #[test]
    fn select_where_abs_of_text_equals_1_is_rejected_with_the_same_error_on_empty_and_non_empty_tables()
     {
        // `abs('x') = 1`は、比較演算子(`=`)自身は正しい形をしていても、`abs`の
        // 引数の型が誤っている。関数の引数型検査(`FunctionRegistry::arg_types`)を
        // `infer_type`から呼ぶことで、この誤りも空・非空どちらのテーブルでも
        // 同じ文言のエラーとして検出できることを確認する。
        let mut empty_db = users_db();
        let empty_message =
            expect_eval_error_message(&mut empty_db, "SELECT id FROM users WHERE abs('x') = 1");

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_message = expect_eval_error_message(
            &mut populated_db,
            "SELECT id FROM users WHERE abs('x') = 1",
        );

        assert_eq!(empty_message, populated_message);
    }

    // ---- UPDATE ----

    #[test]
    fn update_changes_matching_rows() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db
            .execute("UPDATE users SET name = 'Carol' WHERE id = 1")
            .unwrap();
        assert_eq!(result.to_string(), "UPDATE 1");

        let selected = db.execute("SELECT id, name FROM users WHERE id = 1").unwrap();
        assert_eq!(
            selected.rows()[0].values(),
            &[Value::BigInt(1), Value::Text("Carol".to_string())]
        );
        let unaffected = db.execute("SELECT name FROM users WHERE id = 2").unwrap();
        assert_eq!(
            unaffected.rows()[0].values(),
            &[Value::Text("Bob".to_string())]
        );
    }

    #[test]
    fn update_without_where_changes_every_row() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("UPDATE users SET name = 'Same'").unwrap();
        assert_eq!(result.to_string(), "UPDATE 2");
    }

    #[test]
    fn update_set_right_hand_side_sees_the_pre_update_row() {
        // `SET id = id + 1, name = name`のような複数代入で、後続の代入が
        // 直前の代入結果を見ないことを確認する(更新前の行を使って評価する)。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        db.execute("UPDATE users SET id = id + 1").unwrap();
        let result = db.execute("SELECT id FROM users").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn update_rejects_not_null_violation() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        let result = db.execute("UPDATE users SET id = NULL");
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
        // 検査に失敗したら、対象行は一切書き換わらない。
        let selected = db.execute("SELECT id FROM users").unwrap();
        assert_eq!(selected.rows()[0].values(), &[Value::BigInt(1)]);
    }

    #[test]
    fn update_where_1_is_rejected_even_on_an_empty_table() {
        // `select_where_1_is_rejected_even_on_an_empty_table`と同じ理由で、
        // `users`が空でも`UPDATE ... WHERE 1`は束縛時の静的検査で拒否される。
        let mut db = users_db();
        let result = db.execute("UPDATE users SET name = 'x' WHERE 1");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn update_where_null_succeeds_with_no_rows_updated() {
        // `select_where_null_succeeds_with_no_rows`と同じ理由で、`WHERE NULL`は
        // 型エラーではなく、単に0行にマッチする。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("UPDATE users SET name = 'x' WHERE NULL").unwrap();
        assert_eq!(result.to_string(), "UPDATE 0");
    }

    // ---- PRIMARY KEY / UNIQUE(第20章) ----

    fn users_with_constraints_db() -> Database {
        let mut db = Database::memory();
        db.execute(
            "CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        )
        .unwrap();
        db
    }

    #[test]
    fn create_table_rejects_multiple_primary_keys() {
        let mut db = Database::memory();
        let result = db.execute("CREATE TABLE t (a BIGINT PRIMARY KEY, b BIGINT PRIMARY KEY)");
        assert!(matches!(result, Err(DbError::MultiplePrimaryKeys)));
        assert!(db.catalog().table("t").is_none());
    }

    #[test]
    fn primary_key_column_is_not_nullable_even_without_not_null() {
        let mut db = users_with_constraints_db();
        let result = db.execute("INSERT INTO users (email) VALUES ('a@example.com')");
        // `id`列挙げは省略していないが値がNULLになる: `PRIMARY KEY`は
        // `NOT NULL`を含意するため、明示的な`NOT NULL`が無くても拒否される。
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
    }

    #[test]
    fn insert_rejects_primary_key_duplicate_against_existing_row() {
        let mut db = users_with_constraints_db();
        db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')")
            .unwrap();
        let result = db.execute("INSERT INTO users VALUES (1, 'b@example.com', 'Bob')");
        assert!(matches!(result, Err(DbError::PrimaryKeyViolation { column, .. }) if column == "id"));
        // 違反した行は挿入されない。
        assert_eq!(db.execute("SELECT id FROM users").unwrap().rows().len(), 1);
    }

    #[test]
    fn insert_rejects_unique_duplicate_against_existing_row() {
        let mut db = users_with_constraints_db();
        db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')")
            .unwrap();
        let result = db.execute("INSERT INTO users VALUES (2, 'a@example.com', 'Bob')");
        assert!(matches!(result, Err(DbError::UniqueViolation { column, .. }) if column == "email"));
    }

    #[test]
    fn insert_rejects_duplicate_within_the_same_statement() {
        // 同じINSERT文の中の2行同士でも一意性を検査する。
        let mut db = users_with_constraints_db();
        let result = db.execute(
            "INSERT INTO users VALUES (1, 'a@example.com', 'Alice'), (2, 'a@example.com', 'Bob')",
        );
        assert!(matches!(result, Err(DbError::UniqueViolation { column, .. }) if column == "email"));
        // Statement Rollback: 文全体が無効になり、1件も挿入されない。
        assert!(db.execute("SELECT id FROM users").unwrap().rows().is_empty());
    }

    #[test]
    fn insert_allows_multiple_null_unique_values() {
        // UNIQUE列のNULL同士は重複とみなさない(SQL標準の扱い)。
        let mut db = users_with_constraints_db();
        db.execute("INSERT INTO users VALUES (1, NULL, 'Alice')").unwrap();
        let result = db.execute("INSERT INTO users VALUES (2, NULL, 'Bob')");
        assert!(result.is_ok());
        assert_eq!(db.execute("SELECT id FROM users").unwrap().rows().len(), 2);
    }

    #[test]
    fn update_rejects_primary_key_duplicate() {
        let mut db = users_with_constraints_db();
        db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice'), (2, 'b@example.com', 'Bob')")
            .unwrap();
        let result = db.execute("UPDATE users SET id = 1 WHERE id = 2");
        assert!(matches!(result, Err(DbError::PrimaryKeyViolation { column, .. }) if column == "id"));
        // 検査に失敗した行は書き換わらない。
        let bob = db.execute("SELECT id FROM users WHERE name = 'Bob'").unwrap();
        assert_eq!(bob.rows()[0].values()[0], Value::BigInt(2));
    }

    #[test]
    fn update_does_not_conflict_with_its_own_previous_value() {
        // 自分自身の更新前の値との比較で誤って衝突を報告しない
        // (`UPDATE ... SET id = id`は常に成功するべき)。
        let mut db = users_with_constraints_db();
        db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')")
            .unwrap();
        let result = db.execute("UPDATE users SET id = 1 WHERE id = 1");
        assert!(result.is_ok());
    }

    #[test]
    fn update_statement_rollback_leaves_earlier_rows_untouched_on_later_violation() {
        // 複数行を書き換えるUPDATEの後半の行が制約に違反した場合、前半の行の
        // 変更も一切残らないことを確認する(Statement Rollback)。
        let mut db = users_with_constraints_db();
        db.execute(
            "INSERT INTO users VALUES (1, 'a@example.com', 'Alice'), (2, 'b@example.com', 'Bob'), (3, 'c@example.com', 'Carol')",
        )
        .unwrap();
        // id=1とid=2をどちらも'a@example.com'へ書き換えようとする更新。
        // 2行目を処理した時点で1行目との重複が判明し、文全体が失敗する。
        let result = db.execute("UPDATE users SET email = 'a@example.com' WHERE id <= 2");
        assert!(matches!(result, Err(DbError::UniqueViolation { .. })));

        let alice = db.execute("SELECT email FROM users WHERE id = 1").unwrap();
        assert_eq!(alice.rows()[0].values(), &[Value::Text("a@example.com".to_string())]);
        let bob = db.execute("SELECT email FROM users WHERE id = 2").unwrap();
        assert_eq!(bob.rows()[0].values(), &[Value::Text("b@example.com".to_string())]);
    }

    // ---- DELETE ----

    #[test]
    fn delete_removes_matching_rows() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("DELETE FROM users WHERE id = 1").unwrap();
        assert_eq!(result.to_string(), "DELETE 1");
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 1);
    }

    #[test]
    fn delete_without_where_removes_every_row() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("DELETE FROM users").unwrap();
        assert_eq!(result.to_string(), "DELETE 2");
        assert!(db.execute("SELECT * FROM users").unwrap().rows().is_empty());
    }

    #[test]
    fn delete_where_1_is_rejected_even_on_an_empty_table() {
        // `select_where_1_is_rejected_even_on_an_empty_table`と同じ理由で、
        // `users`が空でも`DELETE ... WHERE 1`は束縛時の静的検査で拒否される。
        let mut db = users_db();
        let result = db.execute("DELETE FROM users WHERE 1");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn delete_where_null_succeeds_with_no_rows_deleted() {
        // `select_where_null_succeeds_with_no_rows`と同じ理由で、`WHERE NULL`は
        // 型エラーではなく、単に0行にマッチする。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("DELETE FROM users WHERE NULL").unwrap();
        assert_eq!(result.to_string(), "DELETE 0");
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 2);
    }

    #[test]
    fn insert_select_update_delete_round_trip() {
        // 第1部の到達点を1つのテストとして確認する: INSERT→SELECT→UPDATE→
        // SELECT→DELETE→SELECTが、すべてこの章のコードだけで動く。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 2);

        db.execute("UPDATE users SET name = 'Alicia' WHERE id = 1")
            .unwrap();
        let updated = db.execute("SELECT name FROM users WHERE id = 1").unwrap();
        assert_eq!(
            updated.rows()[0].values(),
            &[Value::Text("Alicia".to_string())]
        );

        db.execute("DELETE FROM users WHERE id = 2").unwrap();
        let remaining = db.execute("SELECT id FROM users").unwrap();
        assert_eq!(remaining.rows().len(), 1);
        assert_eq!(remaining.rows()[0].values(), &[Value::BigInt(1)]);
    }

    // ---- EXPLAIN ----

    /// `EXPLAIN`の結果(`QUERY PLAN`列)を、木を表示した文字列と同じ形の
    /// 複数行の`Vec<String>`として取り出す。
    fn explain_lines(db: &mut Database, sql: &str) -> Vec<String> {
        let result = db.execute(sql).unwrap();
        assert_eq!(result.schema().columns().len(), 1);
        assert_eq!(result.schema().columns()[0].name, "QUERY PLAN");
        result
            .rows()
            .iter()
            .map(|tuple| match &tuple.values()[0] {
                Value::Text(s) => s.clone(),
                other => panic!("QUERY PLAN列はTEXTのはずが{other:?}"),
            })
            .collect()
    }

    #[test]
    fn explain_select_shows_projection_over_filter_over_seq_scan() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN SELECT name FROM users WHERE id = 42");
        assert_eq!(
            lines,
            vec!["Projection(name) rows=5 cost=40.05", "  └─ Filter(id = 42) rows=5 cost=40.00", "    └─ SeqScan(users) rows=1000 cost=30.00"]
        );
    }

    #[test]
    fn explain_select_without_where_has_no_filter_node() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN SELECT id FROM users");
        assert_eq!(lines, vec!["Projection(id) rows=1000 cost=40.00", "  └─ SeqScan(users) rows=1000 cost=30.00"]);
    }

    #[test]
    fn explain_insert_shows_insert_over_values() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN INSERT INTO users VALUES (1, 'Alice')");
        assert_eq!(lines, vec!["Insert(users) rows=1 cost=0.00", "  └─ Values(1 row) rows=1 cost=0.00"]);
    }

    #[test]
    fn explain_update_shows_update_over_seq_scan() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN UPDATE users SET name = 'x' WHERE id = 1");
        assert_eq!(lines, vec!["Update(users) rows=1000 cost=30.00", "  └─ SeqScan(users) rows=1000 cost=30.00"]);
    }

    #[test]
    fn explain_delete_shows_delete_over_seq_scan() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN DELETE FROM users WHERE id = 1");
        assert_eq!(lines, vec!["Delete(users) rows=1000 cost=30.00", "  └─ SeqScan(users) rows=1000 cost=30.00"]);
    }

    #[test]
    fn explain_does_not_execute_the_statement() {
        // `EXPLAIN INSERT`は行を書き込まない。木を見せるだけで実行はしない。
        let mut db = users_db();
        db.execute("EXPLAIN INSERT INTO users VALUES (1, 'Alice')").unwrap();
        assert!(db.execute("SELECT * FROM users").unwrap().rows().is_empty());
    }

    #[test]
    fn explain_rejects_create_table_as_a_syntax_error() {
        // `EXPLAIN`の対象は`SELECT`・`INSERT INTO`・`UPDATE`・`DELETE FROM`の
        // 4種類に限る(Logical Plan/Physical Planを経由しない`CREATE TABLE`は
        // 対象に含まれない)。この制約はParserの文法として表現されているため、
        // 構文エラー(`DbError::Parse`)になる。
        let mut db = users_db();
        let result = db.execute("EXPLAIN CREATE TABLE t (id BIGINT)");
        assert!(matches!(result, Err(DbError::Parse { .. })));
    }

    #[test]
    fn explain_rejects_nested_explain_as_a_syntax_error() {
        let mut db = users_db();
        let result = db.execute("EXPLAIN EXPLAIN SELECT id FROM users");
        assert!(matches!(result, Err(DbError::Parse { .. })));
    }

    // ---- Volcano実行: 中間結果を全件バッファしない ----

    #[test]
    fn select_streams_rows_without_materializing_the_whole_table_at_once() {
        // 10,000行のテーブルに対し、最後の1行だけに一致する`WHERE`を実行する。
        // 第18章までの`eval_query_plan`なら、`Filter`が返す前の中間結果として
        // 10,000行分の`Tuple`を1つの`Vec`にまとめて保持していた。この章の
        // `next()`ループでは、`Filter`・`Projection`のどちらも子から1行ずつ
        // 引いて1行ずつ返すため、最終結果(1行)より大きな`Vec`はどの段階にも
        // 生まれない(この性質そのものは`physical_plan`モジュールの
        // `CountingExecutor`を使ったテストで、子が実際に何回`next()`されたかを
        // 数えて確認している。ここではその上で、10,000行規模でも結果が正しい
        // ことをend-to-endに確認する)。
        const N: i64 = 10_000;
        let mut db = users_db();
        let values: Vec<String> = (1..=N).map(|i| format!("({i}, 'user{i}')")).collect();
        db.execute(&format!("INSERT INTO users VALUES {}", values.join(", "))).unwrap();

        let result = db.execute(&format!("SELECT name FROM users WHERE id = {N}")).unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::Text(format!("user{N}"))]);
    }

    #[test]
    fn select_streams_rows_on_the_disk_backend_too() {
        // 永続モードは、テーブルが使うページ番号の一覧をCatalogページ1枚に
        // 収める設計(第15章)であるため、行数を無制限には増やせない
        // (`storage::tests::catalog_too_large_is_rejected_instead_of_corrupting_the_file`
        // 参照)。ここではストリーミング実行の確認が目的であり、ページを
        // またぐ規模(第14章の`BufferPool`が全ページを同時にキャッシュしきれない
        // 規模)であれば十分なので、`Database::open`が扱える範囲に収まる件数にする。
        const N: i64 = 500;
        let path = temp_db_path("streaming-disk-backend");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)").unwrap();
        let values: Vec<String> = (1..=N).map(|i| format!("({i}, 'user{i}')")).collect();
        db.execute(&format!("INSERT INTO users VALUES {}", values.join(", "))).unwrap();

        let result = db.execute(&format!("SELECT name FROM users WHERE id = {N}")).unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::Text(format!("user{N}"))]);

        std::fs::remove_file(&path).unwrap();
    }

    // ---- Database::open(永続モード) ----
    //
    // 再起動をまたぐ復元そのもの(プロセスの再起動を模して`Database`を作り
    // 直す一連の流れ)は`tests/persistence.rs`の統合テストで確認する。ここでは
    // `Backend::Disk`の配線(`execute_*`が`Storage`を正しく呼び分けること)を、
    // 1つの`Database`を使い回す範囲で確認する。

    fn temp_db_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-database-test-{name}-{}-{:?}",
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
    fn open_on_a_missing_path_creates_a_new_database() {
        let path = temp_db_path("open-missing");
        assert!(!path.exists());

        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        assert_eq!(
            db.execute("INSERT INTO users VALUES (1)").unwrap().to_string(),
            "INSERT 1"
        );

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn disk_backend_supports_the_same_dml_as_the_memory_backend() {
        // insert_select_update_delete_round_tripと同じ流れを、Backend::Diskで
        // 確認する。executor::insert/update/deleteとexecutor::storage_insert/
        // storage_update/storage_deleteが同じ結果になることの確認でもある。
        let path = temp_db_path("dml-round-trip");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 2);

        db.execute("UPDATE users SET name = 'Alicia' WHERE id = 1")
            .unwrap();
        let updated = db.execute("SELECT name FROM users WHERE id = 1").unwrap();
        assert_eq!(
            updated.rows()[0].values(),
            &[Value::Text("Alicia".to_string())]
        );

        db.execute("DELETE FROM users WHERE id = 2").unwrap();
        let remaining = db.execute("SELECT id FROM users").unwrap();
        assert_eq!(remaining.rows().len(), 1);
        assert_eq!(remaining.rows()[0].values(), &[Value::BigInt(1)]);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn flush_on_a_memory_database_is_a_no_op() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        assert!(db.flush().is_ok());
    }

    #[test]
    #[should_panic(expected = "catalog()はDatabase::memory()")]
    fn catalog_panics_on_a_disk_backed_database() {
        let path = temp_db_path("catalog-panics");
        let db = Database::open(&path).unwrap();
        let _ = db.catalog();
        // panicするのでここには到達しないが、後始末のため一応残しておく。
        std::fs::remove_file(&path).unwrap();
    }

    // ---- 第21章: ORDER BY / LIMIT / OFFSET / DISTINCT / GROUP BY / HAVING / 集約 ----

    fn orders_db() -> Database {
        let mut db = Database::memory();
        db.execute("CREATE TABLE orders (dept TEXT, amount BIGINT)").unwrap();
        db.execute(
            "INSERT INTO orders VALUES \
             ('eng', 100), ('eng', 200), ('sales', 50), ('sales', NULL), ('hr', NULL)",
        )
        .unwrap();
        db
    }

    fn rows_as_strings(result: &QueryResult) -> Vec<String> {
        result
            .rows()
            .iter()
            .map(|tuple| tuple.values().iter().map(Value::to_string).collect::<Vec<_>>().join(","))
            .collect()
    }

    #[test]
    fn order_by_sorts_ascending_by_default_with_nulls_first() {
        let mut db = orders_db();
        let result = db.execute("SELECT amount FROM orders ORDER BY amount").unwrap();
        assert_eq!(rows_as_strings(&result), vec!["NULL", "NULL", "50", "100", "200"]);
    }

    #[test]
    fn order_by_desc_puts_nulls_last() {
        let mut db = orders_db();
        let result = db.execute("SELECT amount FROM orders ORDER BY amount DESC").unwrap();
        assert_eq!(rows_as_strings(&result), vec!["200", "100", "50", "NULL", "NULL"]);
    }

    #[test]
    fn order_by_multiple_keys_breaks_ties_with_the_second_key() {
        let mut db = orders_db();
        let result = db.execute("SELECT dept, amount FROM orders ORDER BY dept, amount DESC").unwrap();
        assert_eq!(
            rows_as_strings(&result),
            vec!["eng,200", "eng,100", "hr,NULL", "sales,50", "sales,NULL"]
        );
    }

    #[test]
    fn limit_returns_only_the_first_n_rows_after_ordering() {
        let mut db = orders_db();
        let result = db.execute("SELECT amount FROM orders ORDER BY amount LIMIT 2").unwrap();
        assert_eq!(rows_as_strings(&result), vec!["NULL", "NULL"]);
    }

    #[test]
    fn limit_with_offset_skips_the_first_rows() {
        let mut db = orders_db();
        let result = db.execute("SELECT amount FROM orders ORDER BY amount LIMIT 2 OFFSET 2").unwrap();
        assert_eq!(rows_as_strings(&result), vec!["50", "100"]);
    }

    #[test]
    fn offset_without_limit_returns_the_remaining_rows() {
        let mut db = orders_db();
        let result = db.execute("SELECT amount FROM orders ORDER BY amount OFFSET 3").unwrap();
        assert_eq!(rows_as_strings(&result), vec!["100", "200"]);
    }

    #[test]
    fn distinct_removes_duplicate_rows_and_treats_null_as_equal_to_null() {
        let mut db = orders_db();
        let result = db.execute("SELECT DISTINCT amount FROM orders ORDER BY amount").unwrap();
        assert_eq!(rows_as_strings(&result), vec!["NULL", "50", "100", "200"]);
    }

    #[test]
    fn group_by_aggregates_each_group_independently() {
        let mut db = orders_db();
        let result = db
            .execute("SELECT dept, COUNT(*), SUM(amount), MIN(amount), MAX(amount) FROM orders GROUP BY dept ORDER BY dept")
            .unwrap();
        assert_eq!(
            rows_as_strings(&result),
            vec!["eng,2,300,100,200", "hr,1,NULL,NULL,NULL", "sales,2,50,50,50"]
        );
    }

    #[test]
    fn count_star_on_an_empty_table_still_returns_one_row_with_zero() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE t (x BIGINT)").unwrap();
        let result = db.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(rows_as_strings(&result), vec!["0"]);
    }

    #[test]
    fn group_by_on_an_empty_table_returns_no_groups() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE t (x TEXT)").unwrap();
        let result = db.execute("SELECT x, COUNT(*) FROM t GROUP BY x").unwrap();
        assert!(result.rows().is_empty());
    }

    #[test]
    fn having_filters_groups_after_aggregation() {
        let mut db = orders_db();
        let result =
            db.execute("SELECT dept, COUNT(*) FROM orders GROUP BY dept HAVING COUNT(*) > 1 ORDER BY dept").unwrap();
        assert_eq!(rows_as_strings(&result), vec!["eng,2", "sales,2"]);
    }

    #[test]
    fn group_by_having_order_by_and_limit_compose_together() {
        let mut db = orders_db();
        let result = db
            .execute(
                "SELECT dept, SUM(amount) FROM orders GROUP BY dept HAVING SUM(amount) IS NOT NULL \
                 ORDER BY dept DESC LIMIT 1",
            )
            .unwrap();
        // `HAVING`が`hr`(SUM=NULL)を落とし、残る2グループ(eng・sales)のうち、
        // `ORDER BY dept DESC LIMIT 1`が辞書順で後ろの`sales`だけを残す。
        assert_eq!(rows_as_strings(&result), vec!["sales,50"]);
    }

    #[test]
    fn where_filters_rows_before_grouping() {
        let mut db = orders_db();
        let result = db
            .execute("SELECT dept, COUNT(*) FROM orders WHERE amount IS NOT NULL GROUP BY dept ORDER BY dept")
            .unwrap();
        // `amount IS NOT NULL`が`hr`の1行(amount=NULL)を落としてから集約するため、
        // `hr`はグループごと消える。
        assert_eq!(rows_as_strings(&result), vec!["eng,2", "sales,1"]);
    }

    #[test]
    fn where_cannot_reference_an_aggregate() {
        let mut db = orders_db();
        let result = db.execute("SELECT dept FROM orders WHERE COUNT(*) > 1");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn explain_shows_every_new_operator_in_composition_order() {
        let mut db = orders_db();
        let result = db
            .execute(
                "EXPLAIN SELECT dept, COUNT(*) FROM orders WHERE amount IS NOT NULL \
                 GROUP BY dept HAVING COUNT(*) > 1 ORDER BY dept LIMIT 5",
            )
            .unwrap();
        // 統計未収集(ANALYZE未実行)の`amount IS NOT NULL`は、第4部レビュー
        // 対応前は`IS NOT NULL`を専用に推定せず、既定の不等号選択率
        // (1/3)へフォールバックしていた。今は`IS NOT NULL`を明示的に見積もり、
        // 統計が無いときは「NULLは稀だろう」という既定の等値選択率
        // (`DEFAULT_EQ_SEL`=0.005)をNULL率の代わりに使う(`estimator`モジュールの
        // 説明を参照)ため、rows=995(1000×(1-0.005))になる。
        assert_eq!(
            result.to_string(),
            "QUERY PLAN\n----------\n\
             Limit(limit=5) rows=5 cost=91.03\n  \
             └─ Sort(dept ASC) rows=332 cost=91.03\n    \
             └─ Projection(dept, COUNT(*)) rows=332 cost=63.22\n      \
             └─ Filter(COUNT(*) > 1) rows=332 cost=59.90\n        \
             └─ Aggregate(group_by=[dept], calls=[COUNT(*)]) rows=995 cost=49.95\n          \
             └─ Filter(amount IS NOT NULL) rows=995 cost=40.00\n            \
             └─ SeqScan(orders) rows=1000 cost=30.00\n\
             (7 rows)"
        );
    }

    #[test]
    fn select_distinct_composes_after_projection_and_before_order_by() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE t (a BIGINT, b BIGINT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10), (1, 20), (2, 30)").unwrap();
        // 射影が`a`だけを残すため、元は別々の行だった(1,10)と(1,20)が
        // DISTINCTの時点では同じ行(a=1)になり、1行にまとまる。
        let result = db.execute("SELECT DISTINCT a FROM t ORDER BY a").unwrap();
        assert_eq!(rows_as_strings(&result), vec!["1", "2"]);
    }

    // ---- ORDER BYの隠し列(SELECTの対象式に無い式を参照する場合) ----

    #[test]
    fn order_by_can_reference_a_column_not_in_the_select_list() {
        // `SELECT name FROM t ORDER BY id`という最頻出パターン。`id`は`SELECT`の
        // 対象式に無いが、隠し列として並べ替えにだけ使われ、結果には現れない。
        let mut db = Database::memory();
        db.execute("CREATE TABLE t (name TEXT, id BIGINT)").unwrap();
        db.execute("INSERT INTO t VALUES ('b', 2), ('a', 1), ('c', 3)").unwrap();

        let result = db.execute("SELECT name FROM t ORDER BY id").unwrap();
        assert_eq!(result.schema().columns().len(), 1);
        assert_eq!(result.schema().columns()[0].name, "name");
        assert_eq!(rows_as_strings(&result), vec!["a", "b", "c"]);
    }

    #[test]
    fn order_by_can_reference_an_aggregate_not_in_the_select_list() {
        let mut db = orders_db();
        let result = db.execute("SELECT dept FROM orders GROUP BY dept ORDER BY COUNT(*) DESC, dept").unwrap();
        assert_eq!(result.schema().columns().len(), 1);
        // `eng`・`sales`はどちらも2件、`hr`は1件。件数の降順、同数はdept昇順。
        assert_eq!(rows_as_strings(&result), vec!["eng", "sales", "hr"]);
    }

    #[test]
    fn order_by_matching_the_select_list_does_not_duplicate_the_column() {
        // `ORDER BY dept`の`dept`が射影の`dept`と同じ式なら、新しい隠し列を
        // 増やさずその列をそのまま並べ替えに使う(「射影に同名の出力列が
        // あればそれが優先される」という優先順位)。
        let mut db = orders_db();
        let explain = db.execute("EXPLAIN SELECT dept FROM orders ORDER BY dept").unwrap();
        // トリム用の`Projection`が2重に積まれていないことを、`Projection`が
        // 1回しか現れないことで確認する。
        let projection_count = explain.to_string().matches("Projection(").count();
        assert_eq!(projection_count, 1);
    }

    #[test]
    fn order_by_with_distinct_on_a_non_selected_column_is_rejected() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE t (name TEXT, id BIGINT)").unwrap();
        db.execute("INSERT INTO t VALUES ('a', 1)").unwrap();
        let result = db.execute("SELECT DISTINCT name FROM t ORDER BY id");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn explain_shows_the_extended_projection_and_the_trim_projection() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE t (name TEXT, id BIGINT)").unwrap();
        let result = db.execute("EXPLAIN SELECT name FROM t ORDER BY id").unwrap();
        assert_eq!(
            result.to_string(),
            "QUERY PLAN\n----------\n\
             Projection(name) rows=1000 cost=149.66\n  \
             └─ Sort(id ASC) rows=1000 cost=139.66\n    \
             └─ Projection(name, id) rows=1000 cost=40.00\n      \
             └─ SeqScan(t) rows=1000 cost=30.00\n\
             (4 rows)"
        );
    }

    // ---- JOIN(第22章) ----

    fn join_db() -> Database {
        let mut db = Database::memory();
        db.execute("CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)").unwrap();
        db.execute("CREATE TABLE orders (customer_id BIGINT, item TEXT)").unwrap();
        db.execute("INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')").unwrap();
        db.execute(
            "INSERT INTO orders VALUES (1, 'apple'), (1, 'banana'), (2, 'cherry'), (NULL, 'orphan'), (99, 'nomatch')",
        )
        .unwrap();
        db
    }

    #[test]
    fn inner_join_returns_only_matching_rows() {
        let mut db = join_db();
        let result = db
            .execute("SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id")
            .unwrap();
        // Alice(2件) + Bob(1件) = 3行。Carolに一致する注文は無く、
        // customer_idがNULL・99の注文はどの顧客とも一致しない。
        assert_eq!(result.rows().len(), 3);
    }

    #[test]
    fn join_without_on_matching_predicate_uses_nested_loop_join() {
        let mut db = join_db();
        let explain = db
            .execute("EXPLAIN SELECT customers.name FROM customers JOIN orders ON customers.id <> orders.customer_id")
            .unwrap();
        assert!(explain.to_string().contains("NestedLoopJoin"));
    }

    #[test]
    fn equi_join_uses_hash_join() {
        let mut db = join_db();
        let explain = db
            .execute("EXPLAIN SELECT customers.name FROM customers JOIN orders ON customers.id = orders.customer_id")
            .unwrap();
        assert!(explain.to_string().contains("HashJoin"));
    }

    #[test]
    fn ambiguous_unqualified_column_after_join_is_rejected() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE a (id BIGINT NOT NULL)").unwrap();
        db.execute("CREATE TABLE b (id BIGINT NOT NULL)").unwrap();
        let result = db.execute("SELECT id FROM a JOIN b ON a.id = b.id");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn three_way_join_chains_left_to_right() {
        let mut db = join_db();
        db.execute("CREATE TABLE shippers (item TEXT, carrier TEXT)").unwrap();
        db.execute("INSERT INTO shippers VALUES ('apple', 'FastCo'), ('banana', 'SlowCo')").unwrap();
        let result = db
            .execute(
                "SELECT customers.name, shippers.carrier FROM customers \
                 JOIN orders ON customers.id = orders.customer_id \
                 JOIN shippers ON orders.item = shippers.item \
                 ORDER BY customers.name, shippers.carrier",
            )
            .unwrap();
        assert_eq!(result.rows().len(), 2);
        assert_eq!(result.rows()[0].values()[0], Value::Text("Alice".to_string()));
    }

    #[test]
    fn join_can_be_combined_with_where_group_by_and_order_by() {
        let mut db = join_db();
        let result = db
            .execute(
                "SELECT customers.name, COUNT(*) FROM customers JOIN orders ON customers.id = orders.customer_id \
                 WHERE orders.item <> 'banana' GROUP BY customers.name ORDER BY customers.name",
            )
            .unwrap();
        assert_eq!(result.rows().len(), 2);
    }

    #[test]
    fn empty_table_join_returns_no_rows() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE a (id BIGINT NOT NULL)").unwrap();
        db.execute("CREATE TABLE b (id BIGINT NOT NULL)").unwrap();
        let result = db.execute("SELECT a.id FROM a JOIN b ON a.id = b.id").unwrap();
        assert!(result.rows().is_empty());
    }

    #[test]
    fn nested_loop_and_hash_join_agree_end_to_end() {
        // 同じデータに対し、等値条件(Hash Join)と、`a.id = b.id`と同値になる
        // 非等値の連言(`>=`かつ`<=`、NestedLoopJoinが選ばれる)から、
        // 実質的に同じ行が取り出せることを確認する。
        let mut db = Database::memory();
        db.execute("CREATE TABLE a (id BIGINT NOT NULL)").unwrap();
        db.execute("CREATE TABLE b (id BIGINT NOT NULL)").unwrap();
        db.execute("INSERT INTO a VALUES (1), (2), (3)").unwrap();
        db.execute("INSERT INTO b VALUES (2), (3), (4)").unwrap();

        let via_hash = db.execute("SELECT a.id FROM a JOIN b ON a.id = b.id ORDER BY a.id").unwrap();
        let via_nlj = db.execute("SELECT a.id FROM a JOIN b ON a.id >= b.id AND a.id <= b.id ORDER BY a.id").unwrap();

        assert_eq!(via_hash.rows(), via_nlj.rows());
        assert!(db.execute("EXPLAIN SELECT a.id FROM a JOIN b ON a.id = b.id").unwrap().to_string().contains("HashJoin"));
        assert!(
            db.execute("EXPLAIN SELECT a.id FROM a JOIN b ON a.id >= b.id AND a.id <= b.id")
                .unwrap()
                .to_string()
                .contains("NestedLoopJoin")
        );
    }

    #[test]
    #[ignore = "実行時間の計測用。cargo test -- --ignored --nocapture で実行する"]
    fn nested_loop_join_is_quadratic_while_hash_join_is_linear() {
        // NestedLoopJoinはO(n×m)、Hash JoinはO(n+m)という、この章が主張する
        // 計算量の違いを、実際に実行時間を測って確認する。同じデータ・同じ
        // 意味論(`a.id = b.id`)を持つ2つの条件を使い分け、片方だけを
        // NestedLoopJoinへ強制する(`a.id >= b.id AND a.id <= b.id`は`=`と
        // 同値だが、この章の`split_equi_join_keys`は`>=`・`<=`を等値条件として
        // 認識しないため、必ずNestedLoopJoinが選ばれる)。
        for n in [500usize, 1000, 2000] {
            let mut db = Database::memory();
            db.execute("CREATE TABLE a (id BIGINT NOT NULL)").unwrap();
            db.execute("CREATE TABLE b (id BIGINT NOT NULL)").unwrap();
            let a_values: Vec<String> = (0..n).map(|i| format!("({i})")).collect();
            let b_values: Vec<String> = (0..n).map(|i| format!("({i})")).collect();
            db.execute(&format!("INSERT INTO a VALUES {}", a_values.join(", "))).unwrap();
            db.execute(&format!("INSERT INTO b VALUES {}", b_values.join(", "))).unwrap();

            let hash_sql = "SELECT a.id FROM a JOIN b ON a.id = b.id";
            let nlj_sql = "SELECT a.id FROM a JOIN b ON a.id >= b.id AND a.id <= b.id";
            assert!(db.execute(&format!("EXPLAIN {hash_sql}")).unwrap().to_string().contains("HashJoin"));
            assert!(db.execute(&format!("EXPLAIN {nlj_sql}")).unwrap().to_string().contains("NestedLoopJoin"));

            let start = std::time::Instant::now();
            let hash_result = db.execute(hash_sql).unwrap();
            let hash_elapsed = start.elapsed();

            let start = std::time::Instant::now();
            let nlj_result = db.execute(nlj_sql).unwrap();
            let nlj_elapsed = start.elapsed();

            assert_eq!(hash_result.rows().len(), n);
            assert_eq!(nlj_result.rows().len(), n);
            eprintln!("n={n:>5}  HashJoin={hash_elapsed:>10?}  NestedLoopJoin={nlj_elapsed:>10?}");
        }
    }

    // ---- CREATE INDEX / DROP INDEX / Index Maintenance(第24章) ----

    /// `QueryResult`は`Debug`を実装していない(第5章から変わっていない)ため、
    /// `unwrap_err`はそのまま使えない。`storage`モジュールの`expect_err`と
    /// 同じ理由の小さなヘルパー。
    fn expect_error(result: DbResult<QueryResult>) -> DbError {
        match result {
            Ok(_) => panic!("エラーを期待しましたが成功しました"),
            Err(err) => err,
        }
    }

    fn users_pk_unique_disk_db(path: &std::path::Path) -> Database {
        let mut db = Database::open(path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE, name TEXT)").unwrap();
        db
    }

    #[test]
    fn create_table_with_primary_key_auto_creates_a_unique_index() {
        let path = temp_db_path("auto-index-pk");
        let mut db = users_pk_unique_disk_db(&path);
        // 自動生成された索引名(`{table}_{column}_idx`)へ、同じ名前の
        // CREATE INDEXをぶつけると「すでに存在します」で拒否される。これが、
        // PRIMARY KEY・UNIQUE列に対応する索引がすでに登録されている
        // 間接証拠になる。
        let err = expect_error(db.execute("CREATE INDEX users_id_idx ON users (id)"));
        assert!(matches!(err, DbError::Bind { .. }));
        let err = expect_error(db.execute("CREATE INDEX users_email_idx ON users (email)"));
        assert!(matches!(err, DbError::Bind { .. }));

        std::fs::remove_file(&path).unwrap();
        for suffix in ["users_id_idx", "users_email_idx"] {
            let _ = std::fs::remove_file(format!("{}.idx.{suffix}", path.display()));
        }
    }

    /// 第3部レビュー対応の回帰テスト: `CREATE TABLE`が自動生成しようとする
    /// 制約索引名(`{table}_{column}_idx`)が、既存の(無関係なテーブルへの)
    /// 手動索引とすでに衝突している場合、`CREATE TABLE`はテーブル自体も
    /// 一切作らずに失敗する。
    ///
    /// 修正前は`storage.create_table`でテーブルを永続化した**後**に
    /// 索引名の衝突が発覚し、テーブルだけが(対応する制約索引を持たないまま)
    /// カタログに残っていた。その状態で`INSERT`すると、
    /// `crate::index::check_uniqueness_with_index`が対応する索引を
    /// 見つけられず`unreachable!`でプロセスごと終了していた
    /// (`src/index.rs`の回帰テストが、その状態自体は個別に再現している)。
    #[test]
    fn create_table_does_not_create_the_table_when_a_constraint_index_name_collides() {
        let path = temp_db_path("create-table-index-name-collision");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE dummy (id BIGINT NOT NULL)").unwrap();
        // "users"テーブルはまだ存在しないが、そのPRIMARY KEY列`id`が
        // 自動生成するはずの索引名("users_id_idx")を、無関係な"dummy"
        // テーブルへの手動索引として先取りしておく。
        db.execute("CREATE INDEX users_id_idx ON dummy (id)").unwrap();

        let err = expect_error(db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)"));
        assert!(matches!(err, DbError::DuplicateIndex(name) if name == "users_id_idx"));

        // テーブル自体も作られていないはず。
        let err = expect_error(db.execute("INSERT INTO users VALUES (1, 'Alice')"));
        assert!(matches!(err, DbError::Bind { .. }), "usersテーブルは作られていないはず");
        let err = expect_error(db.execute("SELECT * FROM users"));
        assert!(matches!(err, DbError::Bind { .. }), "usersテーブルは作られていないはず");

        // 衝突していない名前であれば、通常どおりCREATE TABLEできる。
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
        db.execute("INSERT INTO accounts VALUES (1, 'Alice')").unwrap();

        std::fs::remove_file(&path).unwrap();
        for suffix in ["users_id_idx", "accounts_id_idx"] {
            let _ = std::fs::remove_file(format!("{}.idx.{suffix}", path.display()));
        }
    }

    /// 第3部2巡目レビュー対応の回帰テスト: 索引名の衝突ではなく、索引
    /// ファイルのパス長がOSの上限を超えるI/Oエラーによって制約索引の
    /// 作成が失敗するケース。修正前は`storage.create_table`でテーブルを
    /// 先に永続化していたため、この場合もテーブルだけが残り、
    /// `SELECT COUNT(*)`は成功するのに続く`INSERT`が索引欠落の整合性
    /// エラーになっていた。
    #[test]
    fn create_table_does_not_create_the_table_when_a_constraint_index_file_path_is_too_long() {
        let path = temp_db_path("create-table-index-file-too-long");
        let mut db = Database::open(&path).unwrap();
        let long_table_name = "t".repeat(245);

        let err = expect_error(db.execute(&format!("CREATE TABLE {long_table_name} (id BIGINT PRIMARY KEY)")));
        assert!(matches!(err, DbError::Io(_)), "索引ファイルのパス長超過はI/Oエラーとして観測されるはず: {err:?}");

        // テーブル自体も作られていないはず(修正前は`SELECT COUNT(*)`が
        // 成功してしまっていた)。
        let err = expect_error(db.execute(&format!("SELECT COUNT(*) FROM {long_table_name}")));
        assert!(matches!(err, DbError::Bind { .. }), "{long_table_name}テーブルは作られていないはず");

        // 同じデータベースへ、通常のテーブルは問題なく作れる
        // (カタログやnext_table_idが壊れていないことの確認)。
        db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(format!("{}.idx.users_id_idx", path.display()));
    }

    #[test]
    fn create_index_and_drop_index_round_trip() {
        let path = temp_db_path("create-drop-index");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')").unwrap();

        assert_eq!(db.execute("CREATE INDEX idx_name ON users (name)").unwrap().to_string(), "CREATE INDEX");
        // 同じ索引名は二重に作れない。
        let err = expect_error(db.execute("CREATE INDEX idx_name ON users (name)"));
        assert!(matches!(err, DbError::Bind { .. }));

        assert_eq!(db.execute("DROP INDEX idx_name").unwrap().to_string(), "DROP INDEX");
        // 削除済みの索引名はもう指定できない。
        let err = expect_error(db.execute("DROP INDEX idx_name"));
        assert!(matches!(err, DbError::Bind { .. }));

        std::fs::remove_file(&path).unwrap();
    }

    /// 第3部2巡目レビュー対応の回帰テスト: `PRIMARY KEY`・`UNIQUE`列に
    /// 対応して自動生成された制約索引は、`EXPLAIN`で名前が見えていても
    /// SQLの`DROP INDEX`では削除できない。`PRIMARY KEY`と`UNIQUE`の
    /// どちらの制約索引についても確認する。
    #[test]
    fn drop_index_rejects_a_constraint_index_for_both_primary_key_and_unique() {
        let path = temp_db_path("drop-index-rejects-constraint");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE, name TEXT)").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')").unwrap();
        // 自動生成された索引名は、EXPLAINが選ぶアクセスパスから確認できる。
        // 第28章から、アクセスパスはコストで選ぶ(`choose_scan_plan`)ため、
        // 数行だけのテーブルではSeqScanのほうが安く済んでしまい索引が
        // 選ばれないことがある。この確認だけを目的に、行数を増やし
        // `ANALYZE`して点検索を実際に選択的にしておく(第34章でPage LSNの分
        // だけページの実効容量が減り、299行では境界的だったため1000行へ
        // 引き上げてある)。
        let rows: Vec<String> = (2..1000).map(|i| format!("({i}, 'user{i}@example.com', 'User{i}')")).collect();
        db.execute(&format!("INSERT INTO users VALUES {}", rows.join(", "))).unwrap();
        db.execute("ANALYZE users").unwrap();

        let plan = db.execute("EXPLAIN SELECT id FROM users WHERE id = 1").unwrap().to_string();
        assert!(plan.contains("users_id_idx"), "plan={plan}");

        let err = expect_error(db.execute("DROP INDEX users_id_idx"));
        assert!(matches!(err, DbError::CannotDropConstraintIndex(name) if name == "users_id_idx"));
        let err = expect_error(db.execute("DROP INDEX users_email_idx"));
        assert!(matches!(err, DbError::CannotDropConstraintIndex(name) if name == "users_email_idx"));

        // 拒否されただけで、制約自体は引き続き効いている。
        let err = expect_error(db.execute("INSERT INTO users VALUES (1, 'b@example.com', 'Bob')"));
        assert!(matches!(err, DbError::PrimaryKeyViolation { ref column, .. } if column == "id"));
        let err = expect_error(db.execute("INSERT INTO users VALUES (99999, 'a@example.com', 'Carol')"));
        assert!(matches!(err, DbError::UniqueViolation { ref column, .. } if column == "email"));

        // 手動で作った(制約索引ではない)索引は、これまでどおりDROP INDEXできる。
        db.execute("CREATE INDEX idx_name ON users (name)").unwrap();
        assert_eq!(db.execute("DROP INDEX idx_name").unwrap().to_string(), "DROP INDEX");

        // DROP TABLEなら、制約索引ごとテーブルを削除できる。
        assert_eq!(db.execute("DROP TABLE users").unwrap().to_string(), "DROP TABLE");
        db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY)").unwrap();

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(format!("{}.idx.users_id_idx", path.display()));
    }

    #[test]
    fn create_index_on_unknown_table_or_column_is_a_bind_error() {
        let path = temp_db_path("create-index-unknown");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)").unwrap();

        let err = expect_error(db.execute("CREATE INDEX idx ON ghosts (id)"));
        assert!(matches!(err, DbError::Bind { .. }));
        let err = expect_error(db.execute("CREATE INDEX idx ON users (ghost_column)"));
        assert!(matches!(err, DbError::Bind { .. }));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn create_index_and_drop_index_are_rejected_on_the_memory_backend() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)").unwrap();
        let err = expect_error(db.execute("CREATE INDEX idx ON users (id)"));
        assert!(matches!(err, DbError::NotImplemented(_)));
    }

    #[test]
    fn create_index_builds_from_rows_inserted_before_it_existed() {
        let path = temp_db_path("index-build-from-existing");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')").unwrap();

        db.execute("CREATE UNIQUE INDEX idx_name ON users (name)").unwrap();
        // Index Buildが既存の重複を見逃していないことを、後から同じ値を
        // 挿入して確認する。
        let err = expect_error(db.execute("INSERT INTO users VALUES (4, 'Alice')"));
        assert!(matches!(err, DbError::UniqueViolation { .. }));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn create_unique_index_on_a_table_with_existing_duplicates_is_rejected() {
        let path = temp_db_path("index-build-rejects-existing-dup");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Alice')").unwrap();

        let err = expect_error(db.execute("CREATE UNIQUE INDEX idx_name ON users (name)"));
        assert!(matches!(err, DbError::UniqueViolation { .. }));
        // 索引は作られていないので、後からのINSERTも制約されない。
        db.execute("INSERT INTO users VALUES (3, 'Alice')").unwrap();

        std::fs::remove_file(&path).unwrap();
    }

    /// 第20章の走査ベース検査(メモリバックエンド)と、第24章の索引ベース検査
    /// (ディスクバックエンド)が、同じ違反に対して同じ種類のエラーを返すことを
    /// 確認する。
    #[test]
    fn index_based_uniqueness_check_matches_the_scan_based_check() {
        let mem_err = {
            let mut db = Database::memory();
            db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE)").unwrap();
            db.execute("INSERT INTO users VALUES (1, 'a@example.com')").unwrap();
            expect_error(db.execute("INSERT INTO users VALUES (1, 'b@example.com')"))
        };
        assert!(matches!(mem_err, DbError::PrimaryKeyViolation { ref column, .. } if column == "id"));

        let path = temp_db_path("index-matches-scan-pk");
        let disk_err = {
            let mut db = users_pk_unique_disk_db(&path);
            db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')").unwrap();
            expect_error(db.execute("INSERT INTO users VALUES (1, 'b@example.com', 'Bob')"))
        };
        assert!(matches!(disk_err, DbError::PrimaryKeyViolation { ref column, .. } if column == "id"));
        assert_eq!(mem_err.to_string(), disk_err.to_string());

        let mem_unique_err = {
            let mut db = Database::memory();
            db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE)").unwrap();
            db.execute("INSERT INTO users VALUES (1, 'a@example.com')").unwrap();
            expect_error(db.execute("INSERT INTO users VALUES (2, 'a@example.com')"))
        };
        let path2 = temp_db_path("index-matches-scan-unique");
        let disk_unique_err = {
            let mut db = users_pk_unique_disk_db(&path2);
            db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')").unwrap();
            expect_error(db.execute("INSERT INTO users VALUES (2, 'a@example.com', 'Bob')"))
        };
        assert!(matches!(mem_unique_err, DbError::UniqueViolation { ref column, .. } if column == "email"));
        assert_eq!(mem_unique_err.to_string(), disk_unique_err.to_string());

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&path2).unwrap();
    }

    #[test]
    fn insert_select_within_the_same_statement_is_still_checked_against_candidates() {
        // 索引にはまだ無い値同士(同じINSERT文の中の2行)の重複は、索引への
        // lookupだけでは検出できない(crate::index::check_uniqueness_with_indexの
        // ドキュメント参照)。constraints::check_uniquenessのcandidates同士の
        // 検査が引き続きこれを捕まえることを確認する。
        let path = temp_db_path("candidates-within-statement");
        let mut db = users_pk_unique_disk_db(&path);
        let err = expect_error(
            db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice'), (2, 'a@example.com', 'Bob')"),
        );
        assert!(matches!(err, DbError::UniqueViolation { .. }));
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 0, "All-or-Nothingで1行も入らない");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn update_maintains_the_index_including_when_the_record_id_moves() {
        let path = temp_db_path("update-maintenance");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')").unwrap();

        // 十分大きな値へ更新し、同じページに収まらずRecordIdが変わる
        // (ページをまたぐ移動が起きる)状況を作る。
        let long_name = "x".repeat(3000);
        db.execute(&format!("UPDATE users SET name = '{long_name}' WHERE id = 1")).unwrap();

        // 索引(idのPRIMARY KEY索引)は、移動後のRecordIdを指したまま
        // 一意性検査に使えなければならない。id=1と重複するINSERTはやはり拒否される。
        let err = expect_error(db.execute("INSERT INTO users VALUES (1, 'Carol')"));
        assert!(matches!(err, DbError::PrimaryKeyViolation { .. }));

        // id自体を書き換えるUPDATEも、索引を通じて重複を検出する。
        let err = expect_error(db.execute("UPDATE users SET id = 2 WHERE id = 1"));
        assert!(matches!(err, DbError::PrimaryKeyViolation { .. }));

        // 値を変えないUPDATE(自分自身との比較)は誤検出しない。
        db.execute("UPDATE users SET id = 1 WHERE id = 1").unwrap();

        // 2行が互いの値を交換するUPDATEも、索引ベースの検査で正しく許される。
        db.execute("UPDATE users SET id = 3 WHERE id = 1").unwrap();
        db.execute("UPDATE users SET id = 1 WHERE id = 2").unwrap();
        db.execute("UPDATE users SET id = 2 WHERE id = 3").unwrap();
        let mut ids: Vec<i64> = db
            .execute("SELECT id FROM users")
            .unwrap()
            .rows()
            .iter()
            .map(|t| match t.values()[0] {
                Value::BigInt(n) => n,
                _ => panic!("BIGINTのはず"),
            })
            .collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delete_then_reinsert_the_same_unique_value_succeeds() {
        let path = temp_db_path("delete-then-reinsert");
        let mut db = users_pk_unique_disk_db(&path);
        db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')").unwrap();

        db.execute("DELETE FROM users WHERE id = 1").unwrap();
        // 索引からもエントリが取り除かれていなければ、次のINSERTが
        // (実際には存在しない行との)重複として誤って拒否されてしまう。
        db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')").unwrap();
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 1);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn indexed_table_survives_a_reopen_and_keeps_enforcing_constraints() {
        let path = temp_db_path("indexed-table-reopen");
        {
            let mut db = users_pk_unique_disk_db(&path);
            db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')").unwrap();
            db.execute("CREATE INDEX idx_name ON users (name)").unwrap();
            db.flush().unwrap();
        }

        let mut db = Database::open(&path).unwrap();
        // PRIMARY KEY・UNIQUE経由の索引も、明示的なCREATE INDEXの索引も、
        // 再オープン後に引き続き機能する。
        let err = expect_error(db.execute("INSERT INTO users VALUES (1, 'b@example.com', 'Bob')"));
        assert!(matches!(err, DbError::PrimaryKeyViolation { .. }));
        let err = expect_error(db.execute("CREATE INDEX idx_name ON users (name)"));
        assert!(matches!(err, DbError::Bind { .. }), "idx_nameはreopen後も存在するはず");

        db.execute("INSERT INTO users VALUES (2, 'b@example.com', 'Bob')").unwrap();
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 2);

        std::fs::remove_file(&path).unwrap();
        for suffix in ["users_id_idx", "users_email_idx", "idx_name"] {
            let _ = std::fs::remove_file(format!("{}.idx.{suffix}", path.display()));
        }
    }

    #[test]
    fn drop_table_removes_its_auto_created_indexes_too() {
        let path = temp_db_path("drop-table-removes-indexes");
        let mut db = users_pk_unique_disk_db(&path);
        db.execute("DROP TABLE users").unwrap();
        // テーブルが無くなったので、同名の索引をもう一度自動生成できる
        // (=以前の索引がカタログに残っていない)ことを、テーブルの再作成が
        // 成功することで確認する。
        db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE, name TEXT)").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')").unwrap();

        std::fs::remove_file(&path).unwrap();
        for suffix in ["users_id_idx", "users_email_idx"] {
            let _ = std::fs::remove_file(format!("{}.idx.{suffix}", path.display()));
        }
    }

    // ---- Index Scanとアクセスパス(第25章) ----

    fn orders_disk_db(path: &std::path::Path) -> Database {
        let mut db = Database::open(path).unwrap();
        db.execute("CREATE TABLE orders (id BIGINT NOT NULL, amount BIGINT, name TEXT)").unwrap();
        db
    }

    /// 本体ファイルと、`names`が指す索引ファイルをまとめて削除する。
    fn remove_db_and_indexes(path: &std::path::Path, names: &[&str]) {
        std::fs::remove_file(path).unwrap();
        for name in names {
            let _ = std::fs::remove_file(format!("{}.idx.{name}", path.display()));
        }
    }

    /// 第28章から、アクセスパスの選択はコスト最小のものを選ぶ方式になった
    /// (`choose_scan_plan`)。数行しかないテーブルでは、実際にSeqScanのほうが
    /// コストの低い候補になりうる(1ページを読むだけで済むのに対し、
    /// IndexScanは索引の`lookup`ぶんのRandom I/Oを追加で払うため)。索引が
    /// 選ばれることを確かめるこの章のテストは、`ANALYZE`済みの、点検索が
    /// 実際に選択的な規模のテーブル(1,000行)を使う。
    fn insert_many_orders(db: &mut Database, n: i64) {
        let rows: Vec<String> = (0..n).map(|i| format!("({i}, {}, 'name{i}')", i * 10)).collect();
        db.execute(&format!("INSERT INTO orders VALUES {}", rows.join(", "))).unwrap();
        db.execute("ANALYZE orders").unwrap();
    }

    #[test]
    fn point_predicate_on_an_indexed_column_chooses_index_scan_and_absorbs_the_whole_filter() {
        let path = temp_db_path("index-scan-point");
        let mut db = orders_disk_db(&path);
        db.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
        insert_many_orders(&mut db, 1000);

        let plan = db.execute("EXPLAIN SELECT id, amount, name FROM orders WHERE id = 42").unwrap().to_string();
        assert!(plan.contains("IndexScan(idx_id, id = 42)"), "plan={plan}");
        assert!(!plan.contains("Filter("), "id = 42だけの述語はIndexScanへ丸ごと吸収されるはず: {plan}");

        let result = db.execute("SELECT id, amount, name FROM orders WHERE id = 42").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(42), Value::BigInt(420), Value::Text("name42".to_string())]);

        remove_db_and_indexes(&path, &["idx_id"]);
    }

    #[test]
    fn point_predicate_leaves_the_rest_of_a_conjunction_in_a_residual_filter() {
        // `id`だけに索引がある。`id = 42 AND name = 'name42'`は、`id = 42`だけが
        // IndexScanに吸収され、`name = 'name42'`はFilterに残る。
        let path = temp_db_path("index-scan-residual-filter");
        let mut db = orders_disk_db(&path);
        db.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
        insert_many_orders(&mut db, 1000);

        let plan = db
            .execute("EXPLAIN SELECT id, amount, name FROM orders WHERE id = 42 AND name = 'name42'")
            .unwrap()
            .to_string();
        assert!(plan.contains("IndexScan(idx_id, id = 42)"), "plan={plan}");
        assert!(plan.contains("Filter(name = 'name42')"), "plan={plan}");

        let result = db.execute("SELECT id, amount, name FROM orders WHERE id = 42 AND name = 'name42'").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values()[2], Value::Text("name42".to_string()));

        remove_db_and_indexes(&path, &["idx_id"]);
    }

    /// 第25章の`choose_access_path`は、Range述語を見つければ常にRange Index
    /// Scanを選んでいた。第28章のコストベース選択では、事情が変わる。
    ///
    /// 第4部レビュー対応で`bucket_overlap_fraction`(`crate::estimator`)が
    /// BIGINTに対して本物の線形補間を行うようになったため、バケツの境界を
    /// またぐだけの範囲述語(値が一様に近く分布している場合)はもう「バケツ
    /// 1個ぶん」まで過大評価されない。それでも残る限界は、線形補間自体が
    /// 「バケツの`[lower, upper]`区間内で値が一様に分布している」という
    /// 仮定に立っていることである。この仮定は、1つのバケツの中身が実際には
    /// 両端に偏って分布している(中間がほとんど空)ような分布では崩れる。
    /// この章はBitmap Index Scan(索引で得た`RecordId`を先にページ順へ
    /// ソートしてからHeapを読む、PostgreSQLにもある方式。章末の演習課題)を
    /// 持たないため、線形補間が過大評価する範囲述語はSeqScanのままになる。
    #[test]
    fn range_predicate_prefers_seq_scan_when_the_bucket_interior_is_not_uniform() {
        let path = temp_db_path("index-scan-range");
        let mut db = orders_disk_db(&path);
        db.execute("CREATE INDEX idx_amount ON orders (amount)").unwrap();

        // 1400行(バケツ10個、1バケツ=140行)を仕込む。うち200行だけ、
        // ソート順で連続する1つのバケツにちょうど収まるよう`amount`を
        // 1000(100行)と2000(100行)の2値だけに集中させ、残りの1200行は
        // その外側([0,899]と[2001,3200])に均等に散らばせる。この結果、
        // 1つのバケツが`[1000, 2000]`という区間を持ちながら、実際の値は
        // 区間の両端に偏り、中間(1400〜1600)には1行も無い。(第34章で
        // Page LSNの分だけページの実効容量が減ったため、以前の5000行から
        // 引き下げてある。Catalogページの`page_ids`(第15章)が収まる範囲に
        // 収めるためで、バケツ構成の意図は変わらない。)
        let mut rows: Vec<String> = Vec::new();
        for i in 0..600i64 {
            let amount = i * 900 / 600; // [0, 899]
            rows.push(format!("({i}, {amount}, 'name{i}')"));
        }
        for i in 0..100i64 {
            let id = 600 + i;
            rows.push(format!("({id}, 1000, 'name{id}')"));
        }
        for i in 0..100i64 {
            let id = 700 + i;
            rows.push(format!("({id}, 2000, 'name{id}')"));
        }
        for i in 0..600i64 {
            let id = 800 + i;
            let amount = 2001 + i; // [2001, 2600]
            rows.push(format!("({id}, {amount}, 'name{id}')"));
        }
        db.execute(&format!("INSERT INTO orders VALUES {}", rows.join(", "))).unwrap();
        db.execute("ANALYZE orders").unwrap();

        // `amount`が1000か2000の行しか無いため、[1400, 1600]に実際に
        // 一致する行は無い(actual=0)。それでも線形補間は、バケツ
        // `[1000, 2000]`の中で値が一様に分布していると仮定するため、
        // バケツの中央付近を相応の行数(0行ではない)があるものとして見積もる。
        let plan = db
            .execute("EXPLAIN ANALYZE SELECT id FROM orders WHERE amount >= 1400 AND amount <= 1600")
            .unwrap()
            .to_string();
        assert!(plan.contains("SeqScan(orders)"), "plan={plan}");
        assert!(plan.contains("Filter(amount >= 1400 AND amount <= 1600)"), "plan={plan}");
        assert!(plan.contains("actual=0"), "実際に一致する行は無いはず: plan={plan}");
        assert!(!plan.contains("rows=0 "), "見積もりは0行ではないはず(過大評価が残っている): plan={plan}");

        // 選ばれたアクセスパスが変わっても、結果の行集合は空のまま変わらない。
        let result = db.execute("SELECT id FROM orders WHERE amount >= 1400 AND amount <= 1600").unwrap();
        assert!(result.rows().is_empty());

        remove_db_and_indexes(&path, &["idx_amount"]);
    }

    #[test]
    fn seq_scan_is_kept_when_the_predicate_column_has_no_index() {
        let path = temp_db_path("index-scan-no-index");
        let mut db = orders_disk_db(&path);
        db.execute("INSERT INTO orders VALUES (1, 100, 'Alice')").unwrap();

        // 索引を1つも作らなければ、第24章までと同じくSeqScan+Filterのまま
        // (`Database::memory`と同じテキストになる)。
        let plan = db.execute("EXPLAIN SELECT id FROM orders WHERE amount = 100").unwrap().to_string();
        assert_eq!(
            plan,
            "QUERY PLAN\n----------\nProjection(id) rows=5 cost=21.05\n  └─ Filter(amount = 100) rows=5 cost=21.00\n    └─ SeqScan(orders) rows=1000 cost=11.00\n(3 rows)"
        );

        remove_db_and_indexes(&path, &[]);
    }

    #[test]
    fn null_equality_predicate_never_uses_the_index() {
        // `col = NULL`は三値論理でUNKNOWNになり常に0行だが(第8章)、
        // その判定はFilterに任せる。`crate::btree::BTree`は`NULL`をキーに
        // 持てない(第23章)ため、`choose_access_path`はこの述語を索引の対象から
        // 外し、索引はそもそも引かない。
        let path = temp_db_path("index-scan-null-equality");
        let mut db = orders_disk_db(&path);
        db.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
        db.execute("INSERT INTO orders VALUES (1, 100, 'Alice')").unwrap();

        let plan = db.execute("EXPLAIN SELECT id FROM orders WHERE id = NULL").unwrap().to_string();
        assert!(!plan.contains("IndexScan"), "col = NULLは索引を引いてはならない: {plan}");
        assert!(plan.contains("SeqScan"));

        let result = db.execute("SELECT id FROM orders WHERE id = NULL").unwrap();
        assert!(result.rows().is_empty());

        remove_db_and_indexes(&path, &["idx_id"]);
    }

    #[test]
    fn index_scan_and_seq_scan_agree_on_the_same_query_results() {
        // 同一のデータを持つ2つのDB(索引あり/無し)に同じクエリを実行し、
        // 行集合が一致することを確認する。索引を検討するのは
        // `physical_plan::optimize`だけであり、`IndexScanExec`が返す行の
        // 中身(タプルのデコード)は`DiskSeqScanExec`と変わらないはずである。
        let with_index_path = temp_db_path("index-vs-seq-with-index");
        let without_index_path = temp_db_path("index-vs-seq-without-index");

        // 第28章から、アクセスパスはコストで選ぶ(`choose_scan_plan`)。数百行
        // 程度のテーブルでは、実ページ数が少なすぎてSeqScanの方が安くなる
        // ことがある(索引側は`lookup`ぶんのRandom I/Oを追加で払うため)。
        // ここではIndexScanが実際に有利になる規模(1万行)を使い、`ANALYZE`
        // して選択率の推定を実際の分布に合わせる(統計が無ければ、
        // PostgreSQLの`selfuncs.c`にならった慣用のデフォルト定数
        // (`crate::estimator::DEFAULT_EQ_SEL`等)にフォールバックする、第27章)。
        // 第34章でPage LSNの分だけページの実効容量が減り、1万行では
        // `ANALYZE`が集める統計情報を含めてCatalogページ(第15章、第27章)が
        // 収まりきらなくなったため、1000行へ引き下げてある
        // (`point_predicate_on_an_indexed_column_...`が1000行ですでに
        // IndexScanを選ぶことを確認済みで、IndexScanが有利になる規模と
        // いう以前の意図は変わらない)。
        let n = 1_000i64;
        let rows: Vec<String> = (0..n).map(|i| format!("({i}, {}, 'name{i}')", i * 3)).collect();
        let insert_sql = format!("INSERT INTO orders VALUES {}", rows.join(", "));

        let mut with_index = orders_disk_db(&with_index_path);
        with_index.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
        with_index.execute("CREATE INDEX idx_amount ON orders (amount)").unwrap();
        with_index.execute(&insert_sql).unwrap();
        with_index.execute("ANALYZE orders").unwrap();

        let mut without_index = orders_disk_db(&without_index_path);
        without_index.execute(&insert_sql).unwrap();

        // Point述語(`id = 定数`)は、`estimate_equality_selectivity`(第27章)が
        // Histogramのバケツをさらにdistinct値数で割るため、範囲述語より
        // 細かい粒度で一致行数を見積もれる。この規模(1万行)なら、
        // Point述語は確実にIndexScanを選ぶ。範囲述語がSeqScanのままなのは
        // `range_predicate_prefers_seq_scan_when_the_range_is_not_selective_enough`
        // で確認済みの、Histogramの粒度に起因する正直な限界である。
        let point_queries = ["SELECT id, amount, name FROM orders WHERE id = 42", "SELECT id, amount, name FROM orders WHERE id = 999999999"];
        let range_queries = [
            "SELECT id, amount, name FROM orders WHERE amount >= 100 AND amount <= 130".to_string(),
            format!("SELECT id, amount, name FROM orders WHERE amount > {}", (n - 3) * 3),
        ];

        for query in point_queries.iter().map(|q| q.to_string()).chain(range_queries) {
            let query = query.as_str();
            if point_queries.contains(&query) {
                let with_index_plan = with_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string();
                assert!(with_index_plan.contains("IndexScan"), "索引ありDBはPoint述語ならIndexScanを選ぶはず: {with_index_plan}");
            }
            let without_index_plan = without_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string();
            assert!(without_index_plan.contains("SeqScan"), "索引無しDBはSeqScanのまま: {without_index_plan}");

            let mut with_index_rows: Vec<Vec<Value>> =
                with_index.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
            let mut without_index_rows: Vec<Vec<Value>> =
                without_index.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
            with_index_rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            without_index_rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            assert_eq!(with_index_rows, without_index_rows, "クエリ`{query}`の結果が索引の有無で食い違った");
        }

        remove_db_and_indexes(&with_index_path, &["idx_id", "idx_amount"]);
        remove_db_and_indexes(&without_index_path, &[]);
    }

    #[test]
    fn index_scan_excludes_a_row_removed_by_delete() {
        let path = temp_db_path("index-scan-after-delete");
        let mut db = orders_disk_db(&path);
        db.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
        insert_many_orders(&mut db, 1000);

        assert_eq!(db.execute("DELETE FROM orders WHERE id = 2").unwrap().to_string(), "DELETE 1");

        // 削除された`id = 2`はIndexScanでも0行(索引エントリ自体が
        // Index Maintenanceで取り除かれている、第24章)。
        let plan = db.execute("EXPLAIN SELECT id FROM orders WHERE id = 2").unwrap().to_string();
        assert!(plan.contains("IndexScan(idx_id, id = 2)"), "plan={plan}");
        assert!(db.execute("SELECT id FROM orders WHERE id = 2").unwrap().rows().is_empty());

        // 削除していない行は引き続きIndexScanで見つかる。
        let remaining = db.execute("SELECT id FROM orders WHERE id = 1").unwrap();
        assert_eq!(remaining.rows().len(), 1);

        remove_db_and_indexes(&path, &["idx_id"]);
    }

    #[test]
    fn index_scan_reflects_an_update_to_the_indexed_column() {
        let path = temp_db_path("index-scan-after-update");
        let mut db = orders_disk_db(&path);
        db.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
        db.execute("INSERT INTO orders VALUES (1, 100, 'Alice')").unwrap();

        assert_eq!(db.execute("UPDATE orders SET id = 42 WHERE id = 1").unwrap().to_string(), "UPDATE 1");

        // 更新前の値はもう見つからず、更新後の値でIndexScanが引ける
        // (Index Maintenanceが古いエントリを削除し、新しいエントリを
        // 挿入している、第24章)。
        assert!(db.execute("SELECT name FROM orders WHERE id = 1").unwrap().rows().is_empty());
        let updated = db.execute("SELECT name FROM orders WHERE id = 42").unwrap();
        assert_eq!(updated.rows().len(), 1);
        assert_eq!(updated.rows()[0].values(), &[Value::Text("Alice".to_string())]);

        remove_db_and_indexes(&path, &["idx_id"]);
    }

    #[test]
    fn index_nested_loop_join_is_chosen_when_the_inner_join_column_has_an_index() {
        // 第28章から、Join方式もコストで選ぶ(`choose_join_plan`)。内側
        // テーブルを1回全件読むHash Joinのほうが安く済む場合があるため
        // (第25章の`selective`/`dense`の実測が示すとおり、勝敗はデータの
        // 分布次第)、`customers`の行数(`n`、外側)をごく少なく、`orders`の
        // 行数(`m`、内側)を`customers`よりずっと大きく取り、外側の`lookup`
        // 回数そのものを小さく保ってIndex Nested Loop Joinを有利にする。
        // `ANALYZE`して統計に基づく選択率で比較する。
        let path = temp_db_path("index-nlj-chosen");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)").unwrap();
        db.execute("CREATE TABLE orders (id BIGINT, customer_id BIGINT, item TEXT)").unwrap();
        db.execute("CREATE INDEX idx_customer_id ON orders (customer_id)").unwrap();

        let n = 5i64;
        // 第34章でPage LSNの分だけページの実効容量が減り、`ANALYZE`が集める
        // 統計情報を含めてCatalogページが収まらなくなったため、5000から
        // 1250へ引き下げてある(この規模でもIndex Nested Loop Joinが
        // 選ばれることを確認済み。1000行以下ではHash Joinへ逆転する)。
        let m = 1250i64;
        let modulus = n * 1000;
        let customers: Vec<String> = (0..n).map(|i| format!("({i}, 'name{i}')")).collect();
        db.execute(&format!("INSERT INTO customers VALUES {}", customers.join(", "))).unwrap();
        let orders: Vec<String> = (0..m).map(|i| format!("({i}, {}, 'item{i}')", (i * 97) % modulus)).collect();
        db.execute(&format!("INSERT INTO orders VALUES {}", orders.join(", "))).unwrap();
        db.execute("ANALYZE customers").unwrap();
        db.execute("ANALYZE orders").unwrap();

        let plan = db
            .execute("EXPLAIN SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id")
            .unwrap()
            .to_string();
        assert!(plan.contains("IndexNestedLoopJoin"), "plan={plan}");

        let with_index_rows: Vec<Vec<Value>> = db
            .execute(
                "SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id ORDER BY customers.name, orders.item",
            )
            .unwrap()
            .rows()
            .iter()
            .map(|row| row.values().to_vec())
            .collect();
        assert!(!with_index_rows.is_empty(), "選択的な結合でも一致する行が無ければテストの前提が崩れている");

        remove_db_and_indexes(&path, &["idx_customer_id"]);
    }

    /// 第25章の実測ケース(密な結合ではIndex Nested Loop JoinがHash Joinより
    /// 15倍以上遅い)の回収。`customer_id`を`customers`の総数と同じ範囲に
    /// 絞り、`orders`のほぼ全行がどれかの`customers`と一致する密な結合を
    /// 作る。索引が使える(`idx_customer_id`が存在する)にもかかわらず、
    /// コストベースの選択(`choose_join_plan`)はHash Joinを選ぶ。
    #[test]
    fn hash_join_is_chosen_for_a_dense_join_even_when_an_index_exists() {
        let path = temp_db_path("hash-join-dense-chosen");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)").unwrap();
        db.execute("CREATE TABLE orders (id BIGINT, customer_id BIGINT, item TEXT)").unwrap();
        db.execute("CREATE INDEX idx_customer_id ON orders (customer_id)").unwrap();

        let n = 50i64;
        // 第34章でPage LSNの分だけページの実効容量が減り、`ANALYZE`が集める
        // 統計情報を含めてCatalogページが収まらなくなったため、2000から
        // 1000へ引き下げてある(密な結合という以前の意図は変わらない)。
        let m = 1000i64;
        let modulus = n; // 密な結合: customer_idの値域をcustomersの総数だけに絞る。
        let customers: Vec<String> = (0..n).map(|i| format!("({i}, 'name{i}')")).collect();
        db.execute(&format!("INSERT INTO customers VALUES {}", customers.join(", "))).unwrap();
        let orders: Vec<String> = (0..m).map(|i| format!("({i}, {}, 'item{i}')", (i * 97) % modulus)).collect();
        db.execute(&format!("INSERT INTO orders VALUES {}", orders.join(", "))).unwrap();
        db.execute("ANALYZE customers").unwrap();
        db.execute("ANALYZE orders").unwrap();

        let plan = db
            .execute("EXPLAIN SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id")
            .unwrap()
            .to_string();
        assert!(plan.contains("HashJoin"), "密な結合ではHash Joinが選ばれるはず(第25章の実測ケースの回収): plan={plan}");
        assert!(!plan.contains("IndexNestedLoopJoin"), "plan={plan}");

        let rows = db
            .execute(
                "SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id",
            )
            .unwrap();
        // ほぼ全行が一致するはず(密な結合)。
        assert!(rows.rows().len() > (m as usize) / 2, "密な結合のはずが一致行数が少なすぎる: {}", rows.rows().len());

        remove_db_and_indexes(&path, &["idx_customer_id"]);
    }

    #[test]
    fn index_nested_loop_join_and_hash_join_produce_identical_results() {
        // Index Nested Loop Join(索引あり)とHash Join(索引無し)は、同じ
        // データ・同じ等値条件に対して同じ行集合を返すはずである。
        let with_index_path = temp_db_path("index-nlj-vs-hash-with-index");
        let without_index_path = temp_db_path("index-nlj-vs-hash-without-index");
        let sql = [
            "CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)",
            "CREATE TABLE orders (id BIGINT, customer_id BIGINT, item TEXT)",
            "INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
            "INSERT INTO orders VALUES (10, 1, 'apple'), (11, 1, 'banana'), (12, 2, 'cherry'), (13, NULL, 'orphan'), (14, 99, 'nomatch')",
        ];

        let mut with_index = Database::open(&with_index_path).unwrap();
        for statement in sql {
            with_index.execute(statement).unwrap();
        }
        with_index.execute("CREATE INDEX idx_customer_id ON orders (customer_id)").unwrap();

        let mut without_index = Database::open(&without_index_path).unwrap();
        for statement in sql {
            without_index.execute(statement).unwrap();
        }

        let query =
            "SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id ORDER BY customers.name, orders.item";
        // `with_index`側がIndex Nested Loop JoinとHash Joinのどちらを選ぶかは、
        // 第28章からコスト次第である(このテストの数行程度の規模では、内側
        // テーブルを丸ごと読んでも安いHash Joinが選ばれることがある。
        // 実際に選択的な結合でIndex Nested Loop Joinが選ばれることは
        // `index_nested_loop_join_is_chosen_when_the_inner_join_column_has_an_index`
        // で確認済み)。ここで確かめたいのは結果の一致であり、選ばれた
        // アルゴリズムそのものではない。`without_index`側は索引が無いので
        // 引き続きHash Join一択である。
        assert!(without_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string().contains("HashJoin"));

        let with_index_rows: Vec<Vec<Value>> =
            with_index.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
        let without_index_rows: Vec<Vec<Value>> =
            without_index.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
        assert_eq!(with_index_rows, without_index_rows);

        remove_db_and_indexes(&with_index_path, &["idx_customer_id"]);
        remove_db_and_indexes(&without_index_path, &[]);
    }

    #[test]
    #[ignore = "実行時間の計測用。cargo test -- --ignored --nocapture で実行する"]
    fn point_index_scan_grows_logarithmically_while_seq_scan_filter_grows_linearly() {
        // 第24章はINSERT(索引経由の一意性検査)がO(log n)へ変わったことを
        // 測った。この章ではSELECTの側、`WHERE id = 定数`という点検索が
        // IndexScanでO(log n)になり、索引の無いSeqScan+FilterのO(n)から
        // 実際に離れていくことを測る。
        for n in [1_000usize, 2_000, 4_000, 8_000, 16_000] {
            let path = temp_db_path(&format!("index-scan-scaling-{n}"));
            let mut db = orders_disk_db(&path);
            db.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
            let rows: Vec<String> = (0..n).map(|i| format!("({i}, {i}, 'name{i}')", i = i as i64)).collect();
            db.execute(&format!("INSERT INTO orders VALUES {}", rows.join(", "))).unwrap();

            let index_query = format!("SELECT id FROM orders WHERE id = {}", n / 2);
            let seq_query = format!("SELECT id FROM orders WHERE amount = {}", n / 2);
            assert!(db.execute(&format!("EXPLAIN {index_query}")).unwrap().to_string().contains("IndexScan"));
            assert!(db.execute(&format!("EXPLAIN {seq_query}")).unwrap().to_string().contains("SeqScan"));

            let start = std::time::Instant::now();
            db.execute(&index_query).unwrap();
            let index_elapsed = start.elapsed();

            let start = std::time::Instant::now();
            db.execute(&seq_query).unwrap();
            let seq_elapsed = start.elapsed();

            eprintln!("n={n:>6}  IndexScan={index_elapsed:>10?}  SeqScan+Filter={seq_elapsed:>10?}");

            remove_db_and_indexes(&path, &["idx_id"]);
        }
    }

    /// `n`人の`customers`と、そのうち`customer_id`が一致する行の割合を
    /// `selective`で調節した`m`件の`orders`を作り、`customers JOIN orders`を
    /// 索引あり(Index Nested Loop Join)・索引無し(Hash Join)の両方の
    /// `Database`で実行して実行時間を計測する。`selective`が`true`なら
    /// `customer_id`を`customers`の総数よりずっと広い範囲に散らし、一致する
    /// 行はごく一部にとどめる。
    fn measure_join_once(m: usize, selective: bool, label: &str) {
        let n = 50usize;
        let with_index_path = temp_db_path(&format!("index-nlj-scaling-with-{label}-{m}"));
        let without_index_path = temp_db_path(&format!("index-nlj-scaling-without-{label}-{m}"));

        let customers: Vec<String> = (0..n).map(|i| format!("({i}, 'c{i}')")).collect();
        let modulus = if selective { n * 1000 } else { n };
        let orders: Vec<String> = (0..m).map(|i| format!("({i}, {}, 'item{i}')", (i * 97) % modulus)).collect();
        let setup = [
            "CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)".to_string(),
            "CREATE TABLE orders (id BIGINT, customer_id BIGINT, item TEXT)".to_string(),
            format!("INSERT INTO customers VALUES {}", customers.join(", ")),
            format!("INSERT INTO orders VALUES {}", orders.join(", ")),
        ];

        let mut with_index = Database::open(&with_index_path).unwrap();
        for statement in &setup {
            with_index.execute(statement).unwrap();
        }
        with_index.execute("CREATE INDEX idx_customer_id ON orders (customer_id)").unwrap();
        // 第28章から、`with_index`が実際にどちらを選ぶかはコスト次第である
        // (`choose_join_plan`)。この測定はあくまで2つの実行アルゴリズム
        // そのものの実測比較が目的なので、`ANALYZE`して実際の分布に近い
        // 判断をさせたうえで、選ばれたアルゴリズム名も一緒に記録する。
        // `m`が大きいと、Histogramの境界値(TEXT列)がCatalogページの
        // 残り容量(第15章)を超えることがある(`DbError::CatalogTooLarge`)。
        // この測定はコスト計算の正確さそのものを検証する場ではないので、
        // 失敗しても`unwrap`で落とさず、統計が無いまま(デフォルト選択率、
        // 第27章)で計測を続ける。
        let _ = with_index.execute("ANALYZE customers");
        let _ = with_index.execute("ANALYZE orders");

        let mut without_index = Database::open(&without_index_path).unwrap();
        for statement in &setup {
            without_index.execute(statement).unwrap();
        }

        let query = "SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id";
        let with_index_plan = with_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string();
        let chosen = if with_index_plan.contains("IndexNestedLoopJoin") {
            "IndexNestedLoopJoin"
        } else {
            "HashJoin"
        };
        // `without_index`は索引が無いので、等値結合の候補はHash Joinしか
        // 無い(`choose_join_plan`が`IndexNestedLoopJoin`を候補にすら
        // 加えない)。これは第28章のコストとは無関係に常に成り立つ。
        assert!(without_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string().contains("HashJoin"));

        let start = std::time::Instant::now();
        let inlj_result = with_index.execute(query).unwrap();
        let inlj_elapsed = start.elapsed();

        let start = std::time::Instant::now();
        let hash_result = without_index.execute(query).unwrap();
        let hash_elapsed = start.elapsed();

        assert_eq!(inlj_result.rows().len(), hash_result.rows().len());
        eprintln!(
            "{label:<9} m={m:>6}  matches={:>6}  chosen={chosen:<21}  with_index={inlj_elapsed:>10?}  without_index(HashJoin)={hash_elapsed:>10?}",
            inlj_result.rows().len()
        );

        remove_db_and_indexes(&with_index_path, &["idx_customer_id"]);
        remove_db_and_indexes(&without_index_path, &[]);
    }

    #[test]
    #[ignore = "実行時間の計測用。cargo test -- --ignored --nocapture で実行する"]
    fn index_nested_loop_join_is_not_always_faster_than_hash_join() {
        // 単純なルール(索引があればIndex Nested Loop Joinを最優先する、第25章の
        // `physical_plan::optimize`)が、常に正しい選択とは限らないことを
        // 実測で確認する。この章(第28章)の`optimize`はコストベースで選ぶため、
        // `with_index`側が実際にどちらを選ぶかはデータの分布に応じて変わる
        // (`measure_join_once`が選ばれたアルゴリズム名を記録する)。
        //
        // **選択的な結合(selective)**: `customer_id`を`customers`の総数より
        // ずっと広い範囲に散らし、一致する行がごく一部にとどまるようにする。
        // Hash Joinは一致するかどうかによらず`orders`の全`m`行をBuildする一方、
        // Index Nested Loop Joinは`customers`の`n`行ぶんの`lookup`しか行わない。
        // ここではIndex Nested Loop Joinが優位に立つ。
        //
        // **密な結合(dense)**: `customer_id`を`customers`の総数`n`だけに
        // 絞り、`orders`のほぼ全行がどれかの`customer`と一致するようにする。
        // Index Nested Loop Joinは一致した行1件ごとに`Storage::get`で
        // Heapページを個別に読みに行く(`crate::buffer_pool::BufferPool`の
        // 固定容量、第14章)のに対し、Hash JoinのBuildは`Storage::scan`で
        // `orders`を先頭から順に1回だけ読む。一致件数が多いこの場合は、
        // ランダムアクセスの積み重ねがBufferPoolの置き換えを増やし、
        // Index Nested Loop Joinのほうが遅くなる。
        //
        // 「索引があるかどうか」という構文的な性質だけでは、この2つを
        // 区別できない。実際にどちらが速いかはデータの分布(選択性)に
        // 依存しており、それを知るには統計情報とコストモデルが要る
        // (第28章)。
        for m in [2_000usize, 8_000, 32_000] {
            measure_join_once(m, true, "selective");
        }
        for m in [2_000usize, 8_000, 32_000] {
            measure_join_once(m, false, "dense");
        }
    }

    // ------------------------------------------------------------------
    // ANALYZE / EXPLAIN ANALYZE(第27章)
    // ------------------------------------------------------------------

    #[test]
    fn analyze_changes_the_estimated_rows_shown_by_explain() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')").unwrap();

        // ANALYZE前は、統計を持たないテーブルのデフォルト値(1000)が使われる。
        let before = explain_lines(&mut db, "EXPLAIN SELECT id FROM users");
        assert_eq!(before, vec!["Projection(id) rows=1000 cost=40.00", "  └─ SeqScan(users) rows=1000 cost=30.00"]);

        db.execute("ANALYZE users").unwrap();

        // ANALYZE後は、実測した行数(3)が使われる。
        let after = explain_lines(&mut db, "EXPLAIN SELECT id FROM users");
        assert_eq!(after, vec!["Projection(id) rows=3 cost=1.06", "  └─ SeqScan(users) rows=3 cost=1.03"]);
    }

    #[test]
    fn analyze_without_a_table_name_analyzes_every_registered_table() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE a (v BIGINT)").unwrap();
        db.execute("CREATE TABLE b (v BIGINT)").unwrap();
        db.execute("INSERT INTO a VALUES (1), (2)").unwrap();
        db.execute("INSERT INTO b VALUES (1), (2), (3), (4), (5)").unwrap();

        let result = db.execute("ANALYZE").unwrap();
        assert_eq!(result.to_string(), "ANALYZE 2");

        assert_eq!(
            explain_lines(&mut db, "EXPLAIN SELECT v FROM a"),
            vec!["Projection(v) rows=2 cost=1.04", "  └─ SeqScan(a) rows=2 cost=1.02"]
        );
        assert_eq!(
            explain_lines(&mut db, "EXPLAIN SELECT v FROM b"),
            vec!["Projection(v) rows=5 cost=1.10", "  └─ SeqScan(b) rows=5 cost=1.05"]
        );
    }

    #[test]
    fn analyze_rejects_an_unknown_table_name() {
        let mut db = users_db();
        let result = db.execute("ANALYZE does_not_exist");
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn explain_analyze_select_shows_actual_row_counts_alongside_estimates() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')").unwrap();
        db.execute("ANALYZE users").unwrap();

        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT id FROM users WHERE id = 2");
        // rows=は推定値(ANALYZE済みだがHistogramの範囲外に近い等値述語なので
        // 概算になる)。actual=は実測値(1行だけ一致する)。
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("Projection(id) rows="));
        assert!(lines[0].ends_with("actual=1"), "line={}", lines[0]);
        assert!(lines[1].trim_start().starts_with("└─ Filter(id = 2) rows="));
        assert!(lines[1].ends_with("actual=1"), "line={}", lines[1]);
        assert!(lines[2].trim_start().starts_with("└─ SeqScan(users) rows="));
        assert!(lines[2].ends_with("actual=3"), "line={}", lines[2]);
    }

    #[test]
    fn explain_without_analyze_never_shows_actual() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        let lines = explain_lines(&mut db, "EXPLAIN SELECT id FROM users");
        assert!(lines.iter().all(|line| !line.contains("actual=")));
    }

    #[test]
    fn explain_analyze_insert_shows_actual_only_on_the_root_line() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')");
        assert_eq!(lines, vec!["Insert(users) rows=2 cost=0.00 actual=2", "  └─ Values(2 rows) rows=2 cost=0.00"]);

        // EXPLAIN ANALYZE INSERTは実際に書き込みを行う(本文で明記する制約)。
        let result = db.execute("SELECT id FROM users").unwrap();
        assert_eq!(result.rows().len(), 2);
    }

    #[test]
    fn explain_analyze_update_shows_the_actual_affected_row_count() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')").unwrap();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE UPDATE users SET name = 'x' WHERE id <= 2");
        assert_eq!(lines[0], "Update(users) rows=1000 cost=30.00 actual=2");

        let result = db.execute("SELECT name FROM users WHERE id <= 2").unwrap();
        for row in result.rows() {
            assert_eq!(row.values()[0], Value::Text("x".to_string()));
        }
    }

    #[test]
    fn explain_analyze_delete_shows_the_actual_affected_row_count() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')").unwrap();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE DELETE FROM users WHERE id <= 2");
        assert_eq!(lines[0], "Delete(users) rows=1000 cost=30.00 actual=2");

        let result = db.execute("SELECT id FROM users").unwrap();
        assert_eq!(result.rows().len(), 1);
    }

    // ------------------------------------------------------------------
    // NULLを含む列の選択率推定(第4部レビュー対応、codexの再現ケース)
    // ------------------------------------------------------------------

    /// 100行のうち90行が`v IS NULL`、残り10行が`v`=0〜9(各1回)という、
    /// codexレビューが指摘した再現ケースと同じ分布のテーブルを作る。
    fn null_heavy_table() -> Database {
        let mut db = Database::memory();
        db.execute("CREATE TABLE t (v BIGINT)").unwrap();
        let mut rows: Vec<String> = (0..10).map(|v| format!("({v})")).collect();
        rows.extend(std::iter::repeat_n("(NULL)".to_string(), 90));
        db.execute(&format!("INSERT INTO t VALUES {}", rows.join(", "))).unwrap();
        db.execute("ANALYZE t").unwrap();
        db
    }

    /// `line`(`explain_lines`の1要素)から`rows=`・`actual=`の数値を取り出す。
    fn parse_rows_and_actual(line: &str) -> (u64, u64) {
        let rows = line
            .split("rows=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("rows=を読めません: {line}"));
        let actual = line
            .split("actual=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("actual=を読めません: {line}"));
        (rows, actual)
    }

    #[test]
    fn mcv_fixed_point_extraction_catches_a_mid_frequency_value_after_the_dominant_one() {
        // codexレビュー2巡目の再現ケース: 100行が`v`=0(50行)、1(8行)、
        // 2〜43(各1行)という分布。抽出前の平均バケツ行数(10行)だけを見ると
        // `1`(8行)はMCVに入らず、残余のequi-depth Histogramで単一値バケツへ
        // 分割される。`0`を抽出したあとの残り50行に対する平均バケツ行数
        // (5行)を再計算する固定点方式であれば、`1`もMCVへ移り、
        // `v = 1`の見積もりは実測と一致するはず。
        let mut db = Database::memory();
        db.execute("CREATE TABLE t (v BIGINT)").unwrap();
        let mut rows: Vec<String> = std::iter::repeat_n("(0)".to_string(), 50).collect();
        rows.extend(std::iter::repeat_n("(1)".to_string(), 8));
        rows.extend((2..44).map(|v| format!("({v})")));
        db.execute(&format!("INSERT INTO t VALUES {}", rows.join(", "))).unwrap();
        db.execute("ANALYZE t").unwrap();

        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT v FROM t WHERE v = 1");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (8, 8), "line={}", lines[0]);
    }

    #[test]
    fn histogram_does_not_undercount_a_value_split_between_a_singleton_and_a_mixed_bucket() {
        // codexレビュー3巡目の再現ケース: 出現回数[15,14,12,11,10,9,8,7,7,6,6]
        // (値0〜10)+一意値42件(合計147行)というテーブルをANALYZEする。
        // MCV_MAX_ENTRIES(10)件の上限により、0〜9(10個)はMCVへ移るが、
        // 11個目の値`10`(出現回数6)は残余のequi-depth Histogramに残る。
        // `10`をすべての一意値より小さくしてあるため、ソート順で`10`の
        // 6行はまとまって先頭に並ぶ。`build_equi_depth_histogram`が同値の
        // 連続runをバケツ境界で分割しない実装であれば、この6行は1個の
        // 単一値バケツに収まり、`v = 10`の見積もりは実測と一致するはず。
        let mut db = Database::memory();
        db.execute("CREATE TABLE t (v BIGINT)").unwrap();
        let mut rows: Vec<String> = Vec::new();
        for (v, count) in [15, 14, 12, 11, 10, 9, 8, 7, 7, 6, 6].into_iter().enumerate() {
            rows.extend(std::iter::repeat_n(format!("({v})"), count));
        }
        rows.extend((1000..1042).map(|v| format!("({v})")));
        assert_eq!(rows.len(), 147);
        db.execute(&format!("INSERT INTO t VALUES {}", rows.join(", "))).unwrap();
        db.execute("ANALYZE t").unwrap();

        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT v FROM t WHERE v = 10");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (6, 6), "line={}", lines[0]);
    }

    #[test]
    fn a_histogram_with_unequal_bucket_sizes_survives_validate_stats_metadata_and_a_reopen() {
        // 同値の連続runをバケツ境界で分割しない実装(第4部3巡目レビュー対応)は、
        // バケツの行数がもう均等ではないことを意味する。`Storage::set_table_stats`
        // (`validate_one_table_stats`、`Storage::open`時の`validate_stats_metadata`)
        // がこの不均等なバケツ行数を「壊れた統計」と誤検知しないことを、
        // `Backend::Disk`での`ANALYZE`と再オープンの両方で確認する。
        let path = temp_db_path("histogram-unequal-buckets-disk");
        {
            let mut db = Database::open(&path).unwrap();
            db.execute("CREATE TABLE t (v BIGINT)").unwrap();
            let mut rows: Vec<String> = Vec::new();
            for (v, count) in [15, 14, 12, 11, 10, 9, 8, 7, 7, 6, 6].into_iter().enumerate() {
                rows.extend(std::iter::repeat_n(format!("({v})"), count));
            }
            rows.extend((1000..1042).map(|v| format!("({v})")));
            db.execute(&format!("INSERT INTO t VALUES {}", rows.join(", "))).unwrap();
            db.execute("ANALYZE t").unwrap();
            db.flush().unwrap();
        }

        let mut reopened = Database::open(&path).unwrap();
        let lines = explain_lines(&mut reopened, "EXPLAIN ANALYZE SELECT v FROM t WHERE v = 10");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (6, 6), "line={}", lines[0]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn null_aware_equality_matches_the_actual_row_count() {
        // v=0: 非NULL率(10/100)×非NULL内での一致割合(1/10)=0.01→1行。
        let mut db = null_heavy_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT v FROM t WHERE v = 0");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (1, 1), "line={}", lines[0]);
    }

    #[test]
    fn null_aware_not_equal_excludes_unknown_rows_from_the_complement() {
        // v<>0: 90行のNULLは比較がUNKNOWNになり、TRUEとしては数えない。
        // 非NULL率(0.1)からv=0の選択率(0.01)を引いた0.09→9行。
        let mut db = null_heavy_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT v FROM t WHERE v <> 0");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (9, 9), "line={}", lines[0]);
    }

    #[test]
    fn null_aware_not_matches_the_not_equal_selectivity() {
        // NOT(v = 0)は<>と同じ「UNKNOWNを除外した補数」で見積もるため、
        // v<>0と同じrows=9になるはず。
        let mut db = null_heavy_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT v FROM t WHERE NOT (v = 0)");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (9, 9), "line={}", lines[0]);
    }

    #[test]
    fn is_null_selectivity_matches_the_observed_null_fraction() {
        // v IS NULL: null_count(90)/row_count(100)=0.9→90行。
        let mut db = null_heavy_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT v FROM t WHERE v IS NULL");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (90, 90), "line={}", lines[0]);
    }

    #[test]
    fn is_not_null_selectivity_matches_the_observed_non_null_fraction() {
        // v IS NOT NULL: 1 - 0.9 = 0.1→10行。
        let mut db = null_heavy_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT v FROM t WHERE v IS NOT NULL");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (10, 10), "line={}", lines[0]);
    }

    #[test]
    fn equality_against_a_null_literal_never_matches() {
        // v = NULLはSQLの3値論理で常にUNKNOWNになり、決してTRUEにならない。
        let mut db = null_heavy_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT v FROM t WHERE v = NULL");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (0, 0), "line={}", lines[0]);
    }

    /// `a`が常に`0`(`a = 1`は常にFALSE、`a = 0`は常にTRUE)、`b`が常に`NULL`
    /// (`b = 1`は常にUNKNOWN)という100行のテーブル。3値論理の確定規則
    /// (`FALSE AND UNKNOWN`は`FALSE`、`TRUE OR UNKNOWN`は`TRUE`など)を
    /// `AND`・`OR`・`NOT`のそれぞれで確かめる(第4部2巡目レビュー対応、
    /// codexの再現: `NOT (a = 1 AND b = 1)`)。
    fn three_valued_logic_table() -> Database {
        let mut db = Database::memory();
        db.execute("CREATE TABLE tri (a BIGINT, b BIGINT)").unwrap();
        let rows: Vec<String> = std::iter::repeat_n("(0, NULL)".to_string(), 100).collect();
        db.execute(&format!("INSERT INTO tri VALUES {}", rows.join(", "))).unwrap();
        db.execute("ANALYZE tri").unwrap();
        db
    }

    #[test]
    fn and_of_false_and_unknown_is_false() {
        // a = 1(常にFALSE) AND b = 1(常にUNKNOWN) は常にFALSE。
        let mut db = three_valued_logic_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT a FROM tri WHERE a = 1 AND b = 1");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (0, 0), "line={}", lines[0]);
    }

    #[test]
    fn and_of_true_and_unknown_is_unknown() {
        // a = 0(常にTRUE) AND b = 1(常にUNKNOWN) は常にUNKNOWN
        // (WHEREはUNKNOWNの行をFALSEと同じく落とす)。
        let mut db = three_valued_logic_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT a FROM tri WHERE a = 0 AND b = 1");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (0, 0), "line={}", lines[0]);
    }

    #[test]
    fn or_of_true_and_unknown_is_true() {
        // a = 0(常にTRUE) OR b = 1(常にUNKNOWN) は常にTRUE。
        let mut db = three_valued_logic_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT a FROM tri WHERE a = 0 OR b = 1");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (100, 100), "line={}", lines[0]);
    }

    #[test]
    fn or_of_false_and_unknown_is_unknown() {
        // a = 1(常にFALSE) OR b = 1(常にUNKNOWN) は常にUNKNOWN。
        let mut db = three_valued_logic_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT a FROM tri WHERE a = 1 OR b = 1");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (0, 0), "line={}", lines[0]);
    }

    #[test]
    fn not_of_false_and_unknown_is_true() {
        // codexレビュー2巡目の再現そのもの: NOT(FALSE AND UNKNOWN) = NOT(FALSE) = TRUE。
        // 第1巡目の`known_fraction`方式は、この内側のANDをUNKNOWNに近いものと
        // 誤認し、NOT後の見積もりを実際より小さく(rows=0)していた。
        let mut db = three_valued_logic_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT a FROM tri WHERE NOT (a = 1 AND b = 1)");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (100, 100), "line={}", lines[0]);
    }

    #[test]
    fn not_of_true_and_unknown_is_unknown() {
        // NOT(TRUE AND UNKNOWN) = NOT(UNKNOWN) = UNKNOWN。
        let mut db = three_valued_logic_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT a FROM tri WHERE NOT (a = 0 AND b = 1)");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (0, 0), "line={}", lines[0]);
    }

    #[test]
    fn not_of_true_or_unknown_is_false() {
        // NOT(TRUE OR UNKNOWN) = NOT(TRUE) = FALSE。
        let mut db = three_valued_logic_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT a FROM tri WHERE NOT (a = 0 OR b = 1)");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (0, 0), "line={}", lines[0]);
    }

    #[test]
    fn not_of_false_or_unknown_is_unknown() {
        // NOT(FALSE OR UNKNOWN) = NOT(UNKNOWN) = UNKNOWN。
        let mut db = three_valued_logic_table();
        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT a FROM tri WHERE NOT (a = 1 OR b = 1)");
        let (rows, actual) = parse_rows_and_actual(&lines[0]);
        assert_eq!((rows, actual), (0, 0), "line={}", lines[0]);
    }

    #[test]
    fn join_cardinality_excludes_null_keys_from_both_sides() {
        // a.v = b.vの結合キーにNULLの行を含めると、NULL同士は等号で
        // 一致しないにもかかわらず結合行数を過大評価してしまう。両側の
        // 非NULL行数(10ずつ)から見積もれば、実測(10行、各値が1対1で一致)
        // と一致する。
        let mut db = Database::memory();
        db.execute("CREATE TABLE a (v BIGINT)").unwrap();
        db.execute("CREATE TABLE b (v BIGINT)").unwrap();
        let mut rows: Vec<String> = (0..10).map(|v| format!("({v})")).collect();
        rows.extend(std::iter::repeat_n("(NULL)".to_string(), 90));
        db.execute(&format!("INSERT INTO a VALUES {}", rows.join(", "))).unwrap();
        db.execute(&format!("INSERT INTO b VALUES {}", rows.join(", "))).unwrap();
        db.execute("ANALYZE a").unwrap();
        db.execute("ANALYZE b").unwrap();

        let lines = explain_lines(&mut db, "EXPLAIN ANALYZE SELECT a.v FROM a JOIN b ON a.v = b.v");
        let join_line = lines.iter().find(|line| line.contains("Join")).expect("Join行が見つかりません");
        let (rows, actual) = parse_rows_and_actual(join_line);
        assert_eq!((rows, actual), (10, 10), "line={join_line}");
    }

    #[test]
    fn range_selectivity_does_not_overflow_when_the_bucket_spans_i64_min() {
        // i64::MINを含む極値のDistinct値を挿入し、線形補間の差分計算が
        // i64のままoverflowしないことを確認する(第4部2巡目レビュー対応、
        // codexの再現: `attempt to subtract with overflow`)。
        let mut db = Database::memory();
        db.execute("CREATE TABLE extremes (v BIGINT)").unwrap();
        let mut values: Vec<String> = vec![format!("({})", i64::MIN)];
        values.extend((0..10).map(|v| format!("({v})")));
        db.execute(&format!("INSERT INTO extremes VALUES {}", values.join(", "))).unwrap();
        db.execute("ANALYZE extremes").unwrap();

        let lines = explain_lines(&mut db, "EXPLAIN SELECT v FROM extremes WHERE v < -1");
        assert!(lines[0].starts_with("Projection(v) rows="), "line={}", lines[0]);

        // 実行結果もoverflowせず、実際に一致する1行(i64::MINのみ)を返す。
        let result = db.execute("SELECT v FROM extremes WHERE v < -1").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values()[0], Value::BigInt(i64::MIN));
    }

    #[test]
    fn analyze_stats_survive_a_reopen_of_the_disk_backend() {
        let path = temp_db_path("analyze-persists-across-reopen");
        {
            let mut db = Database::open(&path).unwrap();
            db.execute("CREATE TABLE orders (id BIGINT NOT NULL, amount BIGINT)").unwrap();
            db.execute("INSERT INTO orders VALUES (1, 100), (2, 200), (3, 300), (4, 400)").unwrap();
            db.execute("ANALYZE orders").unwrap();
            db.flush().unwrap();

            // 開いたままでも、統計はすでに実測値(4行)を反映している。
            let lines = explain_lines(&mut db, "EXPLAIN SELECT id FROM orders");
            assert_eq!(lines, vec!["Projection(id) rows=4 cost=1.08", "  └─ SeqScan(orders) rows=4 cost=1.04"]);
        }

        // ファイルを閉じて(スコープを抜けて`Storage`を破棄して)再度開く。
        let mut reopened = Database::open(&path).unwrap();
        let lines = explain_lines(&mut reopened, "EXPLAIN SELECT id FROM orders");
        assert_eq!(lines, vec!["Projection(id) rows=4 cost=1.08", "  └─ SeqScan(orders) rows=4 cost=1.04"]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dropping_a_table_also_drops_its_statistics_on_disk() {
        let path = temp_db_path("analyze-drop-table-clears-stats");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (v BIGINT)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2)").unwrap();
        db.execute("ANALYZE t").unwrap();
        db.execute("DROP TABLE t").unwrap();
        db.execute("CREATE TABLE t (v BIGINT)").unwrap();

        // 同名で作り直した新しいテーブルは、前のテーブルの統計を引き継がない。
        let lines = explain_lines(&mut db, "EXPLAIN SELECT v FROM t");
        assert_eq!(lines, vec!["Projection(v) rows=1000 cost=40.00", "  └─ SeqScan(t) rows=1000 cost=30.00"]);

        let _ = std::fs::remove_file(&path);
    }

    // ==================================================================
    // 第29章: Join OrderとPhysical Properties
    // ==================================================================

    /// `customers`(5行、選択的な外部キー)・`orders`(2,000行、ハブ)・
    /// `shipments`(2,000行、`country`という低NDV列だけを共有する粗い結合)の
    /// 3テーブル。`FROM`には`shipments`を先に書く(構文順どおりに結合すると
    /// `orders`・`shipments`という低NDVどうしの結合を先に行うことになり、
    /// 中間結果が大きく膨らむ)。
    fn join_order_fixture() -> Database {
        let mut db = Database::memory();
        db.execute("CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)").unwrap();
        db.execute("CREATE TABLE orders (id BIGINT NOT NULL, customer_id BIGINT, country BIGINT)").unwrap();
        db.execute("CREATE TABLE shipments (id BIGINT NOT NULL, country BIGINT)").unwrap();

        let customers: Vec<String> = (0..5).map(|i| format!("({i}, 'name{i}')")).collect();
        db.execute(&format!("INSERT INTO customers VALUES {}", customers.join(", "))).unwrap();
        // customer_idは0..399へ広く散らし、customersの5件とだけ選択的に一致
        // させる(NDV(customer_id)=400)。countryは0..9の低カーディナリティ
        // (NDV(country)=10)。
        let orders: Vec<String> = (0..2000).map(|i| format!("({i}, {}, {})", i % 400, i % 10)).collect();
        db.execute(&format!("INSERT INTO orders VALUES {}", orders.join(", "))).unwrap();
        let shipments: Vec<String> = (0..2000).map(|i| format!("({i}, {})", i % 10)).collect();
        db.execute(&format!("INSERT INTO shipments VALUES {}", shipments.join(", "))).unwrap();

        db.execute("ANALYZE customers").unwrap();
        db.execute("ANALYZE orders").unwrap();
        db.execute("ANALYZE shipments").unwrap();
        db
    }

    /// `sql`(`SELECT`文)を束縛・ルールベース最適化まで通した`LogicalPlan`を返す。
    fn bind_and_optimize_logically(db: &Database, sql: &str) -> LogicalPlan {
        let select = match Binder::new(db.catalog(), &db.functions, sql).bind(parser::parse_statement(sql).unwrap()).unwrap() {
            BoundStatement::Select(select) => *select,
            other => panic!("Selectのはず: {other:?}"),
        };
        rules::optimize(logical_plan::build_select(select), &db.functions)
    }

    #[test]
    fn join_order_dp_reorders_a_three_table_chain_away_from_syntax_order() {
        let mut db = join_order_fixture();
        let sql = "SELECT customers.name, orders.id FROM shipments JOIN orders ON shipments.country = orders.country JOIN customers ON orders.customer_id = customers.id";

        let plan = db.execute(&format!("EXPLAIN {sql}")).unwrap().to_string();

        // 構文順(shipments→orders→customers)のままなら、木の一番内側
        // (根から最も遠い葉)がshipmentsになるはずである。DPが選んだ計画は
        // それと逆に、選択的なcustomersの結合を先に(内側に)済ませ、低NDV
        // どうしのshipmentsとの結合を最後(根に一番近い側)に回す。`EXPLAIN`の
        // インデント付きツリーでは、根に近い行ほど先(文字列中で手前)に
        // 現れるので、customersが先、shipmentsが後という並びを確認する。
        assert!(plan.contains("HashJoin"), "plan={plan}");
        let customers_pos = plan.find("SeqScan(customers)").expect("SeqScan(customers)があるはず");
        let shipments_pos = plan.find("SeqScan(shipments)").expect("SeqScan(shipments)があるはず");
        assert!(
            customers_pos < shipments_pos,
            "customersが先(内側)、shipmentsが後(根に近い側)に結合されるはず: plan={plan}"
        );

        // コストが実際に構文順より安いことも、cost_model越しに直接確認する
        // (`join_order`単体テストと同じ比較を、実際のANALYZE統計で行う)。
        let logical = bind_and_optimize_logically(&db, sql);
        let mut leaves = Vec::new();
        let mut conditions = Vec::new();
        let join_root = match &logical {
            LogicalPlan::Projection(p) => p.input.as_ref().clone(),
            other => other.clone(),
        };
        physical_plan::flatten_join_chain(join_root, &mut leaves, &mut conditions);
        let leaf_plans: Vec<PhysicalPlan> =
            leaves.into_iter().map(|leaf| physical_plan::optimize(leaf, db.index_storage(), &db)).collect();
        let syntactic = join_order::combine_in_syntactic_order(leaf_plans, conditions, db.index_storage(), &db);
        let syntactic_cost = cost_model::plan_cost(&syntactic, &db, db.index_storage()).value();
        let chosen = physical_plan::optimize(logical, db.index_storage(), &db);
        let chosen_cost = cost_model::plan_cost(&chosen, &db, db.index_storage()).value();
        assert!(chosen_cost < syntactic_cost, "chosen={chosen_cost} syntactic={syntactic_cost}");
    }

    #[test]
    fn join_order_reordering_and_syntax_order_return_the_same_rows() {
        let mut db = join_order_fixture();
        let sql = "SELECT customers.name, orders.id, shipments.id FROM shipments JOIN orders ON shipments.country = orders.country JOIN customers ON orders.customer_id = customers.id ORDER BY customers.name, orders.id, shipments.id";
        let reordered = db.execute(sql).unwrap();

        let sql_syntactic_by_construction = "SELECT customers.name, orders.id, shipments.id FROM customers JOIN orders ON customers.id = orders.customer_id JOIN shipments ON orders.country = shipments.country ORDER BY customers.name, orders.id, shipments.id";
        let same_set = db.execute(sql_syntactic_by_construction).unwrap();
        assert_eq!(reordered.rows(), same_set.rows());
        assert!(!reordered.rows().is_empty(), "テストの前提として一致する行が無ければ意味が無い");
    }

    #[test]
    fn cartesian_product_appears_only_when_no_on_condition_connects_a_table() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE a (id BIGINT NOT NULL)").unwrap();
        db.execute("CREATE TABLE b (id BIGINT NOT NULL)").unwrap();
        db.execute("CREATE TABLE c (id BIGINT NOT NULL)").unwrap();
        db.execute("INSERT INTO a VALUES (1), (2)").unwrap();
        db.execute("INSERT INTO b VALUES (1), (2)").unwrap();
        db.execute("INSERT INTO c VALUES (1), (2)").unwrap();
        db.execute("ANALYZE a").unwrap();
        db.execute("ANALYZE b").unwrap();
        db.execute("ANALYZE c").unwrap();

        // bとcの間には結合条件が無い(`ON true`)。DPは連結できる拡張が無い
        // ときに限りCartesian Productを許す(NestedLoopJoin、モジュール
        // ドキュメント参照)。
        let plan = db.execute("EXPLAIN SELECT a.id FROM a JOIN b ON a.id = b.id JOIN c ON true").unwrap().to_string();
        assert!(plan.contains("NestedLoopJoin"), "plan={plan}");

        let result = db.execute("SELECT a.id FROM a JOIN b ON a.id = b.id JOIN c ON true").unwrap();
        // aとbは2行ずつ一致し(id同士)、cは無条件に2行とも掛かるので、
        // 2 (a=b一致) × 2 (c) = 4行になる。
        assert_eq!(result.rows().len(), 4, "rows={:?}", result.rows());
    }

    // ---- トランザクション境界とAtomicity(第30章) ----

    fn accounts_db() -> Database {
        let mut db = Database::memory();
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
        db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();
        db
    }

    fn balance(db: &mut Database, id: i64) -> i64 {
        let result = db.execute(&format!("SELECT balance FROM accounts WHERE id = {id}")).unwrap();
        match &result.rows()[0].values()[0] {
            Value::BigInt(n) => *n,
            other => panic!("BigIntを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn begin_commit_keeps_the_changes() {
        let mut db = accounts_db();
        db.execute("BEGIN").unwrap();
        db.execute("UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
        db.execute("UPDATE accounts SET balance = balance + 30 WHERE id = 2").unwrap();
        db.execute("COMMIT").unwrap();

        assert_eq!(balance(&mut db, 1), 70);
        assert_eq!(balance(&mut db, 2), 80);
        assert_eq!(db.transaction_state(), None);
    }

    /// `BEGIN`単体(`ISOLATION LEVEL`を省略)は`RepeatableRead`を既定にする
    /// (第32章、`execute_begin`のドキュメント「分離レベルの既定値」を参照)。
    /// 分離レベル自体は`Database`の外から直接観測できないため、`RepeatableRead`
    /// の規律(Sharedロックも`COMMIT`まで保持する)が働いていることを、
    /// Non-repeatable Readが起きないことで間接的に確認する。
    #[test]
    fn begin_without_isolation_level_defaults_to_repeatable_read() {
        let mut db = accounts_db();
        db.execute("BEGIN").unwrap();
        assert_eq!(balance(&mut db, 1), 100);

        let t2 = db.begin_tx();
        assert!(matches!(
            db.execute_in_tx(&t2, "UPDATE accounts SET balance = 999 WHERE id = 1"),
            Err(DbError::WouldBlock)
        ));
        db.execute("COMMIT").unwrap();
    }

    /// `BEGIN ISOLATION LEVEL READ UNCOMMITTED`をSQL経由で発行すると、
    /// 読み取りロックを一切取らなくなる(第32章)。`db.execute`の通常のSQL経路
    /// (`self.tx`)でも、ハーネス(`begin_tx_with_isolation`)と同じ分離レベルの
    /// 規律が働くことを確認する。
    #[test]
    fn begin_isolation_level_read_uncommitted_over_sql_allows_dirty_read() {
        let mut db = accounts_db();
        let t1 = db.begin_tx();
        db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

        db.execute("BEGIN ISOLATION LEVEL READ UNCOMMITTED").unwrap();
        assert_eq!(balance(&mut db, 1), 70, "READ UNCOMMITTEDは未コミットの値を読める(Dirty Read)");
        db.execute("COMMIT").unwrap();

        db.rollback_tx(t1).unwrap();
    }

    #[test]
    fn begin_rollback_discards_the_changes() {
        let mut db = accounts_db();
        db.execute("BEGIN").unwrap();
        db.execute("UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
        db.execute("UPDATE accounts SET balance = balance + 30 WHERE id = 2").unwrap();
        db.execute("ROLLBACK").unwrap();

        assert_eq!(balance(&mut db, 1), 100);
        assert_eq!(balance(&mut db, 2), 50);
        assert_eq!(db.transaction_state(), None);
    }

    #[test]
    fn begin_rollback_undoes_an_insert_and_a_delete_in_the_same_transaction() {
        let mut db = accounts_db();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO accounts VALUES (3, 10)").unwrap();
        db.execute("DELETE FROM accounts WHERE id = 2").unwrap();
        db.execute("ROLLBACK").unwrap();

        let rows = db.execute("SELECT id FROM accounts ORDER BY id").unwrap();
        let ids: Vec<i64> = rows
            .rows()
            .iter()
            .map(|row| match &row.values()[0] {
                Value::BigInt(n) => *n,
                other => panic!("BigIntを期待したが{other:?}が返った"),
            })
            .collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn rollback_undoes_repeated_updates_to_the_same_row_in_reverse_order() {
        // 同一行に対する複数回のUPDATEを、ROLLBACKが正しく逆順に取り消せるかを
        // 確認する。UndoRecordは1回のUPDATEごとに1件積まれるので、3回の更新は
        // 3件のUndoRecordになり、それをLIFOで戻すと元の値に一致するはずである。
        let mut db = accounts_db();
        db.execute("BEGIN").unwrap();
        db.execute("UPDATE accounts SET balance = 200 WHERE id = 1").unwrap();
        db.execute("UPDATE accounts SET balance = 300 WHERE id = 1").unwrap();
        db.execute("UPDATE accounts SET balance = 400 WHERE id = 1").unwrap();
        assert_eq!(balance(&mut db, 1), 400);
        db.execute("ROLLBACK").unwrap();

        assert_eq!(balance(&mut db, 1), 100);
    }

    #[test]
    fn rollback_undoes_repeated_updates_to_the_same_row_on_disk() {
        // Memory版と同じ検証をDiskバックエンドで行う。`Storage::update`は
        // ページ内に収まらない書き換えで`RecordId`を動かすことがあるため、
        // `apply_undo_disk`のRecordId付け替え(remap)が正しく働くことも
        // あわせて確認する。
        let path = temp_db_path("rollback-repeated-update");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
        db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("UPDATE accounts SET balance = 200 WHERE id = 1").unwrap();
        db.execute("UPDATE accounts SET balance = 300 WHERE id = 1").unwrap();
        db.execute("UPDATE accounts SET balance = 400 WHERE id = 1").unwrap();
        db.execute("ROLLBACK").unwrap();

        assert_eq!(balance(&mut db, 1), 100);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn autocommit_persists_each_statement_immediately() {
        let mut db = accounts_db();
        // BEGINを経由しない、これまでどおりの1文ずつの実行(Autocommit)。
        db.execute("UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
        assert_eq!(balance(&mut db, 1), 70);
        assert_eq!(db.transaction_state(), None);
    }

    #[test]
    fn nested_begin_is_rejected_and_the_outer_transaction_is_unaffected() {
        let mut db = accounts_db();
        db.execute("BEGIN").unwrap();
        db.execute("UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
        assert!(matches!(db.execute("BEGIN"), Err(DbError::TransactionAlreadyActive)));

        // 入れ子のBEGIN自体の失敗は、進行中のトランザクションを巻き込まない。
        assert_eq!(db.transaction_state(), Some(TransactionState::Active));
        db.execute("COMMIT").unwrap();
        assert_eq!(balance(&mut db, 1), 70);
    }

    #[test]
    fn commit_or_rollback_without_begin_is_rejected() {
        let mut db = accounts_db();
        assert!(matches!(db.execute("COMMIT"), Err(DbError::NoActiveTransaction)));
        assert!(matches!(db.execute("ROLLBACK"), Err(DbError::NoActiveTransaction)));
    }

    #[test]
    fn a_failing_statement_aborts_the_transaction_and_blocks_further_statements() {
        let mut db = accounts_db();
        db.execute("BEGIN").unwrap();
        db.execute("UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();

        // id=2は既存のPRIMARY KEYと衝突するので失敗する。
        assert!(matches!(
            db.execute("INSERT INTO accounts VALUES (2, 999)"),
            Err(DbError::PrimaryKeyViolation { .. })
        ));
        assert_eq!(db.transaction_state(), Some(TransactionState::Aborted));

        // Aborted状態では、ROLLBACK以外のすべての文が拒否される。
        assert!(matches!(db.execute("SELECT 1"), Err(DbError::TransactionAborted)));
        assert!(matches!(db.execute("COMMIT"), Err(DbError::TransactionAborted)));

        // ROLLBACKだけが受理され、Active中に成功していた1つ目のUPDATEも
        // まとめて取り消される。
        db.execute("ROLLBACK").unwrap();
        assert_eq!(balance(&mut db, 1), 100);
        assert_eq!(db.transaction_state(), None);
    }

    #[test]
    fn statement_rollback_still_applies_inside_an_active_transaction() {
        // 第20章のStatement Rollback(1文の中の部分失敗を巻き戻す)は、この章の
        // トランザクション境界と両立する。複数行のINSERTが1行だけ失敗しても、
        // その文自体は(Active中であっても)何も書き込まない。
        let mut db = accounts_db();
        db.execute("BEGIN").unwrap();
        assert!(matches!(
            db.execute("INSERT INTO accounts VALUES (3, 10), (2, 20)"),
            Err(DbError::PrimaryKeyViolation { .. })
        ));
        // 文自体は失敗したが、トランザクションはAbortedへ遷移する
        // (「Statement Error時のAbort」、本文を参照)。
        assert_eq!(db.transaction_state(), Some(TransactionState::Aborted));
        db.execute("ROLLBACK").unwrap();

        // id=3は1度も反映されていない。
        let rows = db.execute("SELECT id FROM accounts WHERE id = 3").unwrap();
        assert!(rows.rows().is_empty());
    }

    #[test]
    fn begin_commit_and_rollback_work_on_the_disk_backend_too() {
        let path = temp_db_path("tx-disk");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
        db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
        db.execute("DELETE FROM accounts WHERE id = 2").unwrap();
        db.execute("INSERT INTO accounts VALUES (3, 10)").unwrap();
        db.execute("ROLLBACK").unwrap();

        assert_eq!(balance(&mut db, 1), 100);
        let rows = db.execute("SELECT id FROM accounts ORDER BY id").unwrap();
        assert_eq!(rows.rows().len(), 2);

        std::fs::remove_file(&path).unwrap();
    }

    // ---- 決定的インターリーブテストハーネス専用の内部API(第30章) ----

    #[test]
    fn begin_tx_execute_in_tx_and_commit_tx_leave_the_change_in_place() {
        let mut db = accounts_db();
        let tx = db.begin_tx();
        db.execute_in_tx(&tx, "UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
        db.commit_tx(tx).unwrap();

        assert_eq!(balance(&mut db, 1), 70);
    }

    #[test]
    fn rollback_tx_discards_the_change() {
        let mut db = accounts_db();
        let tx = db.begin_tx();
        db.execute_in_tx(&tx, "UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
        db.rollback_tx(tx).unwrap();

        assert_eq!(balance(&mut db, 1), 100);
    }

    #[test]
    fn two_handles_can_be_interleaved_on_one_database() {
        // このAPIが「複数のトランザクションの文を受け付ける最小限の仕組み」
        // として実際に機能することの確認。t1・t2という2つのTxHandleを、
        // どちらもcommit/rollbackする前に交互に使う。
        let mut db = accounts_db();
        let t1 = db.begin_tx();
        let t2 = db.begin_tx();

        // t1・t2はそれぞれ別の行(id=1・id=2)だけを触るが、`accounts`は
        // Memoryバックエンドなのでロックの粒度はテーブル単位である(第31章、
        // `crate::database`モジュール冒頭「ロックの粒度」を参照)。t1が
        // `accounts`のExclusiveロックを持っている間、t2の`UPDATE`は行が
        // 違ってもブロックされる。
        db.execute_in_tx(&t1, "UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
        assert!(matches!(
            db.execute_in_tx(&t2, "UPDATE accounts SET balance = balance - 5 WHERE id = 2"),
            Err(DbError::WouldBlock)
        ));
        db.execute_in_tx(&t1, "UPDATE accounts SET balance = balance + 1 WHERE id = 1").unwrap();
        db.commit_tx(t1).unwrap();

        // t1がコミットしてロックを手放したので、同じ文を再試行すれば通る。
        db.execute_in_tx(&t2, "UPDATE accounts SET balance = balance - 5 WHERE id = 2").unwrap();
        db.rollback_tx(t2).unwrap();

        // t1の2つの更新はコミット済み、t2の更新は取り消し済み。
        assert_eq!(balance(&mut db, 1), 71);
        assert_eq!(balance(&mut db, 2), 50);
    }
}
