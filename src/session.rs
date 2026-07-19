//! 接続単位の状態を一元管理する[`Session`](第37章)。
//!
//! # ch36の前身から何を引き継ぐか
//!
//! 第36章の`crate::server`は、接続ごとのトランザクション状態
//! (`Option<TxHandle>`)だけを持つ`Session`という前身を導入した。その
//! `Session::execute`は、届いたSQLをまず`crate::parser::parse_statement`で
//! 一度パースして`BEGIN`・`COMMIT`・`ROLLBACK`かどうかを判定し、それ以外の
//! 文は`sql`という文字列のまま`SharedDatabase::execute_in_tx`(または
//! Autocommit用の`execute_autocommit`)へ渡していた。ところがこの2つの関数は
//! 内部で`crate::parser::parse_statement(sql)`をもう一度呼ぶ
//! (`Database::execute_in_tx`のドキュメント参照)。つまり`BEGIN`以外の文は
//! 毎回2回パースされていた。
//!
//! この章の[`Session`]は、パースした`Statement`を`BEGIN`等の判定にも
//! 実行にもそのまま使い回す。具体的には、`BEGIN`・`COMMIT`・`ROLLBACK`・
//! `PREPARE`・`EXECUTE`・`DEALLOCATE`のどれでもない文を、[`Database::bind_statement`]
//! (第37章で新設した`Database::bind`の公開版)で**その場で1回だけ**束縛し、
//! 得られた`BoundStatement`を[`Database::execute_in_tx_bound`]・
//! [`Database::execute_bound_statement_prebound`]という、束縛済みの文を
//! 受け取って実行するだけの経路へ渡す。これで「パース1回・束縛1回」になる。
//!
//! # `Database::execute`との関係
//!
//! `Database::execute`(第9章以来の低レベルAPI)は、この章でも変更していない。
//! 構文解析から実行までを1回の呼び出しで済ませる単純さは、ほとんどの章の
//! テストが直接`db.execute("...")`を呼ぶ形で使い続けており、テストのたびに
//! `Session`を組み立てさせる理由が無い。[`Session`]は`Database::execute`を
//! 置き換えるのではなく、その上に接続の寿命(トランザクション状態、
//! Prepared Statementの名前空間)を積む層として追加した。埋め込み用途で
//! `Session`を使わずに`Database::execute`を直接呼ぶ経路も、この章以降
//! 変わらず使える(ただし`PREPARE`・`EXECUTE`・`DEALLOCATE`は`Session`
//! 経由でしか使えない。`Database::execute("PREPARE ...")`は`DbError::Bind`
//! を返す、`crate::binder::Binder::bind`のドキュメント参照)。
//!
//! # Embedded・REPL・Serverの統一
//!
//! [`Session`]は`Arc<SharedDatabase>`を1個持つだけの薄い型なので、3つの
//! 利用箇所すべてが同じ型を使う。
//!
//! * Embedded(ライブラリとして`minidb`を使うコード): `Database`を
//!   `SharedDatabase::new`で包み、`Arc`に入れて`Session::new`へ渡す。
//! * REPL(`src/main.rs`): プロセス全体で`Session`を1個だけ作り、標準入力の
//!   行ごとに`Session::execute`を呼ぶ。
//! * Server(`src/server.rs`): 接続を受け付けるたびに、サーバーが持つ
//!   `Arc<SharedDatabase>`を`Arc::clone`して`Session::new`へ渡す。
//!
//! `SharedDatabase`は元々複数スレッドから安全に共有するための型(第35章)
//! であり、単一スレッドのEmbedded・REPLで使っても`Mutex`のロックが
//! 競合しないぶん、複数スレッドの場合と地続きの型で書ける。
//!
//! # トランザクション状態
//!
//! [`Session::tx`]は、この接続が`BEGIN`で開始した`TxHandle`を保持する。
//! `None`はAutocommitを表す。`BEGIN`・`COMMIT`・`ROLLBACK`の3文だけが
//! この状態を書き換える。それ以外の文は`self.tx`があれば
//! `SharedDatabase::execute_in_tx_bound`、無ければ1文だけの
//! `begin_tx`→`execute_in_tx_bound`→`commit_tx`/`rollback_tx`
//! (Autocommit)として実行する。この規律自体は第36章の前身から変わらない。
//!
//! # Prepared StatementはSessionのものである
//!
//! [`PreparedStatement`]は[`Session`]がフィールドとして直接保持する
//! `HashMap`にだけ存在する。`Database`・`SharedDatabase`はPrepared
//! Statementという概念を一切知らない。この設計により、
//!
//! * 名前空間は接続(`Session`)ごとに独立する。ある接続の`PREPARE p AS ...`は、
//!   同じ`SharedDatabase`につながる別の接続からは`p`という名前として見えない。
//! * 接続が切れて`Session`がdropされれば、`prepared`という`HashMap`ごと
//!   自動的に消える。`DEALLOCATE`を呼び忘れても、後始末のためのコードを
//!   別に持つ必要が無い。
//!
//! # プレースホルダの型をいつ決めるか
//!
//! `PREPARE`本体の`$n`は、`Binder::bind_expr`が束縛の時点(`PREPARE`が
//! 実行された瞬間)で、可能な限り周囲の文脈から型を決める
//! (`crate::binder::BoundExpr::Param`のドキュメント、`coerce_param_type`を
//! 参照)。`col = $1`なら`col`の型、`$1 + 1`なら`BIGINT`、
//! `CAST($1 AS TEXT)`なら`TEXT`、という具合に、比較・算術・論理演算子・
//! `CAST`・関数呼び出しの引数という5箇所で推論する。
//!
//! 一方、`$1 = $2`のように両辺とも`$n`だと、この式1つだけでは型を決める
//! 材料が無い。この場合、型は`None`(未確定)のまま`PreparedStatement`に
//! 残る。`EXECUTE`はこの`$n`に対して、渡された値がどんな型であっても受理する
//! (型検査を省略するのであって、値を拒否するわけではない)。値が式の要求する
//! 型と食い違っていれば(たとえば`$1 = $2`に`EXECUTE p(1, 'x')`のように
//! BIGINTとTEXTを渡す)、`Binder`ではなく`crate::eval::eval_bound_expr`が
//! 実行時の`DbError::Eval`として検出する。これはPREPARE時点の型検査が
//! 「決まる場合には決める」だけの片務的な安全網であり、EXECUTE時点の
//! 実行時エラーへ倒れる余地を最初から許容した設計であることを意味する。
//!
//! この章の限界として、`PREPARE`は本体を1回だけ束縛し、以後の`EXECUTE`は
//! その`BoundStatement`をそのまま(パラメータだけ差し替えて)使い回す。
//! `PREPARE`した後に`ANALYZE`で統計情報が更新されても、すでに`PREPARE`
//! 済みの文の実行計画は作り直さない(`Database::execute_select`が
//! `PREPARE`のたびではなく`EXECUTE`のたびに`physical_plan::optimize`を
//! 呼ぶため、計画そのものは最新の統計を使って毎回組み直る。ここで
//! 古いままなのは、あくまで`rules::optimize`が畳み込む定数式や、
//! 束縛時点で確定した型・列インデックスといった`BoundStatement`の構造で
//! あり、PostgreSQLが"generic plan"と"custom plan"を使い分けて対処する
//! 種類の問題を、この章では扱わない)。
//!
//! # Wire Protocolを拡張しない
//!
//! `PREPARE`・`EXECUTE`・`DEALLOCATE`はどれも、`crate::protocol`が定義する
//! フレーム形式(第36章)の上で、通常の`SELECT`等と同じ「SQL文字列を1本
//! 送る」というメッセージとして表現できる。専用のメッセージ種別(たとえば
//! PostgreSQLのExtended Query Protocolが持つ`Parse`・`Bind`・`Execute`の
//! 3つのメッセージ)を追加する必要はこの章には無い。PostgreSQLがメッセージを
//! 分けている理由は、`Bind`で値を送る段階と`Parse`で構文を送る段階を
//! クライアント側のバイト列レベルで分離し、`Bind`だけを繰り返す(同じ
//! `Parse`のまま値だけ変えて何度も`Execute`する)通信を1往復で済ませる
//! ためである。この章の`Session`は`PREPARE`・`EXECUTE`のどちらもSQL文字列
//! として受け取り、サーバー側の`HashMap`で名前を引く形に留めているため、
//! 通信の往復数を減らす効果はSQL文字列を送るのと変わらない。この差を
//! 埋める設計は発展編Eに譲る。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::ast::{
    DeallocateStatement, ExecuteStatement, Expr, Literal, PrepareStatement, Statement,
};
use crate::binder::{AggregateCall, BoundAggregate, BoundAssignment, BoundDelete, BoundExpr, BoundInsert,
    BoundJoinStep, BoundOrderByItem, BoundSelect, BoundSelectItem, BoundStatement, BoundUpdate};
