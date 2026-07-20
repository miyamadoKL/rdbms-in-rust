# 第30章 トランザクション境界とAtomicity

`accounts`というテーブルから、Aliceの残高を30減らし、Bobの残高を30増やす送金を考えます。

```console
minidb> CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL);
CREATE TABLE
minidb> INSERT INTO accounts VALUES (1, 100), (2, 50);
INSERT 2
minidb> UPDATE accounts SET balance = balance - 30 WHERE id = 1;
UPDATE 1
minidb> UPDATE accounts SET blance = balance + 30 WHERE id = 2;
エラー: 行1列21: 名前解決エラー: 列'blance'が見つかりません
```

1本目の`UPDATE`は`id = 1`のAliceから30を引くことに成功しました。
2本目の`UPDATE`は`balance`を`blance`と書き間違えており、`Binder`(第17章)がその場で名前解決エラーとして拒否します。
書き間違いに気づいて実行を止められたのは幸いですが、問題は1本目がすでに成功してしまっている点にあります。

```console
minidb> SELECT balance FROM accounts WHERE id = 1;
balance
-------
70
(1 row)
```

Aliceの残高は70のまま、Bobの残高は50のままです。
30はAliceの口座から消えましたが、どこにも届いていません。
この2つの`UPDATE`を「両方成功するか、両方とも無かったことにするか」のどちらかに保つ手段が、このクレートにはまだありません。

## 前章の限界: 2文をまとめて取り消せない

第20章はStatement Rollbackを実装し、`INSERT INTO accounts VALUES (1, 100), (2, 'x')`のように、1本の文が複数行を書き込もうとして途中の行で失敗した場合に、それより前の行の書き込みも一切残さないことを保証しました。
`executor::insert`や`executor::update`が「対象行をすべて検証してから、最後に1回だけ書き込む」という順序を守っているのが、その仕組みの中身です。

ところが、この保証が及ぶ範囲は1本の文の中だけです。
冒頭の例では、1本目の`UPDATE`はそれ単体で見れば正しく完了しており、Statement Rollbackが取り消すべき失敗はどこにも起きていません。
失敗したのは2本目の`UPDATE`であり、1本目の`UPDATE`から見れば「自分より後に実行された、無関係な文」の話です。
`Database::execute`は文を1本ずつ独立に実行するだけで、複数の文を「一連の作業のまとまり」として扱う概念そのものを持っていません。
2つの文をまとめて成功させるか、まとめて取り消すかを選べるようにするには、文の集まりに境界を引く仕組みが要ります。

その境界が、この章で実装する**トランザクション**です。
`BEGIN`から`COMMIT`または`ROLLBACK`までの間に実行した文は、`COMMIT`が届けば全部まとめて確定し、`ROLLBACK`が届けば全部まとめて取り消されます。
取り消しを実現する仕組みは、実行した文の逆操作をメモリ上に積んでおき、`ROLLBACK`が届いたら逆順に適用するという素朴な**Undo**です。
このメモリ上のUndoは、この章限りの実装です。
プロセスがクラッシュすればUndoの記録ごと消えてしまうため、クラッシュをまたいだ復元は保証できません。
第33章でWrite-Ahead Loggingを実装すると、この章のUndo Recordが持つ「更新前の内容」は、ディスクへ先に書き込まれるログの**Before Image**として一般化され、この章の実装そのものが書き直されます。

## BEGIN、COMMIT、ROLLBACKを実行する

まず、3つの新しい文を構文として受理できるようにします。
`src/lexer.rs`の第6章のLexerに`BEGIN`、`COMMIT`、`ROLLBACK`という3つの予約語を追加します。

```rust
pub enum Keyword {
    // ...
    /// `BEGIN`文(第30章)。トランザクションを開始する。
    Begin,
    /// `COMMIT`文(第30章)。
    Commit,
    /// `ROLLBACK`文(第30章)。
    Rollback,
}
```

`src/ast.rs`のAST(第7章)には、それぞれ`Span`だけを持つ空の文を3つ追加します。

```rust
/// `BEGIN`文(第30章)。`BEGIN TRANSACTION`のような修飾は持たず、`BEGIN`
/// 単体だけを受理する(`docs-local/chatgpt_opinion.md`の原案どおり)。
#[derive(Debug, Clone, PartialEq)]
pub struct BeginStatement {
    pub span: Span,
}

/// `COMMIT`文(第30章)。
#[derive(Debug, Clone, PartialEq)]
pub struct CommitStatement {
    pub span: Span,
}

/// `ROLLBACK`文(第30章)。
#[derive(Debug, Clone, PartialEq)]
pub struct RollbackStatement {
    pub span: Span,
}
```

`CREATE TABLE`や`ANALYZE`と同じく、この3つは列や式を1つも持たないため、`src/parser.rs`の`Parser`側も1個のキーワードを読むだけで済みます。

```rust
fn parse_begin_statement(&mut self) -> DbResult<BeginStatement> {
    let span = self.expect_keyword(Keyword::Begin, "BEGIN")?;
    Ok(BeginStatement { span })
}
```

