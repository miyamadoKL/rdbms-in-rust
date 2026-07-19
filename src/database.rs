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
//! 4. **物理計画**(`physical_plan::optimize`): `LogicalPlan`を、実行アルゴリズムを
//!    確定した[`crate::physical_plan::PhysicalPlan`]へ変換する。索引がまだ無いこの
//!    章では`Scan`は必ず`SeqScan`になる(第25章でIndex Scanが加わると、ここが
//!    本当の意味での選択になる)。
//! 5. **実行**: `CREATE TABLE`・`DROP TABLE`はどちらの計画も経由せず、テーブル
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

use std::path::Path;

use crate::ast::{CreateTableStatement, DropIndexStatement, DropTableStatement, Statement};
use crate::binder::{Binder, BoundCreateIndex, BoundStatement};
use crate::catalog::Catalog;
use crate::error::{DbError, DbResult};
use crate::eval::FunctionRegistry;
use crate::executor;
use crate::logical_plan::{self, DeleteNode, InsertNode, LogicalPlan, UpdateNode};
use crate::physical_plan::{
    self, DiskSeqScanExec, DistinctExec, Executor, FilterExec, HashAggregateExec, HashJoinExec, IndexNestedLoopJoinExec,
    IndexScanExec, LimitExec, MemSeqScanExec, NestedLoopJoinExec, PhysicalPlan, ProjectionExec, SortExec, ValuesExec,
};
use crate::storage::Storage;
use crate::storage_mem::MemStorage;
use crate::types::{Column, DataType, Schema, Tuple, Value};

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
    Memory { catalog: Catalog, storage: MemStorage },
    Disk { storage: Box<Storage> },
}

