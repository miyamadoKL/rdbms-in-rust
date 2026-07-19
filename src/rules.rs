//! `LogicalPlan`を、意味を変えずに書き換える**ルールベース最適化**。
//!
//! 第25章までの`physical_plan::optimize`は、`LogicalPlan`が渡ってきた形を
//! そのまま信じて`PhysicalPlan`へ変換していた。`WHERE 1 = 1 AND id = 42`と
//! `WHERE id = 42`は同じ行を返す同じ意味のクエリだが、前者は`1 = 1`という
//! 無駄な比較を毎行評価するぶんだけ遅い。`JOIN`をまたいだ`WHERE`(`a.status =
//! 'active'`のように片側のテーブルにしか関係しない条件)も、`Filter`が
//! `Join`の真上に居座ったままでは、`Join`が結合を終えるまで絞り込みが
//! 始まらない(第25章で確認したとおり)。
//!
//! この章の[`Rule`]は、`LogicalPlan`を`LogicalPlan`へ書き換える1個の変換で、
//! **書き換えても同じ行を返す**(意味を変えない)ことだけを条件にする。どんな
//! 書き換えを行うかはルールごとに独立しており、[`optimize`]という1つの
//! ドライバが、登録された全ルールを「これ以上どれも変化を起こさなくなる」
//! **固定点**まで繰り返し適用する。
//!
//! # ルール一覧
//!
//! - [`ConstantFolding`][]: `1 + 1`のような、列を参照しない部分式を実行前に
//!   評価してリテラルへ置き換える。
//! - [`BooleanSimplification`][]: `TRUE AND x`・`x OR FALSE`のような、三値論理の
//!   もとで恒等的に成り立つ簡約を行う。
//! - [`FilterMerge`][]: 連続する`Filter`を1個の`Filter`へまとめる。
//! - [`PredicatePushdown`][]: `Join`の真上にある`Filter`の連言を、それぞれが
//!   参照する片側のテーブルまで押し下げる。
//! - [`ProjectionPruning`][]: `GROUP BY`・`SELECT`が実際に参照する列だけが
//!   `Scan`から先の演算子を流れるよう、使われない列をこの章の中で
//!   安全に剪定できる範囲に限って削る。
//!
//! # 固定点までの反復
//!
//! ルールは互いに新しい適用機会を作り出すことがある。`WHERE (1 + 1 = 2) AND
//! a.x = 5`を例にとると、[`ConstantFolding`]は`1 + 1 = 2`を`TRUE`という
//! リテラルへ畳み込むが、`TRUE AND a.x = 5`を`a.x = 5`へ簡約するのは
//! [`BooleanSimplification`]の仕事であり、[`ConstantFolding`]が畳み込みを
//! 終えるまで`BooleanSimplification`はこの`TRUE`を認識できない(`TRUE`という
//! **リテラル**を構文的に探すのであって、評価すれば`TRUE`になる式を意味的に
//! 探すのではないからである)。1回のパスで全ルールを1度ずつ順に適用するだけ
//! では、ルールの実行順によって畳み込まれる場合と畳み込まれない場合が生まれて
//! しまう。[`optimize`]がどのルールも変化を起こさなくなるまでループする
//! ([`MAX_ITERATIONS`]で反復回数に上限を設ける)のは、この順序依存を
//! 「収束するまで回す」ことで消すためである。

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{BinaryOperator, UnaryOperator};
use crate::binder::{AggregateCall, BoundAssignment, BoundExpr, BoundSelectItem};
use crate::error::DbError;
use crate::eval::{FunctionRegistry, eval_bound_expr};
use crate::lexer::Span;
use crate::logical_plan::{
    AggregateNode, DeleteNode, DistinctNode, FilterNode, InsertNode, JoinNode, LimitNode, LogicalPlan, ScanNode,
    SortKey, SortNode, UpdateNode,
};
use crate::physical_plan::{Side, collect_conjuncts, columns_side, rebuild_conjunction, shift_column_index};
use crate::types::Value;

/// 固定点ループの反復回数の上限。ここで挙げるルールはどれも「式や木を
/// 単調に小さくする」性質を持ち、通常はクエリの構文要素数のオーダーで
/// 収束する。この上限は、将来ルールを追加した際に互いを無限に行き来させる
/// バグを埋め込んでしまった場合の安全弁であり、正常な実行でこの上限に
/// 達することはない。
const MAX_ITERATIONS: usize = 64;

/// `LogicalPlan`に対する1個の書き換えルール。
///
/// `apply`は`plan`を消費し、書き換え後の`LogicalPlan`と、実際に何か変化が
/// あったかを表す`bool`を返す。`changed`が`false`のときは`plan`をそのまま
/// (等価な形で)返す約束であり、[`optimize`]はこの`bool`だけを見て固定点に
/// 達したかどうかを判断する。
pub trait Rule {
    /// `EXPLAIN`のデバッグ表示やログでルールを識別するための名前。
    fn name(&self) -> &str;
    /// `plan`を書き換える。`functions`は[`ConstantFolding`]がScalar Function
    /// 呼び出しを評価するために使う([`crate::eval::FunctionRegistry`]が
    /// 登録する関数は決定的である前提を置く。詳しくは[`ConstantFolding`]の
    /// ドキュメントを参照)。
    fn apply(&self, plan: LogicalPlan, functions: &FunctionRegistry) -> (LogicalPlan, bool);
}

/// 登録済みの全ルールを、この章が既定で使う順序で返す。
///
/// この順序自体に強い意味はない([`Rule`]はどの順で適用されても[`optimize`]が
/// 固定点まで回す限り最終結果は変わらない、モジュール冒頭の説明を参照)。
/// それでも[`PredicatePushdown`]を[`ProjectionPruning`]より先に置いているのは、
/// `WHERE`を個々のテーブルの`Scan`直上まで運んでおいたほうが、
/// [`ProjectionPruning`]が「この`Filter`の直下は`Scan`か」を判定する際に
/// 早い段階から正しい形の木を見られるためである(結果はどちらの順でも
/// 最終的に一致するが、収束までの反復回数がわずかに減る)。
pub fn default_rules() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(ConstantFolding),
        Box::new(BooleanSimplification),
        Box::new(FilterMerge),
        Box::new(PredicatePushdown),
        Box::new(ProjectionPruning),
    ]
}

/// [`default_rules`]が返す全ルールを、変化が無くなるまで(固定点まで)`plan`へ
/// 繰り返し適用する。
///
/// `MAX_ITERATIONS`回を超えても変化が続く場合はパニックにせず、その時点の
/// `plan`をそのまま返す(モジュール冒頭の説明を参照。通常の実行ではここに
/// 達しない)。
pub fn optimize(mut plan: LogicalPlan, functions: &FunctionRegistry) -> LogicalPlan {
    let rules = default_rules();
    for _ in 0..MAX_ITERATIONS {
        let mut changed = false;
        for rule in &rules {
            let (next, rule_changed) = rule.apply(plan, functions);
            plan = next;
            changed |= rule_changed;
        }
        if !changed {
            break;
        }
    }
    plan
}

// ==================================================================
// Constant Folding: 列を参照しない部分式を実行前に評価する
// ==================================================================

/// `1 + 1`・`'a' = 'a'`のような、列を1つも参照しない部分式を、実行前に
/// リテラルへ畳み込む。
///
/// 畳み込みの対象は、[`is_constant`]が`true`を返す式(列参照(`ColumnRef`)も
/// 集約(`Aggregate`、`GROUP BY`の外に現れることはない)も含まない式)に限る。
/// `functions`([`crate::eval::FunctionRegistry`])が登録するScalar Function
/// (`ABS`・`LENGTH`)はどちらも同じ入力に対して常に同じ値を返す決定的な関数
/// であり、この前提がある限り`ABS(-1)`のような呼び出しも安全に畳み込める。
/// 将来、時刻や乱数を返す非決定的な関数を`register`で追加する場合、この前提が
/// 崩れることに注意が必要である(この章ではそのような関数を持たない)。
///
/// 評価が失敗する式(`1 / 0`のようなゼロ除算、`i64`の範囲を超える演算)は
/// 畳み込まず、元の式をそのまま残す。実行時に評価されれば同じ理由で
/// エラーになる式であり、畳み込みを諦めても結果は変わらない。
pub struct ConstantFolding;

impl Rule for ConstantFolding {
    fn name(&self) -> &str {
        "ConstantFolding"
    }

    fn apply(&self, plan: LogicalPlan, functions: &FunctionRegistry) -> (LogicalPlan, bool) {
        map_plan_exprs(plan, &mut |expr| fold_expr(expr, functions))
    }
}