`COMMIT`、`ROLLBACK`もまったく同じ形なので、本文では割愛します。
`src/binder.rs`の`Binder`(第17章)は、`ANALYZE`と同じ理由でこの3つをそのまま素通りさせます。

```rust
Statement::Begin(begin) => Ok(BoundStatement::Begin(begin)),
Statement::Commit(commit) => Ok(BoundStatement::Commit(commit)),
Statement::Rollback(rollback) => Ok(BoundStatement::Rollback(rollback)),
```

`BEGIN`、`COMMIT`、`ROLLBACK`は、突き合わせるべきテーブル名も列名も持ちません。
それでも`Statement`から`BoundStatement`への変換を`Binder`に担わせているのは、`Database::execute`の残り9種類の文(第16〜29章)と同じパイプライン(構文解析→名前解決→実行)を素通りさせるためです。
`Database`側の分岐を、束縛済みの`BoundStatement`という1つの型だけを見て書けるようにする、という一貫性が目的であり、`Binder`に検証してほしい中身がこの3つの文にあるわけではありません。

## Autocommitと明示的トランザクション

`src/database.rs`の`Database`に、現在進行中のトランザクションを表すフィールドを追加します。

```rust
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
    next_txn_id: u64,
    harness_contexts: HashMap<TransactionId, TransactionContext>,
}
```

`tx`が`None`である状態が**Autocommit**です。
第1〜29章のこのクレートは、常にこのAutocommitの状態で動いていました。
`db.execute("UPDATE ...")`を1文実行すれば、それだけで書き込みが確定します。
`tx`が`Some`になっている間だけ、複数の文が1つのトランザクションにまとまります。

`tx`の型`TransactionContext`は、新しいモジュール`transaction`に置きます。
`src/transaction.rs`を新規作成し、次の`TransactionState`と`TransactionContext`を定義します。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionState {
    /// `BEGIN`の直後、まだ`COMMIT`・`ROLLBACK`のどちらも実行していない状態。
    /// この状態の間だけ、`INSERT`・`UPDATE`・`DELETE`・`SELECT`を受け付ける。
    Active,
    /// `COMMIT`によって変更が確定した状態。
    Committed,
    /// `ROLLBACK`によって変更を取り消した状態、または`Active`中に文の実行が
    /// 失敗し、以後`ROLLBACK`しか受け付けなくなった状態(「Statement Error時の
    /// Abort」、本文を参照)。
    Aborted,
}

pub(crate) struct TransactionContext {
    pub id: TransactionId,
    pub state: TransactionState,
    pub undo_log: Vec<UndoRecord>,
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod transaction;
```

`TransactionId`は第13章のNewtype群(`src/ids.rs`)にすでに定義があり、この章で初めて使われます。
`TransactionContext`が`Database`に1個しか無いのは、このクレートがまだ単一接続を前提にしているためです。
複数のクライアントが同時に別々のトランザクションを開くという状況は、第36章でWire Protocolが、第37章でSessionが導入されるまで登場しません。
今のところ`TransactionContext`は`Database`自身が直接持つ1個のフィールドで足り、`Session`という層を先取りして導入する理由がありません。

`src/database.rs`の`execute`は、`BEGIN`、`COMMIT`、`ROLLBACK`をここで直接振り分けます。

```rust
pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
    let statement = match crate::parser::parse_statement(sql) {
        Ok(statement) => statement,
        Err(err) => return self.finish(Err(err)),
    };