use crate::cancellation::CancellationToken;
use crate::database::{QueryResult, SharedDatabase, TxHandle};
use crate::error::{DbError, DbResult};
use crate::lexer::Span;
use crate::types::{DataType, Value};

/// 接続(TCP接続、またはEmbedded/REPLの1プロセス)が持つ状態をまとめたもの。
/// モジュール冒頭のドキュメントを参照。
///
/// # 第38章: 文1本ごとのキャンセル
///
/// [`Session`]は接続1本につき1個の`cancel_flag`(`Arc<AtomicBool>`)を持つ。
/// [`Session::execute`]・[`Session::execute_prepared`]は、文を1本実行するたびに
/// この`cancel_flag`を`false`へ戻してから、それを元にした
/// [`crate::cancellation::CancellationToken`]([`SharedDatabase::make_execution_context`]、
/// 設定済みのタイムアウトがあればその締切も併せ持つ)を作り、その文の実行に
/// 使う。[`Session::cancellation_handle`]は同じ`cancel_flag`を`clone`して返す
/// ([`CancellationToken`]自体を返さないのは、締切がまだ確定していない
/// [`Session::execute`]呼び出し前の時点でも、呼び出し元が先にハンドルを
/// 取得できるようにするため)。呼び出し元(`crate::server`のクライアント切断
/// 検知、またはテストコード)がこのハンドルの`cancel`を呼べば、`self`を
/// `&mut`で借用している`execute`呼び出しとは別のスレッドから、実行中の文を
/// 打ち切れる(本文・テストを参照)。
pub struct Session {
    shared: Arc<SharedDatabase>,
    tx: Option<TxHandle>,
    prepared: HashMap<String, PreparedStatement>,
    cancel_flag: Arc<AtomicBool>,
    /// [`CancellationToken::checkpoints`]をこの接続の外(`cancellation_handle`)と
    /// 内(実行中の文が使うトークン)で共有するための`Arc`。テストが「実行中の
    /// 文が同期ポイントを確かに何度も通過した」ことを外から観測するために使う
    /// (`crate::cancellation`モジュール冒頭を参照、本番のコードは読まない)。
    checkpoints: Arc<std::sync::atomic::AtomicUsize>,
}

/// `PREPARE`が登録する1件。`bound`は`PREPARE`の時点で束縛済みの文、
/// `param_types`は`$1`から順に並べた、文脈から推論できた型
/// (推論できなければ`None`、モジュール冒頭「プレースホルダの型をいつ決めるか」
/// を参照)。`param_types.len()`が、この文が要求する`EXECUTE`の引数の個数になる。
struct PreparedStatement {
    bound: BoundStatement,
    param_types: Vec<Option<DataType>>,
}

