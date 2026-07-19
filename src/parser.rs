//! Token列をASTへ変換する構文解析器。
//!
//! 文(`SELECT` / `CREATE TABLE` / `INSERT INTO`)はRecursive Descentで、式は
//! Pratt Parser(binding power)で解析する。使い分けの理由は本文([第7章](../book))
//! を参照。
//!
//! 対応する構文は次のとおり。
//! - `SELECT <式|*> [, <式|*> ...] [FROM <table>] [WHERE <式>]`
//! - `CREATE TABLE <table> (<col> <type> [NOT NULL], ...)`
//! - `DROP TABLE <table>`
//! - `CREATE [UNIQUE] INDEX <index> ON <table> (<col>)`(第24章)
//! - `DROP INDEX <index>`(第24章)
//! - `INSERT INTO <table> [(<col>, ...)] VALUES (<式>, ...), ...`
//! - `UPDATE <table> SET <col> = <式> [, ...] [WHERE <式>]`
//! - `DELETE FROM <table> [WHERE <式>]`
//! - `EXPLAIN <SELECT|INSERT INTO|UPDATE|DELETE FROM>`(第19章)
//! - 式: リテラル(整数・文字列・真偽値・`NULL`)、列参照、二項演算(`+ - * /`、
//!   比較、`AND` `OR`)、単項演算(`-` `NOT`)、`IS [NOT] NULL`、関数呼び出し、
//!   `CAST(expr AS type)`、括弧
//!
//! 優先順位は低い順に`OR` < `AND` < `NOT` < 比較 < `+` `-` < `*` `/` < 単項`-`。

use crate::ast::{
    AggregateFunc, AnalyzeStatement, Assignment, BinaryOperator, ColumnDef, CreateIndexStatement, CreateTableStatement,
    DeleteStatement, DropIndexStatement, DropTableStatement, ExplainStatement, Expr, FromClause, Ident,
    InsertStatement, JoinClause, JoinKind, OrderByItem, SelectItem, SelectStatement, Statement, UnaryOperator,
    UpdateStatement,
};
use crate::error::{DbError, DbResult};
use crate::lexer::{self, Keyword, Span, Token, TokenKind};