/// 式が列参照・集約を1つも含まないかどうか。`true`なら`row`を渡さずに
/// ([`crate::eval::eval_bound_expr`]へ`None`を渡して)評価できる。
fn is_constant(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. } => true,
        BoundExpr::ColumnRef { .. } | BoundExpr::Aggregate { .. } => false,
        BoundExpr::UnaryOp { expr, .. } | BoundExpr::Paren { expr, .. } | BoundExpr::Cast { expr, .. } => {
            is_constant(expr)
        }
        BoundExpr::BinaryOp { lhs, rhs, .. } => is_constant(lhs) && is_constant(rhs),
        BoundExpr::IsNull { expr, .. } => is_constant(expr),
        BoundExpr::FunctionCall { args, .. } => args.iter().all(is_constant),
    }
}

/// `value`を、同じ`span`を持つ`BoundExpr`のリテラルへ変換する。
fn value_to_literal(value: Value, span: Span) -> BoundExpr {
    match value {
        Value::Null => BoundExpr::NullLiteral { span },
        Value::Boolean(value) => BoundExpr::BoolLiteral { value, span },
        Value::BigInt(value) => BoundExpr::IntLiteral { value, span },
        Value::Text(value) => BoundExpr::StringLiteral { value, span },
    }
}

/// `expr`の子を先に畳み込んでから、`expr`自身が定数式になっていれば評価して
/// リテラルへ置き換える(post-order、子が畳み込まれて初めて親が定数だと
/// 判明する場合があるため)。
fn fold_expr(expr: BoundExpr, functions: &FunctionRegistry) -> (BoundExpr, bool) {
    match expr {
        BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. }
        | BoundExpr::ColumnRef { .. } => (expr, false),
        BoundExpr::UnaryOp { op, expr, data_type, span } => {
            let (expr, changed) = fold_expr(*expr, functions);
            try_fold(BoundExpr::UnaryOp { op, expr: Box::new(expr), data_type, span }, functions, changed)
        }
        BoundExpr::BinaryOp { op, lhs, rhs, data_type, span } => {
            let (lhs, c1) = fold_expr(*lhs, functions);
            let (rhs, c2) = fold_expr(*rhs, functions);
            try_fold(BoundExpr::BinaryOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs), data_type, span }, functions, c1 || c2)
        }
        BoundExpr::IsNull { expr, negated, span } => {
            let (expr, changed) = fold_expr(*expr, functions);
            try_fold(BoundExpr::IsNull { expr: Box::new(expr), negated, span }, functions, changed)
        }
        BoundExpr::FunctionCall { name, args, data_type, span } => {
            let mut changed = false;
            let args: Vec<BoundExpr> = args
                .into_iter()
                .map(|arg| {
                    let (arg, c) = fold_expr(arg, functions);
                    changed |= c;
                    arg
                })
                .collect();
            try_fold(BoundExpr::FunctionCall { name, args, data_type, span }, functions, changed)
        }
        BoundExpr::Paren { expr, span } => {
            let (expr, changed) = fold_expr(*expr, functions);
            try_fold(BoundExpr::Paren { expr: Box::new(expr), span }, functions, changed)
        }
        BoundExpr::Cast { expr, data_type, span } => {
            let (expr, changed) = fold_expr(*expr, functions);
            try_fold(BoundExpr::Cast { expr: Box::new(expr), data_type, span }, functions, changed)
        }
        BoundExpr::Aggregate { .. } => (expr, false),
    }
}

/// `expr`が[`is_constant`]であれば評価し、成功すればリテラルへ置き換える。
/// すでにリテラルなら(これ以上畳み込む余地が無いので)そのまま返す。
/// 評価が失敗した場合(オーバーフロー、ゼロ除算)は`expr`を変えずに返す。
fn try_fold(expr: BoundExpr, functions: &FunctionRegistry, children_changed: bool) -> (BoundExpr, bool) {
    if matches!(
        expr,
        BoundExpr::IntLiteral { .. } | BoundExpr::StringLiteral { .. } | BoundExpr::BoolLiteral { .. } | BoundExpr::NullLiteral { .. }
    ) {
        return (expr, children_changed);
    }
    if !is_constant(&expr) {
        return (expr, children_changed);
    }
    let span = expr.span();
    match eval_bound_expr(&expr, functions, None) {
        // `NULL`は型を持たない値である(`BoundExpr::NullLiteral::data_type()`は
        // 常に`None`、`crate::logical_plan::projection_schema`はこれを
        // `TEXT`扱いする)。`expr`自身が`ABS(id)`のように確定した型
        // (`Some(BigInt)`)を持っていた場合、評価結果が`NULL`だからといって
        // `NullLiteral`へ畳み込むと、この確定していた型を失って`Projection`の
        // 出力スキーマが変わってしまう(`data_type()`が`None`になり`TEXT`へ
        // 後退する)。`Value`は型を持たない`Value::Null`を返すだけで、`expr`
        // 自身が持っていた静的な型を運べないため、このケースだけは畳み込みを
        // 諦め、実行時の評価に委ねる。
        Ok(value) if value.is_null() && expr.data_type().is_some() => (expr, children_changed),
        Ok(value) => (value_to_literal(value, span), true),
        Err(DbError::Eval(_)) => (expr, children_changed),
        Err(other) => unreachable!("eval_bound_exprはDbError::Eval以外を返さない: {other:?}"),
    }
}

// ==================================================================
// Boolean Simplification: 三値論理のもとで恒等的に成り立つ簡約
// ==================================================================

/// `TRUE AND x`・`x OR FALSE`のような、SQLの三値論理([`crate::eval`]の
/// `Tri`・`tri_and`・`tri_or`を参照)のもとで恒等的に成り立つ簡約を行う。
///
/// # 正しさ: `NULL`があっても崩れない理由
///
/// SQLの`AND`・`OR`は`TRUE`・`FALSE`・`UNKNOWN`(`NULL`)の三値論理で定義される
/// (第8章)。この章が行う8つの簡約は、`x`が`TRUE`・`FALSE`・`UNKNOWN`のどれで
/// あっても成り立つことを、`tri_and`・`tri_or`の定義から直接確かめられる。
///
/// - `TRUE AND x → x`、`x AND TRUE → x`:
///   `tri_and(True, r)`は`r`が`False`なら`False`、`Unknown`なら`Unknown`
///   ([`crate::eval`]の`tri_and`の`_ => Tri::Unknown`分岐)、`True`なら`True`と
///   なり、いずれも`r`自身の値と一致する。
/// - `FALSE AND x → FALSE`、`x AND FALSE → FALSE`:
///   `tri_and`の`(Tri::False, _) | (_, Tri::False) => Tri::False`という定義
///   そのものが、もう一方の値(`x`が`UNKNOWN`であっても)に関係なく`FALSE`に
///   なることを示している。
/// - `FALSE OR x → x`、`x OR FALSE → x`、`TRUE OR x → TRUE`、`x OR TRUE →
///   TRUE`も同様に`tri_or`の定義から従う。
/// - `NOT NOT x → x`: `Tri`の`Not`実装は`True`と`False`を入れ替え、
///   `Unknown`はそのまま`Unknown`に留めるため、2回適用すれば元の値に戻る。
///
/// # 副作用: エラーを起こす部分式を消してしまう場合がある
///
/// `crate::eval::eval_binary`(第8章)は`AND`・`OR`の両辺を必ず評価してから
/// `tri_and`・`tri_or`を適用する(短絡評価をしない)。そのため、`FALSE AND (1 /
/// 0 = 1)`という式は、書き換え前ならゼロ除算のエラーになるが、この章の
/// 簡約が`FALSE`に書き換えたあとは、右辺を評価すること自体が無くなり
/// エラーにならない。返す行の集合は変わらない(この式が`WHERE`に現れれば、
/// 書き換え前もエラーになるか0行になるかのどちらかであり、結果を返す
/// クエリにはならない)という意味では「意味を変えない」と言えるが、エラーに
/// なるか成功するかという振る舞いまでは保存しない。実際のRDBMSの多くも
/// 同様にデッドブランチを畳み込んで消すため、この章もその慣行に合わせる
/// (`tests`の`boolean_simplification_short_circuits_false_and`で、この
/// 振る舞いの変化そのものを確認する)。
pub struct BooleanSimplification;

impl Rule for BooleanSimplification {
    fn name(&self) -> &str {
        "BooleanSimplification"
    }

    fn apply(&self, plan: LogicalPlan, _functions: &FunctionRegistry) -> (LogicalPlan, bool) {
        map_plan_exprs(plan, &mut simplify_expr)
    }
}