/// minidbのデータベース1つを表す。
///
/// Scalar Functionのレジストリと、テーブル定義・行を実際に保持する
/// [`Backend`]を持つ。
pub struct Database {
    functions: FunctionRegistry,
    backend: Backend,
}

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
            },
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

    /// SQL文字列を1本実行し、結果を返す。
    ///
    /// 構文解析(`parser::parse_statement`)→名前解決(`Binder::bind`)→計画
    /// (`logical_plan::build_*`)→実行という4段階を順に通す。構文解析の失敗
    /// (`DbError::Lex`・`DbError::Parse`)、名前解決の失敗(`DbError::Bind`)は、
    /// どちらもそのまま呼び出し元に伝わる。`LogicalPlan`への変換自体は失敗しない
    /// (`Binder`がすでに名前・型を確定させているため、`BoundStatement`から
    /// `LogicalPlan`への変換は形を組み替えるだけで、新たに検出すべき誤りが無い)。
    pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
        let statement = crate::parser::parse_statement(sql)?;
        let bound = self.bind(statement, sql)?;
        match bound {
            BoundStatement::Select(select) => self.execute_select(logical_plan::build_select(*select)),
            BoundStatement::CreateTable(create) => self.execute_create_table(&create),
            BoundStatement::DropTable(drop) => self.execute_drop_table(&drop),
            BoundStatement::CreateIndex(create) => self.execute_create_index(create),
            BoundStatement::DropIndex(drop) => self.execute_drop_index(&drop),
            BoundStatement::Insert(insert) => self.execute_insert(logical_plan::build_insert(insert)),
            BoundStatement::Update(update) => self.execute_update(logical_plan::build_update(update)),
            BoundStatement::Delete(delete) => self.execute_delete(logical_plan::build_delete(delete)),
            BoundStatement::Explain(inner) => self.execute_explain(*inner),
        }
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
            Backend::Memory { catalog, storage } => {
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
            Backend::Memory { catalog, storage } => {
                let id = catalog.drop_table(&drop.table.name)?;
                storage.drop_table(id);
            }
            Backend::Disk { storage } => {
                storage.drop_table(&drop.table.name)?;
            }
        }
        Ok(QueryResult::command("DROP TABLE"))
    }

    /// `LogicalPlan`に組み立てた`SELECT`を実行する。
    ///
    /// `logical_plan::build_select`が返す木を`physical_plan::optimize`で
    /// [`PhysicalPlan`]へ変換し、`build_query_executor`で`Box<dyn Executor>`の
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
    fn execute_select(&self, plan: LogicalPlan) -> DbResult<QueryResult> {
        let physical = physical_plan::optimize(plan, self.index_storage());
        let schema = physical.output_schema();
        let mut executor = self.build_query_executor(&physical)?;

        let mut rows = Vec::new();
        while let Some(tuple) = executor.next()? {
            rows.push(tuple);
        }
        Ok(QueryResult { schema, rows, command_tag: None })
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
    fn build_query_executor<'a>(&'a self, plan: &'a PhysicalPlan) -> DbResult<Box<dyn Executor + 'a>> {
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
                let input = self.build_query_executor(&filter.input)?;
                Ok(Box::new(FilterExec::new(input, &filter.predicate, &self.functions)))
            }
            PhysicalPlan::NestedLoopJoin(join) => {
                let left = self.build_query_executor(&join.left)?;
                let right = self.build_query_executor(&join.right)?;
                let exec = NestedLoopJoinExec::new(left, right, &join.condition, &self.functions)?;
                Ok(Box::new(exec))
            }
            PhysicalPlan::HashJoin(join) => {
                let left = self.build_query_executor(&join.left)?;
                let right = self.build_query_executor(&join.right)?;
                let exec = HashJoinExec::new(left, right, &join.keys, &self.functions)?;
                Ok(Box::new(exec))
            }
            PhysicalPlan::IndexNestedLoopJoin(join) => {
                // `IndexScan`と同じ理由で、`optimize`がこのノードを選ぶのは
                // 常に`Backend::Disk`のときだけである。
                let Backend::Disk { storage } = &self.backend else {
                    unreachable!("IndexNestedLoopJoinはBackend::Diskのときにしかoptimizeが選ばない")
                };
                let left = self.build_query_executor(&join.left)?;
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
                let input = self.build_query_executor(&aggregate.input)?;
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
                let input = self.build_query_executor(&projection.input)?;
                Ok(Box::new(ProjectionExec::new(input, &projection.projection, &self.functions)))
            }
            PhysicalPlan::Distinct(distinct) => {
                let input = self.build_query_executor(&distinct.input)?;
                Ok(Box::new(DistinctExec::new(input)))
            }
            PhysicalPlan::Sort(sort) => {
                let input = self.build_query_executor(&sort.input)?;
                Ok(Box::new(SortExec::new(input, &sort.keys, &self.functions)?))
            }
            PhysicalPlan::Limit(limit) => {
                let input = self.build_query_executor(&limit.input)?;
                Ok(Box::new(LimitExec::new(input, limit.limit, limit.offset)))
            }
            PhysicalPlan::Insert(_) | PhysicalPlan::Update(_) | PhysicalPlan::Delete(_) => {
                unreachable!("Insert/Update/DeleteはSELECTの計画に現れない(logical_plan::build_selectは作らない)")
            }
        }
    }

    /// `EXPLAIN`を実行する。対象の文を`LogicalPlan`・`PhysicalPlan`へ変換し、
    /// その木を文字列化しただけの`QueryResult`を返す(実際には何も実行しない)。
    ///
    /// `inner`は`Parser`(第19章)がすでに`SELECT`・`INSERT INTO`・`UPDATE`・
    /// `DELETE FROM`の4種類に絞っているため、`CreateTable`・`DropTable`・
    /// `CreateIndex`・`DropIndex`(第24章)・入れ子の`Explain`はここに渡ってこない。
    fn execute_explain(&self, inner: BoundStatement) -> DbResult<QueryResult> {
        let logical = match inner {
            BoundStatement::Select(select) => logical_plan::build_select(*select),
            BoundStatement::Insert(insert) => logical_plan::build_insert(insert),
            BoundStatement::Update(update) => logical_plan::build_update(update),
            BoundStatement::Delete(delete) => logical_plan::build_delete(delete),
            BoundStatement::CreateTable(_)
            | BoundStatement::DropTable(_)
            | BoundStatement::CreateIndex(_)
            | BoundStatement::DropIndex(_)
            | BoundStatement::Explain(_) => {
                unreachable!(
                    "ParserがEXPLAINの対象をSELECT・INSERT INTO・UPDATE・DELETE FROMに制限している"
                )
            }
        };
        let physical = physical_plan::optimize(logical, self.index_storage());
        Ok(QueryResult::explain(physical.to_string()))
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
    fn execute_insert(&mut self, plan: LogicalPlan) -> DbResult<QueryResult> {
        let LogicalPlan::Insert(InsertNode { table_id, schema, columns, input, .. }) = plan else {
            unreachable!("logical_plan::build_insertは常にLogicalPlan::Insertを返す")
        };
        let LogicalPlan::Values(values) = *input else {
            unreachable!("logical_plan::build_insertはInsertの子に常にValuesを積む")
        };

        let count = match &mut self.backend {
            Backend::Memory { storage, .. } => {
                let mem_table =
                    storage.table_mut(table_id).expect("catalogに登録されたテーブルはstorageにも必ず存在する");
                executor::insert(mem_table, &schema, &self.functions, columns.as_deref(), &values.rows)?
            }
            Backend::Disk { storage } => {
                executor::storage_insert(storage, table_id, &schema, &self.functions, columns.as_deref(), &values.rows)?
            }
        };
        Ok(QueryResult::command_with_count("INSERT", count))
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
    fn execute_update(&mut self, plan: LogicalPlan) -> DbResult<QueryResult> {
        let LogicalPlan::Update(UpdateNode { table_id, schema, assignments, predicate, .. }) = plan else {
            unreachable!("logical_plan::build_updateは常にLogicalPlan::Updateを返す")
        };

        let count = match &mut self.backend {
            Backend::Memory { storage, .. } => {
                let mem_table =
                    storage.table_mut(table_id).expect("catalogに登録されたテーブルはstorageにも必ず存在する");
                executor::update(mem_table, &schema, &self.functions, &assignments, predicate.as_ref())?
            }
            Backend::Disk { storage } => {
                executor::storage_update(storage, table_id, &schema, &self.functions, &assignments, predicate.as_ref())?
            }
        };
        Ok(QueryResult::command_with_count("UPDATE", count))
    }

    /// `DELETE FROM`を実行する。`executor::delete`(または`executor::storage_delete`)
    /// が、`WHERE`に一致した行の削除までを行う。テーブル名・`WHERE`の名前解決と
    /// 型検査は`Binder`の`bind_delete`が済ませている。`DeleteNode::input`を
    /// 実際にたどらない理由は`execute_update`と同じ。
    fn execute_delete(&mut self, plan: LogicalPlan) -> DbResult<QueryResult> {
        let LogicalPlan::Delete(DeleteNode { table_id, schema, predicate, .. }) = plan else {
            unreachable!("logical_plan::build_deleteは常にLogicalPlan::Deleteを返す")
        };

        let count = match &mut self.backend {
            Backend::Memory { storage, .. } => {
                let mem_table =
                    storage.table_mut(table_id).expect("catalogに登録されたテーブルはstorageにも必ず存在する");
                executor::delete(mem_table, &schema, &self.functions, predicate.as_ref())?
            }
            Backend::Disk { storage } => {
                executor::storage_delete(storage, table_id, &schema, &self.functions, predicate.as_ref())?
            }
        };
        Ok(QueryResult::command_with_count("DELETE", count))
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
pub struct QueryResult {
    schema: Schema,
    rows: Vec<Tuple>,
    command_tag: Option<String>,
}

impl QueryResult {
    /// DDL文が完了したことを表す`QueryResult`を作る。
    fn command(tag: &'static str) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DbError;

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
        assert_eq!(lines, vec!["Projection(name)", "  └─ Filter(id = 42)", "    └─ SeqScan(users)"]);
    }

    #[test]
    fn explain_select_without_where_has_no_filter_node() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN SELECT id FROM users");
        assert_eq!(lines, vec!["Projection(id)", "  └─ SeqScan(users)"]);
    }

    #[test]
    fn explain_insert_shows_insert_over_values() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN INSERT INTO users VALUES (1, 'Alice')");
        assert_eq!(lines, vec!["Insert(users)", "  └─ Values(1 row)"]);
    }

    #[test]
    fn explain_update_shows_update_over_seq_scan() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN UPDATE users SET name = 'x' WHERE id = 1");
        assert_eq!(lines, vec!["Update(users)", "  └─ SeqScan(users)"]);
    }

    #[test]
    fn explain_delete_shows_delete_over_seq_scan() {
        let mut db = users_db();
        let lines = explain_lines(&mut db, "EXPLAIN DELETE FROM users WHERE id = 1");
        assert_eq!(lines, vec!["Delete(users)", "  └─ SeqScan(users)"]);
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
        assert_eq!(
            result.to_string(),
            "QUERY PLAN\n----------\n\
             Limit(limit=5)\n  \
             └─ Sort(dept ASC)\n    \
             └─ Projection(dept, COUNT(*))\n      \
             └─ Filter(COUNT(*) > 1)\n        \
             └─ Aggregate(group_by=[dept], calls=[COUNT(*)])\n          \
             └─ Filter(amount IS NOT NULL)\n            \
             └─ SeqScan(orders)\n\
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
             Projection(name)\n  \
             └─ Sort(id ASC)\n    \
             └─ Projection(name, id)\n      \
             └─ SeqScan(t)\n\
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
        let plan = db.execute("EXPLAIN SELECT id FROM users WHERE id = 1").unwrap().to_string();
        assert!(plan.contains("users_id_idx"), "plan={plan}");

        let err = expect_error(db.execute("DROP INDEX users_id_idx"));
        assert!(matches!(err, DbError::CannotDropConstraintIndex(name) if name == "users_id_idx"));
        let err = expect_error(db.execute("DROP INDEX users_email_idx"));
        assert!(matches!(err, DbError::CannotDropConstraintIndex(name) if name == "users_email_idx"));

        // 拒否されただけで、制約自体は引き続き効いている。
        let err = expect_error(db.execute("INSERT INTO users VALUES (1, 'b@example.com', 'Bob')"));
        assert!(matches!(err, DbError::PrimaryKeyViolation { ref column, .. } if column == "id"));
        let err = expect_error(db.execute("INSERT INTO users VALUES (2, 'a@example.com', 'Carol')"));
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

    #[test]
    fn point_predicate_on_an_indexed_column_chooses_index_scan_and_absorbs_the_whole_filter() {
        let path = temp_db_path("index-scan-point");
        let mut db = orders_disk_db(&path);
        db.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
        db.execute("INSERT INTO orders VALUES (1, 100, 'Alice'), (2, 200, 'Bob')").unwrap();

        let plan = db.execute("EXPLAIN SELECT id, amount, name FROM orders WHERE id = 1").unwrap().to_string();
        assert_eq!(
            plan,
            "QUERY PLAN\n----------\nProjection(id, amount, name)\n  └─ IndexScan(idx_id, id = 1)\n(2 rows)"
        );

        let result = db.execute("SELECT id, amount, name FROM orders WHERE id = 1").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(1), Value::BigInt(100), Value::Text("Alice".to_string())]);

        remove_db_and_indexes(&path, &["idx_id"]);
    }

    #[test]
    fn point_predicate_leaves_the_rest_of_a_conjunction_in_a_residual_filter() {
        // `id`だけに索引がある。`id = 1 AND name = 'Alice'`は、`id = 1`だけが
        // IndexScanに吸収され、`name = 'Alice'`はFilterに残る。
        let path = temp_db_path("index-scan-residual-filter");
        let mut db = orders_disk_db(&path);
        db.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
        db.execute("INSERT INTO orders VALUES (1, 100, 'Alice'), (1, 150, 'Zoe')").unwrap();

        let plan = db
            .execute("EXPLAIN SELECT id, amount, name FROM orders WHERE id = 1 AND name = 'Alice'")
            .unwrap()
            .to_string();
        assert_eq!(
            plan,
            "QUERY PLAN\n----------\nProjection(id, amount, name)\n  └─ Filter(name = 'Alice')\n    └─ IndexScan(idx_id, id = 1)\n(3 rows)"
        );

        let result = db.execute("SELECT id, amount, name FROM orders WHERE id = 1 AND name = 'Alice'").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values()[2], Value::Text("Alice".to_string()));

        remove_db_and_indexes(&path, &["idx_id"]);
    }

    #[test]
    fn range_predicate_on_both_bounds_becomes_a_single_range_index_scan() {
        let path = temp_db_path("index-scan-range");
        let mut db = orders_disk_db(&path);
        db.execute("CREATE INDEX idx_amount ON orders (amount)").unwrap();
        db.execute("INSERT INTO orders VALUES (1, 50, 'a'), (2, 100, 'b'), (3, 150, 'c'), (4, 200, 'd'), (5, 250, 'e')")
            .unwrap();

        let plan = db
            .execute("EXPLAIN SELECT id FROM orders WHERE amount >= 100 AND amount <= 200")
            .unwrap()
            .to_string();
        assert_eq!(
            plan,
            "QUERY PLAN\n----------\nProjection(id)\n  └─ IndexScan(idx_amount, amount >= 100 AND amount <= 200)\n(2 rows)"
        );

        let result = db.execute("SELECT id FROM orders WHERE amount >= 100 AND amount <= 200 ORDER BY id").unwrap();
        let ids: Vec<Value> = result.rows().iter().map(|row| row.values()[0].clone()).collect();
        assert_eq!(ids, vec![Value::BigInt(2), Value::BigInt(3), Value::BigInt(4)]);

        // 片側だけの境界(`>`のみ)でも同じくRange Index Scanになる。
        let plan_lower_only = db.execute("EXPLAIN SELECT id FROM orders WHERE amount > 200").unwrap().to_string();
        assert!(plan_lower_only.contains("IndexScan(idx_amount, amount > 200)"));

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
            "QUERY PLAN\n----------\nProjection(id)\n  └─ Filter(amount = 100)\n    └─ SeqScan(orders)\n(3 rows)"
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

        let rows: Vec<String> = (0..200).map(|i| format!("({i}, {}, 'name{i}')", i * 3)).collect();
        let insert_sql = format!("INSERT INTO orders VALUES {}", rows.join(", "));

        let mut with_index = orders_disk_db(&with_index_path);
        with_index.execute("CREATE INDEX idx_id ON orders (id)").unwrap();
        with_index.execute("CREATE INDEX idx_amount ON orders (amount)").unwrap();
        with_index.execute(&insert_sql).unwrap();

        let mut without_index = orders_disk_db(&without_index_path);
        without_index.execute(&insert_sql).unwrap();

        for query in [
            "SELECT id, amount, name FROM orders WHERE id = 42",
            "SELECT id, amount, name FROM orders WHERE amount >= 100 AND amount <= 200",
            "SELECT id, amount, name FROM orders WHERE amount > 590",
            "SELECT id, amount, name FROM orders WHERE id = 999", // 一致なし
        ] {
            let with_index_plan = with_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string();
            assert!(with_index_plan.contains("IndexScan"), "索引ありDBはIndexScanを選ぶはず: {with_index_plan}");
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
        db.execute("INSERT INTO orders VALUES (1, 100, 'Alice'), (2, 200, 'Bob'), (3, 300, 'Carol')").unwrap();

        assert_eq!(db.execute("DELETE FROM orders WHERE id = 2").unwrap().to_string(), "DELETE 1");

        // 削除された`id = 2`はIndexScanでも0行(索引エントリ自体が
        // Index Maintenanceで取り除かれている、第24章)。
        let plan = db.execute("EXPLAIN SELECT id FROM orders WHERE id = 2").unwrap().to_string();
        assert!(plan.contains("IndexScan(idx_id, id = 2)"));
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
        let path = temp_db_path("index-nlj-chosen");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)").unwrap();
        db.execute("CREATE TABLE orders (id BIGINT, customer_id BIGINT, item TEXT)").unwrap();
        db.execute("CREATE INDEX idx_customer_id ON orders (customer_id)").unwrap();
        db.execute("INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')").unwrap();
        db.execute(
            "INSERT INTO orders VALUES (10, 1, 'apple'), (11, 1, 'banana'), (12, 2, 'cherry'), (13, NULL, 'orphan'), (14, 99, 'nomatch')",
        )
        .unwrap();

        let plan = db
            .execute("EXPLAIN SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id")
            .unwrap()
            .to_string();
        assert_eq!(
            plan,
            "QUERY PLAN\n----------\nProjection(customers.name, orders.item)\n  └─ IndexNestedLoopJoin(INNER JOIN, id = customer_id)\n    └─ SeqScan(customers)\n    └─ IndexScan(idx_customer_id, customer_id = id)\n(4 rows)"
        );

        let result = db
            .execute(
                "SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id ORDER BY customers.name, orders.item",
            )
            .unwrap();
        let rows: Vec<(String, String)> = result
            .rows()
            .iter()
            .map(|row| match row.values() {
                [Value::Text(name), Value::Text(item)] => (name.clone(), item.clone()),
                other => panic!("予期しない行: {other:?}"),
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("Alice".to_string(), "apple".to_string()),
                ("Alice".to_string(), "banana".to_string()),
                ("Bob".to_string(), "cherry".to_string()),
            ]
        );

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
        assert!(with_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string().contains("IndexNestedLoopJoin"));
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

        let mut without_index = Database::open(&without_index_path).unwrap();
        for statement in &setup {
            without_index.execute(statement).unwrap();
        }

        let query = "SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id";
        assert!(with_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string().contains("IndexNestedLoopJoin"));
        assert!(without_index.execute(&format!("EXPLAIN {query}")).unwrap().to_string().contains("HashJoin"));

        let start = std::time::Instant::now();
        let inlj_result = with_index.execute(query).unwrap();
        let inlj_elapsed = start.elapsed();

        let start = std::time::Instant::now();
        let hash_result = without_index.execute(query).unwrap();
        let hash_elapsed = start.elapsed();

        assert_eq!(inlj_result.rows().len(), hash_result.rows().len());
        eprintln!(
            "{label:<9} m={m:>6}  matches={:>6}  IndexNestedLoopJoin={inlj_elapsed:>10?}  HashJoin={hash_elapsed:>10?}",
            inlj_result.rows().len()
        );

        remove_db_and_indexes(&with_index_path, &["idx_customer_id"]);
        remove_db_and_indexes(&without_index_path, &[]);
    }

    #[test]
    #[ignore = "実行時間の計測用。cargo test -- --ignored --nocapture で実行する"]
    fn index_nested_loop_join_is_not_always_faster_than_hash_join() {
        // 単純なルール(索引があればIndex Nested Loop Joinを最優先する、この章の
        // `physical_plan::optimize`)が、常に正しい選択とは限らないことを
        // 実測で確認する。
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
}