impl Session {
    /// `shared`につながる新しい接続を表すSessionを作る。トランザクション状態は
    /// Autocommit、Prepared Statementの名前空間は空の状態で始まる。
    pub fn new(shared: Arc<SharedDatabase>) -> Self {
        Session {
            shared,
            tx: None,
            prepared: HashMap::new(),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            checkpoints: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// 現在(または次に)実行中の文をキャンセルするためのハンドルを返す
    /// (第38章)。`&self`だけで呼べるため、`session.execute(...)`(`&mut self`)を
    /// 呼ぶより前に取得しておき、別スレッドへ持って行って`cancel`を呼べる
    /// (型冒頭「第38章: 文1本ごとのキャンセル」を参照)。
    ///
    /// 1回`cancel`されたハンドルは、以後この`Session`が実行するすべての文を
    /// キャンセルし続ける(`cancel_flag`は文をまたいで同じ`Arc`のままで、
    /// `execute`は次の文の開始時に`false`へ戻す。すでに`cancel`済みの
    /// ハンドルを使い回すことは想定していない)。
    pub fn cancellation_handle(&self) -> CancellationToken {
        CancellationToken::with_checkpoints(Arc::clone(&self.cancel_flag), None, Arc::clone(&self.checkpoints))
    }

    /// この文専用の[`crate::cancellation::ExecutionContext`]を作る。呼ぶたびに
    /// `cancel_flag`・`checkpoints`を初期状態へ戻すため、前の文の状態が次の文へ
    /// 漏れることは無い。
    fn new_execution_context(&self) -> crate::cancellation::ExecutionContext {
        self.cancel_flag.store(false, std::sync::atomic::Ordering::Relaxed);
        self.checkpoints.store(0, std::sync::atomic::Ordering::Relaxed);
        self.shared.make_execution_context(Arc::clone(&self.cancel_flag), Arc::clone(&self.checkpoints))
    }

    /// `sql`を1文実行し、結果を返す。
    ///
    /// `sql`は`crate::parser::parse_statement`で一度だけパースする。以後は
    /// `Statement`という構造化された値だけを使い、`sql`という文字列を
    /// 再パースする経路には二度と渡さない(モジュール冒頭「ch36の前身から
    /// 何を引き継ぐか」を参照)。
    pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
        let statement = crate::parser::parse_statement(sql)?;
        match statement {
            Statement::Begin(begin) => {
                if self.tx.is_some() {
                    return Err(DbError::TransactionAlreadyActive);
                }
                let level = begin.isolation_level.unwrap_or(crate::ast::IsolationLevel::RepeatableRead);
                self.tx = Some(self.shared.begin_tx_with_isolation(level));
                Ok(QueryResult::command("BEGIN"))
            }
            Statement::Commit(_) => {
                let handle = self.tx.take().ok_or(DbError::NoActiveTransaction)?;
                self.shared.commit_tx(handle)?;
                Ok(QueryResult::command("COMMIT"))
            }
            Statement::Rollback(_) => {
                let handle = self.tx.take().ok_or(DbError::NoActiveTransaction)?;
                self.shared.rollback_tx(handle)?;
                Ok(QueryResult::command("ROLLBACK"))
            }
            // `CHECKPOINT`はトランザクション境界の外側の操作であり、
            // `SharedDatabase`が`TxHandle`API越しに公開していない
            // (`Database::execute_checkpoint`のドキュメント参照)。
            // 第36章の前身から引き継いだ制約であり、この章では解消しない。
            Statement::Checkpoint(_) => {
                Err(DbError::NotImplemented("CHECKPOINTはSession経由では未対応です".to_string()))
            }
            Statement::Prepare(prepare) => self.execute_prepare(prepare, sql),
            Statement::Execute(execute) => self.execute_execute(&execute),
            Statement::Deallocate(deallocate) => self.execute_deallocate(&deallocate),
            other => {
                let bound = self.shared.bind_statement(other, sql)?;
                let started = std::time::Instant::now();
                let result = self.run_bound(bound);
                crate::slow_query_log::maybe_log(self.shared.slow_query_threshold(), sql, started.elapsed(), &result);
                result
            }
        }
    }

    /// 束縛済みの文を、この接続のトランザクション状態に従って実行する。
    /// `self.tx`があればその中で、無ければAutocommitで実行する(第36章の
    /// 前身が`sql`文字列に対して行っていた分岐を、束縛済みの`BoundStatement`
    /// に対して行う形にそのまま引き継ぐ)。
    fn run_bound(&mut self, bound: BoundStatement) -> DbResult<QueryResult> {
        let ctx = self.new_execution_context();
        match &self.tx {
            Some(handle) => self.shared.execute_in_tx_bound(handle, &bound, &ctx),
            None => self.run_bound_autocommit(bound, &ctx),
        }
    }

    /// 明示的な`BEGIN`の外で届いた束縛済みの文を、それ専用の`TxHandle`で
    /// 実行する。成功すればすぐ`commit_tx`、失敗すれば`rollback_tx`し、
    /// どちらの場合もこの文の実行が終わった時点でロックを持ち越さない
    /// (`crate::server`モジュールの旧`execute_autocommit`と同じ規律)。
    ///
    /// キャンセル・タイムアウトで打ち切られた場合(`ctx`)も、失敗した文の
    /// 1つとして`rollback_tx`する。この文が`begin_tx`以降に書き込んだ変更が
    /// あれば、Autocommitのトランザクション境界に従って取り消される
    /// (本文「キャンセルされたトランザクションはAbortする」を参照)。
    fn run_bound_autocommit(&self, bound: BoundStatement, ctx: &crate::cancellation::ExecutionContext) -> DbResult<QueryResult> {
        let handle = self.shared.begin_tx();
        match self.shared.execute_in_tx_bound(&handle, &bound, ctx) {
            Ok(result) => {
                self.shared.commit_tx(handle)?;
                Ok(result)
            }
            Err(err) => {
                let _ = self.shared.rollback_tx(handle);
                Err(err)
            }
        }
    }

    /// `PREPARE name AS <statement>`を実行する。
    fn execute_prepare(&mut self, prepare: PrepareStatement, sql: &str) -> DbResult<QueryResult> {
        if self.prepared.contains_key(&prepare.name.name) {
            return Err(DbError::PreparedStatementAlreadyExists(prepare.name.name));
        }
        if !matches!(
            prepare.statement.as_ref(),
            Statement::Select(_) | Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_)
        ) {
            return Err(DbError::CannotPrepareStatement);
        }
        let bound = self.shared.bind_statement(*prepare.statement, sql)?;
        let param_types = collect_param_types(&bound)?;
        self.prepared.insert(prepare.name.name.clone(), PreparedStatement { bound, param_types });
        Ok(QueryResult::command("PREPARE"))
    }