/// `expr`が(`Paren`越しに見て)`BoolLiteral`であれば、その真偽値を返す。
fn bool_literal(expr: &BoundExpr) -> Option<bool> {
    match expr {
        BoundExpr::BoolLiteral { value, .. } => Some(*value),
        BoundExpr::Paren { expr, .. } => bool_literal(expr),
        _ => None,
    }
}

fn simplify_expr(expr: BoundExpr) -> (BoundExpr, bool) {
    match expr {
        BoundExpr::BinaryOp { op: BinaryOperator::And, lhs, rhs, data_type, span } => {
            let (lhs, c1) = simplify_expr(*lhs);
            let (rhs, c2) = simplify_expr(*rhs);
            match (bool_literal(&lhs), bool_literal(&rhs)) {
                (Some(true), _) => (rhs, true),
                (_, Some(true)) => (lhs, true),
                (Some(false), _) => (lhs, true),
                (_, Some(false)) => (rhs, true),
                _ => (BoundExpr::BinaryOp { op: BinaryOperator::And, lhs: Box::new(lhs), rhs: Box::new(rhs), data_type, span }, c1 || c2),
            }
        }
        BoundExpr::BinaryOp { op: BinaryOperator::Or, lhs, rhs, data_type, span } => {
            let (lhs, c1) = simplify_expr(*lhs);
            let (rhs, c2) = simplify_expr(*rhs);
            match (bool_literal(&lhs), bool_literal(&rhs)) {
                (Some(true), _) => (lhs, true),
                (_, Some(true)) => (rhs, true),
                (Some(false), _) => (rhs, true),
                (_, Some(false)) => (lhs, true),
                _ => (BoundExpr::BinaryOp { op: BinaryOperator::Or, lhs: Box::new(lhs), rhs: Box::new(rhs), data_type, span }, c1 || c2),
            }
        }
        BoundExpr::UnaryOp { op: UnaryOperator::Not, expr, data_type, span } => {
            let (expr, changed) = simplify_expr(*expr);
            if let Some(value) = bool_literal(&expr) {
                return (BoundExpr::BoolLiteral { value: !value, span }, true);
            }
            if let BoundExpr::UnaryOp { op: UnaryOperator::Not, expr: inner, .. } = expr {
                return (*inner, true);
            }
            (BoundExpr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(expr), data_type, span }, changed)
        }
        BoundExpr::UnaryOp { op, expr, data_type, span } => {
            let (expr, changed) = simplify_expr(*expr);
            (BoundExpr::UnaryOp { op, expr: Box::new(expr), data_type, span }, changed)
        }
        BoundExpr::BinaryOp { op, lhs, rhs, data_type, span } => {
            let (lhs, c1) = simplify_expr(*lhs);
            let (rhs, c2) = simplify_expr(*rhs);
            (BoundExpr::BinaryOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs), data_type, span }, c1 || c2)
        }
        BoundExpr::IsNull { expr, negated, span } => {
            let (expr, changed) = simplify_expr(*expr);
            (BoundExpr::IsNull { expr: Box::new(expr), negated, span }, changed)
        }
        BoundExpr::FunctionCall { name, args, data_type, span } => {
            let mut changed = false;
            let args: Vec<BoundExpr> = args
                .into_iter()
                .map(|arg| {
                    let (arg, c) = simplify_expr(arg);
                    changed |= c;
                    arg
                })
                .collect();
            (BoundExpr::FunctionCall { name, args, data_type, span }, changed)
        }
        BoundExpr::Paren { expr, span } => {
            let (expr, changed) = simplify_expr(*expr);
            (BoundExpr::Paren { expr: Box::new(expr), span }, changed)
        }
        BoundExpr::Cast { expr, data_type, span } => {
            let (expr, changed) = simplify_expr(*expr);
            (BoundExpr::Cast { expr: Box::new(expr), data_type, span }, changed)
        }
        other => (other, false),
    }
}

// ==================================================================
// Filter Merge: 連続するFilterを1個にまとめる
// ==================================================================

/// `Filter(Filter(x, p1), p2)`のように連続する`Filter`を、`Filter(x, p1 AND
/// p2)`という1個の`Filter`へまとめる。
///
/// [`PredicatePushdown`]が同じテーブルへ2つの`WHERE`由来の条件を別々の
/// `Filter`として押し下げた直後など、他のルールが作り出した連続する
/// `Filter`をまとめる場面で働く。1個の`Filter`にまとまることで、
/// `next()`ごとの述語評価が「述語1個」から「複合述語1個」になり、
/// `Executor`の入れ子が1段減る。
pub struct FilterMerge;

impl Rule for FilterMerge {
    fn name(&self) -> &str {
        "FilterMerge"
    }

    fn apply(&self, plan: LogicalPlan, _functions: &FunctionRegistry) -> (LogicalPlan, bool) {
        merge_filters(plan)
    }
}

fn merge_filters(plan: LogicalPlan) -> (LogicalPlan, bool) {
    if !matches!(plan, LogicalPlan::Filter(_)) {
        return map_plan_children(plan, merge_filters);
    }

    let mut predicates = Vec::new();
    let mut current = plan;
    while let LogicalPlan::Filter(filter) = current {
        predicates.push(filter.predicate);
        current = *filter.input;
    }
    let chain_changed = predicates.len() > 1;
    let (input, inner_changed) = merge_filters(current);

    predicates.reverse(); // 元々いちばん内側(Scanに近い)にあった述語を先頭にする
    let predicate = rebuild_conjunction(predicates).expect("Filterノードは必ず1個以上の述語を持つ");
    (LogicalPlan::Filter(FilterNode { input: Box::new(input), predicate }), chain_changed || inner_changed)
}

// ==================================================================
// Predicate Pushdown: JoinをまたぐWHEREを片側のテーブルまで押し下げる
// ==================================================================

/// `Join`の真上にある`Filter`の述語をANDの連言に分解し、`left`だけを参照する
/// 項は`left`の直上へ、`right`だけを参照する項は`right`の直上へ、それぞれ
/// 新しい`Filter`として押し下げる。どちらも参照する項(結合条件そのものに
/// 近い項)や、列を1つも参照しない項は`Join`の上に残す。
///
/// `left`・`right`はどちらも`Scan`単体か、それ自体が`Join`(左深い木の
/// 途中)でありうる。押し下げた先が`Join`であれば、そのまま[`pushdown_plan`]を
/// 再帰させることで、3個以上の`JOIN`を持つ`FROM`でも1回の`apply`呼び出しの
/// 中で必要な段数だけ潜っていく。
///
/// この章の`FROM`が対応する結合は[`crate::ast::JoinKind::Inner`]だけである。
/// `LEFT`・`RIGHT`のような外部結合が無いことが、この押し下げを常に安全にする
/// 理由でもある。外部結合では、`NULL`で埋められる側を参照する`WHERE`条件を
/// 結合より先に評価してしまうと、本来`NULL`埋めされて生き残るはずだった行が
/// 結合前に消え、結果が変わってしまう(`WHERE`は結合後に評価されるべき条件で
/// あり、結合前の`Filter`へ押し下げてよいのは「その条件を満たさない行は
/// `NULL`埋めされても最終的に一致しない」と分かる内部結合のときだけである)。
/// このクレートの構文には外部結合が無いため、この問題はそもそも起こらない。
pub struct PredicatePushdown;

impl Rule for PredicatePushdown {
    fn name(&self) -> &str {
        "PredicatePushdown"
    }

    fn apply(&self, plan: LogicalPlan, _functions: &FunctionRegistry) -> (LogicalPlan, bool) {
        pushdown_plan(plan)
    }
}

fn pushdown_plan(plan: LogicalPlan) -> (LogicalPlan, bool) {
    match plan {
        LogicalPlan::Filter(filter) => match *filter.input {
            LogicalPlan::Join(join) => {
                let left_len = join.left.output_schema().len();
                let mut conjuncts: Vec<&BoundExpr> = Vec::new();
                collect_conjuncts(&filter.predicate, &mut conjuncts);

                let mut left_preds = Vec::new();
                let mut right_preds = Vec::new();
                let mut residual = Vec::new();
                for conjunct in conjuncts {
                    match columns_side(conjunct, left_len) {
                        Some(Side::Left) => left_preds.push(conjunct.clone()),
                        Some(Side::Right) => right_preds.push(shift_column_index(conjunct, left_len)),
                        None => residual.push(conjunct.clone()),
                    }
                }
                let pushed = !left_preds.is_empty() || !right_preds.is_empty();

                let mut new_left = *join.left;
                if let Some(predicate) = rebuild_conjunction(left_preds) {
                    new_left = LogicalPlan::Filter(FilterNode { input: Box::new(new_left), predicate });
                }
                let mut new_right = *join.right;
                if let Some(predicate) = rebuild_conjunction(right_preds) {
                    new_right = LogicalPlan::Filter(FilterNode { input: Box::new(new_right), predicate });
                }
                let (new_left, c1) = pushdown_plan(new_left);
                let (new_right, c2) = pushdown_plan(new_right);

                let new_join = LogicalPlan::Join(JoinNode {
                    left: Box::new(new_left),
                    right: Box::new(new_right),
                    kind: join.kind,
                    condition: join.condition,
                });
                let result = match rebuild_conjunction(residual) {
                    Some(predicate) => LogicalPlan::Filter(FilterNode { input: Box::new(new_join), predicate }),
                    None => new_join,
                };
                (result, pushed || c1 || c2)
            }
            other => {
                let (input, changed) = pushdown_plan(other);
                (LogicalPlan::Filter(FilterNode { input: Box::new(input), predicate: filter.predicate }), changed)
            }
        },
        other => map_plan_children(other, pushdown_plan),
    }
}

