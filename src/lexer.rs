//! SQL文字列をToken列へ変換する字句解析器。
//!
//! 対応するのは識別子、キーワード、整数リテラル、文字列リテラル(`'...'`、
//! `''`によるクォートのエスケープ)、算術・比較演算子、括弧・カンマ・セミコロン。
//! 行コメント(`--`)とブロックコメント(`/* */`)は読み飛ばし、Token列には残さない。
//!
//! 各`Token`はソース中のバイト範囲を`Span`として持つ。字句解析自体が失敗した場合
//! (閉じない文字列リテラルなど)は、その場で分かる行・列を添えた`DbError::Lex`を返す。

use crate::error::{DbError, DbResult};

/// ソースコード中のバイト範囲。`start`を含み`end`を含まない半開区間。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    /// クレート内の他モジュール(構文解析器など)が、複数のTokenのSpanを
    /// 結合してより大きな構文要素のSpanを作れるよう`pub(crate)`にしている。
    pub(crate) fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }
}

/// 1個のトークンと、それがソース中で占めるバイト範囲。
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// SQLの予約語。
///
/// ここに含めるのは、第7〜10章(構文解析、式評価、DDL、DML)で使う予定が
/// 決まっている範囲に限る。未使用のまま予約語を増やすと、その語を識別子として
/// 使いたくなったときに無関係な章で困るため、必要になるたびに追加する方針とする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Select,
    From,
    Where,
    Create,
    Table,
    Drop,
    Insert,
    Into,
    Values,
    Update,
    Set,
    Delete,
    True,
    False,
    Null,
    Not,
    And,
    Or,
    Is,
    As,
    Cast,
    Explain,
    Primary,
    Key,
    Unique,
    Order,
    By,
    Asc,
    Desc,
    Limit,
    Offset,
    Distinct,
    Group,
    Having,
    Inner,
    Join,
    On,
    /// `CREATE INDEX` / `DROP INDEX`(第24章)。
    Index,
    /// `ANALYZE`文、`EXPLAIN ANALYZE`修飾(第27章)。
    Analyze,
    /// `BEGIN`文(第30章)。トランザクションを開始する。
    Begin,
    /// `COMMIT`文(第30章)。
    Commit,
    /// `ROLLBACK`文(第30章)。
    Rollback,
    /// `BEGIN ISOLATION LEVEL ...`(第32章)の`ISOLATION`。
    Isolation,
    /// `BEGIN ISOLATION LEVEL ...`(第32章)の`LEVEL`。
    Level,
    /// `READ UNCOMMITTED` / `READ COMMITTED`(第32章)の`READ`。
    Read,
    /// `READ UNCOMMITTED`(第32章)の`UNCOMMITTED`。
    Uncommitted,
    /// `READ COMMITTED`(第32章)の`COMMITTED`。
    Committed,
    /// `REPEATABLE READ`(第32章)の`REPEATABLE`。
    Repeatable,
    /// `SERIALIZABLE`(第32章)。
    Serializable,
    /// `CHECKPOINT`文(第34章)。
    Checkpoint,
}