    /// `EXECUTE name [(値, ...)]`を実行する。引数はSQL文字列中の
    /// リテラルとして届くので、`literal_to_value`で`Value`へ変換してから
    /// [`Session::execute_prepared`]へ委ねる。
    fn execute_execute(&mut self, execute: &ExecuteStatement) -> DbResult<QueryResult> {
        let values: Vec<Value> = execute.args.iter().map(literal_to_value).collect();
        self.execute_prepared(&execute.name.name, &values)
    }

    /// `name`で`PREPARE`済みの文を、`args`を`$1`から順に束縛して実行する。
    ///
    /// `EXECUTE`(SQL文字列)の内部実装であると同時に、埋め込み用途で
    /// `minidb`をライブラリとして使うRustコードが呼ぶための公開APIでもある。
    /// SQL文字列の`EXECUTE name('...')`は、渡したい値をSQL構文の一部として
    /// テキストへ書き出す必要があるため、値そのものに引用符や制御文字が
    /// 含まれる場合は呼び出し側がSQLの引用規則(`''`によるエスケープ)を
    /// 正しく適用しないと、この経路もまた文字列連結による注入と同じ危険を
    /// 抱える(本文「EXECUTEの引数もテキストである」を参照)。`args`を
    /// `Value`として直接渡すこの関数は、値をSQLのテキスト表現へ一度も
    /// 経由しないため、値の中身がどんな文字列であってもテキスト連結の
    /// 注入対象にならない。これが、Prepared Statementが実際に
    /// SQLインジェクションを防ぐ経路である。
    pub fn execute_prepared(&mut self, name: &str, args: &[Value]) -> DbResult<QueryResult> {
        let prepared =
            self.prepared.get(name).ok_or_else(|| DbError::PreparedStatementNotFound(name.to_string()))?;

        if args.len() != prepared.param_types.len() {
            return Err(DbError::ParamCountMismatch { expected: prepared.param_types.len(), actual: args.len() });
        }
        for (position, value) in args.iter().enumerate() {
            if let Some(expected) = prepared.param_types[position]
                && let Some(actual) = value.data_type()
                && actual != expected
            {
                return Err(DbError::ParamTypeMismatch { index: (position + 1) as u32, expected, actual });
            }
        }

        let bound = substitute_bound_statement(prepared.bound.clone(), args);
        self.run_bound(bound)
    }

    /// `DEALLOCATE name`を実行する。
    fn execute_deallocate(&mut self, deallocate: &DeallocateStatement) -> DbResult<QueryResult> {
        if self.prepared.remove(&deallocate.name.name).is_none() {
            return Err(DbError::PreparedStatementNotFound(deallocate.name.name.clone()));
        }
        Ok(QueryResult::command("DEALLOCATE"))
    }
}

impl Drop for Session {
    /// 接続が切れた時点で未コミットのトランザクションが残っていればROLLBACK
    /// する(第36章の前身から引き継いだ後始末、`crate::server`モジュール
    /// 冒頭「切断時のトランザクション後始末」を参照)。`prepared`は`HashMap`
    /// が自動的にdropされる(モジュール冒頭「Prepared StatementはSessionの
    /// ものである」を参照)ため、ここで明示的に後始末する必要は無い。
    fn drop(&mut self) {
        if let Some(handle) = self.tx.take() {
            let _ = self.shared.rollback_tx(handle);
        }
    }
}

/// `Literal`(`EXECUTE`の引数、`ast::Literal`のドキュメント参照)を`Value`へ
/// 変換する。式評価を経由しない単純な1対1の対応。
fn literal_to_value(literal: &Literal) -> Value {
    match literal {
        Literal::Int { value, .. } => Value::BigInt(*value),
        Literal::Text { value, .. } => Value::Text(value.clone()),
        Literal::Bool { value, .. } => Value::Boolean(*value),
        Literal::Null { .. } => Value::Null,
    }
}

// ---- プレースホルダの型を集める(PREPARE時) ----

/// `bound`(`PREPARE`本体、`Select`・`Insert`・`Update`・`Delete`のいずれか)の
/// 中に現れる`$n`をすべて集め、`$1`から順に並べた型の配列にする。
/// `types[i]`は`$(i+1)`の型(推論できなければ`None`)。同じ番号が矛盾する型で
/// 使われていれば`DbError::ParamTypeConflict`を返す。
fn collect_param_types(bound: &BoundStatement) -> DbResult<Vec<Option<DataType>>> {
    let mut types = Vec::new();
    match bound {
        BoundStatement::Select(select) => collect_select(select, &mut types)?,
        BoundStatement::Insert(insert) => collect_insert(insert, &mut types)?,
        BoundStatement::Update(update) => collect_update(update, &mut types)?,
        BoundStatement::Delete(delete) => collect_delete(delete, &mut types)?,
        // `execute_prepare`がSelect/Insert/Update/Delete以外を弾いてから
        // 呼ぶため、ここには到達しない。
        _ => {}
    }
    Ok(types)
}

fn record_param(types: &mut Vec<Option<DataType>>, index: u32, hint: Option<DataType>) -> DbResult<()> {
    let position = index as usize;
    if position == 0 {
        return Err(DbError::Eval("プレースホルダの番号は1以上である必要があります: $0".to_string()));
    }
    if types.len() < position {
        types.resize(position, None);
    }
    let slot = &mut types[position - 1];
    match (*slot, hint) {
        (Some(existing), Some(new)) if existing != new => {
            return Err(DbError::ParamTypeConflict { index, first: existing, second: new });
        }
        (None, Some(new)) => *slot = Some(new),
        _ => {}
    }
    Ok(())
}