    match statement {
        Statement::Begin(begin) => self.execute_begin(begin),
        Statement::Commit(commit) => self.execute_commit(commit),
        Statement::Rollback(rollback) => self.execute_rollback(rollback),
        statement => {
            if let Some(tx) = &self.tx
                && tx.state == TransactionState::Aborted
            {
                return Err(DbError::TransactionAborted);
            }
            let result = self.execute_bound_statement(statement, sql);
            self.finish(result)
        }
    }
}
```

`BEGIN`の実装(`src/database.rs`の`execute_begin`)は、すでに`Active`なトランザクションがあるかどうかを見るだけです。

```rust
fn execute_begin(&mut self, _begin: BeginStatement) -> DbResult<QueryResult> {
    if self.tx.is_some() {
        return Err(DbError::TransactionAlreadyActive);
    }
    let id = TransactionId(self.next_txn_id);
    self.next_txn_id += 1;
    self.tx = Some(TransactionContext::new(id));
    Ok(QueryResult::command("BEGIN"))
}
```

### BEGINの入れ子をどう扱うか

`Active`なトランザクションの中でもう一度`BEGIN`を実行したら、どう振る舞うべきでしょうか。
PostgreSQLは警告を出したうえで進行中のトランザクションをそのまま続けます(入れ子として扱わず、無視します)。
このSQLサブセットは、それより単純な規則を選びました。
`self.tx`が`Option`である以上、`Database`は`Active`なトランザクションを高々1本しか持てません。
2回目の`BEGIN`を「1本目のトランザクションをそのまま続ける」という意味に読み替えるコードを書くこともできますが、それは`BEGIN`という文が「新しい境界を1つ作る」という以外の意味を持ってしまうことになります。
入れ子の`BEGIN`は`DbError::TransactionAlreadyActive`として素直に拒否し、進行中のトランザクションには一切触れません。
`Savepoint`(トランザクションの中に部分的な巻き戻し地点を作る仕組み)のような、入れ子に近い機能が欲しくなった場合は、`BEGIN`を読み替えるのではなく、別の構文として設計するべきだと判断しました。

`src/database.rs`の`execute_commit`が担う`COMMIT`は、`tx`を手放すだけです。

```rust
fn execute_commit(&mut self, _commit: CommitStatement) -> DbResult<QueryResult> {
    match &self.tx {
        None => Err(DbError::NoActiveTransaction),
        Some(tx) if tx.state == TransactionState::Aborted => Err(DbError::TransactionAborted),
        Some(_) => {
            self.tx = None;
            Ok(QueryResult::command("COMMIT"))
        }
    }
}
```

`Active`の間に行った書き込みは、すでに`backend`(`MemStorage`または`Storage`)へ反映済みです。
`COMMIT`はその状態を追認するだけでよく、積んだ`undo_log`もここでは使わずに捨てます。
`src/database.rs`の`execute_rollback`が担う`ROLLBACK`は逆に、積んだ`undo_log`を逆順に適用してから`tx`を手放します。

```rust
fn execute_rollback(&mut self, _rollback: RollbackStatement) -> DbResult<QueryResult> {
    let Some(tx) = self.tx.take() else {
        return Err(DbError::NoActiveTransaction);
    };
    match &mut self.backend {
        Backend::Memory { storage, .. } => transaction::apply_undo_memory(storage, tx.undo_log),
        Backend::Disk { storage } => transaction::apply_undo_disk(storage, tx.undo_log)?,
    }
    Ok(QueryResult::command("ROLLBACK"))
}
```

この`execute_rollback`を実際に動かしてみます。

```console
minidb> BEGIN;
BEGIN
minidb> UPDATE accounts SET balance = balance - 30 WHERE id = 1;
UPDATE 1
minidb> UPDATE accounts SET balance = balance + 30 WHERE id = 2;
UPDATE 1
minidb> ROLLBACK;
ROLLBACK
minidb> SELECT balance FROM accounts WHERE id = 1;
balance
-------
100
(1 row)
```

2本の`UPDATE`をどちらも実行したあとで`ROLLBACK`すると、Aliceの残高は100のまま(1本目の`UPDATE`より前)に戻ります。
`COMMIT`していれば、この2本の`UPDATE`はどちらも確定していました。

## メモリ上のUndo Record

`ROLLBACK`が逆順に適用する`undo_log`の1件が、`src/transaction.rs`に定義する`UndoRecord`です。

```rust
#[derive(Debug, Clone)]
pub enum UndoRecord {
    /// この行が挿入された。取り消すには削除する。
    Insert { table_id: TableId, tuple: Tuple, rid: Option<RecordId> },
    /// この行が削除された。取り消すには再挿入する。
    Delete { table_id: TableId, tuple: Tuple, rid: Option<RecordId> },
    /// この行が`old`から`new`へ更新された。取り消すには`new`を`old`へ戻す。
    Update { table_id: TableId, old: Tuple, new: Tuple, old_rid: Option<RecordId>, new_rid: Option<RecordId> },
}
```

挿入の逆は削除、削除の逆は再挿入、更新の逆は旧値への復元という3種類の逆操作が、`INSERT`、`DELETE`、`UPDATE`の3つの変種にそのまま対応します。
`rid`(RecordId)フィールドが`Option`になっているのは、Diskバックエンド(`Storage`)とMemoryバックエンド(`MemStorage`)で行の指し方が違うためです。
第13章から`Storage`は行の位置を`RecordId`(ページ番号とスロット番号の組)で指しますが、`MemStorage`はそもそも`RecordId`という概念を持たず、行は`Vec<Tuple>`の並びでしかありません。
Diskバックエンドの記録では`rid`は必ず`Some`、Memoryバックエンドの記録では常に`None`になります。

Memoryバックエンドの逆操作(`src/transaction.rs`の`apply_undo_memory`)は、`RecordId`が無い代わりに`Tuple`の値そのものの一致で対象行を探します。

```rust
pub(crate) fn apply_undo_memory(storage: &mut MemStorage, undo_log: Vec<UndoRecord>) {
    for record in undo_log.into_iter().rev() {
        match record {
            UndoRecord::Insert { table_id, tuple, .. } => {
                if let Some(table) = storage.table_mut(table_id)
                    && let Some(pos) = table.rows().iter().position(|t| t.values() == tuple.values())
                {
                    table.rows_mut().remove(pos);
                }
            }
            UndoRecord::Delete { table_id, tuple, .. } => {
                if let Some(table) = storage.table_mut(table_id) {
                    table.rows_mut().push(tuple);
                }
            }
            UndoRecord::Update { table_id, old, new, .. } => {
                if let Some(table) = storage.table_mut(table_id)
                    && let Some(pos) = table.rows().iter().position(|t| t.values() == new.values())
                {
                    table.rows_mut()[pos] = old;
                }
            }
        }
    }
}
```

`undo_log.into_iter().rev()`が、逆順(LIFO)適用の全体です。
同じトランザクションの中で`id = 1`の行を2回更新していれば、`undo_log`には`Update`が2件、実行した順に並びます。
`ROLLBACK`はこれを後ろから適用するので、2回目の更新を先に元へ戻し、続けて1回目の更新を元へ戻すという、実際に起きた変更と正反対の順序をたどります。

`INSERT`、`UPDATE`、`DELETE`の各演算子(`src/executor.rs`の`executor`モジュール)は、この章から`undo: &mut Vec<UndoRecord>`という引数を追加で受け取ります。

```rust
pub fn insert(
    table: &mut MemTable,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    columns: Option<&[usize]>,
    rows: &[Vec<Expr>],
    undo: &mut Vec<UndoRecord>,
) -> DbResult<usize> {
    let planned = plan_insert_rows(schema, functions, columns, rows)?;
    constraints::check_uniqueness(schema, table.rows().iter(), &planned)?;
    let count = planned.len();
    for tuple in &planned {
        undo.push(UndoRecord::Insert { table_id, tuple: tuple.clone(), rid: None });
    }
    table.rows_mut().extend(planned);
    Ok(count)
}
```

実際に書き込んだ行1件ごとに、その逆操作を`undo`へ積みます。
`update`、`delete`、およびDiskバックエンド向けの`storage_insert`、`storage_update`、`storage_delete`も同じ形で`undo`を受け取り、書き込みと同時に逆操作を記録します。
呼び出し元である`src/database.rs`の`Database::run_insert`は、この`undo`をどう扱うかを自分で決めます。

```rust
fn run_insert(&mut self, plan: LogicalPlan) -> DbResult<usize> {
    let LogicalPlan::Insert(InsertNode { table_id, schema, columns, input, .. }) = plan else {
        unreachable!("logical_plan::build_insertは常にLogicalPlan::Insertを返す")
    };
    let LogicalPlan::Values(values) = *input else {
        unreachable!("logical_plan::build_insertはInsertの子に常にValuesを積む")
    };

    let mut undo = Vec::new();
    let result = match &mut self.backend {
        Backend::Memory { storage, .. } => {
            let mem_table =
                storage.table_mut(table_id).expect("catalogに登録されたテーブルはstorageにも必ず存在する");
            executor::insert(mem_table, table_id, &schema, &self.functions, columns.as_deref(), &values.rows, &mut undo)
        }
        Backend::Disk { storage } => executor::storage_insert(
            storage,
            table_id,
            &schema,
            &self.functions,
            columns.as_deref(),
            &values.rows,
            &mut undo,
        ),
    };
    self.record_undo(undo);
    result
}
```

`src/database.rs`の`record_undo`が、`undo`の行き先を決める1箇所です。

```rust
fn record_undo(&mut self, undo: Vec<transaction::UndoRecord>) {
    if undo.is_empty() {
        return;
    }
    if let Some(tx) = &mut self.tx {
        tx.undo_log.extend(undo);
    }
}
```

`self.tx`が`Some`(明示的なトランザクションが進行中)であれば、その`undo_log`へ積みます。
`self.tx`が`None`(Autocommit)であれば、積まずにそのまま捨てます。
Autocommitで実行した1文は、それ自体がすでに確定したトランザクションであり、取り消す先がありません。
Statement Rollback(第20章)がすでに「1文の中の失敗は、その文の外へ影響を漏らさない」ことを保証しているため、Autocommitで実行した文が失敗した場合、`undo`は空のまま`record_undo`に届きます(書き込みに1件も成功していないので、そもそも積む逆操作が無いのです)。
Autocommit経路にこの章のUndoを働かせる必要が無いのは、この章より前からすでに保証されていた性質のおかげです。

### Diskバックエンドの`RecordId`付け替え

`src/transaction.rs`に定義する、`apply_undo_memory`と対になる`apply_undo_disk`は、単純な逆順適用だけでは済みません。

```rust
pub(crate) fn apply_undo_disk(storage: &mut Storage, undo_log: Vec<UndoRecord>) -> DbResult<()> {
    let mut remap: HashMap<RecordId, RecordId> = HashMap::new();

    fn resolve(remap: &HashMap<RecordId, RecordId>, rid: RecordId) -> RecordId {
        let mut current = rid;
        while let Some(&next) = remap.get(&current) {
            if next == current {
                break;
            }
            current = next;
        }
        current
    }

    for record in undo_log.into_iter().rev() {
        match record {
            UndoRecord::Insert { table_id, tuple, rid } => {
                let rid = rid.expect("Diskバックエンドの UndoRecord::Insert は必ずridを持つ");
                let actual = resolve(&remap, rid);
                storage.delete(table_id, actual)?;
                storage.index_delete_row(table_id, &tuple, actual)?;
            }
            // ...(Delete・Updateも続く)
        }
    }
    Ok(())
}
```

同じ行を同じトランザクションの中で複数回`UPDATE`すると、1回目の更新が`RecordId`を`r1`から`r2`へ動かし、2回目の更新がさらに`r2`から`r3`へ動かすことがあります。
`Storage::update`は、新しい値がページに収まりきらないとき、その行を別のページへ移動させるからです(第15章)。
`ROLLBACK`は2回目の更新から逆順に取り消すので、まず`r3`にある値を`r2`相当の値へ書き戻します。
このとき`storage.update`が新しい`RecordId`(たとえば`r4`)を返す可能性があり、1回目の更新の逆操作が記録している`r2`という宛先は、もうその時点で実在しない古い情報になっています。
`remap`は、逆操作を適用するたびに実際に起きた付け替えを覚えておき、次の(時系列でより古い)逆操作が使うべき`RecordId`を、適用する直前に`resolve`でたどり直します。

この設計にはもう1つ、実装している最中に踏んだ落とし穴があります。
`Storage::update`がページ内で収まり、`RecordId`が変わらなかった場合(`r2`から書き戻した結果が`r2`のまま)、`remap`へ`r2 → r2`という自分自身を指すエントリを追加してしまうと、`resolve`がそこから先へ進めなくなり、無限ループに陥ります。
最初の実装はこの分岐を持たず、残高を書き換えるだけの小さなテスト(`UPDATE`のたびにページ内で収まる)を実行したところ、`cargo test`がそのまま返ってこなくなりました。
`remap.insert`を`next == current`のときだけ省略する形に直してから、ようやくこのテストは通るようになりました。

## Statement Error時のAbort

`Active`なトランザクションの中で実行した文がエラーになったら、それ以降の文をどう扱うべきでしょうか。
考えられる設計は大きく2つあります。
1つは、失敗した文だけを無かったことにして、トランザクション自体は継続する設計です。
もう1つは、PostgreSQLが採る設計で、1文でも失敗したらトランザクション全体を「エラー状態」に固定し、`ROLLBACK`以外のすべての文を拒否するというものです。

このSQLサブセットは後者を選びました。
`src/database.rs`に次の`finish`を定義します。

```rust
fn finish(&mut self, result: DbResult<QueryResult>) -> DbResult<QueryResult> {
    if result.is_err()
        && let Some(tx) = &mut self.tx
        && tx.state == TransactionState::Active
    {
        tx.state = TransactionState::Aborted;
    }
    result
}
```

`execute`は、`BEGIN`、`COMMIT`、`ROLLBACK`以外の通常の文を実行した結果をすべて`finish`へ通します。
`Active`の間に実行した文が失敗すれば、`finish`が`tx.state`を`Aborted`へ遷移させます。
`execute`の冒頭にある`Aborted`の判定が、以後のすべての通常の文を拒否します。

```console
minidb> BEGIN;
BEGIN
minidb> UPDATE accounts SET balance = balance - 30 WHERE id = 1;
UPDATE 1
minidb> INSERT INTO accounts VALUES (2, 999);
エラー: PRIMARY KEY制約違反です: 列'id'の値2が重複しています
minidb> SELECT 1;
エラー: 現在のトランザクションはエラーのため中断されています。ROLLBACKだけ受け付けます
minidb> COMMIT;
エラー: 現在のトランザクションはエラーのため中断されています。ROLLBACKだけ受け付けます
minidb> ROLLBACK;
ROLLBACK
```

`INSERT`が失敗した時点で、それより前に成功していた`UPDATE`はまだ`backend`に残ったままです。
`Aborted`はその後の文を拒否するだけで、変更を自動的に取り消しはしません。
最後の`ROLLBACK`が、`BEGIN`以降に積んだ`undo_log`(この例では`UPDATE`1件ぶん)をまとめて取り消します。

前段で継続を選ばなかった理由は、中途半端な直列化を防ぐためです。
1文だけを無かったことにしてトランザクションを継続させる設計では、後続の文が「失敗した文の直前の状態」を暗黙の前提にして書かれることになりますが、その前提は呼び出し側のアプリケーションコードのどこにも書かれません。
「このトランザクションの中で何が本当に成功したか」を呼び出し側が正確に把握できないまま実行が進むくらいなら、失敗した時点で止めて`ROLLBACK`を強制する方が、Atomicityという性質そのものに忠実です。

この設計は、PostgreSQLの挙動と1点だけ異なります。
PostgreSQLは`Aborted`状態への`COMMIT`を、警告付きの暗黙の`ROLLBACK`として受理します。
このSQLサブセットは`COMMIT`も`DbError::TransactionAborted`で一様に拒否し、`ROLLBACK`だけを唯一の出口にしました。
`COMMIT`と書いたのに実際には`ROLLBACK`相当の動作が起きるという振る舞いは、コードを読むだけでは気づきにくい落とし穴です。
`ROLLBACK`という文字どおりの操作だけが`Aborted`を抜けられる、という規則の方が誤解の余地が小さいと判断しました。

`Aborted`への遷移は、第20章のStatement Rollbackと役割が分かれています。
Statement Rollbackは「1本の文の中の部分的な失敗が、その文の外へ影響を漏らさない」ことを保証する、文単位の仕組みです。
`Aborted`は「1本の文が失敗したという事実が、そのトランザクションの残り全体に影響する」ことを表す、トランザクション単位の仕組みです。
複数行の`INSERT`が1行だけ失敗した場合、Statement Rollbackによってその`INSERT`自体は何も書き込みませんが、`Aborted`への遷移はそれとは別に起こり、同じトランザクションの中で先に成功していた文まで`ROLLBACK`を要求します。
2つの仕組みは対象とする範囲(1本の文か、トランザクション全体か)が違うだけで、どちらも「検証や実行の途中経過を、外から観測できる状態に漏らさない」という同じ動機を共有しています。

## 決定的インターリーブテストハーネス

ここまでで、1つの`Database`が1本のトランザクションを開始し、確定または破棄する仕組みが揃いました。
第5部がこれから実装していくLock Manager(第31章)、Isolation Level(第32章)は、複数のトランザクションが同時に実行されたときに何が起きるかを扱う話です。
複数のトランザクションを試すには、それらを実際に交互に進行させる手段が要ります。

ここで壁にぶつかります。
複数のトランザクションを本当の意味で同時に進行させるには、複数のスレッドから同じ`Database`を触る必要がありますが、`BufferPool`と`BTree`がスレッドセーフになるのは第35章です。
それより前の章(第31〜34章)で、実スレッドを使った並行テストを書くことはできません。

この章は、実スレッドの代わりに「1つの`Database`の上で、複数のトランザクションの文を書いた順序どおりに交互実行する」という形でインターリーブを再現します。
順序はテストコードが明示的に書き下すため、実行結果は毎回決定的です。
これが**決定的インターリーブテストハーネス**であり、第31〜34章の並行テストはすべてこの上で書かれます。

### 複数のトランザクションを同時に開く

ここでもう1つ設計判断が要ります。
`Database`の`tx: Option<TransactionContext>`は、`Active`なトランザクションを高々1本しか保持できません。
複数のトランザクションを行き来しながら進めるテストは、この1本しか無い`tx`をそのまま使えません。

`tx`とは別に、複数のトランザクションを`TransactionId`ごとに保持できる対応表を`src/database.rs`の`Database`に追加しました。

```rust
harness_contexts: HashMap<TransactionId, TransactionContext>,
```

あわせて、テスト側が個々のトランザクションを指すためのハンドル`TxHandle`を`src/database.rs`に定義します。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxHandle(TransactionId);
```