// ==================================================================
// Projection Pruning: GROUP BY・SELECTが使わない列をScan段階で削る
// ==================================================================

/// `GROUP BY`・集約関数の引数(`Aggregate`がある場合)、または`SELECT`の対象式
/// (無い場合)が実際に参照する列だけが、`Scan`から先の演算子を流れるようにする。
///
/// # 適用範囲: `JOIN`を伴う計画に限定する理由と、`Filter`直下の`Scan`を除く理由
///
/// この章のProjection Pruningが列を削るのは、`Aggregate`または`Projection`の
/// 直下が`Filter`・`Join`を経由して`Scan`へ至る場合に限る。`Filter`も`Join`も
/// 経由せず`Aggregate`・`Projection`が直接`Scan`の親であるとき([`prune_plan`]の
/// ガード)は、そもそも列を運ぶ演算子が1段しか無く、剪定用の`Projection`を
/// 挟んでも運ぶ手間が1段から2段に増えるだけで得るものが無いため、対象から
/// 外す。
///
/// `Filter`の直下にある`Scan`も、[`prune_scope`]が意図的に手を付けない
/// (`Filter`と`Scan`の間へ剪定用の`Projection`を挟まない)。第25章の
/// `choose_access_path`は、`Filter`の直下が`Scan`そのものであることを前提に
/// 索引アクセスパスを選ぶ。この位置に`Projection`を挟むと、その前提が崩れて
/// 索引が二度と選ばれなくなってしまう。`Join`の`right`(内側テーブル)が
/// 剥き出しの`Scan`である場合も同様に対象から外す。第25章の
/// `index_scan_target`は`right`が`PhysicalPlan::SeqScan`のままであることを
/// 前提にIndex Nested Loop Joinを選んでおり、ここに`Projection`を挟むと
/// この判定が素通りされてしまう。
///
/// この2つの除外によって、この章のProjection Pruningが実際に列を削るのは
/// 「`WHERE`の効かない列を持つテーブルが`JOIN`の外側(`left`)に来る場合」と
/// 「複数の`JOIN`が連なる左深い木の中間段」にほぼ限られる。単一テーブルの
/// `SELECT`(`Filter`の直下がすぐ`Scan`になる、この教材でこれまで見てきた
/// 大半のクエリ)では、この章のProjection Pruningはほとんど何もしない。
/// これは手落ちではなく、単一テーブルの計画では`Scan`が返す行を読むのは
/// `Filter`・`Projection`のうち高々1つずつであり、削って得をする「使われない
/// 列を持ったまま何段も運ばれる行」がそもそも存在しないからである。実際に
/// 効果が測れるのは、`HashJoin`のBuild段階が内側テーブルの全行をハッシュ
/// テーブルへ積み込む場面である(測ってみる節を参照)。
pub struct ProjectionPruning;

impl Rule for ProjectionPruning {
    fn name(&self) -> &str {
        "ProjectionPruning"
    }

    fn apply(&self, plan: LogicalPlan, _functions: &FunctionRegistry) -> (LogicalPlan, bool) {
        prune_plan(plan)
    }
}

/// `plan`が`Scan`・`Values`・`Filter`・`Join`だけで構成されているか
/// (`GROUP BY`・`SELECT`より下の、テーブルの列を直接参照する領域か)。
fn is_from_where_subtree(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Scan(_) | LogicalPlan::Values(_) => true,
        LogicalPlan::Filter(filter) => is_from_where_subtree(&filter.input),
        LogicalPlan::Join(join) => is_from_where_subtree(&join.left) && is_from_where_subtree(&join.right),
        _ => false,
    }
}

fn prune_plan(plan: LogicalPlan) -> (LogicalPlan, bool) {
    match plan {
        LogicalPlan::Aggregate(mut aggregate) => {
            if is_from_where_subtree(&aggregate.input) && !is_bare_leaf(&aggregate.input) {
                let mut required = BTreeSet::new();
                for expr in &aggregate.group_by {
                    collect_columns(expr, &mut required);
                }
                for call in &aggregate.calls {
                    if let Some(arg) = &call.arg {
                        collect_columns(arg, &mut required);
                    }
                }
                let (input, map, changed) = prune_scope(*aggregate.input, &required);
                aggregate.group_by = aggregate.group_by.into_iter().map(|expr| remap_expr(expr, &map)).collect();
                aggregate.calls = aggregate
                    .calls
                    .into_iter()
                    .map(|call| AggregateCall { func: call.func, arg: call.arg.map(|arg| Box::new(remap_expr(*arg, &map))) })
                    .collect();
                aggregate.input = Box::new(input);
                (LogicalPlan::Aggregate(aggregate), changed)
            } else {
                let (input, changed) = prune_plan(*aggregate.input);
                aggregate.input = Box::new(input);
                (LogicalPlan::Aggregate(aggregate), changed)
            }
        }
        LogicalPlan::Projection(mut projection) => {
            if is_from_where_subtree(&projection.input) && !is_bare_leaf(&projection.input) {
                let mut required = BTreeSet::new();
                for item in &projection.projection {
                    collect_columns(&item.expr, &mut required);
                }
                let (input, map, changed) = prune_scope(*projection.input, &required);
                projection.projection = projection
                    .projection
                    .into_iter()
                    .map(|item| BoundSelectItem { expr: remap_expr(item.expr, &map), output_name: item.output_name })
                    .collect();
                projection.input = Box::new(input);
                (LogicalPlan::Projection(projection), changed)
            } else {
                let (input, changed) = prune_plan(*projection.input);
                projection.input = Box::new(input);
                (LogicalPlan::Projection(projection), changed)
            }
        }
        other => map_plan_children(other, prune_plan),
    }
}

/// `plan`が`Scan`・`Values`そのもの(`Filter`・`Join`を経由しない剥き出しの葉)か。
fn is_bare_leaf(plan: &LogicalPlan) -> bool {
    matches!(plan, LogicalPlan::Scan(_) | LogicalPlan::Values(_))
}

/// `plan`(`Scan`・`Filter`・`Join`だけからなる部分木)を、`required`(この
/// 部分木の出力のうち上位で実際に必要とされる列の添字の集合)だけになるよう
/// 剪定する。戻り値の`BTreeMap`は、剪定前の添字から剪定後の添字への対応で
/// あり、`required`に含まれるどの添字もキーとして存在することが呼び出し側の
/// 前提になる。
fn prune_scope(plan: LogicalPlan, required: &BTreeSet<usize>) -> (LogicalPlan, BTreeMap<usize, usize>, bool) {
    match plan {
        LogicalPlan::Scan(scan) => prune_scan(scan, required),
        LogicalPlan::Values(values) => {
            let map = identity_map(values.schema.len());
            (LogicalPlan::Values(values), map, false)
        }
        LogicalPlan::Filter(filter) => {
            let mut child_required = required.clone();
            collect_columns(&filter.predicate, &mut child_required);
            if matches!(*filter.input, LogicalPlan::Scan(_)) {
                // 第25章のchoose_access_pathがFilter直下のScanに依存する
                // ため、この位置のScanは剪定しない(モジュールドキュメント参照)。
                let LogicalPlan::Scan(scan) = *filter.input else { unreachable!() };
                let map = identity_map(scan.schema.len());
                let plan = LogicalPlan::Filter(FilterNode { input: Box::new(LogicalPlan::Scan(scan)), predicate: filter.predicate });
                (plan, map, false)
            } else {
                let (input, map, changed) = prune_scope(*filter.input, &child_required);
                let predicate = remap_expr(filter.predicate, &map);
                (LogicalPlan::Filter(FilterNode { input: Box::new(input), predicate }), map, changed)
            }
        }
        LogicalPlan::Join(join) => {
            let left_len = join.left.output_schema().len();
            let mut required_left = BTreeSet::new();
            let mut required_right = BTreeSet::new();
            for &global in required {
                split_by_side(global, left_len, &mut required_left, &mut required_right);
            }
            let mut condition_columns = BTreeSet::new();
            collect_columns(&join.condition, &mut condition_columns);
            for global in condition_columns {
                split_by_side(global, left_len, &mut required_left, &mut required_right);
            }

            let (new_left, map_left, c1) = prune_scope(*join.left, &required_left);
            let new_left_len = new_left.output_schema().len();

            let (new_right, map_right, c2) = if matches!(join.right.as_ref(), LogicalPlan::Scan(_)) {
                // 第25章のindex_scan_targetがJoinのright直下のSeqScanに
                // 依存するため、この位置のScanは剪定しない(モジュール
                // ドキュメント参照)。
                let LogicalPlan::Scan(scan) = *join.right else { unreachable!() };
                let map = identity_map(scan.schema.len());
                (LogicalPlan::Scan(scan), map, false)
            } else {
                prune_scope(*join.right, &required_right)
            };

            let mut combined = BTreeMap::new();
            for (&old, &new) in &map_left {
                combined.insert(old, new);
            }
            for (&old, &new) in &map_right {
                combined.insert(left_len + old, new_left_len + new);
            }
            let condition = remap_expr(join.condition, &combined);
            let new_join =
                LogicalPlan::Join(JoinNode { left: Box::new(new_left), right: Box::new(new_right), kind: join.kind, condition });

            let mut out_map = BTreeMap::new();
            for &global in required {
                out_map.insert(global, *combined.get(&global).expect("requiredの各添字はcombinedに含めてある"));
            }
            (new_join, out_map, c1 || c2)
        }
        other => unreachable!("prune_scopeの対象はScan・Values・Filter・Joinに限る(is_from_where_subtreeが保証する): {other:?}"),
    }
}