fn collect_select(select: &BoundSelect, types: &mut Vec<Option<DataType>>) -> DbResult<()> {
    for join in &select.joins {
        collect_bound_expr(&join.condition, types)?;
    }
    for item in &select.projection {
        collect_bound_expr(&item.expr, types)?;
    }
    if let Some(predicate) = &select.predicate {
        collect_bound_expr(predicate, types)?;
    }
    if let Some(aggregate) = &select.aggregate {
        for group_key in &aggregate.group_by {
            collect_bound_expr(group_key, types)?;
        }
        for call in &aggregate.calls {
            if let Some(arg) = &call.arg {
                collect_bound_expr(arg, types)?;
            }
        }
    }
    if let Some(having) = &select.having {
        collect_bound_expr(having, types)?;
    }
    for item in &select.order_by {
        collect_bound_expr(&item.expr, types)?;
    }
    Ok(())
}

fn collect_update(update: &BoundUpdate, types: &mut Vec<Option<DataType>>) -> DbResult<()> {
    for assignment in &update.assignments {
        collect_bound_expr(&assignment.value, types)?;
    }
    if let Some(predicate) = &update.predicate {
        collect_bound_expr(predicate, types)?;
    }
    Ok(())
}

fn collect_delete(delete: &BoundDelete, types: &mut Vec<Option<DataType>>) -> DbResult<()> {
    if let Some(predicate) = &delete.predicate {
        collect_bound_expr(predicate, types)?;
    }
    Ok(())
}

fn collect_bound_expr(expr: &BoundExpr, types: &mut Vec<Option<DataType>>) -> DbResult<()> {
    match expr {
        BoundExpr::Param { index, data_type, .. } => record_param(types, *index, *data_type),
        BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. }
        | BoundExpr::ColumnRef { .. } => Ok(()),
        BoundExpr::UnaryOp { expr, .. }
        | BoundExpr::Paren { expr, .. }
        | BoundExpr::Cast { expr, .. }
        | BoundExpr::IsNull { expr, .. } => collect_bound_expr(expr, types),
        BoundExpr::BinaryOp { lhs, rhs, .. } => {
            collect_bound_expr(lhs, types)?;
            collect_bound_expr(rhs, types)
        }
        BoundExpr::FunctionCall { args, .. } => {
            for arg in args {
                collect_bound_expr(arg, types)?;
            }
            Ok(())
        }
        BoundExpr::Aggregate { arg, .. } => match arg {
            Some(arg) => collect_bound_expr(arg, types),
            None => Ok(()),
        },
    }
}

/// `INSERT`の`VALUES`(束縛されない生の`ast::Expr`のまま持つ、
/// `crate::binder::BoundInsert`のドキュメント参照)から`$n`を集める。
/// 各値の期待型は、`columns`(明示された列名の並び)または位置そのものから
/// `schema`の対応する列の型を引く。
fn collect_insert(insert: &BoundInsert, types: &mut Vec<Option<DataType>>) -> DbResult<()> {
    for row in &insert.rows {
        for (position, value_expr) in row.iter().enumerate() {
            let column_index = match &insert.columns {
                Some(columns) => columns.get(position).copied(),
                None => Some(position),
            };
            let target_type = column_index.and_then(|index| insert.schema.columns().get(index)).map(|c| c.data_type);
            collect_ast_expr(value_expr, target_type, types)?;
        }
    }
    Ok(())
}

/// `Expr`(`INSERT`の`VALUES`)から`$n`を集める。`hint`は、その位置に来る
/// べき値の期待型(`CAST`は自分の対象型で`hint`を上書きする)。`$1 + 1`の
/// ような入れ子は、この章では期待型を伝播させず`None`のまま扱う
/// (`Binder::bind_expr`の二項演算子ほど文脈を追わない、`VALUES`に対する
/// この章のスコープの限界)。
fn collect_ast_expr(expr: &Expr, hint: Option<DataType>, types: &mut Vec<Option<DataType>>) -> DbResult<()> {
    match expr {
        Expr::Param { index, .. } => record_param(types, *index, hint),
        Expr::IntLiteral { .. }
        | Expr::StringLiteral { .. }
        | Expr::BoolLiteral { .. }
        | Expr::NullLiteral { .. }
        | Expr::ColumnRef { .. } => Ok(()),
        Expr::Paren { expr, .. } => collect_ast_expr(expr, hint, types),
        Expr::UnaryOp { expr, .. } | Expr::IsNull { expr, .. } => collect_ast_expr(expr, None, types),
        Expr::BinaryOp { lhs, rhs, .. } => {
            collect_ast_expr(lhs, None, types)?;
            collect_ast_expr(rhs, None, types)
        }
        Expr::Cast { expr, type_name, .. } => collect_ast_expr(expr, DataType::from_sql_name(&type_name.name), types),
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_ast_expr(arg, None, types)?;
            }
            Ok(())
        }
        // 集約関数はVALUESに書けても意味を持たない(実行時に`DbError::Eval`
        // になる、`crate::eval::eval_expr`のAggregate分岐を参照)。
        Expr::Aggregate { .. } => Ok(()),
    }
}

// ---- 値を差し込む(EXECUTE時) ----

/// `bound`が持つすべての`$n`を、`values[n - 1]`のリテラルへ置き換えた
/// 新しい`BoundStatement`を作る。`values`の長さは呼び出し元
/// (`Session::execute_execute`)が`prepared.param_types.len()`とすでに
/// 一致させてあるので、ここでは範囲外アクセスを気にせずインデックスできる。
fn substitute_bound_statement(bound: BoundStatement, values: &[Value]) -> BoundStatement {
    match bound {
        BoundStatement::Select(select) => BoundStatement::Select(Box::new(substitute_select(*select, values))),
        BoundStatement::Insert(insert) => BoundStatement::Insert(substitute_insert(insert, values)),
        BoundStatement::Update(update) => BoundStatement::Update(substitute_update(update, values)),
        BoundStatement::Delete(delete) => BoundStatement::Delete(substitute_delete(delete, values)),
        // `execute_prepare`がSelect/Insert/Update/Delete以外を弾いてから
        // `PreparedStatement`へ保存するため、ここには到達しない。
        other => other,
    }
}