`src/database.rs`の`begin_tx`が新しい`TransactionContext`を`harness_contexts`へ登録し、以後の操作に使う`TxHandle`を返します。

```rust
pub fn begin_tx(&mut self) -> TxHandle {
    let id = TransactionId(self.next_txn_id);
    self.next_txn_id += 1;
    self.harness_contexts.insert(id, TransactionContext::new(id));
    TxHandle(id)
}
```

`src/database.rs`の`execute_in_tx`が、指定した`TxHandle`のトランザクションの中で1文を実行します。

```rust
pub fn execute_in_tx(&mut self, handle: &TxHandle, sql: &str) -> DbResult<QueryResult> {
    let ctx = self
        .harness_contexts
        .remove(&handle.0)
        .expect("TxHandleはすでにcommit_tx・rollback_tx済み、または他のDatabaseのものです");
    if ctx.state == TransactionState::Aborted {
        self.harness_contexts.insert(handle.0, ctx);
        return Err(DbError::TransactionAborted);
    }

    let previous = self.tx.replace(ctx);
    let statement = crate::parser::parse_statement(sql);
    let result = match statement {
        Ok(statement) => self.execute_bound_statement(statement, sql),
        Err(err) => Err(err),
    };
    let result = self.finish(result);
    let ctx = self.tx.take().expect("execute_bound_statementはself.txを取り除かない");
    self.tx = previous;
    self.harness_contexts.insert(handle.0, ctx);
    result
}
```