impl Keyword {
    /// 大文字小文字を無視して`word`を予約語に変換する。予約語でなければ`None`。
    fn from_word(word: &str) -> Option<Keyword> {
        let keyword = match word.to_ascii_uppercase().as_str() {
            "SELECT" => Keyword::Select,
            "FROM" => Keyword::From,
            "WHERE" => Keyword::Where,
            "CREATE" => Keyword::Create,
            "TABLE" => Keyword::Table,
            "DROP" => Keyword::Drop,
            "INSERT" => Keyword::Insert,
            "INTO" => Keyword::Into,
            "VALUES" => Keyword::Values,
            "UPDATE" => Keyword::Update,
            "SET" => Keyword::Set,
            "DELETE" => Keyword::Delete,
            "TRUE" => Keyword::True,
            "FALSE" => Keyword::False,
            "NULL" => Keyword::Null,
            "NOT" => Keyword::Not,
            "AND" => Keyword::And,
            "OR" => Keyword::Or,
            "IS" => Keyword::Is,
            "AS" => Keyword::As,
            "CAST" => Keyword::Cast,
            "EXPLAIN" => Keyword::Explain,
            "PRIMARY" => Keyword::Primary,
            "KEY" => Keyword::Key,
            "UNIQUE" => Keyword::Unique,
            "ORDER" => Keyword::Order,
            "BY" => Keyword::By,
            "ASC" => Keyword::Asc,
            "DESC" => Keyword::Desc,
            "LIMIT" => Keyword::Limit,
            "OFFSET" => Keyword::Offset,
            "DISTINCT" => Keyword::Distinct,
            "GROUP" => Keyword::Group,
            "HAVING" => Keyword::Having,
            "INNER" => Keyword::Inner,
            "JOIN" => Keyword::Join,
            "ON" => Keyword::On,
            "INDEX" => Keyword::Index,
            "ANALYZE" => Keyword::Analyze,
            "BEGIN" => Keyword::Begin,
            "COMMIT" => Keyword::Commit,
            "ROLLBACK" => Keyword::Rollback,
            "ISOLATION" => Keyword::Isolation,
            "LEVEL" => Keyword::Level,
            "READ" => Keyword::Read,
            "UNCOMMITTED" => Keyword::Uncommitted,
            "COMMITTED" => Keyword::Committed,
            "REPEATABLE" => Keyword::Repeatable,
            "SERIALIZABLE" => Keyword::Serializable,
            "CHECKPOINT" => Keyword::Checkpoint,
            _ => return None,
        };
        Some(keyword)
    }
}

/// トークンの種類。
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// 識別子。引用符では囲まれていない、大文字小文字を保持したままのテキスト。
    Ident(String),
    /// 予約語。
    Keyword(Keyword),
    /// 整数リテラル。符号を持たない絶対値のまま保持する。
    ///
    /// `i64::MIN`(`-9223372036854775808`)の絶対値`9223372036854775808`は
    /// `i64`の範囲を超える(`i64::MAX`は`9223372036854775807`)ため、`i64`では
    /// なく`u64`で持つ。符号を`i64`へ適用して範囲検査する仕事は、単項`-`と
    /// このTokenを組み合わせる`Parser`(第7章)に委ねる。
    IntLiteral(u64),
    /// 文字列リテラル。`''`によるエスケープは解決済みの値を持つ。
    StringLiteral(String),
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `=`
    Eq,
    /// `<>`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// `.`。`users.id`のような修飾列参照を書くための区切り(第17章)。
    Dot,
    /// 入力の終端を表す番兵トークン。
    Eof,
}

/// SQL文字列をToken列へ変換する。
///
/// 成功した場合、返る`Vec<Token>`の末尾には必ず`TokenKind::Eof`が1個だけ入る。
/// 閉じない文字列リテラル・閉じないブロックコメント・不明な文字・範囲を超えた
/// 整数リテラルに遭遇した場合は`DbError::Lex`を返す。
pub fn tokenize(source: &str) -> DbResult<Vec<Token>> {
    Lexer::new(source).tokenize()
}