/// SQL文字列を1本のASTへ変換する。
///
/// 字句解析(`lexer::tokenize`)が失敗した場合は`DbError::Lex`を、Token列は
/// 得られたがこのサブセットの文法に適合しない場合は`DbError::Parse`を返す。
/// 末尾のセミコロンは省略でき、あっても無くても同じASTになる。
pub fn parse_statement(source: &str) -> DbResult<Statement> {
    let tokens = lexer::tokenize(source)?;
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
    };
    let statement = parser.parse_statement()?;
    parser.expect_end()?;
    Ok(statement)
}

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Token {
        // `tokenize`が返すTokenの末尾は必ずEofなので、`pos`が配列末尾を
        // 超えることはない。
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    /// `n`個先(`n == 0`は`peek_kind`と同じ)のトークンの種類を覗き見る。配列の
    /// 末尾を超える場合は最後のトークン(常に`Eof`)を返す。`CREATE TABLE`と
    /// `CREATE INDEX`・`CREATE UNIQUE INDEX`(第24章)、`DROP TABLE`と
    /// `DROP INDEX`(第24章)は、どちらも1個目のキーワード(`CREATE`・`DROP`)が
    /// 共通のため、2個目のトークンを覗いてから分岐する。
    fn peek_nth_kind(&self, n: usize) -> &TokenKind {
        let idx = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[idx].kind
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        token
    }

    fn error_at(&self, span: Span, message: impl Into<String>) -> DbError {
        let (line, column) = lexer::line_col(self.source, span.start);
        DbError::Parse {
            message: message.into(),
            line,
            column,
        }
    }

    /// 現在のトークンについてのエラーを作る。「何を期待していたか」を添える。
    fn unexpected(&self, expected: &str) -> DbError {
        let token = self.peek();
        self.error_at(
            token.span,
            format!("{expected}が必要です: {:?}が見つかりました", token.kind),
        )
    }

    fn expect_keyword(&mut self, keyword: Keyword, label: &str) -> DbResult<Span> {
        if let TokenKind::Keyword(k) = self.peek_kind()
            && *k == keyword
        {
            Ok(self.advance().span)
        } else {
            Err(self.unexpected(label))
        }
    }

    fn expect_punct(&mut self, expected: TokenKind, label: &str) -> DbResult<Span> {
        if *self.peek_kind() == expected {
            Ok(self.advance().span)
        } else {
            Err(self.unexpected(label))
        }
    }

    fn expect_ident(&mut self) -> DbResult<Ident> {
        match self.peek_kind() {
            TokenKind::Ident(name) => {
                let name = name.clone();
                let span = self.advance().span;
                Ok(Ident { name, span })
            }
            _ => Err(self.unexpected("識別子")),
        }
    }

    /// 文の解析後、末尾の`;`(あれば)を読み飛ばしたうえで、残りが`Eof`だけに
    /// なっていることを確認する。余分なトークンが残っていれば構文エラーとする。
    fn expect_end(&mut self) -> DbResult<()> {
        if *self.peek_kind() == TokenKind::Semicolon {
            self.advance();
        }
        if *self.peek_kind() == TokenKind::Eof {
            Ok(())
        } else {
            Err(self.unexpected("文の終端"))
        }
    }

    fn parse_statement(&mut self) -> DbResult<Statement> {
        match self.peek_kind() {
            TokenKind::Keyword(Keyword::Select) => {
                self.parse_select_statement().map(|s| Statement::Select(Box::new(s)))
            }
            TokenKind::Keyword(Keyword::Create) => self.parse_create_statement(),
            TokenKind::Keyword(Keyword::Drop) => self.parse_drop_statement(),
            TokenKind::Keyword(Keyword::Insert) => {
                self.parse_insert_statement().map(Statement::Insert)
            }
            TokenKind::Keyword(Keyword::Update) => {
                self.parse_update_statement().map(Statement::Update)
            }
            TokenKind::Keyword(Keyword::Delete) => {
                self.parse_delete_statement().map(Statement::Delete)
            }
            TokenKind::Keyword(Keyword::Explain) => {
                self.parse_explain_statement().map(Statement::Explain)
            }
            TokenKind::Keyword(Keyword::Analyze) => {
                self.parse_analyze_statement().map(Statement::Analyze)
            }
            _ => Err(self.unexpected(
                "SELECT・CREATE TABLE・DROP TABLE・CREATE INDEX・DROP INDEX・INSERT INTO・UPDATE・\
                 DELETE FROM・EXPLAIN・ANALYZEのいずれか",
            )),
        }
    }

    /// `CREATE`の直後を覗き見て、`CREATE TABLE`と`CREATE [UNIQUE] INDEX`(第24章)を
    /// 振り分ける。
    fn parse_create_statement(&mut self) -> DbResult<Statement> {
        match self.peek_nth_kind(1) {
            TokenKind::Keyword(Keyword::Table) => self.parse_create_table_statement().map(Statement::CreateTable),
            TokenKind::Keyword(Keyword::Index) => self.parse_create_index_statement(false).map(Statement::CreateIndex),
            TokenKind::Keyword(Keyword::Unique) => self.parse_create_index_statement(true).map(Statement::CreateIndex),
            _ => Err(self.unexpected("TABLE・INDEX・UNIQUE INDEXのいずれか")),
        }
    }

    /// `DROP`の直後を覗き見て、`DROP TABLE`と`DROP INDEX`(第24章)を振り分ける。
    fn parse_drop_statement(&mut self) -> DbResult<Statement> {
        match self.peek_nth_kind(1) {
            TokenKind::Keyword(Keyword::Table) => self.parse_drop_table_statement().map(Statement::DropTable),
            TokenKind::Keyword(Keyword::Index) => self.parse_drop_index_statement().map(Statement::DropIndex),
            _ => Err(self.unexpected("TABLE・INDEXのいずれか")),
        }
    }

    // ---- EXPLAIN ----

    /// `EXPLAIN [ANALYZE] <SELECT|INSERT INTO|UPDATE|DELETE FROM>`を解析する。
    ///
    /// 対象を`SELECT`・`INSERT INTO`・`UPDATE`・`DELETE FROM`の4種類に限るのは、
    /// `EXPLAIN`が見せるのはLogical Plan/Physical Planに変換できる文だけだから
    /// である(第19章)。`CREATE TABLE`・`DROP TABLE`はどちらの計画も経由しない
    /// (`Database::execute_create_table`等を直接呼ぶ)ため対象に含めない。
    /// `EXPLAIN EXPLAIN ...`のような入れ子も、この関数が生の`parse_statement`
    /// ではなく`SELECT`等4種の解析関数だけを呼ぶことで、構文の時点で拒否される。
    ///
    /// `EXPLAIN`の直後に`ANALYZE`キーワードが続けば(第27章、PostgreSQLの
    /// `EXPLAIN ANALYZE`に相当)、対象の文を実際に実行して実測行数も見せる
    /// `ExplainStatement::analyze = true`として解析する。`ANALYZE`が無ければ
    /// 従来どおり推定のみの`EXPLAIN`(`analyze = false`)になる。
    fn parse_explain_statement(&mut self) -> DbResult<ExplainStatement> {
        let start = self.expect_keyword(Keyword::Explain, "EXPLAIN")?.start;

        let analyze = if let TokenKind::Keyword(Keyword::Analyze) = self.peek_kind() {
            self.advance();
            true
        } else {
            false
        };

        let statement = match self.peek_kind() {
            TokenKind::Keyword(Keyword::Select) => self.parse_select_statement().map(|s| Statement::Select(Box::new(s)))?,
            TokenKind::Keyword(Keyword::Insert) => self.parse_insert_statement().map(Statement::Insert)?,
            TokenKind::Keyword(Keyword::Update) => self.parse_update_statement().map(Statement::Update)?,
            TokenKind::Keyword(Keyword::Delete) => self.parse_delete_statement().map(Statement::Delete)?,
            _ => return Err(self.unexpected("SELECT・INSERT INTO・UPDATE・DELETE FROMのいずれか")),
        };

        let end = statement.span().end;
        Ok(ExplainStatement { statement: Box::new(statement), analyze, span: Span { start, end } })
    }

    // ---- ANALYZE(第27章) ----

    /// `ANALYZE [テーブル名]`を解析する。テーブル名を省略した場合は`table`が
    /// `None`になり、`Database::execute`が登録済みの全テーブルを対象にする。
    fn parse_analyze_statement(&mut self) -> DbResult<AnalyzeStatement> {
        let start = self.expect_keyword(Keyword::Analyze, "ANALYZE")?.start;

        let table = match self.peek_kind() {
            TokenKind::Ident(_) => Some(self.expect_ident()?),
            _ => None,
        };
        let end = table.as_ref().map(|t| t.span.end).unwrap_or(start + "ANALYZE".len());

        Ok(AnalyzeStatement { table, span: Span::new(start, end) })
    }

    // ---- SELECT ----

    fn parse_select_statement(&mut self) -> DbResult<SelectStatement> {
        let start = self.expect_keyword(Keyword::Select, "SELECT")?.start;

        let distinct = if let TokenKind::Keyword(Keyword::Distinct) = self.peek_kind() {
            self.advance();
            true
        } else {
            false
        };

        let mut items = vec![self.parse_select_item()?];
        while *self.peek_kind() == TokenKind::Comma {
            self.advance();
            items.push(self.parse_select_item()?);
        }
        let mut end = items.last().expect("直前にpushしたばかり").span().end;

        let from = if let TokenKind::Keyword(Keyword::From) = self.peek_kind() {
            self.advance();
            let table = self.expect_ident()?;
            let mut from_end = table.span.end;
            let alias = if let TokenKind::Keyword(Keyword::As) = self.peek_kind() {
                self.advance();
                let alias = self.expect_ident()?;
                from_end = alias.span.end;
                Some(alias)
            } else {
                None
            };

            let mut joins = Vec::new();
            while let Some(join) = self.parse_join_clause()? {
                from_end = join.span.end;
                joins.push(join);
            }

            end = from_end;
            Some(FromClause {
                span: Span::new(table.span.start, from_end),
                table,
                alias,
                joins,
            })
        } else {
            None
        };

        let where_clause = if let TokenKind::Keyword(Keyword::Where) = self.peek_kind() {
            self.advance();
            let expr = self.parse_expr(0)?;
            end = expr.span().end;
            Some(expr)
        } else {
            None
        };

        let group_by = if let TokenKind::Keyword(Keyword::Group) = self.peek_kind() {
            self.advance();
            self.expect_keyword(Keyword::By, "BY")?;
            let mut exprs = vec![self.parse_expr(0)?];
            while *self.peek_kind() == TokenKind::Comma {
                self.advance();
                exprs.push(self.parse_expr(0)?);
            }
            end = exprs.last().expect("直前にpushしたばかり").span().end;
            exprs
        } else {
            Vec::new()
        };

        let having = if let TokenKind::Keyword(Keyword::Having) = self.peek_kind() {
            self.advance();
            let expr = self.parse_expr(0)?;
            end = expr.span().end;
            Some(expr)
        } else {
            None
        };

        let order_by = if let TokenKind::Keyword(Keyword::Order) = self.peek_kind() {
            self.advance();
            self.expect_keyword(Keyword::By, "BY")?;
            let mut items = vec![self.parse_order_by_item()?];
            while *self.peek_kind() == TokenKind::Comma {
                self.advance();
                items.push(self.parse_order_by_item()?);
            }
            end = items.last().expect("直前にpushしたばかり").span.end;
            items
        } else {
            Vec::new()
        };

        let limit = if let TokenKind::Keyword(Keyword::Limit) = self.peek_kind() {
            self.advance();
            let expr = self.parse_expr(0)?;
            end = expr.span().end;
            Some(expr)
        } else {
            None
        };

        let offset = if let TokenKind::Keyword(Keyword::Offset) = self.peek_kind() {
            self.advance();
            let expr = self.parse_expr(0)?;
            end = expr.span().end;
            Some(expr)
        } else {
            None
        };

        Ok(SelectStatement {
            distinct,
            items,
            from,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
            span: Span::new(start, end),
        })
    }

    /// `ORDER BY`の要素1個(`<式> [ASC|DESC]`)を読む。
    fn parse_order_by_item(&mut self) -> DbResult<OrderByItem> {
        let expr = self.parse_expr(0)?;
        let mut end = expr.span().end;
        let desc = match self.peek_kind() {
            TokenKind::Keyword(Keyword::Asc) => {
                end = self.advance().span.end;
                false
            }
            TokenKind::Keyword(Keyword::Desc) => {
                end = self.advance().span.end;
                true
            }
            _ => false,
        };
        let span = Span::new(expr.span().start, end);
        Ok(OrderByItem { expr, desc, span })
    }

    /// `FROM`の直後、または直前の`JOIN`の直後に続く`[INNER] JOIN <table>
    /// [AS <alias>] ON <expr>`を1個読む(第22章)。次のトークンが`JOIN`・
    /// `INNER`のどちらでもなければ、`JOIN`の連鎖はここで終わりなので`None`を
    /// 返す(呼び出し側の`while let`が抜ける)。
    ///
    /// `JOIN`単独は`INNER JOIN`の別名として受理する。標準SQLも`JOIN`だけを
    /// 書いた場合は`INNER JOIN`とみなす規則なので、`Parser`の時点で
    /// `JoinKind::Inner`へ統一してしまい、`Binder`以降はこの2つの書き方の
    /// 違いを一切意識しない。
    fn parse_join_clause(&mut self) -> DbResult<Option<JoinClause>> {
        let start = match self.peek_kind() {
            TokenKind::Keyword(Keyword::Join) => self.peek().span.start,
            TokenKind::Keyword(Keyword::Inner) => self.peek().span.start,
            _ => return Ok(None),
        };

        if let TokenKind::Keyword(Keyword::Inner) = self.peek_kind() {
            self.advance();
            self.expect_keyword(Keyword::Join, "JOIN")?;
        } else {
            self.advance();
        }

        let table = self.expect_ident()?;
        let alias = if let TokenKind::Keyword(Keyword::As) = self.peek_kind() {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect_keyword(Keyword::On, "ON")?;
        let on = self.parse_expr(0)?;
        let end = on.span().end;

        Ok(Some(JoinClause {
            kind: JoinKind::Inner,
            table,
            alias,
            on,
            span: Span::new(start, end),
        }))
    }

    /// `*`は`parse_expr`(乗算の`*`と同じToken)に渡すと式として解釈できないため、
    /// ここで先読みして`SelectItem::Wildcard`に振り分ける。
    fn parse_select_item(&mut self) -> DbResult<SelectItem> {
        if *self.peek_kind() == TokenKind::Star {
            let span = self.advance().span;
            return Ok(SelectItem::Wildcard { span });
        }
        let expr = self.parse_expr(0)?;
        let span = expr.span();
        Ok(SelectItem::Expr { expr, span })
    }

    // ---- CREATE TABLE ----

    fn parse_create_table_statement(&mut self) -> DbResult<CreateTableStatement> {
        let start = self.expect_keyword(Keyword::Create, "CREATE")?.start;
        self.expect_keyword(Keyword::Table, "TABLE")?;
        let table = self.expect_ident()?;
        self.expect_punct(TokenKind::LParen, "(")?;

        let mut columns = vec![self.parse_column_def()?];
        while *self.peek_kind() == TokenKind::Comma {
            self.advance();
            columns.push(self.parse_column_def()?);
        }

        let end = self.expect_punct(TokenKind::RParen, ")")?.end;

        Ok(CreateTableStatement {
            table,
            columns,
            span: Span::new(start, end),
        })
    }

    /// 列定義の型名に続く列制約(`NOT NULL`・`PRIMARY KEY`・`UNIQUE`)を、
    /// 現れる限り任意の順序・任意の個数だけ読む(第20章)。同じ制約が複数回
    /// 現れても構文としては受理し、`Database::execute_create_table`が実際に
    /// 意味のある組み合わせかどうかを検査する(複合`PRIMARY KEY`の拒否など)。
    fn parse_column_def(&mut self) -> DbResult<ColumnDef> {
        let name = self.expect_ident()?;
        let type_name = self.expect_ident()?;
        let mut end = type_name.span.end;

        let mut not_null = false;
        let mut primary_key = false;
        let mut unique = false;

        loop {
            match self.peek_kind() {
                TokenKind::Keyword(Keyword::Not) => {
                    self.advance();
                    end = self.expect_keyword(Keyword::Null, "NULL")?.end;
                    not_null = true;
                }
                TokenKind::Keyword(Keyword::Primary) => {
                    self.advance();
                    end = self.expect_keyword(Keyword::Key, "KEY")?.end;
                    primary_key = true;
                }
                TokenKind::Keyword(Keyword::Unique) => {
                    end = self.advance().span.end;
                    unique = true;
                }
                _ => break,
            }
        }

        Ok(ColumnDef {
            span: Span::new(name.span.start, end),
            name,
            type_name,
            not_null,
            primary_key,
            unique,
        })
    }

    // ---- DROP TABLE ----

    fn parse_drop_table_statement(&mut self) -> DbResult<DropTableStatement> {
        let start = self.expect_keyword(Keyword::Drop, "DROP")?.start;
        self.expect_keyword(Keyword::Table, "TABLE")?;
        let table = self.expect_ident()?;
        let end = table.span.end;

        Ok(DropTableStatement {
            table,
            span: Span::new(start, end),
        })
    }

    // ---- CREATE INDEX / DROP INDEX(第24章) ----

    /// `CREATE [UNIQUE] INDEX <index> ON <table> (<column>)`を解析する。
    /// `unique`は`parse_create_statement`が`CREATE`の2個先を覗いて渡す
    /// (`UNIQUE`キーワードを読んでいるかどうか)。
    fn parse_create_index_statement(&mut self, unique: bool) -> DbResult<CreateIndexStatement> {
        let start = self.expect_keyword(Keyword::Create, "CREATE")?.start;
        if unique {
            self.expect_keyword(Keyword::Unique, "UNIQUE")?;
        }
        self.expect_keyword(Keyword::Index, "INDEX")?;
        let index = self.expect_ident()?;
        self.expect_keyword(Keyword::On, "ON")?;
        let table = self.expect_ident()?;
        self.expect_punct(TokenKind::LParen, "(")?;
        let column = self.expect_ident()?;
        let end = self.expect_punct(TokenKind::RParen, ")")?.end;

        Ok(CreateIndexStatement {
            unique,
            index,
            table,
            column,
            span: Span::new(start, end),
        })
    }

    fn parse_drop_index_statement(&mut self) -> DbResult<DropIndexStatement> {
        let start = self.expect_keyword(Keyword::Drop, "DROP")?.start;
        self.expect_keyword(Keyword::Index, "INDEX")?;
        let index = self.expect_ident()?;
        let end = index.span.end;

        Ok(DropIndexStatement {
            index,
            span: Span::new(start, end),
        })
    }

    // ---- INSERT INTO ----

    fn parse_insert_statement(&mut self) -> DbResult<InsertStatement> {
        let start = self.expect_keyword(Keyword::Insert, "INSERT")?.start;
        self.expect_keyword(Keyword::Into, "INTO")?;
        let table = self.expect_ident()?;

        let columns = if *self.peek_kind() == TokenKind::LParen {
            self.advance();
            let mut columns = vec![self.expect_ident()?];
            while *self.peek_kind() == TokenKind::Comma {
                self.advance();
                columns.push(self.expect_ident()?);
            }
            self.expect_punct(TokenKind::RParen, ")")?;
            Some(columns)
        } else {
            None
        };

        self.expect_keyword(Keyword::Values, "VALUES")?;

        let (first_row, mut end) = self.parse_values_row()?;
        let mut rows = vec![first_row];
        while *self.peek_kind() == TokenKind::Comma {
            self.advance();
            let (row, row_end) = self.parse_values_row()?;
            rows.push(row);
            end = row_end;
        }

        Ok(InsertStatement {
            table,
            columns,
            rows,
            span: Span::new(start, end),
        })
    }

    /// `VALUES`の1行分、`(<式>, <式>, ...)`を読む。式の並びと、閉じ括弧の
    /// 終端位置(`InsertStatement`全体のSpanを組み立てるのに使う)を返す。
    fn parse_values_row(&mut self) -> DbResult<(Vec<Expr>, usize)> {
        self.expect_punct(TokenKind::LParen, "(")?;

        let mut values = vec![self.parse_expr(0)?];
        while *self.peek_kind() == TokenKind::Comma {
            self.advance();
            values.push(self.parse_expr(0)?);
        }

        let end = self.expect_punct(TokenKind::RParen, ")")?.end;
        Ok((values, end))
    }

    // ---- UPDATE ----

    fn parse_update_statement(&mut self) -> DbResult<UpdateStatement> {
        let start = self.expect_keyword(Keyword::Update, "UPDATE")?.start;
        let table = self.expect_ident()?;
        self.expect_keyword(Keyword::Set, "SET")?;

        let mut assignments = vec![self.parse_assignment()?];
        while *self.peek_kind() == TokenKind::Comma {
            self.advance();
            assignments.push(self.parse_assignment()?);
        }
        let mut end = assignments.last().expect("直前にpushしたばかり").span.end;

        let where_clause = if let TokenKind::Keyword(Keyword::Where) = self.peek_kind() {
            self.advance();
            let expr = self.parse_expr(0)?;
            end = expr.span().end;
            Some(expr)
        } else {
            None
        };

        Ok(UpdateStatement {
            table,
            assignments,
            where_clause,
            span: Span::new(start, end),
        })
    }

    fn parse_assignment(&mut self) -> DbResult<Assignment> {
        let column = self.expect_ident()?;
        self.expect_punct(TokenKind::Eq, "=")?;
        let value = self.parse_expr(0)?;
        let span = Span::new(column.span.start, value.span().end);
        Ok(Assignment {
            column,
            value,
            span,
        })
    }

    // ---- DELETE FROM ----

    fn parse_delete_statement(&mut self) -> DbResult<DeleteStatement> {
        let start = self.expect_keyword(Keyword::Delete, "DELETE")?.start;
        self.expect_keyword(Keyword::From, "FROM")?;
        let table = self.expect_ident()?;
        let mut end = table.span.end;

        let where_clause = if let TokenKind::Keyword(Keyword::Where) = self.peek_kind() {
            self.advance();
            let expr = self.parse_expr(0)?;
            end = expr.span().end;
            Some(expr)
        } else {
            None
        };

        Ok(DeleteStatement {
            table,
            where_clause,
            span: Span::new(start, end),
        })
    }

    // ---- 式(Pratt Parser) ----

    /// `min_bp`以上のbinding powerを持つ演算子だけを、このループが自分で消費する。
    /// `min_bp`未満の演算子に出会ったら、それは呼び出し元(より外側の演算子や
    /// 文レベルの解析)が消費すべきものなので、消費せずに返る。
    fn parse_expr(&mut self, min_bp: u8) -> DbResult<Expr> {
        let mut lhs = self.parse_prefix()?;

        loop {
            if let TokenKind::Keyword(Keyword::Is) = self.peek_kind() {
                const IS_NULL_BP: u8 = 6;
                if IS_NULL_BP < min_bp {
                    break;
                }
                self.advance();
                let negated = if let TokenKind::Keyword(Keyword::Not) = self.peek_kind() {
                    self.advance();
                    true
                } else {
                    false
                };
                let end = self.expect_keyword(Keyword::Null, "NULL")?.end;
                let span = Span::new(lhs.span().start, end);
                lhs = Expr::IsNull {
                    expr: Box::new(lhs),
                    negated,
                    span,
                };
                continue;
            }

            let Some((op, lbp, rbp)) = infix_binding_power(self.peek_kind()) else {
                break;
            };
            if lbp < min_bp {
                break;
            }
            self.advance();
            let rhs = self.parse_expr(rbp)?;
            let span = Span::new(lhs.span().start, rhs.span().end);
            lhs = Expr::BinaryOp {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }

        Ok(lhs)
    }

    /// 前置演算子(`NOT`、単項`-`)を読み、無ければ`parse_primary`に委ねる。
    fn parse_prefix(&mut self) -> DbResult<Expr> {
        const NOT_RBP: u8 = 5;
        const NEGATE_RBP: u8 = 12;

        match self.peek_kind() {
            TokenKind::Keyword(Keyword::Not) => {
                let start = self.advance().span.start;
                let operand = self.parse_expr(NOT_RBP)?;
                let span = Span::new(start, operand.span().end);
                Ok(Expr::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(operand),
                    span,
                })
            }
            TokenKind::Minus => {
                let start = self.advance().span.start;

                // `-`の直後が整数リテラルなら、`UnaryOp`を経由せずその場で
                // 符号付きの`Expr::IntLiteral`へ組み立てる。`i64::MIN`の絶対値
                // (`9223372036854775808`)は正の`i64`として表現できないため、
                // 一度正の`i64`にしてから`checked_neg`する経路(`UnaryOp` +
                // `eval_unary`)では`-9223372036854775808`を書けない。ここで
                // 符号と絶対値を同時に見て`i64`へ変換することで、この値だけを
                // 特別扱いせずに済む。
                if let TokenKind::IntLiteral(magnitude) = *self.peek_kind() {
                    let magnitude_token = self.advance();
                    let value = negate_u64_to_i64(magnitude).ok_or_else(|| {
                        self.error_at(
                            magnitude_token.span,
                            format!("整数リテラルの範囲を超えています: -{magnitude}"),
                        )
                    })?;
                    let span = Span::new(start, magnitude_token.span.end);
                    return Ok(Expr::IntLiteral { value, span });
                }

                let operand = self.parse_expr(NEGATE_RBP)?;
                let span = Span::new(start, operand.span().end);
                Ok(Expr::UnaryOp {
                    op: UnaryOperator::Negate,
                    expr: Box::new(operand),
                    span,
                })
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> DbResult<Expr> {
        match self.peek_kind().clone() {
            TokenKind::IntLiteral(magnitude) => {
                let token = self.advance();
                // 符号の無い整数リテラルは非負の`i64`にしか変換できない
                // (`i64::MIN`は符号付きの経路、`parse_prefix`のMinus分岐でのみ
                // 書ける)。`i64::MAX`(`9223372036854775807`)を超える場合は
                // ここで構文エラーにする。
                let value = i64::try_from(magnitude).map_err(|_| {
                    self.error_at(
                        token.span,
                        format!("整数リテラルの範囲を超えています: {magnitude}"),
                    )
                })?;
                Ok(Expr::IntLiteral { value, span: token.span })
            }
            TokenKind::StringLiteral(value) => {
                let span = self.advance().span;
                Ok(Expr::StringLiteral { value, span })
            }
            TokenKind::Keyword(Keyword::True) => {
                let span = self.advance().span;
                Ok(Expr::BoolLiteral { value: true, span })
            }
            TokenKind::Keyword(Keyword::False) => {
                let span = self.advance().span;
                Ok(Expr::BoolLiteral { value: false, span })
            }
            TokenKind::Keyword(Keyword::Null) => {
                let span = self.advance().span;
                Ok(Expr::NullLiteral { span })
            }
            TokenKind::Ident(name) => {
                let start_span = self.advance().span;
                if *self.peek_kind() == TokenKind::Dot {
                    // `u.id`。関数呼び出しは`schema.func()`のような修飾名を
                    // このSQLサブセットでは扱わないため、`.`を見た時点で
                    // 列参照だと確定できる。
                    self.advance();
                    let column = self.expect_ident()?;
                    let span = Span::new(start_span.start, column.span.end);
                    Ok(Expr::ColumnRef {
                        qualifier: Some(Ident {
                            name,
                            span: start_span,
                        }),
                        name: column.name,
                        span,
                    })
                } else if *self.peek_kind() == TokenKind::LParen {
                    self.parse_function_call(name, start_span)
                } else {
                    Ok(Expr::ColumnRef {
                        qualifier: None,
                        name,
                        span: start_span,
                    })
                }
            }
            TokenKind::LParen => {
                let start = self.advance().span.start;
                let inner = self.parse_expr(0)?;
                let end = self.expect_punct(TokenKind::RParen, ")")?.end;
                Ok(Expr::Paren {
                    expr: Box::new(inner),
                    span: Span::new(start, end),
                })
            }
            TokenKind::Keyword(Keyword::Cast) => self.parse_cast(),
            _ => Err(self.unexpected("式")),
        }
    }

    /// `CAST(expr AS type_name)`を読む。`type_name`は`CREATE TABLE`の列定義と
    /// 同じく、型名の一覧と突き合わせずに`Ident`のまま保持する。
    fn parse_cast(&mut self) -> DbResult<Expr> {
        let start = self.expect_keyword(Keyword::Cast, "CAST")?.start;
        self.expect_punct(TokenKind::LParen, "(")?;
        let expr = self.parse_expr(0)?;
        self.expect_keyword(Keyword::As, "AS")?;
        let type_name = self.expect_ident()?;
        let end = self.expect_punct(TokenKind::RParen, ")")?.end;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            type_name,
            span: Span::new(start, end),
        })
    }

    /// `name(...)`を読む。`name`が集約関数(`COUNT`・`SUM`・`MIN`・`MAX`、
    /// 大文字小文字を無視)の名前と一致する場合は[`parse_aggregate_call`]に
    /// 委ね、それ以外はScalar Functionの呼び出しとして読む。
    ///
    /// 集約関数を予約語にせず識別子のまま特別扱いしているのは、`abs`・
    /// `length`(第8章)と同じくScalar Functionの名前が予約語ではない設計を
    /// 崩さないためである。`count`という名前を列名やテーブル名として使いたい
    /// 場合は、この関数を経由しない(`(`が続かない)限り、これまでどおり
    /// 識別子として解決される。
    fn parse_function_call(&mut self, name: String, name_span: Span) -> DbResult<Expr> {
        if let Some(func) = AggregateFunc::from_name(&name) {
            return self.parse_aggregate_call(func, name_span);
        }

        self.expect_punct(TokenKind::LParen, "(")?;

        let mut args = Vec::new();
        if *self.peek_kind() != TokenKind::RParen {
            args.push(self.parse_expr(0)?);
            while *self.peek_kind() == TokenKind::Comma {
                self.advance();
                args.push(self.parse_expr(0)?);
            }
        }

        let end = self.expect_punct(TokenKind::RParen, ")")?.end;
        Ok(Expr::FunctionCall {
            name,
            args,
            span: Span::new(name_span.start, end),
        })
    }

    /// 集約関数呼び出し`COUNT(*)` / `COUNT(<式>)` / `SUM(<式>)` / `MIN(<式>)` /
    /// `MAX(<式>)`を読む。`*`が引数として書けるのは`COUNT`だけである
    /// (`SUM(*)`のような構文はここで拒否する)。
    fn parse_aggregate_call(&mut self, func: AggregateFunc, name_span: Span) -> DbResult<Expr> {
        self.expect_punct(TokenKind::LParen, "(")?;

        let arg = if func == AggregateFunc::Count && *self.peek_kind() == TokenKind::Star {
            self.advance();
            None
        } else if *self.peek_kind() == TokenKind::Star {
            return Err(self.unexpected(&format!("{}の引数には式が必要です(*は使えません)", func.name())));
        } else {
            Some(Box::new(self.parse_expr(0)?))
        };

        let end = self.expect_punct(TokenKind::RParen, ")")?.end;
        Ok(Expr::Aggregate {
            func,
            arg,
            span: Span::new(name_span.start, end),
        })
    }
}

/// 符号の無い絶対値`magnitude`に負符号を適用し、`i64`へ変換する。
///
/// `i64`の範囲(`-9223372036854775808..=9223372036854775807`)に収まらない
/// 場合は`None`を返す。`i128`へ一度持ち上げてから引き算することで、
/// `magnitude`が`u64::MAX`まで取りうる値でもオーバーフローせずに判定できる。
fn negate_u64_to_i64(magnitude: u64) -> Option<i64> {
    let negated = -(magnitude as i128);
    i64::try_from(negated).ok()
}

/// 二項演算子のbinding power(左結合力, 右結合力)。
///
/// 左結合力(`lbp`)は「この演算子を今のループで消費してよいか」の判定に、
/// 右結合力(`rbp`)は「右辺を解析する再帰呼び出しに渡す`min_bp`」に使う。
/// 左結合の演算子は`rbp = lbp + 1`にすることで、`1 - 2 - 3`が`(1 - 2) - 3`に
/// なる(右辺の再帰呼び出しが同じ優先順位の演算子を自分では取り込まない)。
///
/// 優先順位は低い順に`OR`(1) < `AND`(3) < 比較(6) < `+` `-`(8) < `*` `/`(10)。
/// `NOT`(前置, rbp=5)と`IS [NOT] NULL`(lbp=6)は`parse_expr`側で扱う。
fn infix_binding_power(kind: &TokenKind) -> Option<(BinaryOperator, u8, u8)> {
    let (op, lbp) = match kind {
        TokenKind::Keyword(Keyword::Or) => (BinaryOperator::Or, 1),
        TokenKind::Keyword(Keyword::And) => (BinaryOperator::And, 3),
        TokenKind::Eq => (BinaryOperator::Eq, 6),
        TokenKind::NotEq => (BinaryOperator::NotEq, 6),
        TokenKind::Lt => (BinaryOperator::Lt, 6),
        TokenKind::LtEq => (BinaryOperator::LtEq, 6),
        TokenKind::Gt => (BinaryOperator::Gt, 6),
        TokenKind::GtEq => (BinaryOperator::GtEq, 6),
        TokenKind::Plus => (BinaryOperator::Add, 8),
        TokenKind::Minus => (BinaryOperator::Subtract, 8),
        TokenKind::Star => (BinaryOperator::Multiply, 10),
        TokenKind::Slash => (BinaryOperator::Divide, 10),
        _ => return None,
    };
    Some((op, lbp, lbp + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Statement;

    fn parse_expr_only(sql: &str) -> Expr {
        match parse_statement(&format!("SELECT {sql}")).unwrap() {
            Statement::Select(select) => {
                assert_eq!(select.items.len(), 1, "式は1個だけのはず");
                match select.items.into_iter().next().unwrap() {
                    SelectItem::Expr { expr, .. } => expr,
                    SelectItem::Wildcard { .. } => panic!("式を期待したがWildcardが返った"),
                }
            }
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
    }

    fn int(v: i64) -> Box<Expr> {
        Box::new(Expr::IntLiteral {
            value: v,
            span: Span::new(0, 0),
        })
    }

    /// テストではSpanの中身までは比較しない(位置は別のテストで確認する)ので、
    /// すべてのSpanをダミー値に潰してから比較する。
    fn strip_spans(expr: Expr) -> Expr {
        let dummy = Span::new(0, 0);
        match expr {
            Expr::IntLiteral { value, .. } => Expr::IntLiteral { value, span: dummy },
            Expr::StringLiteral { value, .. } => Expr::StringLiteral { value, span: dummy },
            Expr::BoolLiteral { value, .. } => Expr::BoolLiteral { value, span: dummy },
            Expr::NullLiteral { .. } => Expr::NullLiteral { span: dummy },
            Expr::ColumnRef { qualifier, name, .. } => Expr::ColumnRef {
                qualifier: qualifier.map(|q| Ident { name: q.name, span: dummy }),
                name,
                span: dummy,
            },
            Expr::UnaryOp { op, expr, .. } => Expr::UnaryOp {
                op,
                expr: Box::new(strip_spans(*expr)),
                span: dummy,
            },
            Expr::BinaryOp { op, lhs, rhs, .. } => Expr::BinaryOp {
                op,
                lhs: Box::new(strip_spans(*lhs)),
                rhs: Box::new(strip_spans(*rhs)),
                span: dummy,
            },
            Expr::IsNull { expr, negated, .. } => Expr::IsNull {
                expr: Box::new(strip_spans(*expr)),
                negated,
                span: dummy,
            },
            Expr::FunctionCall { name, args, .. } => Expr::FunctionCall {
                name,
                args: args.into_iter().map(strip_spans).collect(),
                span: dummy,
            },
            Expr::Aggregate { func, arg, .. } => Expr::Aggregate {
                func,
                arg: arg.map(|expr| Box::new(strip_spans(*expr))),
                span: dummy,
            },
            Expr::Paren { expr, .. } => Expr::Paren {
                expr: Box::new(strip_spans(*expr)),
                span: dummy,
            },
            Expr::Cast { expr, type_name, .. } => Expr::Cast {
                expr: Box::new(strip_spans(*expr)),
                type_name: Ident {
                    name: type_name.name,
                    span: dummy,
                },
                span: dummy,
            },
        }
    }

    fn assert_expr_eq(sql: &str, expected: Expr) {
        assert_eq!(strip_spans(parse_expr_only(sql)), strip_spans(expected));
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        // `1 + 2 * 3`は`1 + (2 * 3)`であって`(1 + 2) * 3`ではない。
        assert_expr_eq(
            "1 + 2 * 3",
            Expr::BinaryOp {
                op: BinaryOperator::Add,
                lhs: int(1),
                rhs: Box::new(Expr::BinaryOp {
                    op: BinaryOperator::Multiply,
                    lhs: int(2),
                    rhs: int(3),
                    span: Span::new(0, 0),
                }),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn unary_minus_binds_tighter_than_addition() {
        // `-1 + 2`は`(-1) + 2`であって`-(1 + 2)`ではない。
        // `-`の直後が整数リテラルの場合、`UnaryOp::Negate`は経由せず符号込みの
        // `Expr::IntLiteral`へ直接組み立てる(`i64::MIN`を書けるようにする
        // ための変更。詳細は`parse_prefix`のコメント参照)。
        assert_expr_eq(
            "-1 + 2",
            Expr::BinaryOp {
                op: BinaryOperator::Add,
                lhs: int(-1),
                rhs: int(2),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn unary_minus_before_a_non_literal_still_uses_unary_op() {
        // `-`の直後がリテラル以外(括弧で囲まれた式など)の場合は、これまで
        // どおり`UnaryOp::Negate`を使う。
        assert_expr_eq(
            "-(1 + 2)",
            Expr::UnaryOp {
                op: UnaryOperator::Negate,
                expr: Box::new(Expr::Paren {
                    expr: Box::new(Expr::BinaryOp {
                        op: BinaryOperator::Add,
                        lhs: int(1),
                        rhs: int(2),
                        span: Span::new(0, 0),
                    }),
                    span: Span::new(0, 0),
                }),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn addition_is_left_associative() {
        // `1 + 2 + 3`は`(1 + 2) + 3`。
        assert_expr_eq(
            "1 + 2 + 3",
            Expr::BinaryOp {
                op: BinaryOperator::Add,
                lhs: Box::new(Expr::BinaryOp {
                    op: BinaryOperator::Add,
                    lhs: int(1),
                    rhs: int(2),
                    span: Span::new(0, 0),
                }),
                rhs: int(3),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parens_override_precedence() {
        // `(1 + 2) * 3`は括弧を優先し、加算が先に評価される木になる。
        assert_expr_eq(
            "(1 + 2) * 3",
            Expr::BinaryOp {
                op: BinaryOperator::Multiply,
                lhs: Box::new(Expr::Paren {
                    expr: Box::new(Expr::BinaryOp {
                        op: BinaryOperator::Add,
                        lhs: int(1),
                        rhs: int(2),
                        span: Span::new(0, 0),
                    }),
                    span: Span::new(0, 0),
                }),
                rhs: int(3),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn not_binds_looser_than_comparison() {
        // `NOT 1 = 2`は`NOT (1 = 2)`であって`(NOT 1) = 2`ではない。
        assert_expr_eq(
            "NOT 1 = 2",
            Expr::UnaryOp {
                op: UnaryOperator::Not,
                expr: Box::new(Expr::BinaryOp {
                    op: BinaryOperator::Eq,
                    lhs: int(1),
                    rhs: int(2),
                    span: Span::new(0, 0),
                }),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn not_binds_tighter_than_and() {
        // `NOT true AND false`は`(NOT true) AND false`であって`NOT (true AND false)`ではない。
        assert_expr_eq(
            "NOT true AND false",
            Expr::BinaryOp {
                op: BinaryOperator::And,
                lhs: Box::new(Expr::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(Expr::BoolLiteral {
                        value: true,
                        span: Span::new(0, 0),
                    }),
                    span: Span::new(0, 0),
                }),
                rhs: Box::new(Expr::BoolLiteral {
                    value: false,
                    span: Span::new(0, 0),
                }),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // `true OR false AND false`は`true OR (false AND false)`。
        assert_expr_eq(
            "true OR false AND false",
            Expr::BinaryOp {
                op: BinaryOperator::Or,
                lhs: Box::new(Expr::BoolLiteral {
                    value: true,
                    span: Span::new(0, 0),
                }),
                rhs: Box::new(Expr::BinaryOp {
                    op: BinaryOperator::And,
                    lhs: Box::new(Expr::BoolLiteral {
                        value: false,
                        span: Span::new(0, 0),
                    }),
                    rhs: Box::new(Expr::BoolLiteral {
                        value: false,
                        span: Span::new(0, 0),
                    }),
                    span: Span::new(0, 0),
                }),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_is_null_and_is_not_null() {
        assert_expr_eq(
            "1 IS NULL",
            Expr::IsNull {
                expr: int(1),
                negated: false,
                span: Span::new(0, 0),
            },
        );
        assert_expr_eq(
            "1 IS NOT NULL",
            Expr::IsNull {
                expr: int(1),
                negated: true,
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_function_call_with_multiple_args() {
        assert_expr_eq(
            "f(1, 2)",
            Expr::FunctionCall {
                name: "f".to_string(),
                args: vec![*int(1), *int(2)],
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_function_call_with_no_args() {
        assert_expr_eq(
            "f()",
            Expr::FunctionCall {
                name: "f".to_string(),
                args: vec![],
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_cast() {
        assert_expr_eq(
            "CAST(1 AS TEXT)",
            Expr::Cast {
                expr: int(1),
                type_name: Ident {
                    name: "TEXT".to_string(),
                    span: Span::new(0, 0),
                },
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_column_ref() {
        assert_expr_eq(
            "id",
            Expr::ColumnRef {
                qualifier: None,
                name: "id".to_string(),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_qualified_column_ref() {
        assert_expr_eq(
            "u.id",
            Expr::ColumnRef {
                qualifier: Some(Ident {
                    name: "u".to_string(),
                    span: Span::new(0, 0),
                }),
                name: "id".to_string(),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_from_with_alias() {
        let statement = parse_statement("SELECT u.id FROM users AS u").unwrap();
        match statement {
            Statement::Select(select) => {
                let from = select.from.expect("FROMがあるはず");
                assert_eq!(from.table.name, "users");
                assert_eq!(from.alias.map(|a| a.name), Some("u".to_string()));
            }
            other => panic!("Statement::Selectを期待したが{other:?}が返った"),
        }
    }

    // ---- JOIN(第22章) ----

    #[test]
    fn parses_inner_join_with_on_clause() {
        let statement = parse_statement("SELECT a.x FROM a INNER JOIN b ON a.id = b.id").unwrap();
        match statement {
            Statement::Select(select) => {
                let from = select.from.expect("FROMがあるはず");
                assert_eq!(from.joins.len(), 1);
                assert_eq!(from.joins[0].kind, JoinKind::Inner);
                assert_eq!(from.joins[0].table.name, "b");
            }
            other => panic!("Statement::Selectを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn join_alone_is_treated_as_inner_join() {
        let statement = parse_statement("SELECT a.x FROM a JOIN b ON a.id = b.id").unwrap();
        match statement {
            Statement::Select(select) => {
                assert_eq!(select.from.unwrap().joins[0].kind, JoinKind::Inner);
            }
            other => panic!("Statement::Selectを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_join_with_alias() {
        let statement = parse_statement("SELECT x.id FROM a AS x JOIN b AS y ON x.id = y.id").unwrap();
        match statement {
            Statement::Select(select) => {
                let from = select.from.unwrap();
                assert_eq!(from.alias.map(|a| a.name), Some("x".to_string()));
                assert_eq!(from.joins[0].alias.as_ref().map(|a| a.name.as_str()), Some("y"));
            }
            other => panic!("Statement::Selectを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_multiple_chained_joins() {
        let statement =
            parse_statement("SELECT a.x FROM a JOIN b ON a.id = b.id JOIN c ON b.id = c.id").unwrap();
        match statement {
            Statement::Select(select) => {
                let from = select.from.unwrap();
                assert_eq!(from.joins.len(), 2);
                assert_eq!(from.joins[0].table.name, "b");
                assert_eq!(from.joins[1].table.name, "c");
            }
            other => panic!("Statement::Selectを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn join_without_on_is_a_syntax_error() {
        let err = parse_statement("SELECT a.x FROM a JOIN b").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn comma_separated_from_is_still_rejected() {
        // カンマ結合(`FROM a, b`)はこの章では対応しない(本文の解説を参照)。
        let err = parse_statement("SELECT a.x FROM a, b").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn parses_select_with_multiple_items() {
        let statement = parse_statement("SELECT 1, 2 + 3").unwrap();
        match statement {
            Statement::Select(select) => assert_eq!(select.items.len(), 2),
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_select_with_from_and_where() {
        let statement = parse_statement("SELECT id FROM users WHERE id = 1").unwrap();
        match statement {
            Statement::Select(select) => {
                assert_eq!(select.from.map(|t| t.table.name), Some("users".to_string()));
                assert!(select.where_clause.is_some());
            }
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_create_table() {
        let statement =
            parse_statement("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)").unwrap();
        match statement {
            Statement::CreateTable(create) => {
                assert_eq!(create.table.name, "users");
                assert_eq!(create.columns.len(), 2);
                assert_eq!(create.columns[0].name.name, "id");
                assert_eq!(create.columns[0].type_name.name, "BIGINT");
                assert!(create.columns[0].not_null);
                assert_eq!(create.columns[1].name.name, "name");
                assert!(!create.columns[1].not_null);
            }
            other => panic!("CREATE TABLE文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_primary_key_and_unique_column_constraints() {
        let statement = parse_statement(
            "CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        )
        .unwrap();
        match statement {
            Statement::CreateTable(create) => {
                assert!(create.columns[0].primary_key);
                assert!(!create.columns[0].unique);
                assert!(create.columns[1].unique);
                assert!(!create.columns[1].primary_key);
                assert!(!create.columns[2].primary_key);
                assert!(!create.columns[2].unique);
            }
            other => panic!("CREATE TABLE文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_column_constraints_in_any_order() {
        // `NOT NULL`・`PRIMARY KEY`・`UNIQUE`はどの順序で書いても構文として
        // 受理する。
        let statement =
            parse_statement("CREATE TABLE t (id BIGINT UNIQUE NOT NULL PRIMARY KEY)").unwrap();
        match statement {
            Statement::CreateTable(create) => {
                assert!(create.columns[0].not_null);
                assert!(create.columns[0].primary_key);
                assert!(create.columns[0].unique);
            }
            other => panic!("CREATE TABLE文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_drop_table() {
        let statement = parse_statement("DROP TABLE users").unwrap();
        match statement {
            Statement::DropTable(drop) => assert_eq!(drop.table.name, "users"),
            other => panic!("DROP TABLE文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_create_index() {
        let statement = parse_statement("CREATE INDEX idx_users_id ON users (id)").unwrap();
        match statement {
            Statement::CreateIndex(create) => {
                assert!(!create.unique);
                assert_eq!(create.index.name, "idx_users_id");
                assert_eq!(create.table.name, "users");
                assert_eq!(create.column.name, "id");
            }
            other => panic!("CREATE INDEX文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_create_unique_index() {
        let statement = parse_statement("CREATE UNIQUE INDEX idx_users_email ON users (email)").unwrap();
        match statement {
            Statement::CreateIndex(create) => assert!(create.unique),
            other => panic!("CREATE UNIQUE INDEX文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_drop_index() {
        let statement = parse_statement("DROP INDEX idx_users_id").unwrap();
        match statement {
            Statement::DropIndex(drop) => assert_eq!(drop.index.name, "idx_users_id"),
            other => panic!("DROP INDEX文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_insert() {
        let statement = parse_statement("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        match statement {
            Statement::Insert(insert) => {
                assert_eq!(insert.table.name, "users");
                assert!(insert.columns.is_none());
                assert_eq!(insert.rows.len(), 1);
                assert_eq!(insert.rows[0].len(), 2);
            }
            other => panic!("INSERT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_insert_with_multiple_rows() {
        let statement =
            parse_statement("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')").unwrap();
        match statement {
            Statement::Insert(insert) => {
                assert_eq!(insert.rows.len(), 2);
                assert_eq!(insert.rows[1].len(), 2);
            }
            other => panic!("INSERT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_insert_with_explicit_columns() {
        let statement =
            parse_statement("INSERT INTO users (id, name) VALUES (1, 'Alice')").unwrap();
        match statement {
            Statement::Insert(insert) => {
                let columns = insert.columns.expect("列名を明示したはず");
                assert_eq!(
                    columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                    vec!["id", "name"]
                );
            }
            other => panic!("INSERT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_update() {
        let statement =
            parse_statement("UPDATE users SET name = 'Bob', id = id + 1 WHERE id = 1").unwrap();
        match statement {
            Statement::Update(update) => {
                assert_eq!(update.table.name, "users");
                assert_eq!(update.assignments.len(), 2);
                assert_eq!(update.assignments[0].column.name, "name");
                assert!(update.where_clause.is_some());
            }
            other => panic!("UPDATE文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_update_without_where() {
        let statement = parse_statement("UPDATE users SET name = 'Bob'").unwrap();
        match statement {
            Statement::Update(update) => assert!(update.where_clause.is_none()),
            other => panic!("UPDATE文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_delete() {
        let statement = parse_statement("DELETE FROM users WHERE id = 1").unwrap();
        match statement {
            Statement::Delete(delete) => {
                assert_eq!(delete.table.name, "users");
                assert!(delete.where_clause.is_some());
            }
            other => panic!("DELETE文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_delete_without_where() {
        let statement = parse_statement("DELETE FROM users").unwrap();
        match statement {
            Statement::Delete(delete) => assert!(delete.where_clause.is_none()),
            other => panic!("DELETE文を期待したが{other:?}が返った"),
        }
    }

    // ---- EXPLAIN ----

    #[test]
    fn parses_explain_select() {
        let statement = parse_statement("EXPLAIN SELECT id FROM users").unwrap();
        match statement {
            Statement::Explain(explain) => assert!(matches!(*explain.statement, Statement::Select(_))),
            other => panic!("EXPLAIN文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_explain_insert() {
        let statement = parse_statement("EXPLAIN INSERT INTO users VALUES (1)").unwrap();
        match statement {
            Statement::Explain(explain) => assert!(matches!(*explain.statement, Statement::Insert(_))),
            other => panic!("EXPLAIN文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_explain_update() {
        let statement = parse_statement("EXPLAIN UPDATE users SET id = 1").unwrap();
        match statement {
            Statement::Explain(explain) => assert!(matches!(*explain.statement, Statement::Update(_))),
            other => panic!("EXPLAIN文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_explain_delete() {
        let statement = parse_statement("EXPLAIN DELETE FROM users").unwrap();
        match statement {
            Statement::Explain(explain) => assert!(matches!(*explain.statement, Statement::Delete(_))),
            other => panic!("EXPLAIN文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn explain_span_covers_the_keyword_through_the_inner_statement() {
        let statement = parse_statement("EXPLAIN SELECT id FROM users").unwrap();
        let Statement::Explain(explain) = statement else {
            panic!("EXPLAIN文を期待した");
        };
        assert_eq!(explain.span.start, 0);
        assert_eq!(explain.span.end, "EXPLAIN SELECT id FROM users".len());
    }

    #[test]
    fn explain_rejects_create_table() {
        let err = parse_statement("EXPLAIN CREATE TABLE t (id BIGINT)").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn explain_rejects_nested_explain() {
        let err = parse_statement("EXPLAIN EXPLAIN SELECT id FROM users").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn parses_explain_without_analyze_as_false() {
        let statement = parse_statement("EXPLAIN SELECT id FROM users").unwrap();
        let Statement::Explain(explain) = statement else {
            panic!("EXPLAIN文を期待した");
        };
        assert!(!explain.analyze);
    }

    #[test]
    fn parses_explain_analyze_select() {
        let statement = parse_statement("EXPLAIN ANALYZE SELECT id FROM users").unwrap();
        let Statement::Explain(explain) = statement else {
            panic!("EXPLAIN文を期待した");
        };
        assert!(explain.analyze);
        assert!(matches!(*explain.statement, Statement::Select(_)));
    }

    #[test]
    fn parses_explain_analyze_insert_update_delete() {
        for sql in [
            "EXPLAIN ANALYZE INSERT INTO users VALUES (1)",
            "EXPLAIN ANALYZE UPDATE users SET id = 1",
            "EXPLAIN ANALYZE DELETE FROM users",
        ] {
            let statement = parse_statement(sql).unwrap();
            let Statement::Explain(explain) = statement else {
                panic!("{sql}: EXPLAIN文を期待した");
            };
            assert!(explain.analyze, "{sql}: analyze=trueを期待した");
        }
    }

    // ---- ANALYZE(第27章) ----

    #[test]
    fn parses_analyze_with_a_table_name() {
        let statement = parse_statement("ANALYZE users").unwrap();
        let Statement::Analyze(analyze) = statement else {
            panic!("ANALYZE文を期待した");
        };
        assert_eq!(analyze.table.map(|t| t.name), Some("users".to_string()));
    }

    #[test]
    fn parses_analyze_without_a_table_name() {
        let statement = parse_statement("ANALYZE").unwrap();
        let Statement::Analyze(analyze) = statement else {
            panic!("ANALYZE文を期待した");
        };
        assert_eq!(analyze.table, None);
    }

    #[test]
    fn parses_select_wildcard() {
        let statement = parse_statement("SELECT * FROM users").unwrap();
        match statement {
            Statement::Select(select) => {
                assert_eq!(select.items.len(), 1);
                assert!(matches!(select.items[0], SelectItem::Wildcard { .. }));
            }
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn accepts_trailing_semicolon() {
        assert!(parse_statement("SELECT 1;").is_ok());
    }

    // ---- 整数リテラルの符号と範囲(i64::MINを含む) ----

    #[test]
    fn parses_i64_min_as_a_single_signed_int_literal() {
        // `-9223372036854775808`は`UnaryOp::Negate`を経由せず、符号込みの
        // `Expr::IntLiteral`1個になる(`parse_prefix`のMinus分岐のコメント参照)。
        assert_expr_eq(
            "-9223372036854775808",
            Expr::IntLiteral {
                value: i64::MIN,
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_i64_max() {
        assert_expr_eq(
            "9223372036854775807",
            Expr::IntLiteral {
                value: i64::MAX,
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn magnitude_one_more_than_i64_min_is_rejected() {
        let err = parse_statement("SELECT -9223372036854775809").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn magnitude_one_more_than_i64_max_is_rejected() {
        let err = parse_statement("SELECT 9223372036854775808").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn syntax_error_reports_position_of_bad_token() {
        let err = parse_statement("SELECT 1 +").unwrap_err();
        match err {
            DbError::Parse { line, column, .. } => assert_eq!((line, column), (1, 11)),
            other => panic!("DbError::Parseを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn syntax_error_on_missing_select_target() {
        let err = parse_statement("SELECT FROM users").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn syntax_error_on_trailing_garbage() {
        let err = parse_statement("SELECT 1 2").unwrap_err();
        match err {
            DbError::Parse { line, column, .. } => assert_eq!((line, column), (1, 10)),
            other => panic!("DbError::Parseを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn syntax_error_on_unknown_statement_start() {
        let err = parse_statement("MERGE users USING x").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn propagates_lex_error() {
        let err = parse_statement("SELECT 'abc").unwrap_err();
        assert!(matches!(err, DbError::Lex { .. }));
    }
}