ここでの工夫は、対応表から取り出した`TransactionContext`を、この1文の間だけ`self.tx`へ差し替えていることです。
差し替えたあとは、`execute`の通常の文が使う`execute_bound_statement`(Bindと実行)、`finish`(Abortedへの遷移)をそのまま呼びます。
`INSERT`、`UPDATE`、`DELETE`が呼ぶ`run_insert`等も、`self.tx`を見て`record_undo`する既存のコードのままです。
つまり`execute_in_tx`は、実行ロジックを1行も複製せず、「今どの`TransactionContext`を`self.tx`として使うか」を差し替えるだけの薄い配線です。
実行が終わったら、変化した(`undo_log`が伸びた、または`Aborted`へ遷移したかもしれない)`TransactionContext`を対応表へ戻し、差し替え前の`self.tx`を元に戻します。

`src/database.rs`の`commit_tx`、`rollback_tx`も対応表から取り出すだけで、`rollback_tx`は`execute_rollback`と同じ`apply_undo_memory`、`apply_undo_disk`を呼びます。

```rust
pub fn rollback_tx(&mut self, handle: TxHandle) -> DbResult<()> {
    let ctx = self
        .harness_contexts
        .remove(&handle.0)
        .expect("TxHandleはすでにcommit_tx・rollback_tx済み、または他のDatabaseのものです");
    match &mut self.backend {
        Backend::Memory { storage, .. } => transaction::apply_undo_memory(storage, ctx.undo_log),
        Backend::Disk { storage } => transaction::apply_undo_disk(storage, ctx.undo_log)?,
    }
    Ok(())
}
```