fn split_by_side(global: usize, left_len: usize, left: &mut BTreeSet<usize>, right: &mut BTreeSet<usize>) {
    if global < left_len {
        left.insert(global);
    } else {
        right.insert(global - left_len);
    }
}

/// `scan`の列のうち`required`に含まれるものだけを残す。`required`が全列を
/// 覆っている(削れる列が無い)場合は`scan`をそのまま返す。
fn prune_scan(scan: ScanNode, required: &BTreeSet<usize>) -> (LogicalPlan, BTreeMap<usize, usize>, bool) {
    let width = scan.schema.len();
    if required.len() >= width {
        return (LogicalPlan::Scan(scan), identity_map(width), false);
    }

    let kept: Vec<usize> = required.iter().copied().collect(); // BTreeSetなので昇順
    let map: BTreeMap<usize, usize> = kept.iter().enumerate().map(|(new_index, &old_index)| (old_index, new_index)).collect();
    let projection: Vec<BoundSelectItem> = kept
        .iter()
        .map(|&old_index| {
            let column = &scan.schema.columns()[old_index];
            BoundSelectItem {
                expr: BoundExpr::ColumnRef {
                    table_ordinal: 0,
                    column_index: old_index,
                    name: column.name.clone(),
                    data_type: column.data_type,
                    span: Span::new(0, 0),
                },
                output_name: column.name.clone(),
            }
        })
        .collect();
    let plan = crate::logical_plan::LogicalPlan::Projection(crate::logical_plan::ProjectionNode {
        input: Box::new(LogicalPlan::Scan(scan)),
        projection,
    });
    (plan, map, true)
}

fn identity_map(width: usize) -> BTreeMap<usize, usize> {
    (0..width).map(|i| (i, i)).collect()
}

/// `expr`が参照する列(`ColumnRef::column_index`)をすべて`out`へ集める。
fn collect_columns(expr: &BoundExpr, out: &mut BTreeSet<usize>) {
    match expr {
        BoundExpr::ColumnRef { column_index, .. } => {
            out.insert(*column_index);
        }
        BoundExpr::IntLiteral { .. } | BoundExpr::StringLiteral { .. } | BoundExpr::BoolLiteral { .. } | BoundExpr::NullLiteral { .. } => {}
        BoundExpr::UnaryOp { expr, .. } | BoundExpr::Paren { expr, .. } | BoundExpr::Cast { expr, .. } => collect_columns(expr, out),
        BoundExpr::BinaryOp { lhs, rhs, .. } => {
            collect_columns(lhs, out);
            collect_columns(rhs, out);
        }
        BoundExpr::IsNull { expr, .. } => collect_columns(expr, out),
        BoundExpr::FunctionCall { args, .. } => args.iter().for_each(|arg| collect_columns(arg, out)),
        BoundExpr::Aggregate { arg, .. } => {
            if let Some(arg) = arg {
                collect_columns(arg, out);
            }
        }
    }
}

/// `expr`の中の`ColumnRef::column_index`を、`map`が指す新しい添字へ書き換える。
/// `map`は`expr`が参照する全ての添字をキーとして持つことが前提
/// ([`collect_columns`]で集めた添字を`required`へ含めてから[`prune_scope`]を
/// 呼ぶことで保証する)。
fn remap_expr(expr: BoundExpr, map: &BTreeMap<usize, usize>) -> BoundExpr {
    match expr {
        BoundExpr::ColumnRef { table_ordinal, column_index, name, data_type, span } => {
            let column_index = *map.get(&column_index).unwrap_or_else(|| {
                unreachable!("プルーニング対象の列はcollect_columnsで必ずrequiredに含めてある: {column_index}")
            });
            BoundExpr::ColumnRef { table_ordinal, column_index, name, data_type, span }
        }
        literal @ (BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. }) => literal,
        BoundExpr::UnaryOp { op, expr, data_type, span } => {
            BoundExpr::UnaryOp { op, expr: Box::new(remap_expr(*expr, map)), data_type, span }
        }
        BoundExpr::BinaryOp { op, lhs, rhs, data_type, span } => BoundExpr::BinaryOp {
            op,
            lhs: Box::new(remap_expr(*lhs, map)),
            rhs: Box::new(remap_expr(*rhs, map)),
            data_type,
            span,
        },
        BoundExpr::IsNull { expr, negated, span } => BoundExpr::IsNull { expr: Box::new(remap_expr(*expr, map)), negated, span },
        BoundExpr::FunctionCall { name, args, data_type, span } => BoundExpr::FunctionCall {
            name,
            args: args.into_iter().map(|arg| remap_expr(arg, map)).collect(),
            data_type,
            span,
        },
        BoundExpr::Paren { expr, span } => BoundExpr::Paren { expr: Box::new(remap_expr(*expr, map)), span },
        BoundExpr::Cast { expr, data_type, span } => BoundExpr::Cast { expr: Box::new(remap_expr(*expr, map)), data_type, span },
        BoundExpr::Aggregate { func, arg, data_type, span } => {
            BoundExpr::Aggregate { func, arg: arg.map(|arg| Box::new(remap_expr(*arg, map))), data_type, span }
        }
    }
}

// ==================================================================
// 共通のツリー巡回
// ==================================================================

/// `f`を`plan`の直接の子([`LogicalPlan`]が持つ`input`・`left`/`right`)へ
/// 再帰的に適用する。`f`自身が個々のノードに固有の書き換えを持つ場合
/// ([`merge_filters`]・[`pushdown_plan`]・[`prune_plan`]がそのノード型を
/// 特別扱いした場合)は、この関数の対象外(呼び出し元がすでに処理済み)である。
fn map_plan_children(plan: LogicalPlan, mut f: impl FnMut(LogicalPlan) -> (LogicalPlan, bool)) -> (LogicalPlan, bool) {
    match plan {
        LogicalPlan::Scan(_) | LogicalPlan::Values(_) => (plan, false),
        LogicalPlan::Filter(mut node) => {
            let (input, changed) = f(*node.input);
            node.input = Box::new(input);
            (LogicalPlan::Filter(node), changed)
        }
        LogicalPlan::Join(mut node) => {
            let (left, c1) = f(*node.left);
            let (right, c2) = f(*node.right);
            node.left = Box::new(left);
            node.right = Box::new(right);
            (LogicalPlan::Join(node), c1 || c2)
        }
        LogicalPlan::Aggregate(mut node) => {
            let (input, changed) = f(*node.input);
            node.input = Box::new(input);
            (LogicalPlan::Aggregate(node), changed)
        }
        LogicalPlan::Projection(mut node) => {
            let (input, changed) = f(*node.input);
            node.input = Box::new(input);
            (LogicalPlan::Projection(node), changed)
        }
        LogicalPlan::Distinct(mut node) => {
            let (input, changed) = f(*node.input);
            node.input = Box::new(input);
            (LogicalPlan::Distinct(node), changed)
        }
        LogicalPlan::Sort(mut node) => {
            let (input, changed) = f(*node.input);
            node.input = Box::new(input);
            (LogicalPlan::Sort(node), changed)
        }
        LogicalPlan::Limit(mut node) => {
            let (input, changed) = f(*node.input);
            node.input = Box::new(input);
            (LogicalPlan::Limit(node), changed)
        }
        LogicalPlan::Insert(mut node) => {
            let (input, changed) = f(*node.input);
            node.input = Box::new(input);
            (LogicalPlan::Insert(node), changed)
        }
        LogicalPlan::Update(mut node) => {
            let (input, changed) = f(*node.input);
            node.input = Box::new(input);
            (LogicalPlan::Update(node), changed)
        }
        LogicalPlan::Delete(mut node) => {
            let (input, changed) = f(*node.input);
            node.input = Box::new(input);
            (LogicalPlan::Delete(node), changed)
        }
    }
}