/// バイトオフセットから、ソース中の1始まりの行番号・列番号を求める。
///
/// `Token`のSpanはバイトオフセットしか持たないため、利用者向けにエラー位置を
/// 表示する側(構文解析器など)がこの関数で行・列へ変換する。字句解析器自身が
/// 検出するエラー(`DbError::Lex`)は、走査中にすでに行・列を追っているため
/// この関数を経由しない。
pub fn line_col(source: &str, byte_offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for (offset, ch) in source.char_indices() {
        if offset >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

struct Lexer<'a> {
    source: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    line: usize,
    column: usize,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Lexer {
            source,
            chars: source.char_indices().peekable(),
            line: 1,
            column: 1,
        }
    }

    /// 次の文字を消費し、行・列を更新して返す。
    fn bump(&mut self) -> Option<(usize, char)> {
        let next = self.chars.next();
        if let Some((_, ch)) = next {
            if ch == '\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
        next
    }

    fn peek_char(&mut self) -> Option<char> {
        self.chars.peek().map(|&(_, c)| c)
    }

    fn peek_offset(&mut self) -> Option<usize> {
        self.chars.peek().map(|&(offset, _)| offset)
    }

    /// 現在位置から末尾までの残りの文字列。コメントの先頭記号を調べるのに使う。
    fn remaining(&mut self) -> &'a str {
        match self.peek_offset() {
            Some(offset) => &self.source[offset..],
            None => "",
        }
    }

    fn tokenize(mut self) -> DbResult<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments()?;

            let Some(start) = self.peek_offset() else {
                let end = self.source.len();
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    span: Span::new(end, end),
                });
                break;
            };

            let ch = self.peek_char().expect("直前にNoneでないことを確認済み");
            let kind = if ch.is_ascii_digit() {
                self.lex_number()?
            } else if ch == '\'' {
                self.lex_string()?
            } else if is_ident_start(ch) {
                self.lex_ident_or_keyword()
            } else {
                self.lex_operator_or_punct()?
            };
            let end = self.peek_offset().unwrap_or(self.source.len());
            tokens.push(Token {
                kind,
                span: Span::new(start, end),
            });
        }
        Ok(tokens)
    }

    /// 空白、行コメント(`--`)、ブロックコメント(`/* */`)を読み飛ばす。
    ///
    /// 3種類が交互に現れる入力(`  -- 注釈\n  /* 注釈 */  SELECT`)にも対応するため、
    /// どれにも該当しなくなるまでループする。
    fn skip_whitespace_and_comments(&mut self) -> DbResult<()> {
        loop {
            if let Some(c) = self.peek_char()
                && c.is_whitespace()
            {
                self.bump();
                continue;
            }

            if self.remaining().starts_with("--") {
                self.bump();
                self.bump();
                while let Some(c) = self.peek_char() {
                    if c == '\n' {
                        break;
                    }
                    self.bump();
                }
                continue;
            }

            if self.remaining().starts_with("/*") {
                let start_line = self.line;
                let start_column = self.column;
                self.bump();
                self.bump();
                loop {
                    if self.remaining().starts_with("*/") {
                        self.bump();
                        self.bump();
                        break;
                    }
                    if self.bump().is_none() {
                        return Err(DbError::Lex {
                            message: "閉じないブロックコメントです".to_string(),
                            line: start_line,
                            column: start_column,
                        });
                    }
                }
                continue;
            }

            break;
        }
        Ok(())
    }

    /// 数字が連続する間読み進め、`u64`として解析する(符号は付けない。
    /// `TokenKind::IntLiteral`のドキュメント参照)。
    ///
    /// 数字の直後に識別子文字(英字・数字・`_`)が続く場合(`1abc`、`1_2`)は、
    /// `1`と`abc`の2個のTokenへ黙って分割せず、字句エラーにする。区切りの
    /// 無い数字と識別子の並びを許すと、`1abc`が`1 abc`と書いたのと同じ意味に
    /// 誤読されかねないため。
    fn lex_number(&mut self) -> DbResult<TokenKind> {
        let start_line = self.line;
        let start_column = self.column;
        let start = self
            .peek_offset()
            .expect("呼び出し元が数字の存在を確認済み");

        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }

        let end = self.peek_offset().unwrap_or(self.source.len());
        let text = &self.source[start..end];

        if let Some(c) = self.peek_char()
            && is_ident_continue(c)
        {
            // メッセージに残りの識別子文字も含めるため、字句解析の位置は
            // 動かさずに`remaining()`を覗き見して`is_ident_continue`が
            // 続く分だけを切り出す。
            let tail_len = self
                .remaining()
                .char_indices()
                .find(|(_, c)| !is_ident_continue(*c))
                .map(|(offset, _)| offset)
                .unwrap_or_else(|| self.remaining().len());
            let tail = &self.remaining()[..tail_len];
            return Err(DbError::Lex {
                message: format!("数値リテラルの直後に識別子文字が続いています: {text}{tail}"),
                line: start_line,
                column: start_column,
            });
        }

        text.parse::<u64>()
            .map(TokenKind::IntLiteral)
            .map_err(|_| DbError::Lex {
                message: format!("整数リテラルの範囲を超えています: {text}"),
                line: start_line,
                column: start_column,
            })
    }

    /// 開きの`'`から閉じの`'`までを読む。`''`は1個の`'`として値に含める。
    fn lex_string(&mut self) -> DbResult<TokenKind> {
        let start_line = self.line;
        let start_column = self.column;
        self.bump(); // 開きの'

        let mut value = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(DbError::Lex {
                        message: "閉じない文字列リテラルです".to_string(),
                        line: start_line,
                        column: start_column,
                    });
                }
                Some((_, '\'')) => {
                    if self.peek_char() == Some('\'') {
                        self.bump();
                        value.push('\'');
                    } else {
                        break;
                    }
                }
                Some((_, c)) => value.push(c),
            }
        }
        Ok(TokenKind::StringLiteral(value))
    }

    /// 識別子として読める間読み進め、予約語なら`Keyword`、そうでなければ`Ident`にする。
    fn lex_ident_or_keyword(&mut self) -> TokenKind {
        let start = self
            .peek_offset()
            .expect("呼び出し元が識別子開始文字の存在を確認済み");

        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                self.bump();
            } else {
                break;
            }
        }

        let end = self.peek_offset().unwrap_or(self.source.len());
        let text = &self.source[start..end];
        match Keyword::from_word(text) {
            Some(keyword) => TokenKind::Keyword(keyword),
            None => TokenKind::Ident(text.to_string()),
        }
    }

    /// 演算子・括弧・カンマ・セミコロンを1個読む。
    fn lex_operator_or_punct(&mut self) -> DbResult<TokenKind> {
        let start_line = self.line;
        let start_column = self.column;
        let (_, ch) = self.bump().expect("呼び出し元が文字の存在を確認済み");

        let kind = match ch {
            '+' => TokenKind::Plus,
            '-' => TokenKind::Minus,
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '=' => TokenKind::Eq,
            '<' => {
                if self.peek_char() == Some('=') {
                    self.bump();
                    TokenKind::LtEq
                } else if self.peek_char() == Some('>') {
                    self.bump();
                    TokenKind::NotEq
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                if self.peek_char() == Some('=') {
                    self.bump();
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
            }
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semicolon,
            '.' => TokenKind::Dot,
            other => {
                return Err(DbError::Lex {
                    message: format!("不明な文字です: {other:?}"),
                    line: start_line,
                    column: start_column,
                });
            }
        };
        Ok(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        tokenize(source)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn tokenizes_keywords_case_insensitively() {
        assert_eq!(
            kinds("select FROM Where"),
            vec![
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Keyword(Keyword::From),
                TokenKind::Keyword(Keyword::Where),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn ident_preserves_original_case() {
        assert_eq!(
            kinds("UserName"),
            vec![TokenKind::Ident("UserName".to_string()), TokenKind::Eof]
        );
    }

    #[test]
    fn ident_allows_underscore_and_digits_after_first_char() {
        assert_eq!(
            kinds("user_1"),
            vec![TokenKind::Ident("user_1".to_string()), TokenKind::Eof]
        );
    }

    #[test]
    fn tokenizes_integer_literal() {
        assert_eq!(kinds("42"), vec![TokenKind::IntLiteral(42), TokenKind::Eof]);
    }

    #[test]
    fn tokenizes_a_magnitude_close_to_i64_min_as_u64() {
        // `i64::MIN`(`-9223372036854775808`)の絶対値`9223372036854775808`は
        // `i64::MAX`(`9223372036854775807`)を1超えるため`i64`には収まらないが、
        // `IntLiteral`が`u64`を持つようになったことでLexerの時点では問題なく
        // 読める。符号を適用した最終的な範囲検査はParserの仕事(第7章)。
        assert_eq!(
            kinds("9223372036854775808"),
            vec![TokenKind::IntLiteral(9223372036854775808), TokenKind::Eof]
        );
    }

    #[test]
    fn integer_literal_beyond_u64_range_is_a_lex_error() {
        let err = tokenize("99999999999999999999999999999999").unwrap_err();
        assert!(matches!(err, DbError::Lex { .. }));
    }

    #[test]
    fn integer_literal_immediately_followed_by_identifier_chars_is_a_lex_error() {
        let err = tokenize("1abc").unwrap_err();
        match err {
            DbError::Lex { message, .. } => assert!(message.contains("1abc")),
            other => panic!("DbError::Lexを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn integer_literal_immediately_followed_by_underscore_and_digit_is_a_lex_error() {
        let err = tokenize("1_2").unwrap_err();
        assert!(matches!(err, DbError::Lex { .. }));
    }

    #[test]
    fn integer_literal_followed_by_whitespace_then_identifier_still_lexes_as_two_tokens() {
        assert_eq!(
            kinds("1 abc"),
            vec![
                TokenKind::IntLiteral(1),
                TokenKind::Ident("abc".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn tokenizes_string_literal_with_escaped_quote() {
        assert_eq!(
            kinds("'it''s'"),
            vec![TokenKind::StringLiteral("it's".to_string()), TokenKind::Eof]
        );
    }

    #[test]
    fn tokenizes_all_comparison_and_arithmetic_operators() {
        assert_eq!(
            kinds("+ - * / = <> < <= > >="),
            vec![
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Eq,
                TokenKind::NotEq,
                TokenKind::Lt,
                TokenKind::LtEq,
                TokenKind::Gt,
                TokenKind::GtEq,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn tokenizes_parens_comma_semicolon() {
        assert_eq!(
            kinds("(1, 2);"),
            vec![
                TokenKind::LParen,
                TokenKind::IntLiteral(1),
                TokenKind::Comma,
                TokenKind::IntLiteral(2),
                TokenKind::RParen,
                TokenKind::Semicolon,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn tokenizes_dot_for_qualified_column_refs() {
        assert_eq!(
            kinds("u.id"),
            vec![
                TokenKind::Ident("u".to_string()),
                TokenKind::Dot,
                TokenKind::Ident("id".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn skips_line_comment_until_newline() {
        assert_eq!(
            kinds("1 -- コメント\n2"),
            vec![
                TokenKind::IntLiteral(1),
                TokenKind::IntLiteral(2),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn skips_block_comment_across_lines() {
        assert_eq!(
            kinds("1 /* comment\nspans lines */ 2"),
            vec![
                TokenKind::IntLiteral(1),
                TokenKind::IntLiteral(2),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn minus_is_not_confused_with_line_comment() {
        assert_eq!(
            kinds("1-2"),
            vec![
                TokenKind::IntLiteral(1),
                TokenKind::Minus,
                TokenKind::IntLiteral(2),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn token_span_covers_exact_bytes() {
        let tokens = tokenize("SELECT 1").unwrap();
        assert_eq!(tokens[0].span, Span::new(0, 6));
        assert_eq!(tokens[1].span, Span::new(7, 8));
    }

    #[test]
    fn unterminated_string_literal_is_a_lex_error_at_open_quote() {
        let err = tokenize("SELECT 'abc").unwrap_err();
        match err {
            DbError::Lex { line, column, .. } => assert_eq!((line, column), (1, 8)),
            other => panic!("DbError::Lexを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn unterminated_string_literal_reports_line_of_the_opening_quote() {
        let err = tokenize("SELECT 1;\n'abc").unwrap_err();
        match err {
            DbError::Lex { line, column, .. } => assert_eq!((line, column), (2, 1)),
            other => panic!("DbError::Lexを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn unterminated_block_comment_is_a_lex_error() {
        let err = tokenize("1 /* comment").unwrap_err();
        match err {
            DbError::Lex { message, .. } => {
                assert!(message.contains("ブロックコメント"))
            }
            other => panic!("DbError::Lexを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn unknown_character_is_a_lex_error() {
        let err = tokenize("SELECT @").unwrap_err();
        match err {
            DbError::Lex { line, column, .. } => assert_eq!((line, column), (1, 8)),
            other => panic!("DbError::Lexを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn line_col_tracks_newlines() {
        assert_eq!(line_col("ab\ncd", 0), (1, 1));
        assert_eq!(line_col("ab\ncd", 3), (2, 1));
        assert_eq!(line_col("ab\ncd", 4), (2, 2));
    }
}