このAPIはSQLの構文(`BEGIN`、`COMMIT`、`ROLLBACK`)を経由しません。
`TxHandle`は`Database`の外からは中身の見えない不透明な識別子であり、`db.execute("BEGIN")`という通常のSQL経路とは完全に独立しています。
通常のSQL経路(REPL、`tests/golden`等)は、これまでどおり単一のトランザクションしか扱えないままです。
複数のトランザクションを同時に開ける経路を持つのは、このハーネス専用のAPIだけに限定しました。

## 並行実行の異常を先にテストとして固定する

このハーネスを使い、`tests/interleave.rs`に4種類の異常を再現するテストを書きます。
Lost Update、Dirty Read、Non-repeatable Read、Phantomは、教科書がAtomicityとIsolationの節でほぼ必ず取り上げる、代表的な異常です。
この章の時点でminidbが持つ並行制御は「無い」ため、この4つは実際に起こります。
以下のテストは、その異常が「起きる」ことを`assert`する形で固定します。

`tests/interleave.rs`のDirty Readのテストを見てみます。

```rust
#[test]
fn dirty_read_is_not_prevented_yet() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    // T1が残高を減らすが、まだコミットしていない。
    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    // T2は、T1がコミットしたかどうかに関係なく、その未コミットの値を読める。
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let dirty_value = int_value(&read, 0, 0);

    // T1は結局ロールバックする。
    db.rollback_tx(t1).unwrap();
    db.commit_tx(t2).unwrap();

    assert_eq!(dirty_value, 70, "T2はT1のロールバックされる運命の値(70)を読めてしまっている(Dirty Read)");
    let after = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&after, 0, 0), 100);
}
```