/// [`map_plan_children`]と違い、木を再帰的にたどりながら**各ノードが持つ
/// `BoundExpr`フィールド**([`Filter::predicate`]・[`JoinNode::condition`]・
/// `Aggregate`の`group_by`/`calls`・`Projection`の`projection`・`Sort`の
/// `keys`・`Update`の`assignments`/`predicate`・`Delete`の`predicate`)へ
/// `expr_fn`を適用する。[`ConstantFolding`]・[`BooleanSimplification`]は
/// どちらも「木の形は変えず、木の中の式だけを書き換える」ルールであり、
/// この巡回を共有することで同じ12種類の`match`をルールごとに書き下ろす
/// 重複を避けている(第18章の`projection_schema`と同じ、決め方を1箇所に
/// 集める方針)。
fn map_plan_exprs(plan: LogicalPlan, expr_fn: &mut impl FnMut(BoundExpr) -> (BoundExpr, bool)) -> (LogicalPlan, bool) {
    match plan {
        LogicalPlan::Scan(_) | LogicalPlan::Values(_) => (plan, false),
        LogicalPlan::Filter(mut node) => {
            let (input, c1) = map_plan_exprs(*node.input, expr_fn);
            let (predicate, c2) = expr_fn(node.predicate);
            node.input = Box::new(input);
            node.predicate = predicate;
            (LogicalPlan::Filter(node), c1 || c2)
        }
        LogicalPlan::Join(mut node) => {
            let (left, c1) = map_plan_exprs(*node.left, expr_fn);
            let (right, c2) = map_plan_exprs(*node.right, expr_fn);
            let (condition, c3) = expr_fn(node.condition);
            node.left = Box::new(left);
            node.right = Box::new(right);
            node.condition = condition;
            (LogicalPlan::Join(node), c1 || c2 || c3)
        }
        LogicalPlan::Aggregate(node) => {
            let AggregateNode { input, group_by, calls, schema } = node;
            let (input, mut changed) = map_plan_exprs(*input, expr_fn);
            let group_by: Vec<BoundExpr> = group_by
                .into_iter()
                .map(|expr| {
                    let (expr, c) = expr_fn(expr);
                    changed |= c;
                    expr
                })
                .collect();
            let calls: Vec<AggregateCall> = calls
                .into_iter()
                .map(|call| {
                    let arg = call.arg.map(|arg| {
                        let (arg, c) = expr_fn(*arg);
                        changed |= c;
                        Box::new(arg)
                    });
                    AggregateCall { func: call.func, arg }
                })
                .collect();
            (LogicalPlan::Aggregate(AggregateNode { input: Box::new(input), group_by, calls, schema }), changed)
        }
        LogicalPlan::Projection(mut node) => {
            let (input, mut changed) = map_plan_exprs(*node.input, expr_fn);
            let projection: Vec<BoundSelectItem> = node
                .projection
                .into_iter()
                .map(|item| {
                    let (expr, c) = expr_fn(item.expr);
                    changed |= c;
                    BoundSelectItem { expr, output_name: item.output_name }
                })
                .collect();
            node.input = Box::new(input);
            node.projection = projection;
            (LogicalPlan::Projection(node), changed)
        }
        LogicalPlan::Distinct(node) => {
            let DistinctNode { input } = node;
            let (input, changed) = map_plan_exprs(*input, expr_fn);
            (LogicalPlan::Distinct(DistinctNode { input: Box::new(input) }), changed)
        }
        LogicalPlan::Sort(node) => {
            let SortNode { input, keys } = node;
            let (input, mut changed) = map_plan_exprs(*input, expr_fn);
            let keys: Vec<SortKey> = keys
                .into_iter()
                .map(|key| {
                    let (expr, c) = expr_fn(key.expr);
                    changed |= c;
                    SortKey { expr, desc: key.desc }
                })
                .collect();
            (LogicalPlan::Sort(SortNode { input: Box::new(input), keys }), changed)
        }
        LogicalPlan::Limit(node) => {
            let LimitNode { input, limit, offset } = node;
            let (input, changed) = map_plan_exprs(*input, expr_fn);
            (LogicalPlan::Limit(LimitNode { input: Box::new(input), limit, offset }), changed)
        }
        LogicalPlan::Insert(node) => {
            let InsertNode { table_id, table_name, schema, columns, input } = node;
            let (input, changed) = map_plan_exprs(*input, expr_fn);
            (LogicalPlan::Insert(InsertNode { table_id, table_name, schema, columns, input: Box::new(input) }), changed)
        }
        LogicalPlan::Update(node) => {
            let UpdateNode { table_id, table_name, schema, assignments, predicate, input } = node;
            let (input, mut changed) = map_plan_exprs(*input, expr_fn);
            let assignments: Vec<BoundAssignment> = assignments
                .into_iter()
                .map(|assignment| {
                    let (value, c) = expr_fn(assignment.value);
                    changed |= c;
                    BoundAssignment { column_index: assignment.column_index, value }
                })
                .collect();
            let predicate = predicate.map(|predicate| {
                let (predicate, c) = expr_fn(predicate);
                changed |= c;
                predicate
            });
            (
                LogicalPlan::Update(UpdateNode { table_id, table_name, schema, assignments, predicate, input: Box::new(input) }),
                changed,
            )
        }
        LogicalPlan::Delete(node) => {
            let DeleteNode { table_id, table_name, schema, predicate, input } = node;
            let (input, mut changed) = map_plan_exprs(*input, expr_fn);
            let predicate = predicate.map(|predicate| {
                let (predicate, c) = expr_fn(predicate);
                changed |= c;
                predicate
            });
            (LogicalPlan::Delete(DeleteNode { table_id, table_name, schema, predicate, input: Box::new(input) }), changed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{Binder, BoundStatement};
    use crate::catalog::Catalog;
    use crate::logical_plan::build_select;
    use crate::parser::parse_statement;
    use crate::types::{Column, DataType, Schema};

    type TableSpec<'a> = (&'a str, &'a [(&'a str, DataType, bool)]);

    fn catalog_with(tables: &[TableSpec]) -> Catalog {
        let mut catalog = Catalog::new();
        for (name, columns) in tables {
            let schema = Schema::new(columns.iter().map(|(n, t, nullable)| Column::new(*n, *t, *nullable)).collect());
            catalog.create_table(name, schema).unwrap();
        }
        catalog
    }

    fn plan(sql: &str, catalog: &Catalog) -> LogicalPlan {
        let statement = parse_statement(sql).unwrap();
        let functions = FunctionRegistry::with_builtins();
        let bound = Binder::new(catalog, &functions, sql).bind(statement).unwrap();
        let BoundStatement::Select(select) = bound else { panic!("Selectを期待した") };
        build_select(*select)
    }

    fn users_orders_catalog() -> Catalog {
        catalog_with(&[
            ("users", &[("id", DataType::BigInt, false), ("name", DataType::Text, true), ("bio", DataType::Text, true)]),
            ("orders", &[("id", DataType::BigInt, false), ("user_id", DataType::BigInt, false), ("amount", DataType::BigInt, false)]),
        ])
    }

    // ---- ConstantFolding ----

    #[test]
    fn constant_folding_evaluates_arithmetic_without_columns() {
        let catalog = catalog_with(&[("t", &[("id", DataType::BigInt, false)])]);
        let logical = plan("SELECT id FROM t WHERE id = 1 + 1", &catalog);
        let functions = FunctionRegistry::with_builtins();
        let (folded, changed) = ConstantFolding.apply(logical, &functions);
        assert!(changed);
        assert_eq!(folded.to_string(), "Projection(id)\n  └─ Filter(id = 2)\n    └─ Scan(t)\n");
    }

    #[test]
    fn constant_folding_leaves_division_by_zero_unfolded() {
        let catalog = catalog_with(&[("t", &[("id", DataType::BigInt, false)])]);
        let logical = plan("SELECT id FROM t WHERE id = 1 / 0", &catalog);
        let functions = FunctionRegistry::with_builtins();
        let (folded, changed) = ConstantFolding.apply(logical, &functions);
        assert!(!changed);
        assert_eq!(folded.to_string(), "Projection(id)\n  └─ Filter(id = 1 / 0)\n    └─ Scan(t)\n");
    }

    #[test]
    fn constant_folding_does_not_touch_column_references() {
        let catalog = catalog_with(&[("t", &[("id", DataType::BigInt, false)])]);
        let logical = plan("SELECT id FROM t WHERE id = id", &catalog);
        let functions = FunctionRegistry::with_builtins();
        let (folded, changed) = ConstantFolding.apply(logical, &functions);
        assert!(!changed);
        assert_eq!(folded.to_string(), "Projection(id)\n  └─ Filter(id = id)\n    └─ Scan(t)\n");
    }

    // ---- BooleanSimplification ----

    #[test]
    fn boolean_simplification_drops_true_and() {
        let catalog = catalog_with(&[("t", &[("id", DataType::BigInt, false)])]);
        let logical = plan("SELECT id FROM t WHERE TRUE AND id = 1", &catalog);
        let functions = FunctionRegistry::with_builtins();
        let (simplified, changed) = BooleanSimplification.apply(logical, &functions);
        assert!(changed);
        assert_eq!(simplified.to_string(), "Projection(id)\n  └─ Filter(id = 1)\n    └─ Scan(t)\n");
    }

    #[test]
    fn boolean_simplification_short_circuits_false_and() {
        let catalog = catalog_with(&[("t", &[("id", DataType::BigInt, false)])]);
        let logical = plan("SELECT id FROM t WHERE FALSE AND id = 1", &catalog);
        let functions = FunctionRegistry::with_builtins();
        let (simplified, changed) = BooleanSimplification.apply(logical, &functions);
        assert!(changed);
        assert_eq!(simplified.to_string(), "Projection(id)\n  └─ Filter(false)\n    └─ Scan(t)\n");
    }

    #[test]
    fn boolean_simplification_collapses_double_not() {
        let catalog = catalog_with(&[("t", &[("id", DataType::BigInt, false)])]);
        let logical = plan("SELECT id FROM t WHERE NOT NOT (id = 1)", &catalog);
        let functions = FunctionRegistry::with_builtins();
        let (simplified, changed) = BooleanSimplification.apply(logical, &functions);
        assert!(changed);
        assert_eq!(simplified.to_string(), "Projection(id)\n  └─ Filter((id = 1))\n    └─ Scan(t)\n");
    }

    /// 三値論理のもとでの正しさ: `col`が`NULL`の行を含む複数の行に対して、
    /// 簡約前後で`WHERE`の一致結果が変わらないことを確認する
    /// (`FALSE AND x`・`x AND FALSE`・`TRUE OR x`・`x OR TRUE`の4パターン)。
    #[test]
    fn boolean_simplification_preserves_results_with_null_rows() {
        use crate::database::Database;

        for predicate in [
            "FALSE AND col = 1",
            "col = 1 AND FALSE",
            "TRUE OR col = 1",
            "col = 1 OR TRUE",
            "TRUE AND col = 1",
            "col = 1 AND TRUE",
            "FALSE OR col = 1",
            "col = 1 OR FALSE",
        ] {
            let mut db = Database::memory();
            db.execute("CREATE TABLE t (id BIGINT NOT NULL, col BIGINT)").unwrap();
            db.execute("INSERT INTO t VALUES (1, 1), (2, NULL), (3, 2)").unwrap();

            let query = format!("SELECT id FROM t WHERE {predicate}");
            let with_rules: Vec<i64> = db
                .execute(&query)
                .unwrap()
                .rows()
                .iter()
                .map(|row| match &row.values()[0] {
                    crate::types::Value::BigInt(n) => *n,
                    other => panic!("BigIntを期待した: {other:?}"),
                })
                .collect();

            // 手で三値論理を評価した期待値(FALSE AND x/x AND FALSEは常に空、
            // TRUE OR x/x OR TRUEは常に全行、TRUE AND x/x AND TRUE・
            // FALSE OR x/x OR FALSEはcol = 1のみ)。
            let expected: Vec<i64> = if predicate.contains("FALSE AND") || predicate.contains("AND FALSE") {
                vec![]
            } else if predicate.contains("TRUE OR") || predicate.contains("OR TRUE") {
                vec![1, 2, 3]
            } else {
                vec![1]
            };
            assert_eq!(with_rules, expected, "predicate={predicate}");
        }
    }

    // ---- FilterMerge ----

    #[test]
    fn filter_merge_combines_consecutive_filters() {
        let catalog = catalog_with(&[("t", &[("id", DataType::BigInt, false), ("amount", DataType::BigInt, false)])]);
        // Filter(Filter(Scan))という形を直接組み立てる(build_selectは1個の
        // WHEREから連続するFilterを作らないため、テスト用に手で構築する)。
        let base = plan("SELECT id FROM t WHERE id = 1", &catalog);
        let LogicalPlan::Projection(projection) = base else { panic!() };
        let LogicalPlan::Filter(inner) = *projection.input else { panic!() };
        let outer_predicate = BoundExpr::BinaryOp {
            op: BinaryOperator::Eq,
            lhs: Box::new(BoundExpr::ColumnRef {
                table_ordinal: 0,
                column_index: 1,
                name: "amount".to_string(),
                data_type: DataType::BigInt,
                span: Span::new(0, 0),
            }),
            rhs: Box::new(BoundExpr::IntLiteral { value: 100, span: Span::new(0, 0) }),
            data_type: DataType::Boolean,
            span: Span::new(0, 0),
        };
        let nested = LogicalPlan::Filter(FilterNode {
            input: Box::new(LogicalPlan::Filter(inner)),
            predicate: outer_predicate,
        });

        let functions = FunctionRegistry::with_builtins();
        let (merged, changed) = FilterMerge.apply(nested, &functions);
        assert!(changed);
        assert_eq!(merged.to_string(), "Filter(id = 1 AND amount = 100)\n  └─ Scan(t)\n");
    }

    // ---- PredicatePushdown ----

    #[test]
    fn predicate_pushdown_splits_conjuncts_by_side() {
        let catalog = users_orders_catalog();
        let logical = plan(
            "SELECT name FROM users JOIN orders ON users.id = orders.user_id \
             WHERE users.id = 1 AND orders.amount > 100",
            &catalog,
        );
        let functions = FunctionRegistry::with_builtins();
        let (pushed, changed) = PredicatePushdown.apply(logical, &functions);
        assert!(changed);
        assert_eq!(
            pushed.to_string(),
            "Projection(name)\n  \
             └─ Join(INNER JOIN, id = user_id)\n    \
             └─ Filter(id = 1)\n      \
             └─ Scan(users)\n    \
             └─ Filter(amount > 100)\n      \
             └─ Scan(orders)\n"
        );
    }

    #[test]
    fn predicate_pushdown_keeps_cross_table_predicate_above_join() {
        let catalog = users_orders_catalog();
        let logical = plan("SELECT name FROM users JOIN orders ON users.id = orders.user_id WHERE users.id < orders.amount", &catalog);
        let functions = FunctionRegistry::with_builtins();
        let (pushed, changed) = PredicatePushdown.apply(logical, &functions);
        assert!(!changed);
        assert_eq!(
            pushed.to_string(),
            "Projection(name)\n  \
             └─ Filter(id < amount)\n    \
             └─ Join(INNER JOIN, id = user_id)\n      \
             └─ Scan(users)\n      \
             └─ Scan(orders)\n"
        );
    }

    #[test]
    fn predicate_pushdown_does_not_change_query_results() {
        use crate::database::Database;

        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)").unwrap();
        db.execute("CREATE TABLE orders (id BIGINT NOT NULL, user_id BIGINT NOT NULL, amount BIGINT NOT NULL)").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')").unwrap();
        db.execute("INSERT INTO orders VALUES (1, 1, 50), (2, 1, 150), (3, 2, 200)").unwrap();

        let query = "SELECT users.name, orders.amount FROM users JOIN orders ON users.id = orders.user_id \
                     WHERE users.id = 1 AND orders.amount > 100";
        let rows: Vec<Vec<Value>> = db.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
        assert_eq!(rows, vec![vec![Value::Text("Alice".to_string()), Value::BigInt(150)]]);
    }

    // ---- ProjectionPruning ----

    #[test]
    fn projection_pruning_alone_keeps_join_key_even_if_unprojected() {
        let catalog = users_orders_catalog();
        // PredicatePushdownを経由していないので、WHEREはまだJoinの真上にある。
        // usersのbioは誰にも参照されないので落ちるが、idはJoin条件がまだ
        // 参照しているので(Filterの述語もJoin条件も両方requiredに入る)残る。
        let logical = plan(
            "SELECT name, amount FROM users JOIN orders ON users.id = orders.user_id WHERE users.id = 1",
            &catalog,
        );
        let functions = FunctionRegistry::with_builtins();
        let (pruned, changed) = ProjectionPruning.apply(logical, &functions);
        assert!(changed);
        assert_eq!(
            pruned.to_string(),
            "Projection(name, amount)\n  \
             └─ Filter(id = 1)\n    \
             └─ Join(INNER JOIN, id = user_id)\n      \
             └─ Projection(id, name)\n        \
             └─ Scan(users)\n      \
             └─ Scan(orders)\n"
        );
    }

    #[test]
    fn optimize_prunes_the_unfiltered_left_side_but_protects_the_filtered_right_side() {
        // ordersだけが列を持て余す(noteは誰も使わない)。WHEREはordersにしか
        // 掛からないので、PredicatePushdown後にordersはFilter直下のScanになり
        // (第25章の索引アクセスパス保護の対象、この章では剪定しない)、
        // 4列のまま残る。一方usersにはWHEREが無いので、Join直下の剥き出しの
        // Scanのまま(第25章のIndex Nested Loop Join保護はJoinの`right`にしか
        // 掛からないので、`left`側のusersはこの保護の対象にならない)剪定でき、
        // Join条件が必要とする`id`だけを残してname・bioを落とす。
        let catalog = catalog_with(&[
            ("users", &[("id", DataType::BigInt, false), ("name", DataType::Text, true), ("bio", DataType::Text, true)]),
            (
                "orders",
                &[
                    ("id", DataType::BigInt, false),
                    ("user_id", DataType::BigInt, false),
                    ("amount", DataType::BigInt, false),
                    ("note", DataType::Text, true),
                ],
            ),
        ]);
        let logical = plan(
            "SELECT amount FROM users JOIN orders ON users.id = orders.user_id WHERE orders.amount > 100",
            &catalog,
        );
        let functions = FunctionRegistry::with_builtins();
        let optimized = optimize(logical, &functions);
        assert_eq!(
            optimized.to_string(),
            "Projection(amount)\n  \
             └─ Join(INNER JOIN, id = user_id)\n    \
             └─ Projection(id)\n      \
             └─ Scan(users)\n    \
             └─ Filter(amount > 100)\n      \
             └─ Scan(orders)\n"
        );
    }

    #[test]
    fn projection_pruning_leaves_single_table_plan_unchanged() {
        let catalog = catalog_with(&[("t", &[("id", DataType::BigInt, false), ("name", DataType::Text, true)])]);
        let logical = plan("SELECT id FROM t WHERE id = 1", &catalog);
        let functions = FunctionRegistry::with_builtins();
        let (pruned, changed) = ProjectionPruning.apply(logical, &functions);
        assert!(!changed);
        assert_eq!(pruned.to_string(), "Projection(id)\n  └─ Filter(id = 1)\n    └─ Scan(t)\n");
    }

    #[test]
    fn projection_pruning_does_not_change_query_results() {
        use crate::database::Database;

        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT, bio TEXT)").unwrap();
        db.execute("CREATE TABLE orders (id BIGINT NOT NULL, user_id BIGINT NOT NULL, amount BIGINT NOT NULL)").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice', 'hi'), (2, 'Bob', 'yo')").unwrap();
        db.execute("INSERT INTO orders VALUES (1, 1, 50), (2, 1, 150), (3, 2, 200)").unwrap();

        let query = "SELECT users.name, orders.amount FROM users JOIN orders ON users.id = orders.user_id";
        let rows: Vec<Vec<Value>> = db.execute(query).unwrap().rows().iter().map(|row| row.values().to_vec()).collect();
        let expected = vec![
            vec![Value::Text("Alice".to_string()), Value::BigInt(50)],
            vec![Value::Text("Alice".to_string()), Value::BigInt(150)],
            vec![Value::Text("Bob".to_string()), Value::BigInt(200)],
        ];
        assert_eq!(rows.len(), expected.len());
        for row in &expected {
            assert!(rows.contains(row), "{row:?}が結果に含まれていない: {rows:?}");
        }
    }

    // ---- optimize: 固定点までの反復 ----

    #[test]
    fn optimize_folds_then_simplifies_in_one_call() {
        // ConstantFoldingが`1 + 1 = 2`をTRUEへ畳み込んだ直後でなければ、
        // BooleanSimplificationは`TRUE AND id = 5`を認識できない
        // (モジュールドキュメントの例そのもの)。1回のoptimize呼び出しの中で
        // 両方のルールが繰り返し適用され、最終的に単純化されることを確認する。
        let catalog = catalog_with(&[("t", &[("id", DataType::BigInt, false)])]);
        let logical = plan("SELECT id FROM t WHERE (1 + 1 = 2) AND id = 5", &catalog);
        let functions = FunctionRegistry::with_builtins();
        let optimized = optimize(logical, &functions);
        assert_eq!(optimized.to_string(), "Projection(id)\n  └─ Filter(id = 5)\n    └─ Scan(t)\n");
    }

    #[test]
    fn optimize_is_idempotent() {
        let catalog = users_orders_catalog();
        let logical = plan(
            "SELECT users.name FROM users JOIN orders ON users.id = orders.user_id WHERE users.id = 1 AND orders.amount > 100",
            &catalog,
        );
        let functions = FunctionRegistry::with_builtins();
        let once = optimize(logical, &functions);
        let twice = optimize(once.clone(), &functions);
        assert_eq!(once, twice);
    }

    // ---- 測って確認する ----

    /// `WHERE`が片側のテーブルだけを参照する場合(押し下げ可能)と、両側を
    /// 参照する場合(押し下げ不可能、`Join`の真上に残る)とで、`HashJoin`の
    /// Build段階が読む`orders`の行数がどう変わるかを実測する。どちらの
    /// `WHERE`も`orders`のうち一致する行の割合は同じに揃えてあるので、
    /// 差の理由は「述語が`orders`のScan直上まで運ばれているか」だけである。
    #[test]
    #[ignore = "実行時間の計測用。cargo test --release --lib -- --ignored --nocapture で実行する"]
    fn predicate_pushdown_shrinks_hash_join_build_side() {
        use crate::database::Database;

        for m in [2_000usize, 8_000, 32_000] {
            let mut db = Database::memory();
            db.execute("CREATE TABLE customers (id BIGINT NOT NULL, token BIGINT NOT NULL)").unwrap();
            db.execute("CREATE TABLE orders (customer_id BIGINT NOT NULL, status BIGINT NOT NULL)").unwrap();

            let customer_rows: Vec<String> = (0..50i64).map(|i| format!("({i}, {i})")).collect();
            db.execute(&format!("INSERT INTO customers VALUES {}", customer_rows.join(", "))).unwrap();
            // statusが0の行だけが一致する(1/100の選択率)。cross-table版では
            // 同じ選択率を「customers.tokenと一致する」という、pushdownが
            // 対象にしない形の条件で表す。
            let order_rows: Vec<String> =
                (0..m as i64).map(|i| format!("({}, {})", i % 50, if i % 100 == 0 { 0 } else { i + 1 })).collect();
            db.execute(&format!("INSERT INTO orders VALUES {}", order_rows.join(", "))).unwrap();

            let pushable = "SELECT customers.id FROM customers JOIN orders ON customers.id = orders.customer_id \
                             WHERE orders.status = 0";
            let not_pushable = "SELECT customers.id FROM customers JOIN orders ON customers.id = orders.customer_id \
                                 WHERE orders.status = customers.token";

            let pushable_plan = db.execute(&format!("EXPLAIN {pushable}")).unwrap().to_string();
            assert!(pushable_plan.contains("Filter(status = 0)"), "{pushable_plan}");
            let not_pushable_plan = db.execute(&format!("EXPLAIN {not_pushable}")).unwrap().to_string();
            assert!(not_pushable_plan.contains("Filter(status = token)"), "{not_pushable_plan}");

            let start = std::time::Instant::now();
            db.execute(pushable).unwrap();
            let pushable_elapsed = start.elapsed();

            let start = std::time::Instant::now();
            db.execute(not_pushable).unwrap();
            let not_pushable_elapsed = start.elapsed();

            eprintln!("m={m:>6}  pushable={pushable_elapsed:>10?}  not_pushable={not_pushable_elapsed:>10?}");
        }
    }
}
