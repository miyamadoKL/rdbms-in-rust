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

use crate::ast::{CreateTableStatement, DropTableStatement, Statement};
use crate::binder::{Binder, BoundStatement};
use crate::catalog::Catalog;
use crate::error::{DbError, DbResult};
use crate::eval::FunctionRegistry;
use crate::executor;
use crate::logical_plan::{self, DeleteNode, InsertNode, LogicalPlan, UpdateNode};
use crate::physical_plan::{
    self, DiskSeqScanExec, DistinctExec, Executor, FilterExec, HashAggregateExec, LimitExec, MemSeqScanExec,
    PhysicalPlan, ProjectionExec, SortExec, ValuesExec,
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
enum Backend {
    Memory { catalog: Catalog, storage: MemStorage },
    Disk { storage: Storage },
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
            backend: Backend::Disk { storage },
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
            Backend::Disk { storage } => Binder::new(storage, &self.functions, sql).bind(statement),
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
            BoundStatement::Select(select) => self.execute_select(logical_plan::build_select(select)),
            BoundStatement::CreateTable(create) => self.execute_create_table(&create),
            BoundStatement::DropTable(drop) => self.execute_drop_table(&drop),
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
        match &mut self.backend {
            Backend::Memory { catalog, storage } => {
                let id = catalog.create_table(&create.table.name, schema)?;
                storage.create_table(id);
            }
            Backend::Disk { storage } => {
                storage.create_table(&create.table.name, schema)?;
            }
        }
        Ok(QueryResult::command("CREATE TABLE"))
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
        let physical = physical_plan::optimize(plan);
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
            PhysicalPlan::Values(values) => {
                let exec = ValuesExec::new(values.schema.clone(), &self.functions, &values.rows)?;
                Ok(Box::new(exec))
            }
            PhysicalPlan::Filter(filter) => {
                let input = self.build_query_executor(&filter.input)?;
                Ok(Box::new(FilterExec::new(input, &filter.predicate, &self.functions)))
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
    /// 入れ子の`Explain`はここに渡ってこない。
    fn execute_explain(&self, inner: BoundStatement) -> DbResult<QueryResult> {
        let logical = match inner {
            BoundStatement::Select(select) => logical_plan::build_select(select),
            BoundStatement::Insert(insert) => logical_plan::build_insert(insert),
            BoundStatement::Update(update) => logical_plan::build_update(update),
            BoundStatement::Delete(delete) => logical_plan::build_delete(delete),
            BoundStatement::CreateTable(_) | BoundStatement::DropTable(_) | BoundStatement::Explain(_) => {
                unreachable!("ParserがEXPLAINの対象をSELECT・INSERT INTO・UPDATE・DELETE FROMに制限している")
            }
        };
        let physical = physical_plan::optimize(logical);
        Ok(QueryResult::explain(physical.to_string()))
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
}