T1が`UPDATE`した時点で、その変更はすでに`backend`(`MemStorage`または`Storage`)へ反映されています。
このクレートには、コミット前の変更を他のトランザクションから隠す仕組み(ロックやMVCCのスナップショット)が無いため、T2の`SELECT`はT1がまだ確定させていない値をそのまま読めます。
T1が結局`ROLLBACK`すれば、T2が読んだ70という値は、一度もコミットされたことの無い値だったことになります。
これがDirty Readです。

残り3つも同じ形で再現します。

Lost Updateは、2つのトランザクションが同じ行を「読んで、計算して、書く」というパターンを別々に行うと起こります。
T1とT2がどちらも残高100を読み、T2が先に「100 + 20」を書いてコミットし、T1がそのあと(T2の書き込みを読み直さずに)自分が最初に読んだ100を根拠に「100 + 10」を書いてコミットすると、最終的な残高は110になり、T2が足した20はどこにも残りません。
アプリケーションコードの`balance + 10`のような相対更新ではなく、それぞれが読んだ値をもとに計算した**リテラルな**新しい値を書くことで、実際にこの異常を引き起こすアプリケーションの書き方を再現しています。

Non-repeatable Readは、同じトランザクションの中で同じ行を2回読むと起こります。
T1が1回目に残高を読んだあと、T2が割り込んで更新してコミットし、T1が(まだコミットもロールバックもしないまま)同じ行を2回目に読むと、1回目と2回目で異なる値が返ってきます。

Phantomは、同じトランザクションの中で同じ`WHERE`条件を2回集計すると起こります。
T1が「残高が40より大きい行」を1回目に数えたあと、T2がその条件に一致する新しい行を挿入してコミットし、T1が2回目に同じ条件で数えると、1回目には無かった行(幻、Phantom)が数に含まれます。

この4つの`assert`は、第31章でLock Managerが、第32章でIsolation Levelが実装されれば反転します。
Strict 2PLのExclusive Lockが働けば、T1がまだコミットしていない書き込みにT2がShared Lockを取れなくなり、Dirty Readは起こらなくなります。
このハーネスの役割は、その反転が実際に起きたことを、将来の章で同じテストの`assert`を書き換えるだけで確認できるようにしておくことです。

## テストで確認する

`src/database.rs`には、`BEGIN`、`COMMIT`、`ROLLBACK`の基本動作、Autocommit、入れ子の`BEGIN`が拒否されること、Statement Error時のAbort、同一行への複数回の`UPDATE`をUndoが正しく逆順適用できることを、MemoryバックエンドとDiskバックエンドの両方で確認するテストを追加しました。