fn substitute_select(select: BoundSelect, values: &[Value]) -> BoundSelect {
    let BoundSelect {
        tables,
        joins,
        distinct,
        projection,
        hidden_column_count,
        predicate,
        aggregate,
        having,
        order_by,
        limit,
        offset,
        span,
    } = select;
    BoundSelect {
        tables,
        joins: joins
            .into_iter()
            .map(|join| BoundJoinStep { kind: join.kind, condition: substitute_bound_expr(join.condition, values) })
            .collect(),
        distinct,
        projection: projection
            .into_iter()
            .map(|item| BoundSelectItem { expr: substitute_bound_expr(item.expr, values), output_name: item.output_name })
            .collect(),
        hidden_column_count,
        predicate: predicate.map(|predicate| substitute_bound_expr(predicate, values)),
        aggregate: aggregate.map(|aggregate| {
            let BoundAggregate { group_by, calls, schema } = aggregate;
            BoundAggregate {
                group_by: group_by.into_iter().map(|expr| substitute_bound_expr(expr, values)).collect(),
                calls: calls
                    .into_iter()
                    .map(|call| AggregateCall {
                        func: call.func,
                        arg: call.arg.map(|arg| Box::new(substitute_bound_expr(*arg, values))),
                    })
                    .collect(),
                schema,
            }
        }),
        having: having.map(|having| substitute_bound_expr(having, values)),
        order_by: order_by
            .into_iter()
            .map(|item| BoundOrderByItem { expr: substitute_bound_expr(item.expr, values), desc: item.desc })
            .collect(),
        limit,
        offset,
        span,
    }
}

fn substitute_insert(insert: BoundInsert, values: &[Value]) -> BoundInsert {
    let BoundInsert { table_id, table_name, schema, columns, rows, span } = insert;
    let rows = rows
        .into_iter()
        .map(|row| row.into_iter().map(|expr| substitute_ast_expr(expr, values)).collect())
        .collect();
    BoundInsert { table_id, table_name, schema, columns, rows, span }
}

fn substitute_update(update: BoundUpdate, values: &[Value]) -> BoundUpdate {
    let BoundUpdate { table_id, table_name, schema, assignments, predicate, span } = update;
    BoundUpdate {
        table_id,
        table_name,
        schema,
        assignments: assignments
            .into_iter()
            .map(|assignment| BoundAssignment {
                column_index: assignment.column_index,
                value: substitute_bound_expr(assignment.value, values),
            })
            .collect(),
        predicate: predicate.map(|predicate| substitute_bound_expr(predicate, values)),
        span,
    }
}

fn substitute_delete(delete: BoundDelete, values: &[Value]) -> BoundDelete {
    let BoundDelete { table_id, table_name, schema, predicate, span } = delete;
    BoundDelete { table_id, table_name, schema, predicate: predicate.map(|predicate| substitute_bound_expr(predicate, values)), span }
}

fn substitute_bound_expr(expr: BoundExpr, values: &[Value]) -> BoundExpr {
    match expr {
        BoundExpr::Param { index, span, .. } => value_to_bound_literal(values[index as usize - 1].clone(), span),
        BoundExpr::UnaryOp { op, expr, data_type, span } => {
            BoundExpr::UnaryOp { op, expr: Box::new(substitute_bound_expr(*expr, values)), data_type, span }
        }
        BoundExpr::BinaryOp { op, lhs, rhs, data_type, span } => BoundExpr::BinaryOp {
            op,
            lhs: Box::new(substitute_bound_expr(*lhs, values)),
            rhs: Box::new(substitute_bound_expr(*rhs, values)),
            data_type,
            span,
        },
        BoundExpr::IsNull { expr, negated, span } => {
            BoundExpr::IsNull { expr: Box::new(substitute_bound_expr(*expr, values)), negated, span }
        }
        BoundExpr::FunctionCall { name, args, data_type, span } => BoundExpr::FunctionCall {
            name,
            args: args.into_iter().map(|arg| substitute_bound_expr(arg, values)).collect(),
            data_type,
            span,
        },
        BoundExpr::Aggregate { func, arg, data_type, span } => BoundExpr::Aggregate {
            func,
            arg: arg.map(|arg| Box::new(substitute_bound_expr(*arg, values))),
            data_type,
            span,
        },
        BoundExpr::Paren { expr, span } => BoundExpr::Paren { expr: Box::new(substitute_bound_expr(*expr, values)), span },
        BoundExpr::Cast { expr, data_type, span } => {
            BoundExpr::Cast { expr: Box::new(substitute_bound_expr(*expr, values)), data_type, span }
        }
        literal @ (BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. }
        | BoundExpr::ColumnRef { .. }) => literal,
    }
}

fn substitute_ast_expr(expr: Expr, values: &[Value]) -> Expr {
    match expr {
        Expr::Param { index, span } => value_to_ast_literal(values[index as usize - 1].clone(), span),
        Expr::UnaryOp { op, expr, span } => Expr::UnaryOp { op, expr: Box::new(substitute_ast_expr(*expr, values)), span },
        Expr::BinaryOp { op, lhs, rhs, span } => Expr::BinaryOp {
            op,
            lhs: Box::new(substitute_ast_expr(*lhs, values)),
            rhs: Box::new(substitute_ast_expr(*rhs, values)),
            span,
        },
        Expr::IsNull { expr, negated, span } => {
            Expr::IsNull { expr: Box::new(substitute_ast_expr(*expr, values)), negated, span }
        }
        Expr::FunctionCall { name, args, span } => {
            Expr::FunctionCall { name, args: args.into_iter().map(|arg| substitute_ast_expr(arg, values)).collect(), span }
        }
        Expr::Aggregate { func, arg, span } => {
            Expr::Aggregate { func, arg: arg.map(|arg| Box::new(substitute_ast_expr(*arg, values))), span }
        }
        Expr::Paren { expr, span } => Expr::Paren { expr: Box::new(substitute_ast_expr(*expr, values)), span },
        Expr::Cast { expr, type_name, span } => {
            Expr::Cast { expr: Box::new(substitute_ast_expr(*expr, values)), type_name, span }
        }
        other => other,
    }
}

fn value_to_bound_literal(value: Value, span: Span) -> BoundExpr {
    match value {
        Value::Null => BoundExpr::NullLiteral { span },
        Value::Boolean(value) => BoundExpr::BoolLiteral { value, span },
        Value::BigInt(value) => BoundExpr::IntLiteral { value, span },
        Value::Text(value) => BoundExpr::StringLiteral { value, span },
    }
}