```rust
#[test]
fn rollback_undoes_repeated_updates_to_the_same_row_on_disk() {
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
```

同じ行を3回書き換えてから`ROLLBACK`すると、残高は最初の100まで戻ります。
`apply_undo_disk`の`remap`が正しく働いていなければ、このテストは(無限ループに陥るか、途中の値のどれかで止まるかのどちらかで)失敗します。

`tests/interleave.rs`には、4つの異常の再現テストに加えて、3本のトランザクションを同時に開き、`commit_tx`、`rollback_tx`を混ぜて呼んでもそれぞれ独立に効くことを確認するテストを追加しました。

```console
$ cargo test --lib
test result: ok. 753 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out
$ cargo test --test interleave
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## この章の限界

この章のUndoは、変更を成功のたびに1件ずつメモリへ積むだけで、ディスクには何も書きません。
`ROLLBACK`の直後にプロセスがクラッシュしても実害はありませんが、`Active`なトランザクションの途中でプロセスがクラッシュすれば、それまでの変更が`backend`にどこまで反映されていたかを知る手段がなく、Undoの記録ごと失われます。
この章はこの経路を閉じません。
閉じるには、変更をページへ書く前にその変更を表すログを先にディスクへ書く、Write-Ahead Logging(第33章)と、そのログから再起動後に状態を復元するCrash Recovery(第34章)が要ります。

`apply_undo_disk`の失敗経路も同様です。
`storage.update`や`storage.index_insert_row`が`BufferPool`のI/Oエラーで失敗すると、そのトランザクションは一部だけ取り消された中途半端な状態のまま残ります。
これは第15章と第20章がすでに明文化した「正直なギャップ」と同じ割り切りであり、この章で新たに広げたものではなく、この章のUndoにもそのまま引き継がれています。

Undoの対象も、`INSERT`、`UPDATE`、`DELETE`というDMLに限られます。
`CREATE TABLE`、`DROP TABLE`、`CREATE INDEX`、`DROP INDEX`はこの章のUndoの対象外で、`Active`なトランザクションの中でこれらを実行してから`ROLLBACK`しても、テーブルや索引の定義そのものは取り消されません。
DDLをトランザクションに含める設計(PostgreSQLのようにDDLも`ROLLBACK`できる方式と、多くのRDBMSのようにDDLが暗黙にコミットされる方式があります)は、この章では選ばず、演習課題として残します。

## 演習問題

### 必須課題

1. `execute_commit`は`Aborted`状態を`DbError::TransactionAborted`で拒否しますが、本文で触れたとおりPostgreSQLはこれを暗黙の`ROLLBACK`として受理します。`execute_commit`をPostgreSQL方式に変更し(`Aborted`状態への`COMMIT`が`undo_log`を適用してから成功を返す)、`a_failing_statement_aborts_the_transaction_and_blocks_further_statements`をこの新しい挙動に合わせて書き換えてください。書き換えたあと、どちらの設計が読み手にとって誤解しにくいか、自分の言葉で比較してください。
2. `apply_undo_disk`の`remap`から、`if new_rid != rid { ... }`と`if old_rid != result_rid { ... }`という自己参照防止のガードを取り除くとどうなるか、実際に試してください。どのテストがハングするか(または無限ループに陥るか)を確認し、`resolve`関数側の`if next == current { break; }`だけを残した場合と、両方とも残した場合とで、動作にどんな違いが生じるか考察してください。
3. `tests/interleave.rs`のLost Updateのテストは、`SELECT`で読んだ値をテストコード(Rust側)が保持し、その値をもとにした`UPDATE`のリテラルを組み立てています。これをテストコードではなくSQLの中だけで再現しようとするとなぜ難しいか(`UPDATE accounts SET balance = balance + 10`という相対更新ではLost Updateを再現できない理由)を説明してください。

### 発展課題

1. この章のトランザクションは、`Database`が1本しか持てません。第37章を先取りして、複数の`TransactionContext`を`Session`ごとに持てるよう`Database`を設計変更してみてください。この章の`harness_contexts`(`HashMap<TransactionId, TransactionContext>`)が、その設計のヒントになるはずです。
2. `UndoRecord`は`INSERT`、`UPDATE`、`DELETE`だけを持ち、`CREATE TABLE`、`DROP TABLE`のUndoレコードを持ちません。`CREATE TABLE`、`DROP TABLE`を`ROLLBACK`可能にするための`UndoRecord`の追加バリアントを設計し、実装してください。`DROP TABLE`の取り消しは、削除された行をどこかに退避しておく必要があることに注意してください。
3. `tests/interleave.rs`のPhantomのテストを拡張し、`INSERT`だけでなく`DELETE`によるPhantom(1回目には存在した行が、2回目には消えている)も再現するテストを追加してください。この異常は一般に「Phantom」と呼ばれますが、`INSERT`による場合と`DELETE`による場合とで、後の章のどの仕組み(Lock、Isolation Level)が有効な対策になるかを考えてみてください。