fn value_to_ast_literal(value: Value, span: Span) -> Expr {
    match value {
        Value::Null => Expr::NullLiteral { span },
        Value::Boolean(value) => Expr::BoolLiteral { value, span },
        Value::BigInt(value) => Expr::IntLiteral { value, span },
        Value::Text(value) => Expr::StringLiteral { value, span },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, ResourceLimits};
    use std::time::Duration;

    fn new_session() -> Session {
        Session::new(Arc::new(SharedDatabase::new(Database::memory())))
    }

    fn setup_users(session: &mut Session) {
        session.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT)").unwrap();
        session.execute("INSERT INTO users VALUES (1, 'Alice', 30)").unwrap();
        session.execute("INSERT INTO users VALUES (2, 'Bob', 25)").unwrap();
    }

    #[test]
    fn prepare_execute_deallocate_roundtrip() {
        let mut session = new_session();
        setup_users(&mut session);

        session.execute("PREPARE by_id AS SELECT name FROM users WHERE id = $1").unwrap();
        let result = session.execute("EXECUTE by_id(1)").unwrap();
        assert_eq!(result.rows().len(), 1);

        session.execute("DEALLOCATE by_id").unwrap();
        let err = session.execute("EXECUTE by_id(1)").unwrap_err();
        assert!(matches!(err, DbError::PreparedStatementNotFound(_)));
    }

    #[test]
    fn execute_rejects_wrong_arg_count() {
        let mut session = new_session();
        setup_users(&mut session);
        session.execute("PREPARE by_id AS SELECT name FROM users WHERE id = $1").unwrap();
        let err = session.execute("EXECUTE by_id(1, 2)").unwrap_err();
        assert!(matches!(err, DbError::ParamCountMismatch { expected: 1, actual: 2 }));
    }

    #[test]
    fn execute_rejects_wrong_arg_type() {
        let mut session = new_session();
        setup_users(&mut session);
        session.execute("PREPARE by_id AS SELECT name FROM users WHERE id = $1").unwrap();
        let err = session.execute("EXECUTE by_id('not-a-number')").unwrap_err();
        assert!(matches!(err, DbError::ParamTypeMismatch { index: 1, .. }));
    }

    #[test]
    fn execute_allows_null_regardless_of_inferred_type() {
        let mut session = new_session();
        setup_users(&mut session);
        session.execute("PREPARE by_name AS SELECT id FROM users WHERE name = $1").unwrap();
        // NULLはどの型のプレースホルダに対しても許す(通常の列と同じ扱い)。
        let result = session.execute("EXECUTE by_name(NULL)").unwrap();
        assert_eq!(result.rows().len(), 0);
    }

    #[test]
    fn prepare_rejects_duplicate_name() {
        let mut session = new_session();
        setup_users(&mut session);
        session.execute("PREPARE p AS SELECT id FROM users").unwrap();
        let err = session.execute("PREPARE p AS SELECT name FROM users").unwrap_err();
        assert!(matches!(err, DbError::PreparedStatementAlreadyExists(_)));
    }

    #[test]
    fn prepare_rejects_non_dml_statement() {
        let mut session = new_session();
        let err = session.execute("PREPARE p AS CREATE TABLE t (id BIGINT)").unwrap_err();
        assert!(matches!(err, DbError::CannotPrepareStatement));
    }

    #[test]
    fn prepared_statement_is_scoped_to_its_session() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let mut session_a = Session::new(Arc::clone(&shared));
        setup_users(&mut session_a);
        session_a.execute("PREPARE p AS SELECT id FROM users").unwrap();

        let mut session_b = Session::new(Arc::clone(&shared));
        let err = session_b.execute("EXECUTE p()").unwrap_err();
        assert!(matches!(err, DbError::PreparedStatementNotFound(_)));
    }

    #[test]
    fn dropping_session_discards_its_prepared_statements() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let mut session = Session::new(Arc::clone(&shared));
        setup_users(&mut session);
        session.execute("PREPARE p AS SELECT id FROM users").unwrap();
        drop(session);

        // 同じSharedDatabaseにつながる新しい接続からは、もう`p`は見えない。
        let mut fresh = Session::new(Arc::clone(&shared));
        let err = fresh.execute("EXECUTE p()").unwrap_err();
        assert!(matches!(err, DbError::PreparedStatementNotFound(_)));
    }

    #[test]
    fn execute_works_inside_an_explicit_transaction() {
        let mut session = new_session();
        setup_users(&mut session);
        session.execute("PREPARE set_age AS UPDATE users SET age = $1 WHERE id = $2").unwrap();

        session.execute("BEGIN").unwrap();
        session.execute("EXECUTE set_age(99, 1)").unwrap();
        session.execute("ROLLBACK").unwrap();

        let result = session.execute("SELECT age FROM users WHERE id = 1").unwrap();
        let age: i64 = match result.rows()[0].values()[0].clone() {
            Value::BigInt(v) => v,
            other => panic!("BIGINTを期待しましたが{other:?}でした"),
        };
        assert_eq!(age, 30, "ROLLBACKしたのでEXECUTEの変更は残らない");
    }

    #[test]
    fn sql_injection_via_string_concatenation_is_prevented_by_parameters() {
        let mut session = new_session();
        setup_users(&mut session);

        // 文字列連結でWHERE句を組み立てると、ユーザー入力が構文の一部に
        // 混入し、常に真になる条件へすり替えられる(教材内の安全な再現)。
        let malicious_input = "x' OR '1'='1";
        let injected_sql = format!("SELECT name FROM users WHERE name = '{malicious_input}'");
        // 壊れた構文(閉じないリテラル)としてそのままエラーになる場合と、
        // 意図せず全行を返してしまう場合があるが、どちらにせよ
        // 「name = 'x' OR '1'='1'」を安全に検索条件として扱えていない。
        let by_concatenation = session.execute(&injected_sql);
        let unsafe_result_count = match by_concatenation {
            Ok(result) => result.rows().len(),
            Err(_) => 0,
        };

        // Parameter Bindingでは、同じ入力を`Value`としてそのまま渡す
        // (`execute_prepared`)。SQLのテキスト表現を一度も経由しないため、
        // 値の中身に`'`が何個含まれていても構文の一部としては解釈されない。
        // 該当する行が無いので0件になる。
        session.execute("PREPARE find_by_name AS SELECT name FROM users WHERE name = $1").unwrap();
        let result = session
            .execute_prepared("find_by_name", &[Value::Text(malicious_input.to_string())])
            .unwrap();
        assert_eq!(result.rows().len(), 0, "パラメータ化された入力は値としてしか扱われない");
        assert_ne!(
            unsafe_result_count, 0,
            "この例では文字列連結側が実際に全行を返してしまうことを確認する"
        );
    }

    // ---- 第38章: Cancellation・Timeout・メモリ上限 ----

    /// `id`から`count`件の行を持つテーブル`name`を作る。大きなCartesian積を
    /// 安価に用意するための共通ヘルパー。
    fn seed_table(session: &mut Session, name: &str, count: i64) {
        session.execute(&format!("CREATE TABLE {name} (id BIGINT)")).unwrap();
        for i in 0..count {
            session.execute(&format!("INSERT INTO {name} VALUES ({i})")).unwrap();
        }
    }

    #[test]
    fn cancellation_handle_stops_a_long_running_query_from_another_thread() {
        let mut session = new_session();
        seed_table(&mut session, "t", 300);

        // `cancellation_handle`は`&self`だけで呼べるため、`execute`(`&mut self`)を
        // 別スレッドへ`move`する前に取得しておける(型冒頭のドキュメント参照)。
        let handle = session.cancellation_handle();
        let worker = std::thread::spawn(move || {
            // `ON 1 = 1`は等値結合の形をしていない(定数同士の比較)ため
            // `Nested Loop Join`が選ばれ、300 × 300 = 90,000通りの組み合わせを
            // すべて`WHERE`と同じ三値論理で評価しながら返す。300行だけでも
            // `execute_select`の駆動ループ(`crate::cancellation`モジュール冒頭の
            // 主要な同期ポイント)を実測1,000回以上通過するのに十分な件数である。
            session.execute("SELECT * FROM t AS a JOIN t AS b ON 1 = 1")
        });

        // `sleep`による時間待ちではなく、実行中の文が同期ポイントを実際に
        // 1,000回通過したことを確認してから`cancel`する。総行数(90,000)は
        // この閾値よりはるかに大きいため、まだ実行の途中であることが保証される。
        handle.wait_for_checkpoints(1_000);
        handle.cancel();

        let result = worker.join().expect("ワーカースレッドがpanicした");
        assert!(matches!(result, Err(DbError::QueryCancelled)), "{result:?}");
    }

    #[test]
    fn statement_timeout_aborts_a_query_that_runs_past_the_deadline() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let mut session = Session::new(Arc::clone(&shared));
        seed_table(&mut session, "t", 700);

        // 締切(1ミリ秒)に対して、700 × 700 = 490,000通りの組み合わせを評価する
        // クエリは常に十分長くかかる(実測で数十ミリ秒以上)。締切とクエリの
        // 実行時間の比を大きく取ることで、実行環境の速度差によるflakyさを避ける。
        shared.set_resource_limits(ResourceLimits { statement_timeout: Some(Duration::from_millis(1)), ..Default::default() });
        let result = session.execute("SELECT * FROM t AS a JOIN t AS b ON 1 = 1");
        assert!(matches!(result, Err(DbError::QueryTimeout)), "{result:?}");
    }

    #[test]
    fn max_operator_rows_aborts_sort_once_the_collected_rows_exceed_the_limit() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let mut session = Session::new(Arc::clone(&shared));
        seed_table(&mut session, "t", 10);
        shared.set_resource_limits(ResourceLimits { max_operator_rows: Some(5), ..Default::default() });

        let result = session.execute("SELECT id FROM t ORDER BY id");
        assert!(matches!(result, Err(DbError::MemoryLimitExceeded { operator: "Sort", limit: 5 })), "{result:?}");
    }

    #[test]
    fn max_operator_rows_allows_sort_within_the_limit() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let mut session = Session::new(Arc::clone(&shared));
        seed_table(&mut session, "t", 5);
        shared.set_resource_limits(ResourceLimits { max_operator_rows: Some(5), ..Default::default() });

        let result = session.execute("SELECT id FROM t ORDER BY id").unwrap();
        assert_eq!(result.rows().len(), 5);
    }

    #[test]
    fn max_operator_rows_aborts_hash_join_build_once_the_right_side_exceeds_the_limit() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let mut session = Session::new(Arc::clone(&shared));
        seed_table(&mut session, "a", 3);
        seed_table(&mut session, "b", 10);
        shared.set_resource_limits(ResourceLimits { max_operator_rows: Some(5), ..Default::default() });

        // `b`(Build側、右辺)が上限を超える。`crate::physical_plan::optimize`が
        // 小さい側をBuildに選ぶとは限らないため、両方が上限を超えるように
        // `a`・`b`とも上限より多い行を持たせたいところだが、この章のテストでは
        // どちらがBuild側に選ばれても上限に触れることを狙い、両テーブルとも
        // 上限(5)を超える行数にしてある。
        let result = session.execute("SELECT a.id FROM a JOIN b ON a.id = b.id");
        assert!(matches!(result, Err(DbError::MemoryLimitExceeded { operator: "Hash Join", .. })), "{result:?}");
    }

    #[test]
    fn max_operator_rows_aborts_hash_aggregate_once_the_distinct_groups_exceed_the_limit() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let mut session = Session::new(Arc::clone(&shared));
        seed_table(&mut session, "t", 10);
        shared.set_resource_limits(ResourceLimits { max_operator_rows: Some(5), ..Default::default() });

        // `id`は1行ごとに異なるので、`GROUP BY id`は10個の別々のグループを作る。
        let result = session.execute("SELECT id, COUNT(*) FROM t GROUP BY id");
        assert!(matches!(result, Err(DbError::MemoryLimitExceeded { operator: "Hash Aggregate", .. })), "{result:?}");
    }
}
